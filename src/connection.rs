//! Which database a call uses: `--db PATH` (SQLite file) or `--connection NAME`, resolved from
//! the environment variable `LMSQL_<NAME>` (upper-cased). The value is a SQLite path or a DSN.
//! Neither the path nor the DSN is ever echoed back: errors name the connection, not its value.

use crate::backend::Target;
use crate::error::DbError;
use lmrust::server::ErrorCode;
use std::path::PathBuf;

/// The environment variable that holds connection `name`.
pub fn env_var_name(name: &str) -> String {
  format!("LMSQL_{}", name.to_ascii_uppercase())
}

fn valid_name(name: &str) -> bool {
  !name.is_empty()
    && name.len() <= 64
    && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Resolve the target of one call. `lookup` reads an environment variable.
pub fn resolve(
  db: Option<&str>,
  connection: Option<&str>,
  lookup: impl Fn(&str) -> Option<String>,
) -> Result<Target, DbError> {
  match (db, connection) {
    (Some(_), Some(_)) => Err(DbError::new(
      ErrorCode::ValidationError,
      "CONNECTION_CONFLICT",
      "pass either db or connection, not both",
    )),
    (None, None) => Err(DbError::new(
      ErrorCode::ValidationError,
      "CONNECTION_REQUIRED",
      "pass db (a SQLite file) or connection (a name configured as LMSQL_<NAME>)",
    )),
    (Some(path), None) => {
      if path.is_empty() {
        return Err(DbError::new(
          ErrorCode::ValidationError,
          "CONNECTION_REQUIRED",
          "db is empty",
        ));
      }
      // A postgres:// DSN on --db is accepted (open mode favours simplicity). Note that argv
      // is visible to other users via ps / /proc, so LMSQL_<NAME> + --connection keeps a DSN
      // with a password out of the process table; the README says so.
      if crate::backend::postgres::dsn::is_postgres_dsn(path) {
        return Ok(Target::Postgres(path.trim().to_string()));
      }
      Ok(Target::Sqlite(PathBuf::from(path)))
    }
    (None, Some(name)) => {
      if !valid_name(name) {
        return Err(DbError::new(
          ErrorCode::ValidationError,
          "INVALID_CONNECTION_NAME",
          "connection names use letters, digits and underscores (at most 64)",
        ));
      }
      let var = env_var_name(name);
      let value = lookup(&var)
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| {
          DbError::new(
            ErrorCode::NotFound,
            "CONNECTION_NOT_FOUND",
            format!("connection {name:?} is not configured (set {var})"),
          )
        })?;
      parse_dsn(name, &value)
    }
  }
}

/// A connection value: `sqlite:PATH`, `sqlite://PATH` or a plain path. Other schemes are later
/// backends. The value never appears in the error.
pub fn parse_dsn(name: &str, value: &str) -> Result<Target, DbError> {
  let lower = value.to_ascii_lowercase();
  if lower.starts_with("sqlite:") {
    let path = &value["sqlite:".len()..];
    let path = path.strip_prefix("//").unwrap_or(path);
    if path.is_empty() {
      return Err(DbError::new(
        ErrorCode::ValidationError,
        "INVALID_CONNECTION",
        format!("connection {name:?} has an empty sqlite path"),
      ));
    }
    return Ok(Target::Sqlite(PathBuf::from(path)));
  }
  if crate::backend::postgres::dsn::is_postgres_dsn(value) {
    return Ok(Target::Postgres(value.trim().to_string()));
  }
  if let Some((scheme, _)) = lower.split_once("://") {
    return Err(DbError::new(
      ErrorCode::ValidationError,
      "UNSUPPORTED_BACKEND",
      format!("connection {name:?} uses an unsupported scheme {scheme:?} (SQLite paths and postgres:// DSNs are supported)"),
    ));
  }
  Ok(Target::Sqlite(PathBuf::from(value)))
}

#[cfg(test)]
mod tests {
  use super::*;

  fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
    move |k| {
      pairs
        .iter()
        .find(|(n, _)| *n == k)
        .map(|(_, v)| v.to_string())
    }
  }

  #[test]
  fn db_and_connection_are_exclusive_and_one_is_required() {
    let none = env(&[]);
    assert_eq!(
      resolve(None, None, &none).unwrap_err().tool_code,
      "CONNECTION_REQUIRED"
    );
    assert_eq!(
      resolve(Some("a"), Some("b"), &none).unwrap_err().tool_code,
      "CONNECTION_CONFLICT"
    );
    assert_eq!(
      resolve(Some(""), None, &none).unwrap_err().tool_code,
      "CONNECTION_REQUIRED"
    );
    assert_eq!(
      resolve(Some("x.db"), None, &none).unwrap(),
      Target::Sqlite("x.db".into())
    );
  }

  #[test]
  fn named_connections_come_from_lmsql_env_vars() {
    let e = env(&[
      ("LMSQL_MAIN", "/data/main.db"),
      ("LMSQL_URI", "sqlite:///data/u.db"),
      ("LMSQL_SHORT", "sqlite:rel.db"),
      ("LMSQL_PG", "postgres://user:secret@host/db"),
      ("LMSQL_ODD", "mysql://user:secret@host/db"),
      ("LMSQL_BLANK", "  "),
      ("LMSQL_EMPTYSQLITE", "sqlite://"),
    ]);
    assert_eq!(
      resolve(None, Some("main"), &e).unwrap(),
      Target::Sqlite("/data/main.db".into())
    );
    assert_eq!(
      resolve(None, Some("Main"), &e).unwrap(),
      Target::Sqlite("/data/main.db".into())
    );
    assert_eq!(
      resolve(None, Some("uri"), &e).unwrap(),
      Target::Sqlite("/data/u.db".into())
    );
    assert_eq!(
      resolve(None, Some("short"), &e).unwrap(),
      Target::Sqlite("rel.db".into())
    );
    assert_eq!(
      resolve(None, Some("pg"), &e).unwrap(),
      Target::Postgres("postgres://user:secret@host/db".into())
    );
    // A DSN on --db is accepted directly in open mode.
    assert_eq!(
      resolve(Some("postgresql://user:secret@host/db"), None, &e).unwrap(),
      Target::Postgres("postgresql://user:secret@host/db".into())
    );
    for (name, code) in [
      ("odd", "UNSUPPORTED_BACKEND"),
      ("blank", "CONNECTION_NOT_FOUND"),
      ("missing", "CONNECTION_NOT_FOUND"),
      ("emptysqlite", "INVALID_CONNECTION"),
      ("bad-name", "INVALID_CONNECTION_NAME"),
      ("", "INVALID_CONNECTION_NAME"),
    ] {
      let err = resolve(None, Some(name), &e).unwrap_err();
      assert_eq!(err.tool_code, code, "{name}");
      // The value (paths, passwords) is never part of the message.
      for secret in ["secret", "/data", "host/db", "user:"] {
        assert!(!err.message.contains(secret), "{name}: {}", err.message);
      }
    }
  }
}
