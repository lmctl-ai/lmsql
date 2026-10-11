//! Errors: every failure carries a standard lmapi code (exit code) and a tool code kept in
//! `error.data.tool_code`. Messages are scrubbed of database paths and DSNs before they leave.

use lmrust::server::{CliError, ErrorCode};
use rusqlite::ffi;

#[derive(Debug, Clone, PartialEq)]
pub struct DbError {
  pub code: ErrorCode,
  pub tool_code: String,
  pub message: String,
  /// Byte offset of a syntax error in the statement, when SQLite reports one.
  pub offset: Option<i64>,
}

impl DbError {
  pub fn new(code: ErrorCode, tool_code: impl Into<String>, message: impl Into<String>) -> Self {
    DbError {
      code,
      tool_code: tool_code.into(),
      message: message.into(),
      offset: None,
    }
  }

  /// The lmapi envelope, with every secret replaced by `<db>`.
  pub fn into_cli_error(self, secrets: &[String]) -> CliError {
    let mut err = CliError::new(self.code, scrub(&self.message, secrets)).tool_code(self.tool_code);
    if let Some(offset) = self.offset {
      err = err.with("offset", offset.into());
    }
    err
  }
}

/// Replace every secret (longest first) with `<db>`.
pub fn scrub(text: &str, secrets: &[String]) -> String {
  let mut secrets: Vec<&String> = secrets.iter().filter(|s| !s.is_empty()).collect();
  secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
  let mut out = text.to_string();
  for s in secrets {
    out = out.replace(s.as_str(), "<db>");
  }
  out
}

/// SQLite's name for a (possibly extended) result code, e.g. `SQLITE_CONSTRAINT_UNIQUE`.
pub fn sqlite_code_name(extended: i32) -> String {
  let name = match extended {
    ffi::SQLITE_CONSTRAINT_CHECK => "SQLITE_CONSTRAINT_CHECK",
    ffi::SQLITE_CONSTRAINT_FOREIGNKEY => "SQLITE_CONSTRAINT_FOREIGNKEY",
    ffi::SQLITE_CONSTRAINT_NOTNULL => "SQLITE_CONSTRAINT_NOTNULL",
    ffi::SQLITE_CONSTRAINT_PRIMARYKEY => "SQLITE_CONSTRAINT_PRIMARYKEY",
    ffi::SQLITE_CONSTRAINT_UNIQUE => "SQLITE_CONSTRAINT_UNIQUE",
    ffi::SQLITE_CONSTRAINT_TRIGGER => "SQLITE_CONSTRAINT_TRIGGER",
    ffi::SQLITE_CONSTRAINT_ROWID => "SQLITE_CONSTRAINT_ROWID",
    ffi::SQLITE_READONLY_DBMOVED => "SQLITE_READONLY_DBMOVED",
    ffi::SQLITE_BUSY_TIMEOUT => "SQLITE_BUSY_TIMEOUT",
    ffi::SQLITE_CANTOPEN_ISDIR => "SQLITE_CANTOPEN_ISDIR",
    _ => match extended & 0xff {
      ffi::SQLITE_ERROR => "SQLITE_ERROR",
      ffi::SQLITE_INTERNAL => "SQLITE_INTERNAL",
      ffi::SQLITE_PERM => "SQLITE_PERM",
      ffi::SQLITE_ABORT => "SQLITE_ABORT",
      ffi::SQLITE_BUSY => "SQLITE_BUSY",
      ffi::SQLITE_LOCKED => "SQLITE_LOCKED",
      ffi::SQLITE_NOMEM => "SQLITE_NOMEM",
      ffi::SQLITE_READONLY => "SQLITE_READONLY",
      ffi::SQLITE_INTERRUPT => "SQLITE_INTERRUPT",
      ffi::SQLITE_IOERR => "SQLITE_IOERR",
      ffi::SQLITE_CORRUPT => "SQLITE_CORRUPT",
      ffi::SQLITE_FULL => "SQLITE_FULL",
      ffi::SQLITE_CANTOPEN => "SQLITE_CANTOPEN",
      ffi::SQLITE_PROTOCOL => "SQLITE_PROTOCOL",
      ffi::SQLITE_SCHEMA => "SQLITE_SCHEMA",
      ffi::SQLITE_TOOBIG => "SQLITE_TOOBIG",
      ffi::SQLITE_CONSTRAINT => "SQLITE_CONSTRAINT",
      ffi::SQLITE_MISMATCH => "SQLITE_MISMATCH",
      ffi::SQLITE_MISUSE => "SQLITE_MISUSE",
      ffi::SQLITE_AUTH => "SQLITE_AUTH",
      ffi::SQLITE_RANGE => "SQLITE_RANGE",
      ffi::SQLITE_NOTADB => "SQLITE_NOTADB",
      _ => return format!("SQLITE_{extended}"),
    },
  };
  name.to_string()
}

