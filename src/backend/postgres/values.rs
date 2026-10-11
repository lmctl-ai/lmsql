//! PostgreSQL values -> JSON. Every value is read in PostgreSQL's text output format (cursor
//! `FETCH` over the simple protocol, or an explicit `::text` cast), with the session pinned to
//! `TimeZone=UTC`, `DateStyle=ISO, YMD`, `IntervalStyle=iso_8601`, `bytea_output=hex`,
//! `extra_float_digits=3`, so the text is stable. The column's type then decides the JSON:
//!
//! | Type | JSON |
//! | --- | --- |
//! | bool | true / false |
//! | int2, int4, int8, oid | number (exact; int8 above 2^53 loses precision in JavaScript) |
//! | float4, float8 | number; NaN and +/-Infinity become `null` |
//! | numeric | string (exact: `"12345.6789"`, `"NaN"`, `"Infinity"`) |
//! | json, jsonb | the JSON value itself |
//! | bytea | `{"$blob_base64": "..."}` |
//! | timestamptz | RFC 3339 UTC string `2026-01-02T03:04:05.123456Z`; `infinity`, `-infinity` and BC dates stay as PostgreSQL prints them |
//! | timestamp | `2026-01-02T03:04:05` (no zone) |
//! | everything else (text, date, time, interval, uuid, inet, money, arrays, ranges, enums, composites, domains, extension types, ...) | the PostgreSQL text form as a string |

use crate::values::base64;
use serde_json::{json, Value};

/// The JSON for one value in text format, given the column's type name.
pub fn to_json(type_name: &str, text: Option<&str>) -> Value {
  let Some(text) = text else {
    return Value::Null;
  };
  match type_name {
    "bool" => match text {
      "t" => Value::Bool(true),
      "f" => Value::Bool(false),
      other => Value::String(other.into()),
    },
    "int2" | "int4" | "int8" | "oid" => text
      .parse::<i64>()
      .map(|n| json!(n))
      .unwrap_or_else(|_| Value::String(text.into())),
    "float4" | "float8" => match text.parse::<f64>() {
      Ok(f) if f.is_finite() => json!(f),
      _ => Value::Null,
    },
    "json" | "jsonb" => serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.into())),
    "bytea" => match hex_bytes(text) {
      Some(bytes) => json!({"$blob_base64": base64(&bytes)}),
      None => Value::String(text.into()),
    },
    "timestamptz" => Value::String(rfc3339_utc(text)),
    "timestamp" => Value::String(iso_local(text)),
    _ => Value::String(text.into()),
  }
}

/// A lower bound of the serialized size, computed without decoding (huge values are refused
/// before they are converted, as for SQLite).
pub fn min_json_len(type_name: &str, text: Option<&str>) -> usize {
  match text {
    None => 4,
    Some(t) if type_name == "bytea" => t.len().saturating_sub(2) / 2 * 4 / 3 + 19,
    Some(t)
      if matches!(
        type_name,
        "int2" | "int4" | "int8" | "oid" | "float4" | "float8" | "bool"
      ) =>
    {
      t.len().min(1)
    }
    // JSON text re-serializes to at least half its length (whitespace dropped); strings keep it.
    Some(t) if matches!(type_name, "json" | "jsonb") => t.len() / 2,
    Some(t) => t.len() + 2,
  }
}

/// `\x0a1b` (bytea hex output) -> bytes.
fn hex_bytes(text: &str) -> Option<Vec<u8>> {
  let hex = text.strip_prefix("\\x")?;
  if hex.len() % 2 != 0 {
    return None;
  }
  (0..hex.len())
    .step_by(2)
    .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
    .collect()
}

fn is_iso_date_time(text: &str) -> bool {
  let b = text.as_bytes();
  b.len() >= 19
    && b[4] == b'-'
    && b[7] == b'-'
    && b[10] == b' '
    && b[13] == b':'
    && b[16] == b':'
    && b[..4].iter().all(u8::is_ascii_digit)
}

