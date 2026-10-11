//! SQLite backend (rusqlite, bundled SQLite).
//!
//! Read operations (`query`, `tables`, `describe`) are read-only by four independent guards, each
//! enough on its own and each tested on its own (`tests` below):
//!
//! 1. the file is opened with `SQLITE_OPEN_READ_ONLY` (a `file:` name is made a literal path,
//!    see [`literal_path`], so no URI parameter can apply);
//! 2. `PRAGMA query_only = ON`;
//! 3. `query` refuses a statement unless `sqlite3_stmt_readonly` says it is read-only;
//! 4. an authorizer refuses ATTACH/DETACH, PRAGMA assignments and `load_extension`.
//!
//! Every connection (read or write) also gets `SQLITE_LIMIT_ATTACHED = 0` (no ATTACH, no
//! `VACUUM INTO`: no other file can be created or written), `SQLITE_DBCONFIG_DEFENSIVE`
//! (no `writable_schema` corruption) and `SQLITE_DBCONFIG_TRUSTED_SCHEMA = off`. Extension
//! loading is never enabled (and the `load_extension` function is denied).
//!
//! Exactly one statement per call: [`single_statement`] prepares with `sqlite3_prepare_v2` and
//! uses its tail (via `rusqlite::Batch`) to reject anything after the first statement.
//!
//! Resources: every connection gets size and shape limits ([`MAX_VALUE_BYTES`] and friends) so
//! one builtin (`printf`, `randomblob`, `zeroblob`, `hex`) cannot allocate past them
//! (`SQLITE_TOOBIG`), and the process has a hard SQLite heap limit ([`HEAP_LIMIT_BYTES`],
//! `SQLITE_NOMEM`). Time: one deadline per call, checked every [`PROGRESS_STEPS`] VM steps by the
//! progress handler and by the busy handler while waiting for a lock. A single VM step (one
//! builtin call) is not interruptible; the limits above bound what it can do.
//!
//! Paths: [`check_path`] refuses anything but an existing regular file (symlinks followed),
//! `:memory:` and blank names before SQLite opens anything (a fifo would block the open).

use super::{Backend, Column, ColumnInfo, ExecOutcome, Rows, TableDescription, TableEntry};
use crate::error::DbError;
use crate::values::{min_json_len, to_json, Limits};
use lmrust::server::ErrorCode;
use rusqlite::config::DbConfig;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::limits::Limit;
use rusqlite::{Batch, Connection, OpenFlags, Statement};
use serde_json::Value;
use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// VM instructions between deadline checks (a few microseconds of work).
pub const PROGRESS_STEPS: i32 = 1_000;

/// Longest string or blob SQLite may build or read (`SQLITE_LIMIT_LENGTH`): twice the hard
/// `maxBytes` cap, so every value a result can carry fits and nothing much larger is allocated.
pub const MAX_VALUE_BYTES: i32 = 32 << 20;
/// Longest statement text (`SQLITE_LIMIT_SQL_LENGTH`).
pub const MAX_STATEMENT_BYTES: i32 = 16 << 20;
/// Expression nesting (`SQLITE_LIMIT_EXPR_DEPTH`; SQLite's default is 1000).
pub const MAX_EXPR_DEPTH: i32 = 500;
/// Terms of a compound SELECT (`SQLITE_LIMIT_COMPOUND_SELECT`; default 500).
pub const MAX_COMPOUND_SELECT: i32 = 200;
/// Arguments of one function call (`SQLITE_LIMIT_FUNCTION_ARG`; pinned at SQLite's default).
pub const MAX_FUNCTION_ARGS: i32 = 127;
/// Length of a LIKE/GLOB pattern (`SQLITE_LIMIT_LIKE_PATTERN_LENGTH`; default 50000).
pub const MAX_LIKE_PATTERN: i32 = 10_000;
/// Hard ceiling on SQLite's heap for the whole process (`sqlite3_hard_heap_limit64`).
pub const HEAP_LIMIT_BYTES: i64 = 256 << 20;

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

/// How a connection is opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
  Read,
  Write { create: bool },
}

/// Guard 1: the open flags. No `SQLITE_OPEN_URI`: `file:x?mode=rwc` stays a plain file name.
pub fn open_flags(mode: Mode) -> OpenFlags {
  let base = OpenFlags::SQLITE_OPEN_NO_MUTEX;
  match mode {
    Mode::Read => base | OpenFlags::SQLITE_OPEN_READ_ONLY,
    Mode::Write { create: false } => base | OpenFlags::SQLITE_OPEN_READ_WRITE,
    Mode::Write { create: true } => {
      base | OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
    }
  }
}

