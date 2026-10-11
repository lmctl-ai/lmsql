//! SQLite backend (rusqlite, bundled SQLite), open mode.
//!
//! Every connection is opened read-write and the file is created if missing. lmsql runs whatever
//! SQL it is given — any number of statements (`execute_batch` semantics; the last statement's
//! rows are returned) — with no read-only guard, authorizer, pragma or ATTACH restriction. The
//! earlier guards are in git history (branch `phase2-postgres`, commit 0a3877f).
//!
//! Rows are stepped one at a time into the [`Sink`] (inline, or streamed to a file for a big
//! result; never all in memory). `timeout` is the busy-wait limit and a progress-handler deadline
//! that interrupts a long statement.
//!
//! A `file:` name is made a literal path (the bundled SQLite is built with `SQLITE_USE_URI`), so a
//! name is never reinterpreted as a URI with its own parameters.

use super::{Backend, Column, ColumnInfo, Mode, RunStats, TableDescription, TableEntry};
use crate::error::DbError;
use crate::output::Sink;
use crate::settings::{Level, TableRules};
use crate::values::{to_json, Limits};
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::{Batch, Connection, OpenFlags, Statement};
use serde_json::Value;
use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// VM instructions between deadline checks (a few microseconds of work).
pub const PROGRESS_STEPS: i32 = 1_000;

thread_local! {
  /// The deadline of the call running on this thread, shared by the busy handler and the
  /// progress handler (lmsql runs one call per process).
  static DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// Busy handler: keep waiting for the lock only until the call's deadline.
fn wait_until_deadline(_attempt: i32) -> bool {
  let Some(deadline) = DEADLINE.with(Cell::get) else {
    return false;
  };
  let now = Instant::now();
  if now >= deadline {
    return false;
  }
  std::thread::sleep((deadline - now).min(Duration::from_millis(5)));
  true
}

pub struct Sqlite {
  path: PathBuf,
  /// `read_only`: opened `SQLITE_OPEN_READ_ONLY` (never created) with `PRAGMA query_only = ON`;
  /// `table_rules`: per-table levels enforced by the authorizer.
  mode: Mode,
}

impl Sqlite {
  pub fn new(path: PathBuf) -> Self {
    Sqlite::with_mode(path, Mode::default())
  }

  pub fn with_mode(path: PathBuf, mode: Mode) -> Self {
    Sqlite { path, mode }
  }

  /// Open the handle for this call, with the table rules installed when there are any. The
  /// returned cell names the table a rule refused, if one did.
  fn connect(&self, limits: &Limits) -> Result<(Connection, Denied), DbError> {
    let conn = open(&self.path, limits, self.mode.read_only)?;
    let denied = Denied::default();
    if let Some(rules) = &self.mode.table_rules {
      install_table_rules(&conn, rules.clone(), denied.clone());
    }
    Ok((conn, denied))
  }
}

/// The table the authorizer refused (shared with the authorizer closure).
#[derive(Debug, Clone, Default)]
struct Denied(Arc<Mutex<Option<String>>>);

impl Denied {
  /// Attach the refused table to an error so it becomes the permission denial.
  fn mark(&self, mut e: DbError) -> DbError {
    if let Some(table) = self.0.lock().ok().and_then(|mut t| t.take()) {
      e.denied_table = Some(table);
    }
    e
  }
}

/// Per-table levels through the SQLite authorizer, which reports the table of every read, write
/// and DDL action whatever the SQL text looks like: `none` refuses any access, `readonly`
/// refuses writes and DDL. SQLite's own `sqlite_*` tables are bookkeeping (a CREATE TABLE also
/// writes `sqlite_schema`); the DDL action itself is what is checked.
fn install_table_rules(conn: &Connection, rules: TableRules, denied: Denied) {
  conn.authorizer(Some(move |ctx: AuthContext<'_>| {
    use AuthAction as A;
    let (table, write) = match ctx.action {
      A::Read { table_name, .. } => (table_name, false),
      A::Insert { table_name }
      | A::Delete { table_name }
      | A::Update { table_name, .. }
      | A::CreateTable { table_name }
      | A::CreateTempTable { table_name }
      | A::DropTable { table_name }
      | A::DropTempTable { table_name }
      | A::AlterTable { table_name, .. }
      | A::CreateIndex { table_name, .. }
      | A::CreateTempIndex { table_name, .. }
      | A::DropIndex { table_name, .. }
      | A::DropTempIndex { table_name, .. }
      | A::CreateTrigger { table_name, .. }
      | A::CreateTempTrigger { table_name, .. }
      | A::DropTrigger { table_name, .. }
      | A::DropTempTrigger { table_name, .. }
      | A::Analyze { table_name }
      | A::CreateVtable { table_name, .. }
      | A::DropVtable { table_name, .. } => (table_name, true),
      A::CreateView { view_name }
      | A::CreateTempView { view_name }
      | A::DropView { view_name }
      | A::DropTempView { view_name } => (view_name, true),
      _ => return Authorization::Allow,
    };
    if table.len() >= 7 && table[..7].eq_ignore_ascii_case("sqlite_") {
      return Authorization::Allow;
    }
    let allowed = match rules.level(table) {
      Level::None => false,
      Level::ReadOnly => !write,
      Level::ReadWrite => true,
    };
    if allowed {
      Authorization::Allow
    } else {
      if let Ok(mut slot) = denied.0.lock() {
        slot.get_or_insert_with(|| table.to_string());
      }
      Authorization::Deny
    }
  }));
}