/// `2026-01-02 03:04:05.5+00` (TimeZone=UTC) -> `2026-01-02T03:04:05.5Z`.
pub fn rfc3339_utc(text: &str) -> String {
  if !is_iso_date_time(text) || text.ends_with(" BC") {
    return text.to_string();
  }
  match text.strip_suffix("+00") {
    Some(rest) => format!("{}T{}Z", &rest[..10], &rest[11..]),
    None => format!("{}T{}", &text[..10], &text[11..]),
  }
}

/// `2026-01-02 03:04:05` -> `2026-01-02T03:04:05`.
pub fn iso_local(text: &str) -> String {
  if !is_iso_date_time(text) || text.ends_with(" BC") {
    return text.to_string();
  }
  format!("{}T{}", &text[..10], &text[11..])
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn type_mapping_table() {
    assert_eq!(to_json("int4", None), Value::Null);
    assert_eq!(to_json("bool", Some("t")), json!(true));
    assert_eq!(to_json("bool", Some("f")), json!(false));
    assert_eq!(
      to_json("int8", Some("9223372036854775807")),
      json!(i64::MAX)
    );
    assert_eq!(
      to_json("int8", Some("-9223372036854775808")),
      json!(i64::MIN)
    );
    assert_eq!(to_json("float8", Some("1.5")), json!(1.5));
    for t in ["NaN", "Infinity", "-Infinity"] {
      assert_eq!(to_json("float8", Some(t)), Value::Null, "{t}");
    }
    assert_eq!(
      to_json("numeric", Some("12345678901234567890.123")),
      json!("12345678901234567890.123")
    );
    assert_eq!(to_json("numeric", Some("NaN")), json!("NaN"));
    assert_eq!(
      to_json("jsonb", Some("{\"a\": [1, null]}")),
      json!({"a": [1, null]})
    );
    assert_eq!(to_json("json", Some("not json")), json!("not json"));
    assert_eq!(
      to_json("bytea", Some("\\x00ff10")),
      json!({"$blob_base64": "AP8Q"})
    );
    assert_eq!(to_json("bytea", Some("\\x")), json!({"$blob_base64": ""}));
    assert_eq!(
      to_json("timestamptz", Some("2026-01-02 03:04:05.123456+00")),
      json!("2026-01-02T03:04:05.123456Z")
    );
    assert_eq!(
      to_json("timestamptz", Some("2026-01-02 03:04:05+00")),
      json!("2026-01-02T03:04:05Z")
    );
    assert_eq!(to_json("timestamptz", Some("infinity")), json!("infinity"));
    assert_eq!(
      to_json("timestamptz", Some("-infinity")),
      json!("-infinity")
    );
    assert_eq!(
      to_json("timestamptz", Some("0044-03-15 12:00:00+00 BC")),
      json!("0044-03-15 12:00:00+00 BC")
    );
    assert_eq!(
      to_json("timestamp", Some("2026-01-02 03:04:05")),
      json!("2026-01-02T03:04:05")
    );
    assert_eq!(to_json("interval", Some("P1DT2H")), json!("P1DT2H"));
    assert_eq!(
      to_json("_int4", Some("{{1,2},{3,4}}")),
      json!("{{1,2},{3,4}}")
    );
    assert_eq!(to_json("mood", Some("happy")), json!("happy"));
  }

  #[test]
  fn min_len_is_a_lower_bound() {
    for (ty, text) in [
      ("int4", Some("42")),
      ("float8", Some("NaN")),
      ("bytea", Some("\\x00ff10ab")),
      ("jsonb", Some("{\"a\": 1}")),
      ("text", Some("abc")),
      ("bool", Some("t")),
      ("text", None),
    ] {
      let real = serde_json::to_string(&to_json(ty, text)).unwrap().len();
      assert!(min_json_len(ty, text) <= real, "{ty} {text:?}");
    }
  }
}
