//! Database backends: SQLite (phase 1) and PostgreSQL (phase 2). A backend is chosen per call
//! from the resolved [`Target`] and implements [`Backend`].

pub mod postgres;
pub mod sqlite;

use crate::error::DbError;
use crate::values::Limits;
use serde_json::{json, Value};
use std::path::PathBuf;

/// Where a call goes. Never serialized: paths and DSNs are not echoed to callers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
  /// A SQLite database file.
  Sqlite(PathBuf),
  /// A PostgreSQL DSN (from `LMSQL_<NAME>` only; never from argv).
  Postgres(String),
}

impl Target {
  pub fn backend(&self) -> Box<dyn Backend> {
    match self {
      Target::Sqlite(path) => Box::new(sqlite::Sqlite::new(path.clone())),
      Target::Postgres(dsn) => Box::new(postgres::Postgres::new(dsn.clone())),
    }
  }

  /// Strings that must never appear in output or errors (the path as given and as resolved).
  pub fn secrets(&self) -> Vec<String> {
    match self {
      Target::Sqlite(path) => {
        let mut out = vec![path.display().to_string()];
        if let Ok(real) = path.canonicalize() {
          out.push(real.display().to_string());
        }
        out.retain(|s| !s.is_empty());
        out
      }
      Target::Postgres(dsn) => match postgres::dsn::parse(dsn) {
        Ok(parsed) => parsed.secrets,
        Err(_) => vec![dsn.clone()],
      },
    }
  }
}

/// One result column: its name and declared type (`null` for expressions).
#[derive(Debug, Clone, PartialEq)]
pub struct Column {
  pub name: String,
  pub decl_type: Option<String>,
}

/// Rows of a statement, capped by [`Limits`]. `truncated_by` says which cap stopped it.
#[derive(Debug, Clone, PartialEq)]
pub struct Rows {
  pub columns: Vec<Column>,
  pub rows: Vec<Vec<Value>>,
  pub truncated_by: Option<&'static str>,
}

impl Rows {
  pub fn to_json(&self) -> Value {
    json!({
      "columns": self
        .columns
        .iter()
        .map(|c| json!({"name": c.name, "type": c.decl_type}))
        .collect::<Vec<_>>(),
      "rows": self.rows,
      "row_count": self.rows.len(),
      "truncated": self.truncated_by.is_some(),
      "truncated_by": self.truncated_by,
    })
  }
}

/// What `exec` changed. `returning` holds rows of a statement that produces them
/// (`INSERT ... RETURNING`, a `PRAGMA` that reports).
#[derive(Debug, Clone, PartialEq)]
pub struct ExecOutcome {
  pub rows_affected: u64,
  /// SQLite's last rowid; `None` (JSON `null`) for PostgreSQL, which has none (use RETURNING).
  pub last_insert_rowid: Option<i64>,
  pub returning: Option<Rows>,
}

impl ExecOutcome {
  pub fn to_json(&self) -> Value {
    let mut out = json!({
      "rows_affected": self.rows_affected,
      "last_insert_rowid": self.last_insert_rowid,
    });
    if let Some(rows) = &self.returning {
      out["returning"] = rows.to_json();
    }
    out
  }
}

/// A table or view.
#[derive(Debug, Clone, PartialEq)]
pub struct TableEntry {
  pub name: String,
  pub kind: String,
  /// The PostgreSQL schema; `None` for SQLite (the key is omitted).
  pub schema: Option<String>,
}

impl TableEntry {
  pub fn to_json(&self) -> Value {
    let mut out = json!({"name": self.name, "type": self.kind});
    if let Some(schema) = &self.schema {
      out["schema"] = json!(schema);
    }
    out
  }
}

/// One column of `describe`.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnInfo {
  pub cid: i64,
  pub name: String,
  pub decl_type: String,
  pub notnull: bool,
  pub default: Option<String>,
  /// 0 when not part of the primary key, else the 1-based position in it.
  pub pk: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TableDescription {
  pub name: String,
  pub kind: String,
  /// The PostgreSQL schema; `None` for SQLite (the key is omitted).
  pub schema: Option<String>,
  pub columns: Vec<ColumnInfo>,
}

impl TableDescription {
  pub fn to_json(&self) -> Value {
    let mut out = json!({
      "table": self.name,
      "type": self.kind,
      "columns": self.columns.iter().map(|c| json!({
        "cid": c.cid,
        "name": c.name,
        "type": c.decl_type,
        "notnull": c.notnull,
        "default": c.default,
        "pk": c.pk,
      })).collect::<Vec<_>>(),
    });
    if let Some(schema) = &self.schema {
      out["schema"] = json!(schema);
    }
    out
  }
}

/// The operations every backend provides. Read operations must be impossible to turn into
/// writes; `exec` is the only write path.
pub trait Backend {
  /// One read-only statement.
  fn query(&self, sql: &str, limits: &Limits) -> Result<Rows, DbError>;
  /// One statement of any kind. `create` allows creating a missing database.
  fn exec(&self, sql: &str, limits: &Limits, create: bool) -> Result<ExecOutcome, DbError>;
  /// Tables and views (internal objects excluded), capped by `limits.max_rows`; `schema` limits
  /// the list to one schema.
  fn tables(
    &self,
    schema: Option<&str>,
    limits: &Limits,
  ) -> Result<(Vec<TableEntry>, bool), DbError>;
  /// Columns of one table or view (`schema` names it exactly; otherwise the backend's lookup).
  fn describe(
    &self,
    schema: Option<&str>,
    table: &str,
    limits: &Limits,
  ) -> Result<TableDescription, DbError>;
}
