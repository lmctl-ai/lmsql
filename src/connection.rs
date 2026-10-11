//! Which database a call uses: `--db PATH` (SQLite file) or `--connection NAME`, resolved from
//! the environment variable `LMSQL_<NAME>` (upper-cased). The value is a SQLite path or a DSN.
//! Neither the path nor the DSN is ever echoed back: errors name the connection, not its value.

use crate::backend::Mode;
use crate::backend::Target;
use crate::error::DbError;
use crate::settings::{Driver, Level, Login, Settings, TableRules};
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

/// What the settings allow on the resolved connection, and what a denial must say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Access {
  /// The connection default (`permissions.default`).
  pub level: Level,
  /// SQLite per-table levels (`permissions.tables`; empty otherwise).
  pub tables: TableRules,
  /// The settings connection (`None` without a settings file, or for an allowed `--db`).
  pub rule: Option<Rule>,
}

/// Where the level came from: for the denial message and the operator's manual command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
  pub connection: String,
  pub settings: PathBuf,
  pub manual: Manual,
}

/// How the operator reaches the database with the original client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Manual {
  /// `sqlite3 <path as written in the settings file>`.
  Sqlite(String),
  /// `psql "$VAR"` (the variable name only).
  PsqlEnv(String),
  /// The DSN is written in the settings file: a placeholder, never the value.
  PsqlLiteral,
}

