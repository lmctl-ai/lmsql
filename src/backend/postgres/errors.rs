//! PostgreSQL errors -> lmapi codes. `tool_code` is `PG_<SQLSTATE>` (`PG_42P01`, `PG_23505`),
//! except for lmsql's own classes: `TIMEOUT`, `CONNECT_FAILED`.
//!
//! Connection failures (network, TLS, authentication, unknown database or role) all become one
//! `CONNECT_FAILED` with a fixed message: the caller learns nothing about which part was wrong
//! (no account or host probing through lmsql) and nothing about the DSN.

use crate::error::DbError;
use lmrust::server::ErrorCode;

pub const CONNECT_FAILED_MESSAGE: &str =
  "could not connect to the PostgreSQL server (address, TLS, authentication or database name was rejected)";

pub fn connect_failed() -> DbError {
  DbError::new(
    ErrorCode::ServiceUnavailable,
    "CONNECT_FAILED",
    CONNECT_FAILED_MESSAGE,
  )
}

pub fn timeout() -> DbError {
  DbError::new(
    ErrorCode::Timeout,
    "TIMEOUT",
    "the call exceeded timeoutMs and was cancelled",
  )
}

/// The standard code for a SQLSTATE.
pub fn standard_code(sqlstate: &str) -> ErrorCode {
  match sqlstate {
    "57014" | "25P03" => ErrorCode::Timeout,
    "55P03" | "40001" | "40P01" => ErrorCode::Conflict,
    "42501" | "25006" => ErrorCode::Forbidden,
    _ => match &sqlstate[..2.min(sqlstate.len())] {
      // integrity constraint violation
      "23" => ErrorCode::Conflict,
      // connection exception, invalid authorization, invalid catalog name (no such database)
      "08" | "28" | "3D" => ErrorCode::ServiceUnavailable,
      // syntax/access rule, data exception, feature not supported, invalid transaction
      // state/termination, program limit exceeded: the request is at fault
      "42" | "22" | "0A" | "25" | "2D" | "54" | "34" | "26" | "3F" => ErrorCode::ValidationError,
      _ => ErrorCode::CommandFailed,
    },
  }
}

/// Map a driver error. `connecting` is true until the session is established.
pub fn from_pg(error: postgres::Error, connecting: bool, deadline_passed: bool) -> DbError {
  if let Some(db) = error.as_db_error() {
    let state = db.code().code();
    let code = standard_code(state);
    if connecting || code == ErrorCode::ServiceUnavailable {
      return connect_failed();
    }
    if state == "57014" {
      return timeout();
    }
    return DbError {
      foreign: true,
      ..DbError::new(code, format!("PG_{state}"), db.message())
    };
  }
  if deadline_passed {
    return timeout();
  }
  if connecting {
    return connect_failed();
  }
  // I/O or protocol failure after connecting: no detail (it may carry addresses).
  DbError::new(
    ErrorCode::CommandFailed,
    "CONNECTION_LOST",
    "the connection to the PostgreSQL server failed during the call",
  )
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn sqlstate_classes() {
    for (state, code) in [
      ("57014", ErrorCode::Timeout),
      ("55P03", ErrorCode::Conflict),
      ("23505", ErrorCode::Conflict),
      ("23503", ErrorCode::Conflict),
      ("40P01", ErrorCode::Conflict),
      ("42501", ErrorCode::Forbidden),
      ("25006", ErrorCode::Forbidden),
      ("42P01", ErrorCode::ValidationError),
      ("42601", ErrorCode::ValidationError),
      ("22012", ErrorCode::ValidationError),
      ("54000", ErrorCode::ValidationError),
      ("28P01", ErrorCode::ServiceUnavailable),
      ("3D000", ErrorCode::ServiceUnavailable),
      ("08006", ErrorCode::ServiceUnavailable),
      ("53200", ErrorCode::CommandFailed),
      ("XX000", ErrorCode::CommandFailed),
    ] {
      assert_eq!(standard_code(state), code, "{state}");
    }
  }
}