/// The bundled SQLite is built with `SQLITE_USE_URI`, so any name starting with `file:` is a URI
/// whatever the open flags (`?vfs=`, `?immutable=1`, `?mode=`). Such a name is made a literal
/// relative path instead; absolute paths never start with `file:`.
pub fn literal_path(path: &std::path::Path) -> PathBuf {
  let looks_like_uri = path
    .to_str()
    .is_some_and(|p| p.len() >= 5 && p[..5].eq_ignore_ascii_case("file:"));
  if looks_like_uri {
    std::path::Path::new(".").join(path)
  } else {
    path.to_path_buf()
  }
}

/// Refuse what must never reach `sqlite3_open`: special and blank names, and anything that is
/// not an existing regular file (symlinks followed). A missing file is fine only for
/// `exec --create`. Checked before opening, so a fifo cannot block the call.
pub fn check_path(path: &Path, mode: Mode) -> Result<(), DbError> {
  let text = path.to_string_lossy();
  if text.trim().is_empty() || text == ":memory:" {
    return Err(DbError::new(
      ErrorCode::ValidationError,
      "INVALID_DB_PATH",
      "db must name a database file (not blank, not :memory:)",
    ));
  }
  match std::fs::metadata(path) {
    Ok(meta) if meta.is_file() => Ok(()),
    Ok(_) => Err(DbError::new(
      ErrorCode::ValidationError,
      "NOT_A_REGULAR_FILE",
      "db is not a regular file (a directory, device or fifo)",
    )),
    Err(e) if e.kind() == std::io::ErrorKind::NotFound => match mode {
      Mode::Write { create: true } => Ok(()),
      _ => Err(DbError::new(
        ErrorCode::NotFound,
        "SQLITE_CANTOPEN",
        "the database file does not exist",
      )),
    },
    Err(_) => Err(DbError::new(
      ErrorCode::Forbidden,
      "SQLITE_CANTOPEN",
      "the database file is not accessible",
    )),
  }
}

/// Process-wide SQLite heap ceiling; idempotent.
pub fn set_heap_limit() {
  // SAFETY: plain configuration call on the bundled SQLite; safe at any time.
  unsafe {
    rusqlite::ffi::sqlite3_hard_heap_limit64(HEAP_LIMIT_BYTES);
  }
}

/// Open and harden a connection (see the module docs); the call's deadline starts now.
pub fn open(path: &Path, mode: Mode, limits: &Limits) -> Result<Connection, DbError> {
  let path = &literal_path(path);
  check_path(path, mode)?;
  set_heap_limit();
  let deadline = Instant::now() + limits.timeout;
  DEADLINE.with(|d| d.set(Some(deadline)));
  let conn = Connection::open_with_flags(path, open_flags(mode))?;
  harden(&conn, mode)?;
  // Lock waits and statement work share the one deadline.
  conn.busy_handler(Some(wait_until_deadline))?;
  // Interrupts long statements (SQLITE_INTERRUPT -> TIMEOUT) and long row iteration.
  conn.progress_handler(PROGRESS_STEPS, Some(move || Instant::now() >= deadline));
  Ok(conn)
}

/// Guards 2 and 4 plus the per-connection hardening shared by every mode.
pub fn harden(conn: &Connection, mode: Mode) -> Result<(), DbError> {
  conn.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
  conn.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false)?;
  conn.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0);
  conn.set_limit(Limit::SQLITE_LIMIT_LENGTH, MAX_VALUE_BYTES);
  conn.set_limit(Limit::SQLITE_LIMIT_SQL_LENGTH, MAX_STATEMENT_BYTES);
  conn.set_limit(Limit::SQLITE_LIMIT_EXPR_DEPTH, MAX_EXPR_DEPTH);
  conn.set_limit(Limit::SQLITE_LIMIT_COMPOUND_SELECT, MAX_COMPOUND_SELECT);
  conn.set_limit(Limit::SQLITE_LIMIT_FUNCTION_ARG, MAX_FUNCTION_ARGS);
  conn.set_limit(Limit::SQLITE_LIMIT_LIKE_PATTERN_LENGTH, MAX_LIKE_PATTERN);
  if mode == Mode::Read {
    set_query_only(conn)?;
  }
  // Last: the authorizer would refuse the PRAGMA assignment above.
  conn.authorizer(Some(move |ctx: AuthContext<'_>| {
    authorize(mode, &ctx.action)
  }));
  Ok(())
}

