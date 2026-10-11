//! Limits and the SQL value -> JSON mapping (documented in README "Type mapping").
//!
//! | SQL storage class | JSON |
//! | --- | --- |
//! | NULL | `null` |
//! | INTEGER | number (exact 64-bit integer; JavaScript clients lose precision above 2^53) |
//! | REAL, finite | number |
//! | REAL, NaN or +/-Infinity | `null` (JSON has no such numbers) |
//! | TEXT, valid UTF-8 | string (NUL bytes kept, escaped as `\u0000`) |
//! | TEXT, invalid UTF-8 | `{"$text_base64": "..."}` (bytes kept exactly) |
//! | BLOB | `{"$blob_base64": "..."}` (standard alphabet, padded) |

use crate::error::DbError;
use lmrust::server::ErrorCode;
use serde_json::{json, Value};
use std::time::Duration;

pub const DEFAULT_MAX_ROWS: u64 = 200;
pub const HARD_MAX_ROWS: u64 = 10_000;
pub const DEFAULT_MAX_BYTES: u64 = 1 << 20;
pub const HARD_MAX_BYTES: u64 = 16 << 20;
pub const DEFAULT_TIMEOUT_MS: u64 = 10_000;
pub const HARD_TIMEOUT_MS: u64 = 300_000;

/// Caps for one call. `max_bytes` bounds the serialized `rows` array.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
  pub max_rows: usize,
  pub max_bytes: usize,
  pub timeout: Duration,
}

impl Default for Limits {
  fn default() -> Self {
    Limits {
      max_rows: DEFAULT_MAX_ROWS as usize,
      max_bytes: DEFAULT_MAX_BYTES as usize,
      timeout: Duration::from_millis(DEFAULT_TIMEOUT_MS),
    }
  }
}

impl Limits {
  /// Checked limits: each must be a whole number in `1..=hard cap`. Values above the hard cap
  /// are refused (never silently clamped).
  pub fn new(
    max_rows: Option<f64>,
    max_bytes: Option<f64>,
    timeout_ms: Option<f64>,
  ) -> Result<Self, DbError> {
    let check = |name: &str, value: Option<f64>, default: u64, cap: u64| -> Result<u64, DbError> {
      let Some(v) = value else {
        return Ok(default);
      };
      if v.fract() != 0.0 || v < 1.0 || v > cap as f64 {
        return Err(DbError::new(
          ErrorCode::ValidationError,
          "LIMIT_OUT_OF_RANGE",
          format!("{name} must be a whole number from 1 to {cap}"),
        ));
      }
      Ok(v as u64)
    };
    Ok(Limits {
      max_rows: check("maxRows", max_rows, DEFAULT_MAX_ROWS, HARD_MAX_ROWS)? as usize,
      max_bytes: check("maxBytes", max_bytes, DEFAULT_MAX_BYTES, HARD_MAX_BYTES)? as usize,
      timeout: Duration::from_millis(check(
        "timeoutMs",
        timeout_ms,
        DEFAULT_TIMEOUT_MS,
        HARD_TIMEOUT_MS,
      )?),
    })
  }
}

/// A SQLite value as JSON (see the module table).
pub fn to_json(value: rusqlite::types::ValueRef<'_>) -> Value {
  use rusqlite::types::ValueRef;
  match value {
    ValueRef::Null => Value::Null,
    ValueRef::Integer(i) => json!(i),
    ValueRef::Real(f) if f.is_finite() => json!(f),
    ValueRef::Real(_) => Value::Null,
    ValueRef::Text(bytes) => match std::str::from_utf8(bytes) {
      Ok(text) => Value::String(text.to_string()),
      Err(_) => json!({"$text_base64": base64(bytes)}),
    },
    ValueRef::Blob(bytes) => json!({"$blob_base64": base64(bytes)}),
  }
}