/// A `file:` name is kept as a literal relative path rather than parsed as a URI.
pub fn literal_path(path: &Path) -> PathBuf {
  let looks_like_uri = path
    .to_str()
    .is_some_and(|p| p.len() >= 5 && p[..5].eq_ignore_ascii_case("file:"));
  if looks_like_uri {
    Path::new(".").join(path)
  } else {
    path.to_path_buf()
  }
}

/// Open the database — read-write (creating the file if missing), or for `read_only` read-only
/// with `query_only` — and start the call's deadline.
pub fn open(path: &Path, limits: &Limits, read_only: bool) -> Result<Connection, DbError> {
  let path = literal_path(path);
  let deadline = Instant::now() + limits.timeout;
  DEADLINE.with(|d| d.set(Some(deadline)));
  let flags = if read_only {
    OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_READ_ONLY
  } else {
    OpenFlags::SQLITE_OPEN_NO_MUTEX
      | OpenFlags::SQLITE_OPEN_READ_WRITE
      | OpenFlags::SQLITE_OPEN_CREATE
  };
  let conn = Connection::open_with_flags(&path, flags)?;
  if read_only {
    conn.pragma_update(None, "query_only", true)?;
  }
  conn.busy_handler(Some(wait_until_deadline))?;
  conn.progress_handler(PROGRESS_STEPS, Some(move || Instant::now() >= deadline));
  Ok(conn)
}

fn columns_of(stmt: &Statement<'_>) -> Vec<Column> {
  stmt
    .columns()
    .iter()
    .map(|c| Column {
      name: c.name().to_string(),
      decl_type: c.decl_type().map(str::to_string),
    })
    .collect()
}

/// Step `stmt` to the end, handing each row to the sink.
fn stream_rows(stmt: &mut Statement<'_>, sink: &mut Sink) -> Result<(), DbError> {
  let columns = columns_of(stmt);
  let width = columns.len();
  sink.begin(columns)?;
  let mut cursor = stmt.query([])?;
  while let Some(row) = cursor.next()? {
    let values: Vec<Value> = (0..width)
      .map(|i| row.get_ref(i).map(to_json))
      .collect::<Result<_, _>>()?;
    sink.row(values)?;
  }
  Ok(())
}

/// Run every statement in `sql`; each one with rows starts a new result set in the sink, each one
/// without rows an empty one, so the call ends with the last statement's rows.
fn run_batch(conn: &Connection, sql: &str, sink: &mut Sink) -> Result<(), DbError> {
  let mut batch = Batch::new(conn, sql);
  while let Some(mut stmt) = batch.next()? {
    if stmt.column_count() > 0 {
      stream_rows(&mut stmt, sink)?;
    } else {
      stmt.raw_execute()?;
      sink.begin(Vec::new())?;
    }
  }
  Ok(())
}

impl Backend for Sqlite {
  fn run(&self, sql: &str, limits: &Limits, sink: &mut Sink) -> Result<RunStats, DbError> {
    let (conn, denied) = self.connect(limits)?;
    run_batch(&conn, sql, sink).map_err(|e| denied.mark(e))?;
    Ok(RunStats {
      rows_affected: conn.changes(),
      last_insert_rowid: Some(conn.last_insert_rowid()),
    })
  }