/// Guard 2.
pub fn set_query_only(conn: &Connection) -> Result<(), DbError> {
  conn.pragma_update(None, "query_only", true)?;
  Ok(())
}

/// Pragmas that take an argument and only read (schema introspection and checks).
pub const READ_PRAGMAS_WITH_ARGUMENT: [&str; 10] = [
  "table_info",
  "table_xinfo",
  "table_list",
  "index_info",
  "index_xinfo",
  "index_list",
  "foreign_key_list",
  "foreign_key_check",
  "integrity_check",
  "quick_check",
];

/// Guard 4: what statements may do at all.
pub fn authorize(mode: Mode, action: &AuthAction<'_>) -> Authorization {
  match action {
    AuthAction::Attach { .. } | AuthAction::Detach { .. } => Authorization::Deny,
    AuthAction::Function { function_name }
      if function_name.eq_ignore_ascii_case("load_extension") =>
    {
      Authorization::Deny
    }
    // SQLite passes a pragma's argument (`PRAGMA table_info(t)`) exactly like an assigned value
    // (`PRAGMA query_only = OFF`), so read mode allows a value only for introspection pragmas
    // that cannot write. Pragmas without a value only read their setting.
    AuthAction::Pragma {
      pragma_name,
      pragma_value: Some(_),
    } if mode == Mode::Read
      && !READ_PRAGMAS_WITH_ARGUMENT.contains(&pragma_name.to_ascii_lowercase().as_str()) =>
    {
      Authorization::Deny
    }
    _ => Authorization::Allow,
  }
}

/// Prepare exactly one statement. Empty input (whitespace/comments only) and any further
/// statement in the tail are refused; the tail is found by `sqlite3_prepare_v2`, never by
/// splitting text.
pub fn single_statement<'c>(conn: &'c Connection, sql: &str) -> Result<Statement<'c>, DbError> {
  if sql.contains('\0') {
    // SQLite stops reading at NUL: text after it would be silently ignored.
    return Err(DbError::new(
      ErrorCode::ValidationError,
      "NUL_IN_STATEMENT",
      "the statement contains a NUL byte",
    ));
  }
  let mut batch = Batch::new(conn, sql);
  let Some(stmt) = batch.next()? else {
    return Err(DbError::new(
      ErrorCode::ValidationError,
      "EMPTY_STATEMENT",
      "the statement is empty (only whitespace or comments)",
    ));
  };
  // Anything after the first statement other than whitespace and comments: either another
  // statement or text that does not even prepare.
  match batch.next() {
    Ok(None) => Ok(stmt),
    Ok(Some(_)) | Err(_) => Err(DbError::new(
      ErrorCode::ValidationError,
      "TOO_MANY_STATEMENTS",
      "exactly one SQL statement per call; remove everything after the first statement",
    )),
  }
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

impl Backend for Sqlite {
  fn query(&self, sql: &str, limits: &Limits) -> Result<Rows, DbError> {
    let conn = open(&self.path, Mode::Read, limits)?;
    let mut stmt = single_statement(&conn, sql)?;
    // Guard 3.
    if !stmt.readonly() {
      return Err(DbError::new(
        ErrorCode::Forbidden,
        "NOT_READ_ONLY",
        "query runs read-only statements only; use exec to change data",
      ));
    }
    collect_rows(&mut stmt, limits)
  }

  fn exec(&self, sql: &str, limits: &Limits, create: bool) -> Result<ExecOutcome, DbError> {
    let conn = open(&self.path, Mode::Write { create }, limits)?;
    let mut stmt = single_statement(&conn, sql)?;
    let returning = if stmt.column_count() > 0 {
      Some(collect_rows(&mut stmt, limits)?)
    } else {
      stmt.raw_execute()?;
      None
    };
    drop(stmt);
    Ok(ExecOutcome {
      rows_affected: conn.changes(),
      last_insert_rowid: conn.last_insert_rowid(),
      returning,
    })
  }