/// A lower bound of the serialized size of `value` (exact for numbers and plain ASCII text),
/// computed without encoding: huge values are refused before they are copied.
pub fn min_json_len(value: rusqlite::types::ValueRef<'_>) -> usize {
  use rusqlite::types::ValueRef;
  match value {
    ValueRef::Null => 4,
    ValueRef::Integer(_) | ValueRef::Real(_) => 1,
    ValueRef::Text(bytes) => bytes.len() + 2,
    ValueRef::Blob(bytes) => bytes.len().div_ceil(3) * 4 + 19, // {"$blob_base64":"..."}
  }
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard padded base64 (RFC 4648 section 4).
pub fn base64(bytes: &[u8]) -> String {
  let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
  for chunk in bytes.chunks(3) {
    let b = [
      chunk[0],
      *chunk.get(1).unwrap_or(&0),
      *chunk.get(2).unwrap_or(&0),
    ];
    let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
    for i in 0..4 {
      if i <= chunk.len() {
        out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
      } else {
        out.push('=');
      }
    }
  }
  out
}

#[cfg(test)]
mod tests {
  use super::*;
  use rusqlite::types::ValueRef;

  #[test]
  fn base64_matches_rfc4648_vectors() {
    for (input, expected) in [
      ("", ""),
      ("f", "Zg=="),
      ("fo", "Zm8="),
      ("foo", "Zm9v"),
      ("foob", "Zm9vYg=="),
      ("fooba", "Zm9vYmE="),
      ("foobar", "Zm9vYmFy"),
    ] {
      assert_eq!(base64(input.as_bytes()), expected, "{input}");
    }
    assert_eq!(base64(&[0, 0xff, 0x10]), "AP8Q");
  }

  #[test]
  fn value_mapping_table() {
    assert_eq!(to_json(ValueRef::Null), Value::Null);
    assert_eq!(to_json(ValueRef::Integer(i64::MAX)), json!(i64::MAX));
    assert_eq!(to_json(ValueRef::Integer(i64::MIN)), json!(i64::MIN));
    assert_eq!(to_json(ValueRef::Real(1.5)), json!(1.5));
    assert_eq!(to_json(ValueRef::Real(f64::NAN)), Value::Null);
    assert_eq!(to_json(ValueRef::Real(f64::INFINITY)), Value::Null);
    assert_eq!(to_json(ValueRef::Real(f64::NEG_INFINITY)), Value::Null);
    assert_eq!(to_json(ValueRef::Text(b"a\0b")), json!("a\u{0}b"));
    assert_eq!(to_json(ValueRef::Text("é☃".as_bytes())), json!("é☃"));
    assert_eq!(
      to_json(ValueRef::Text(&[0xff, 0xfe])),
      json!({"$text_base64": "//4="})
    );
    assert_eq!(
      to_json(ValueRef::Blob(&[1, 2, 3])),
      json!({"$blob_base64": "AQID"})
    );
    assert_eq!(to_json(ValueRef::Blob(&[])), json!({"$blob_base64": ""}));
  }

  #[test]
  fn min_len_never_exceeds_the_real_length() {
    for v in [
      ValueRef::Null,
      ValueRef::Integer(7),
      ValueRef::Real(0.5),
      ValueRef::Text(b"abc"),
      ValueRef::Text(b"a\0\"b"),
      ValueRef::Blob(&[1, 2, 3, 4]),
    ] {
      assert!(
        min_json_len(v) <= serde_json::to_string(&to_json(v)).unwrap().len(),
        "{v:?}"
      );
    }
  }

  #[test]
  fn limits_are_checked_never_clamped() {
    assert_eq!(Limits::new(None, None, None).unwrap(), Limits::default());
    let l = Limits::new(Some(5.0), Some(100.0), Some(50.0)).unwrap();
    assert_eq!(
      (l.max_rows, l.max_bytes, l.timeout.as_millis()),
      (5, 100, 50)
    );
    for (rows, bytes, ms) in [
      (Some(0.0), None, None),
      (Some(10_001.0), None, None),
      (Some(1.5), None, None),
      (None, Some(-1.0), None),
      (None, Some((16u64 << 20) as f64 + 1.0), None),
      (None, None, Some(300_001.0)),
      (None, None, Some(f64::NAN)),
    ] {
      let err = Limits::new(rows, bytes, ms).unwrap_err();
      assert_eq!(
        err.tool_code, "LIMIT_OUT_OF_RANGE",
        "{rows:?} {bytes:?} {ms:?}"
      );
    }
  }
}
