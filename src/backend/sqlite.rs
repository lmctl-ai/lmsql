//! SQLite backend (rusqlite, bundled SQLite), open mode.
//!
//! Every connection is opened read-write and the file is created if missing. lmsql runs whatever
//! SQL it is given — any number of statements (`execute_batch` semantics; the last statement's
//! rows are returned) — with no read-only guard, authorizer, pragma or ATTACH restriction. The
//! earlier guards are in git history (branch `phase2-postgres`, commit 0a3877f).
//!
//! Still enforced, as operational caps (they bound the result, not what SQL may do): `max_rows`
//! and `max_bytes` stop fetching rows, and `timeout` is the busy-wait limit and a progress-handler
//! deadline that interrupts a long statement.
//!
//! A `file:` name is made a literal path (the bundled SQLite is built with `SQLITE_USE_URI`), so a
//! name is never reinterpreted as a URI with its own parameters.

use super::{Backend, Column, ColumnInfo, Rows, RunOutcome, TableDescription, TableEntry};
use crate::error::DbError;
use crate::values::{min_json_len, to_json, Limits};
use rusqlite::{Batch, Connection, OpenFlags, Statement};
use serde_json::Value;
use std::cell::Cell;
use std::path::{Path, PathBuf};
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
}

impl Sqlite {
  pub fn new(path: PathBuf) -> Self {
    Sqlite { path }
  }
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

/// Open the database read-write (creating the file if missing); start the call's deadline.
pub fn open(path: &Path, limits: &Limits) -> Result<Connection, DbError> {
  let path = literal_path(path);
  let deadline = Instant::now() + limits.timeout;
  DEADLINE.with(|d| d.set(Some(deadline)));
  let flags = OpenFlags::SQLITE_OPEN_NO_MUTEX
    | OpenFlags::SQLITE_OPEN_READ_WRITE
    | OpenFlags::SQLITE_OPEN_CREATE;
  let conn = Connection::open_with_flags(&path, flags)?;
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

/// Step `stmt` to the end or until a cap: an extra row is read before reporting `max_rows`, so
/// `truncated` is true only when rows were really left out.
pub fn collect_rows(stmt: &mut Statement<'_>, limits: &Limits) -> Result<Rows, DbError> {
  let columns = columns_of(stmt);
  let width = columns.len();
  let mut rows: Vec<Vec<Value>> = Vec::new();
  let mut bytes = 2; // "[]"
  let mut truncated_by = None;
  let mut cursor = stmt.query([])?;
  while let Some(row) = cursor.next()? {
    if rows.len() == limits.max_rows {
      truncated_by = Some("max_rows");
      break;
    }
    let mut lower_bound = 2 + width.saturating_sub(1);
    for i in 0..width {
      lower_bound += min_json_len(row.get_ref(i)?);
    }
    if bytes + lower_bound + 1 > limits.max_bytes {
      truncated_by = Some("max_bytes");
      break;
    }
    let values: Vec<Value> = (0..width)
      .map(|i| row.get_ref(i).map(to_json))
      .collect::<Result<_, _>>()?;
    let size = serde_json::to_string(&values)
      .map(|s| s.len())
      .unwrap_or(usize::MAX)
      + 1;
    if bytes + size > limits.max_bytes {
      truncated_by = Some("max_bytes");
      break;
    }
    bytes += size;
    rows.push(values);
  }
  Ok(Rows {
    columns,
    rows,
    truncated_by,
  })
}

/// Run every statement in `sql`; return the last statement's rows (empty if it returns none).
fn run_batch(conn: &Connection, sql: &str, limits: &Limits) -> Result<Rows, DbError> {
  let mut last = Rows {
    columns: Vec::new(),
    rows: Vec::new(),
    truncated_by: None,
  };
  let mut batch = Batch::new(conn, sql);
  while let Some(mut stmt) = batch.next()? {
    if stmt.column_count() > 0 {
      last = collect_rows(&mut stmt, limits)?;
    } else {
      stmt.raw_execute()?;
      last = Rows {
        columns: Vec::new(),
        rows: Vec::new(),
        truncated_by: None,
      };
    }
  }
  Ok(last)
}

impl Backend for Sqlite {
  fn run(&self, sql: &str, limits: &Limits) -> Result<RunOutcome, DbError> {
    let conn = open(&self.path, limits)?;
    let rows = run_batch(&conn, sql, limits)?;
    Ok(RunOutcome {
      rows,
      rows_affected: conn.changes(),
      last_insert_rowid: Some(conn.last_insert_rowid()),
    })
  }

  fn tables(
    &self,
    schema: Option<&str>,
    limits: &Limits,
  ) -> Result<(Vec<TableEntry>, bool), DbError> {
    let conn = open(&self.path, limits)?;
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
    let conn = open(&self.path, limits)?;
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

  #[test]
  fn open_mode_runs_any_statement_including_writes_and_multi_statement() {
    let (_d, path) = temp_db();
    let db = Sqlite::new(path.clone());
    let limits = Limits::default();
    // A write through the same entry point as a read.
    let out = db
      .run("INSERT INTO t(v) VALUES ('b'), ('c')", &limits)
      .unwrap();
    assert_eq!(out.rows_affected, 2);
    // Multi-statement text: the last statement's rows come back.
    let out = db
      .run(
        "UPDATE t SET v = 'x' WHERE id = 1; SELECT id, v FROM t ORDER BY id",
        &limits,
      )
      .unwrap();
    assert_eq!(out.rows.rows.len(), 3);
    assert_eq!(
      out.rows.rows[0],
      vec![serde_json::json!(1), serde_json::json!("x")]
    );
    // A read returns rows; last_insert_rowid is present for SQLite.
    let out = db.run("SELECT count(*) FROM t", &limits).unwrap();
    assert_eq!(out.rows.rows, vec![vec![serde_json::json!(3)]]);
    assert!(out.last_insert_rowid.is_some());
  }

  #[test]
  fn missing_file_is_created() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("new.db");
    let db = Sqlite::new(path.clone());
    db.run("CREATE TABLE a(x)", &Limits::default()).unwrap();
    assert!(path.exists());
  }

  #[test]
  fn row_cap_still_applies() {
    let (_d, path) = temp_db();
    let db = Sqlite::new(path);
    let limits = Limits {
      max_rows: 2,
      ..Limits::default()
    };
    let sql =
      "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c LIMIT 1000) SELECT x FROM c";
    let out = db.run(sql, &limits).unwrap();
    assert_eq!(out.rows.rows.len(), 2);
    assert_eq!(out.rows.truncated_by, Some("max_rows"));
  }
}