  fn tables(&self, limits: &Limits) -> Result<(Vec<TableEntry>, bool), DbError> {
    let conn = open(&self.path, Mode::Read, limits)?;
    let mut stmt = conn.prepare(
      "SELECT name, type FROM sqlite_schema \
       WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\' \
       ORDER BY name",
    )?;
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
      });
    }
    Ok((out, truncated))
  }

  fn describe(&self, table: &str, limits: &Limits) -> Result<TableDescription, DbError> {
    let conn = open(&self.path, Mode::Read, limits)?;
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
        ErrorCode::NotFound,
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
      columns,
    })
  }
}

#[cfg(test)]
mod tests {
  //! Each read-only guard is tested in isolation: removing any one of them fails a test here.
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

  fn count(path: &std::path::Path) -> i64 {
    Connection::open(path)
      .unwrap()
      .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
      .unwrap()
  }

  #[test]
  fn guard1_read_only_open_refuses_writes_without_the_other_guards() {
    let (_d, path) = temp_db();
    // Only guard 1: the bare open flags, no query_only, no authorizer, no readonly check.
    let conn = Connection::open_with_flags(&path, open_flags(Mode::Read)).unwrap();
    let err = conn
      .execute("INSERT INTO t(v) VALUES ('x')", [])
      .unwrap_err();
    assert_eq!(DbError::from(err).tool_code, "SQLITE_READONLY");
    assert!(!open_flags(Mode::Read).contains(OpenFlags::SQLITE_OPEN_URI));
    assert!(!open_flags(Mode::Read).contains(OpenFlags::SQLITE_OPEN_CREATE));
    assert_eq!(count(&path), 1);
  }

  #[test]
  fn guard2_query_only_refuses_writes_on_a_writable_connection() {
    let (_d, path) = temp_db();
    // Only guard 2: a read-write connection with query_only set.
    let conn = Connection::open(&path).unwrap();
    set_query_only(&conn).unwrap();
    let err = conn
      .execute("INSERT INTO t(v) VALUES ('x')", [])
      .unwrap_err();
    assert_eq!(DbError::from(err).tool_code, "SQLITE_READONLY");
    // And `open` in read mode sets it.
    let conn = open(&path, Mode::Read, &Limits::default()).unwrap();
    let on: i64 = conn
      .query_row("PRAGMA query_only", [], |r| r.get(0))
      .unwrap();
    assert_eq!(on, 1);
    assert_eq!(count(&path), 1);
  }

  #[test]
  fn guard3_query_refuses_statements_sqlite_does_not_call_read_only() {
    let (_d, path) = temp_db();
    let err = Sqlite::new(path.clone())
      .query("INSERT INTO t(v) VALUES ('x')", &Limits::default())
      .unwrap_err();
    // NOT_READ_ONLY (guard 3), not SQLITE_READONLY (guards 1/2 at step time).
    assert_eq!(
      (err.code, err.tool_code.as_str()),
      (ErrorCode::Forbidden, "NOT_READ_ONLY")
    );
    let err = Sqlite::new(path.clone())
      .query(
        "WITH x(v) AS (SELECT 'y') INSERT INTO t(v) SELECT v FROM x",
        &Limits::default(),
      )
      .unwrap_err();
    assert_eq!(err.tool_code, "NOT_READ_ONLY");
    assert_eq!(count(&path), 1);
  }