/// What the denied call wanted to do, for the manual command.
#[derive(Debug, Clone, Copy)]
pub enum Intent<'a> {
  Sql(&'a str),
  Tables,
  Describe(&'a str),
}

/// POSIX shell single-quoting.
pub fn shell_quote(text: &str) -> String {
  format!("'{}'", text.replace('\'', "'\\''"))
}

impl Access {
  /// No settings file, or `--db` allowed by `allow_direct`: what the database allows.
  pub fn open() -> Access {
    Access {
      level: Level::ReadWrite,
      tables: TableRules::new(Level::ReadWrite, &Default::default()),
      rule: None,
    }
  }

  /// The most any table on this connection allows (`none` = nothing at all is allowed).
  pub fn max_level(&self) -> Level {
    self.tables.max()
  }

  /// The level of one table.
  pub fn level_of(&self, table: &str) -> Level {
    self.tables.level(table)
  }

  /// How the database handle is restricted: read-only when no table allows writes; the
  /// authorizer when some table rule differs from the default.
  pub fn mode(&self) -> Mode {
    Mode {
      read_only: self.rule.is_some() && self.max_level() <= Level::ReadOnly,
      table_rules: self.tables.any_rule().then(|| self.tables.clone()),
    }
  }

  /// The operator's equivalent command for `intent`.
  pub fn manual_command(&self, intent: Intent<'_>) -> Option<String> {
    let rule = self.rule.as_ref()?;
    Some(match &rule.manual {
      Manual::Sqlite(written) => {
        let path = match written.strip_prefix("~/") {
          Some(rest) => format!("~/{}", shell_quote(rest)),
          None => shell_quote(written),
        };
        let arg = match intent {
          Intent::Sql(sql) => sql.to_string(),
          Intent::Tables => ".tables".to_string(),
          Intent::Describe(table) => format!(".schema {table}"),
        };
        format!("sqlite3 {path} {}", shell_quote(&arg))
      }
      Manual::PsqlEnv(_) | Manual::PsqlLiteral => {
        let target = match &rule.manual {
          Manual::PsqlEnv(var) => format!("\"${var}\""),
          _ => shell_quote(&format!(
            "<the dsn of connection {} in {}>",
            rule.connection,
            rule.settings.display()
          )),
        };
        let arg = match intent {
          Intent::Sql(sql) => sql.to_string(),
          Intent::Tables => "\\dt".to_string(),
          Intent::Describe(table) => format!("\\d {table}"),
        };
        format!("psql {target} -c {}", shell_quote(&arg))
      }
    })
  }

  /// The one short denial: FORBIDDEN / PERMISSION_DENIED, naming the connection, the rule and the
  /// settings file, with the operator's manual command in the error data.
  pub fn denied(&self, intent: Intent<'_>) -> DbError {
    let (connection, settings) = match &self.rule {
      Some(r) => (r.connection.as_str(), r.settings.display().to_string()),
      None => ("?", "?".to_string()),
    };
    let what = match intent {
      Intent::Sql(_) if self.level == Level::ReadOnly => "it does not allow writes",
      _ => "it allows no access",
    };
    let mut err = DbError::new(
      ErrorCode::Forbidden,
      "PERMISSION_DENIED",
      format!(
        "permission denied: connection {connection:?} is {} (permissions.default in {settings}); {what}",
        self.level.as_str()
      ),
    );
    err.manual_command = self.manual_command(intent);
    err
  }

  /// The denial for a table rule: names the table, its level and where the rule is.
  pub fn denied_table(&self, table: &str, intent: Intent<'_>) -> DbError {
    let (connection, settings) = match &self.rule {
      Some(r) => (r.connection.as_str(), r.settings.display().to_string()),
      None => ("?", "?".to_string()),
    };
    let level = self.level_of(table);
    let key = if self.tables.tables.contains_key(&table.to_lowercase()) {
      format!("permissions.tables.{table}")
    } else {
      "permissions.default".to_string()
    };
    let what = if level == Level::ReadOnly {
      "it does not allow writes"
    } else {
      "it allows no access"
    };
    let mut err = DbError::new(
      ErrorCode::Forbidden,
      "PERMISSION_DENIED",
      format!(
        "permission denied: table {table:?} is {} on connection {connection:?} ({key} in {settings}); {what}",
        level.as_str()
      ),
    );
    err.manual_command = self.manual_command(intent);
    err
  }

  /// A table refused by the authorizer, or a write refused by the read-only open / READ ONLY
  /// transaction of a `readonly` connection, is a permission denial; anything else passes through
  /// unchanged.
  pub fn map_error(&self, e: DbError, intent: Intent<'_>) -> DbError {
    if self.rule.is_none() {
      return e;
    }
    if let Some(table) = &e.denied_table {
      return self.denied_table(table, intent);
    }
    let refused_write = matches!(
      e.tool_code.as_str(),
      "SQLITE_READONLY" | "PG_25006" | "PG_42501"
    );
    if self.max_level() == Level::ReadOnly && refused_write {
      self.denied(intent)
    } else {
      e
    }
  }
}

/// Resolve the target of one call under `settings`. Without a settings file this is [`resolve`]
/// (today's behavior). With one: `--connection` resolves only through the file (no
/// `LMSQL_<NAME>` fallback), and `--db` needs `"allow_direct": true`.
pub fn resolve_with(
  db: Option<&str>,
  connection: Option<&str>,
  settings: &Settings,
  lookup: impl Fn(&str) -> Option<String>,
) -> Result<(Target, Access), DbError> {
  let Some(file) = &settings.source else {
    return Ok((resolve(db, connection, lookup)?, Access::open()));
  };
  match (db, connection) {
    (Some(_), None) if !settings.allow_direct => Err(DbError::new(
      ErrorCode::Forbidden,
      "DIRECT_NOT_ALLOWED",
      format!(
        "db (a direct path or DSN) is not allowed by the settings file {}; use connection NAME \
         (the operator can set \"allow_direct\": true)",
        file.display()
      ),
    )),
    (Some(_), None) | (Some(_), Some(_)) | (None, None) => {
      Ok((resolve(db, connection, lookup)?, Access::open()))
    }
    (None, Some(name)) => {
      let conn = settings.connections.get(name).ok_or_else(|| {
        DbError::new(
          ErrorCode::NotFound,
          "CONNECTION_NOT_FOUND",
          format!(
            "connection {name:?} is not in the settings file {}",
            file.display()
          ),
        )
      })?;
      let rule = |manual| Rule {
        connection: name.to_string(),
        settings: file.clone(),
        manual,
      };
      match &conn.driver {
        Driver::Sqlite { written, path } => Ok((
          Target::Sqlite(path.clone()),
          Access {
            level: conn.default,
            tables: TableRules::new(conn.default, &conn.tables),
            rule: Some(rule(Manual::Sqlite(written.clone()))),
          },
        )),
        Driver::Postgres {
          readonly,
          readwrite,
        } => {
          // readonly: the readonly login if there is one (else the readwrite login, with every
          // transaction READ ONLY); readwrite: the readwrite login, or the only one.
          let login = match conn.default {
            Level::ReadOnly => readonly.as_ref().or(readwrite.as_ref()),
            _ => readwrite.as_ref().or(readonly.as_ref()),
          }
          .expect("a postgres connection has a login");
          let operator_login = readwrite.as_ref().or(readonly.as_ref()).expect("a login");
          let manual = match operator_login {
            Login::Env(var) => Manual::PsqlEnv(var.clone()),
            Login::Literal(_) => Manual::PsqlLiteral,
          };
          let access = Access {
            level: conn.default,
            tables: TableRules::new(conn.default, &Default::default()),
            rule: Some(rule(manual)),
          };
          if conn.default == Level::None {
            // Never connects; no DSN needed.
            return Ok((Target::Postgres(String::new()), access));
          }
          let dsn = match login {
            Login::Literal(dsn) => dsn.clone(),
            Login::Env(var) => lookup(var)
              .filter(|v| !v.trim().is_empty())
              .ok_or_else(|| {
                DbError::new(
                  ErrorCode::NotFound,
                  "CONNECTION_NOT_FOUND",
                  format!(
                    "connection {name:?}: environment variable {var} (named in {}) is not set",
                    file.display()
                  ),
                )
              })?,
          };
          Ok((Target::Postgres(dsn.trim().to_string()), access))
        }
      }
    }
  }
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