  fn tables(
    &self,
    schema: Option<&str>,
    limits: &Limits,
  ) -> Result<(Vec<TableEntry>, bool), DbError> {
    let (conn, _) = self.connect(limits)?;
    // SQLite's attached schema name; `main` by default.
    let db = schema.unwrap_or("main");
    let mut stmt = conn.prepare(&format!(
      "SELECT name, type FROM {}.sqlite_schema \
       WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\' \
       ORDER BY name",
      quote_ident(db)
    ))?;
    let mut out = Vec::new();
    let mut truncated = false;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
      if out.len() == limits.max_rows {
        truncated = true;
        break;
      }
      out.push(TableEntry {
        name: row.get(0)?,
        kind: row.get(1)?,
        schema: None,
      });
    }
    Ok((out, truncated))
  }

  fn describe(
    &self,
    _schema: Option<&str>,
    table: &str,
    limits: &Limits,
  ) -> Result<TableDescription, DbError> {
    let (conn, _) = self.connect(limits)?;
    let found: Option<(String, String)> = conn
      .prepare(
        "SELECT name, type FROM sqlite_schema \
         WHERE type IN ('table', 'view') AND name = ?1 COLLATE NOCASE",
      )?
      .query_row([table], |r| Ok((r.get(0)?, r.get(1)?)))
      .map(Some)
      .or_else(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => Ok(None),
        other => Err(other),
      })?;
    let Some((name, kind)) = found else {
      return Err(DbError::new(
        lmrust::server::ErrorCode::NotFound,
        "TABLE_NOT_FOUND",
        format!("no table or view named {table:?}"),
      ));
    };
    let mut stmt = conn.prepare(
      "SELECT cid, name, type, \"notnull\", dflt_value, pk FROM pragma_table_info(?1) ORDER BY cid",
    )?;
    let columns = stmt
      .query_map([&name], |r| {
        Ok(ColumnInfo {
          cid: r.get(0)?,
          name: r.get(1)?,
          decl_type: r.get(2)?,
          notnull: r.get::<_, i64>(3)? != 0,
          default: r.get(4)?,
          pk: r.get(5)?,
        })
      })?
      .collect::<Result<Vec<_>, _>>()?;
    Ok(TableDescription {
      name,
      kind,
      schema: None,
      columns,
    })
  }
}

/// Double-quote a SQLite identifier (for the schema name in `tables`).
fn quote_ident(name: &str) -> String {
  format!("\"{}\"", name.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::output::{Collected, OutputConfig};
  use serde_json::json;

  fn temp_db() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    let conn = Connection::open(&path).unwrap();
    conn
      .execute_batch(
        "CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT); INSERT INTO t(v) VALUES ('a');",
      )
      .unwrap();
    (dir, path)
  }

  fn run(db: &Sqlite, sql: &str, inline_rows: usize) -> (Collected, RunStats) {
    let mut sink = Sink::new(OutputConfig::new(None, None, inline_rows, 1 << 20, false).unwrap());
    let stats = db.run(sql, &Limits::default(), &mut sink).unwrap();
    (sink.finish().unwrap(), stats)
  }

  fn inline_rows(c: Collected) -> Vec<Vec<Value>> {
    match c {
      Collected::Inline { rows, .. } => rows,
      other => panic!("expected inline rows, got {other:?}"),
    }
  }

  #[test]
  fn open_mode_runs_any_statement_including_writes_and_multi_statement() {
    let (_d, path) = temp_db();
    let db = Sqlite::new(path);
    let (_, stats) = run(&db, "INSERT INTO t(v) VALUES ('b'), ('c')", 200);
    assert_eq!(stats.rows_affected, 2);
    let (rows, _) = run(
      &db,
      "UPDATE t SET v = 'x' WHERE id = 1; SELECT id, v FROM t ORDER BY id",
      200,
    );
    let rows = inline_rows(rows);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0], vec![json!(1), json!("x")]);
    // A trailing statement without rows leaves an empty result.
    let (rows, stats) = run(&db, "SELECT * FROM t; DELETE FROM t WHERE id = 3", 200);
    assert!(inline_rows(rows).is_empty());
    assert_eq!(stats.rows_affected, 1);
  }

  #[test]
  fn missing_file_is_created() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("new.db");
    run(&Sqlite::new(path.clone()), "CREATE TABLE a(x)", 200);
    assert!(path.exists());
  }

  #[test]
  fn past_the_inline_threshold_the_result_spills_to_a_file() {
    let (_d, path) = temp_db();
    let out = tempfile::tempdir().unwrap();
    std::env::set_var(crate::output::OUT_DIR_ENV, out.path());
    let sql =
      "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c LIMIT 50) SELECT x FROM c";
    let (c, _) = run(&Sqlite::new(path), sql, 10);
    let Collected::File {
      row_count,
      file,
      spilled,
      preview,
      ..
    } = c
    else {
      panic!("no spill");
    };
    assert!(spilled);
    assert_eq!((row_count, file.rows), (50, 50));
    assert_eq!(preview.len(), 10);
    assert_eq!(
      std::fs::read_to_string(&file.path).unwrap().lines().count(),
      50
    );
  }
}