  #[test]
  fn guard4_authorizer_refuses_pragma_writes_attach_and_extensions() {
    let (_d, path) = temp_db();
    // Only guard 4: a read-write connection with just the authorizer.
    let conn = Connection::open(&path).unwrap();
    conn.authorizer(Some(|ctx: AuthContext<'_>| {
      authorize(Mode::Read, &ctx.action)
    }));
    for sql in [
      "PRAGMA query_only = OFF",
      "PRAGMA writable_schema = 1",
      "PRAGMA user_version = 7",
    ] {
      let err = conn.execute_batch(sql).unwrap_err();
      assert_eq!(DbError::from(err).tool_code, "SQLITE_AUTH", "{sql}");
    }
    let dir = tempfile::tempdir().unwrap();
    let other = dir.path().join("other.db");
    let err = conn
      .execute_batch(&format!("ATTACH DATABASE '{}' AS o", other.display()))
      .unwrap_err();
    assert_eq!(DbError::from(err).tool_code, "SQLITE_AUTH");
    assert!(!other.exists());
    let err = conn
      .query_row("SELECT load_extension('nope')", [], |r| {
        r.get_ref(0).map(|_| ())
      })
      .unwrap_err();
    assert_eq!(DbError::from(err).tool_code, "SQLITE_AUTH");
    // Reads of pragmas stay allowed.
    let n: i64 = conn
      .query_row("SELECT count(*) FROM pragma_table_info('t')", [], |r| {
        r.get(0)
      })
      .unwrap();
    assert_eq!(n, 2);
    // Write mode allows pragma assignments but still no attach or extensions.
    assert_eq!(
      authorize(
        Mode::Write { create: false },
        &AuthAction::Pragma {
          pragma_name: "user_version",
          pragma_value: Some("1")
        }
      ),
      Authorization::Allow
    );
    assert_eq!(
      authorize(
        Mode::Write { create: false },
        &AuthAction::Function {
          function_name: "LOAD_EXTENSION"
        }
      ),
      Authorization::Deny
    );
  }

  #[test]
  fn harden_sets_the_limit_and_db_config_in_every_mode() {
    let (_d, path) = temp_db();
    for mode in [Mode::Read, Mode::Write { create: false }] {
      let conn = open(&path, mode, &Limits::default()).unwrap();
      assert_eq!(conn.limit(Limit::SQLITE_LIMIT_ATTACHED), 0, "{mode:?}");
      assert!(
        conn.db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE).unwrap(),
        "{mode:?}"
      );
      assert!(
        !conn
          .db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA)
          .unwrap(),
        "{mode:?}"
      );
    }
  }

  #[test]
  fn values_over_the_byte_cap_are_refused_before_they_are_encoded() {
    use crate::values::ENCODED_BYTES;
    let conn = Connection::open_in_memory().unwrap();
    let limits = Limits {
      max_bytes: 1_000,
      ..Limits::default()
    };
    ENCODED_BYTES.with(|n| n.set(0));
    let mut stmt = conn.prepare("SELECT zeroblob(20000000)").unwrap();
    let rows = collect_rows(&mut stmt, &limits).unwrap();
    assert_eq!((rows.rows.len(), rows.truncated_by), (0, Some("max_bytes")));
    assert_eq!(
      ENCODED_BYTES.with(|n| n.get()),
      0,
      "the 20 MB blob was encoded"
    );
    // A value within the cap is encoded (the counter is live).
    let mut stmt = conn.prepare("SELECT zeroblob(30)").unwrap();
    let rows = collect_rows(&mut stmt, &limits).unwrap();
    assert_eq!(rows.rows.len(), 1);
    assert_eq!(ENCODED_BYTES.with(|n| n.get()), 30);
  }

  #[test]
  fn harden_sets_size_and_shape_limits_in_every_mode() {
    let (_d, path) = temp_db();
    for mode in [Mode::Read, Mode::Write { create: false }] {
      let conn = open(&path, mode, &Limits::default()).unwrap();
      assert_eq!(conn.limit(Limit::SQLITE_LIMIT_LENGTH), 32 << 20, "{mode:?}");
      assert_eq!(
        conn.limit(Limit::SQLITE_LIMIT_SQL_LENGTH),
        16 << 20,
        "{mode:?}"
      );
      assert_eq!(conn.limit(Limit::SQLITE_LIMIT_EXPR_DEPTH), 500, "{mode:?}");
      assert_eq!(
        conn.limit(Limit::SQLITE_LIMIT_COMPOUND_SELECT),
        200,
        "{mode:?}"
      );
      assert_eq!(
        conn.limit(Limit::SQLITE_LIMIT_FUNCTION_ARG),
        127,
        "{mode:?}"
      );
      assert_eq!(
        conn.limit(Limit::SQLITE_LIMIT_LIKE_PATTERN_LENGTH),
        10_000,
        "{mode:?}"
      );
      let err = conn
        .query_row("SELECT length(zeroblob(40000000))", [], |r| {
          r.get::<_, i64>(0)
        })
        .unwrap_err();
      assert_eq!(DbError::from(err).tool_code, "SQLITE_TOOBIG", "{mode:?}");
    }
  }

  #[test]
  fn opening_sets_the_hard_heap_limit() {
    let (_d, path) = temp_db();
    let _conn = open(&path, Mode::Read, &Limits::default()).unwrap();
    // A negative argument reads the current limit without changing it.
    let limit = unsafe { rusqlite::ffi::sqlite3_hard_heap_limit64(-1) };
    assert_eq!(limit, 256 << 20);
  }

  #[test]
  fn the_busy_wait_shares_the_call_deadline() {
    let (_d, path) = temp_db();
    let limits = Limits {
      timeout: std::time::Duration::from_millis(600),
      ..Limits::default()
    };
    let conn = open(&path, Mode::Read, &limits).unwrap();
    // Most of the budget is spent before the lock is needed.
    std::thread::sleep(std::time::Duration::from_millis(450));
    let holder = Connection::open(&path).unwrap();
    holder.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let started = Instant::now();
    let err = conn
      .query_row("SELECT count(*) FROM t", [], |r| r.get::<_, i64>(0))
      .unwrap_err();
    assert_eq!(DbError::from(err).tool_code, "SQLITE_BUSY");
    // About 150 ms were left; a fresh per-wait timeout would wait the full 600 ms.
    assert!(
      started.elapsed() < std::time::Duration::from_millis(400),
      "{:?}",
      started.elapsed()
    );
  }

  #[test]
  fn harden_installs_the_authorizer_in_every_mode() {
    let (_d, path) = temp_db();
    for mode in [Mode::Read, Mode::Write { create: false }] {
      let conn = open(&path, mode, &Limits::default()).unwrap();
      // The authorizer refuses at prepare time ("not authorized"); without it the attach limit
      // would refuse at run time ("too many attached databases").
      let err = conn
        .execute_batch("ATTACH DATABASE ':memory:' AS o")
        .unwrap_err();
      assert!(
        err.to_string().contains("not authorized"),
        "{mode:?}: {err}"
      );
    }
  }

  #[test]
  fn attach_limit_blocks_new_files_even_for_exec() {
    let (_d, path) = temp_db();
    let dir = tempfile::tempdir().unwrap();
    let other = dir.path().join("other.db");
    // Only the limit: a plain connection with SQLITE_LIMIT_ATTACHED = 0.
    let conn = Connection::open(&path).unwrap();
    conn.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0);
    assert!(conn
      .execute_batch(&format!("ATTACH DATABASE '{}' AS o", other.display()))
      .is_err());
    assert!(conn
      .execute_batch(&format!("VACUUM INTO '{}'", other.display()))
      .is_err());
    assert!(!other.exists());
  }

  #[test]
  fn file_uris_are_literal_paths() {
    assert_eq!(
      literal_path(std::path::Path::new("file:x.db?mode=rwc")),
      PathBuf::from("./file:x.db?mode=rwc")
    );
    assert_eq!(
      literal_path(std::path::Path::new("FILE:x")),
      PathBuf::from("./FILE:x")
    );
    assert_eq!(
      literal_path(std::path::Path::new("/abs/file:x")),
      PathBuf::from("/abs/file:x")
    );
    assert_eq!(
      literal_path(std::path::Path::new("data.db")),
      PathBuf::from("data.db")
    );
    let dir = tempfile::tempdir().unwrap();
    let uri = format!("file:{}?mode=rwc", dir.path().join("made.db").display());
    let err = open(
      std::path::Path::new(&uri),
      Mode::Write { create: false },
      &Limits::default(),
    )
    .unwrap_err();
    assert_eq!(err.tool_code, "SQLITE_CANTOPEN");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
  }

  #[test]
  fn one_statement_exactly() {
    let conn = Connection::open_in_memory().unwrap();
    for sql in [
      "select 1",
      "select 1;",
      "  select 1 ;  ",
      "select 1; -- trailing comment",
      "select 1; /* c */",
    ] {
      assert!(single_statement(&conn, sql).is_ok(), "{sql:?}");
    }
    for sql in ["", "   ", ";", "-- only a comment", "/* x */ ;"] {
      assert_eq!(
        single_statement(&conn, sql).unwrap_err().tool_code,
        "EMPTY_STATEMENT",
        "{sql:?}"
      );
    }
    for sql in [
      "select 1; select 2",
      "select 1;select 2;",
      "create table a(x); insert into a values (1)",
      "select 1; garbage that does not parse",
      "select ';'; select 2",
    ] {
      assert_eq!(
        single_statement(&conn, sql).unwrap_err().tool_code,
        "TOO_MANY_STATEMENTS",
        "{sql:?}"
      );
    }
    assert_eq!(
      single_statement(&conn, "select 1\0; drop table t")
        .unwrap_err()
        .tool_code,
      "NUL_IN_STATEMENT"
    );
  }
}
