//! Database backends: SQLite (phase 1) and PostgreSQL (phase 2). A backend is chosen per call
//! from the resolved [`Target`] and implements [`Backend`].

pub mod postgres;
pub mod sqlite;

use crate::error::DbError;
use crate::output::Sink;
use crate::settings::TableRules;
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
  /// The backend for this call's [`Mode`].
  pub fn backend(&self, mode: Mode) -> Box<dyn Backend> {
    match self {
      Target::Sqlite(path) => Box::new(sqlite::Sqlite::with_mode(path.clone(), mode)),
      Target::Postgres(dsn) => Box::new(postgres::Postgres::with_mode(dsn.clone(), mode)),
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

/// What a dry run found: the SQL only reads (it ran normally, rows are in the sink), or some
/// statement would change something (nothing was executed).
#[derive(Debug, Clone, PartialEq)]
pub enum DryRun {
  Read(RunStats),
  WouldChange(Vec<Planned>),
}

/// One statement of a dry run that would change something (or, for SQLite multi-statement text,
/// any statement of it), with the database's own plan where it can give one.
#[derive(Debug, Clone, PartialEq)]
pub struct Planned {
  pub statement: String,
  pub would_change: bool,
  pub plan: Value,
  pub estimated_rows: Option<f64>,
}

/// How the settings restrict a call's database handle.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Mode {
  /// Level `readonly` everywhere: SQLite opened read-only with `query_only`; every PostgreSQL
  /// transaction READ ONLY.
  pub read_only: bool,
  /// SQLite per-table levels, enforced by the authorizer (`None`: no table rule differs from the
  /// connection default).
  pub table_rules: Option<TableRules>,
}

/// One result column: its name and declared type (`null` for expressions).
#[derive(Debug, Clone, PartialEq)]
pub struct Column {
  pub name: String,
  pub decl_type: Option<String>,
}

/// What a `query` / `exec` changed; the rows themselves go to the [`Sink`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RunStats {
  /// Rows changed by the last statement (PostgreSQL: its command tag count).
  pub rows_affected: u64,
  /// SQLite's last inserted rowid; `None` (JSON `null`) for PostgreSQL, which has none.
  pub last_insert_rowid: Option<i64>,
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

/// The operations every backend provides. In open mode a backend runs any SQL the database
/// accepts (the earlier read-only guards are in git history on `phase2-postgres` / at 0a3877f);
/// big results go to a file ([`crate::output`]); time is capped by [`Limits`].
pub trait Backend {
  /// Run the request's SQL, streaming each result set's rows into `sink` as they are fetched
  /// (never all in memory), and return what the last statement changed. `query` and `exec` both
  /// call this.
  fn run(&self, sql: &str, limits: &Limits, sink: &mut Sink) -> Result<RunStats, DbError>;
  /// Dry run: estimate without changing anything. Reads run as in [`Backend::run`]; if anything
  /// would change, nothing is executed and the statements are reported with their plans.
  fn dry_run(&self, sql: &str, limits: &Limits, sink: &mut Sink) -> Result<DryRun, DbError>;
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