/// The standard lmapi code for a SQLite primary result code.
pub fn standard_code(extended: i32) -> ErrorCode {
  match extended & 0xff {
    ffi::SQLITE_CONSTRAINT | ffi::SQLITE_BUSY | ffi::SQLITE_LOCKED => ErrorCode::Conflict,
    ffi::SQLITE_READONLY | ffi::SQLITE_AUTH | ffi::SQLITE_PERM => ErrorCode::Forbidden,
    ffi::SQLITE_CANTOPEN => ErrorCode::NotFound,
    ffi::SQLITE_INTERRUPT => ErrorCode::Timeout,
    // Bad SQL (syntax, unknown table/column/function, too many attached databases), a value
    // too big or of the wrong type, a misused API: the request is at fault.
    ffi::SQLITE_ERROR | ffi::SQLITE_TOOBIG | ffi::SQLITE_MISMATCH | ffi::SQLITE_RANGE => {
      ErrorCode::ValidationError
    }
    _ => ErrorCode::CommandFailed,
  }
}

/// SQLite reports some authorizer denials (functions) and disabled features as `SQLITE_ERROR`
/// "not authorized ..."; they are refusals like `SQLITE_AUTH`.
fn refusal(message: &str) -> Option<DbError> {
  message
    .starts_with("not authorized")
    .then(|| DbError::new(ErrorCode::Forbidden, "SQLITE_AUTH", message))
}

impl From<rusqlite::Error> for DbError {
  fn from(error: rusqlite::Error) -> Self {
    let denied = match &error {
      rusqlite::Error::SqliteFailure(_, Some(message)) => refusal(message),
      rusqlite::Error::SqlInputError { msg, .. } => refusal(msg),
      _ => None,
    };
    if let Some(denied) = denied {
      return denied;
    }
    match error {
      // lmsql interrupts only at its deadline (progress handler): an interrupt is a timeout.
      rusqlite::Error::SqliteFailure(e, _) if e.extended_code & 0xff == ffi::SQLITE_INTERRUPT => {
        DbError::new(
          ErrorCode::Timeout,
          "TIMEOUT",
          "the statement exceeded timeoutMs and was interrupted",
        )
      }
      rusqlite::Error::SqliteFailure(e, message) => {
        let message = message.unwrap_or_else(|| e.to_string());
        DbError::new(
          standard_code(e.extended_code),
          sqlite_code_name(e.extended_code),
          message,
        )
      }
      rusqlite::Error::SqlInputError {
        error, msg, offset, ..
      } => DbError {
        code: standard_code(error.extended_code),
        tool_code: sqlite_code_name(error.extended_code),
        message: msg,
        offset: Some(offset as i64),
      },
      rusqlite::Error::NulError(_) => DbError::new(
        ErrorCode::ValidationError,
        "NUL_IN_STATEMENT",
        "the statement contains a NUL byte",
      ),
      other => DbError::new(ErrorCode::InternalError, "INTERNAL", other.to_string()),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn scrub_replaces_longest_secret_first() {
    let secrets = vec![
      "/tmp/a".to_string(),
      "/tmp/a/b.db".to_string(),
      String::new(),
    ];
    assert_eq!(
      scrub("cannot open /tmp/a/b.db (in /tmp/a)", &secrets),
      "cannot open <db> (in <db>)"
    );
  }

  #[test]
  fn codes_map_to_standard_codes() {
    assert_eq!(
      standard_code(ffi::SQLITE_CONSTRAINT_UNIQUE),
      ErrorCode::Conflict
    );
    assert_eq!(standard_code(ffi::SQLITE_BUSY), ErrorCode::Conflict);
    assert_eq!(standard_code(ffi::SQLITE_READONLY), ErrorCode::Forbidden);
    assert_eq!(standard_code(ffi::SQLITE_AUTH), ErrorCode::Forbidden);
    assert_eq!(standard_code(ffi::SQLITE_CANTOPEN), ErrorCode::NotFound);
    assert_eq!(standard_code(ffi::SQLITE_INTERRUPT), ErrorCode::Timeout);
    assert_eq!(standard_code(ffi::SQLITE_ERROR), ErrorCode::ValidationError);
    assert_eq!(standard_code(ffi::SQLITE_NOTADB), ErrorCode::CommandFailed);
    assert_eq!(standard_code(ffi::SQLITE_CORRUPT), ErrorCode::CommandFailed);
    assert_eq!(
      sqlite_code_name(ffi::SQLITE_CONSTRAINT_UNIQUE),
      "SQLITE_CONSTRAINT_UNIQUE"
    );
    assert_eq!(sqlite_code_name(ffi::SQLITE_IOERR_READ), "SQLITE_IOERR");
    assert_eq!(sqlite_code_name(250), "SQLITE_250");
  }
}
