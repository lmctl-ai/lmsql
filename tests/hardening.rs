//! Review findings (FableReviewer, phase 1): each test reproduces one finding end to end.

mod support;

use serde_json::json;
use std::path::Path;
use std::time::{Duration, Instant};
use support::*;

fn quick(args: &[&str]) -> Out {
  // A 3 s harness: anything that hangs (fifo) or runs unbounded fails here.
  run_with(args, &[], Duration::from_secs(3))
}

// Finding 1: one SQLite builtin could allocate and run unbounded inside a single VM step.
#[test]
fn oversized_values_fail_fast_with_sqlite_toobig() {
  let (_d, db) = fixture();
  for (sql, expect_toobig) in [
    // SQLite's printf() returns NULL (no error) when its result would pass the length limit.
    ("SELECT length(printf('%*c', 800000000, 'x'))", false),
    ("SELECT length(hex(randomblob(300000000)))", true),
    ("SELECT zeroblob(300000000)", true),
    ("SELECT length(zeroblob(40000000))", true),
  ] {
    let started = Instant::now();
    let out = quick(&[
      "query",
      "--db",
      &s(&db),
      sql,
      "--timeout-ms",
      "100",
      "--max-bytes",
      "100",
    ]);
    if expect_toobig {
      assert_eq!(
        (out.code, out.tool_code().as_str()),
        (2, "SQLITE_TOOBIG"),
        "{sql}: {}",
        out.text()
      );
    } else {
      assert_eq!(
        (out.code, out.json()["rows"].clone()),
        (0, json!([[null]])),
        "{sql}"
      );
    }
    assert!(
      started.elapsed() < Duration::from_secs(2),
      "{sql}: {:?}",
      started.elapsed()
    );
  }
  // Values within the length limit still work.
  let out = quick(&["query", "--db", &s(&db), "SELECT length(zeroblob(1000000))"]);
  assert_eq!(out.json()["rows"], json!([[1000000]]));
}

// Finding 2: reading a WAL database in a read-only directory said "attempt to write a readonly
// database" (FORBIDDEN/SQLITE_READONLY), misleading for a read.
#[cfg(unix)]
#[test]
fn a_wal_database_in_a_read_only_directory_gets_a_clear_error() {
  use std::os::unix::fs::PermissionsExt;
  let dir = tempfile::tempdir().unwrap();
  let ro = dir.path().join("ro");
  std::fs::create_dir(&ro).unwrap();
  let db = ro.join("w.db");
  {
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn
      .execute_batch("PRAGMA journal_mode = WAL; CREATE TABLE t(x); INSERT INTO t VALUES (1);")
      .unwrap();
  }
  // Closing the last connection removed -wal/-shm; make the directory read-only.
  std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
  let writable = std::fs::write(ro.join("probe"), "x").is_ok(); // root ignores modes
  let out = quick(&["query", "--db", &s(&db), "SELECT count(*) FROM t"]);
  std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).unwrap();
  if writable {
    return;
  }
  let doc = out.json();
  assert_eq!(
    (out.code, out.tool_code().as_str()),
    (6, "DIRECTORY_READONLY"),
    "{doc}"
  );
  let message = doc["error"]["message"].as_str().unwrap();
  assert!(
    message.contains("WAL") && message.contains("-shm"),
    "{message}"
  );
  assert!(!message.contains("attempt to write"), "{message}");
}

// Finding 2 (documented behaviour): a read never changes the database file; for a WAL
// database SQLite creates its -wal/-shm coordination files when they are missing.
#[test]
fn reading_a_wal_database_leaves_the_database_file_unchanged() {
  let dir = tempfile::tempdir().unwrap();
  let db = dir.path().join("w.db");
  {
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn
      .execute_batch("PRAGMA journal_mode = WAL; CREATE TABLE t(x); INSERT INTO t VALUES (1);")
      .unwrap();
  }
  let before = snapshot(&db);
  let out = quick(&["query", "--db", &s(&db), "SELECT count(*) FROM t"]);
  assert_eq!(out.json()["rows"], json!([[1]]));
  assert!(snapshot(&db) == before);
  let mut names: Vec<String> = std::fs::read_dir(dir.path())
    .unwrap()
    .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
    .collect();
  names.sort();
  assert_eq!(names, ["w.db", "w.db-shm", "w.db-wal"]);
  // The -wal file holds no frames from the read.
  assert_eq!(
    std::fs::metadata(dir.path().join("w.db-wal"))
      .unwrap()
      .len(),
    0
  );
}

// Finding 3: non-regular --db paths (fifo hang, devices, directories) and special names.
#[test]
fn non_regular_paths_and_special_names_are_refused_without_opening() {
  let dir = tempfile::tempdir().unwrap();
  let fifo = dir.path().join("fifo.db");
  #[cfg(unix)]
  {
    let ok = std::process::Command::new("mkfifo")
      .arg(&fifo)
      .status()
      .unwrap()
      .success();
    assert!(ok);
  }
  let mut cases: Vec<(String, &str)> = vec![
    (s(dir.path()), "NOT_A_REGULAR_FILE"),
    (":memory:".into(), "INVALID_DB_PATH"),
    (" ".into(), "INVALID_DB_PATH"),
    ("\t \n".into(), "INVALID_DB_PATH"),
  ];
  if cfg!(unix) {
    cases.push((s(&fifo), "NOT_A_REGULAR_FILE"));
    cases.push(("/dev/zero".into(), "NOT_A_REGULAR_FILE"));
    cases.push(("/dev/null".into(), "NOT_A_REGULAR_FILE"));
  }
  for (path, tool) in &cases {
    for args in [
      vec!["query", "--db", path.as_str(), "select 1"],
      vec!["tables", "--db", path.as_str()],
      vec!["describe", "--db", path.as_str(), "t"],
      vec![
        "exec",
        "--db",
        path.as_str(),
        "create table x(a)",
        "--confirm",
      ],
      vec![
        "exec",
        "--db",
        path.as_str(),
        "create table x(a)",
        "--confirm",
        "--create",
      ],
    ] {
      let out = quick(&args);
      assert_eq!(
        (out.code, out.tool_code().as_str()),
        (2, *tool),
        "{args:?}: {}",
        out.text()
      );
    }
  }
  // Nothing was created by a refused :memory: or blank name.
  assert_eq!(
    std::fs::read_dir(dir.path()).unwrap().count(),
    if cfg!(unix) { 1 } else { 0 }
  );
}

#[cfg(unix)]
#[test]
fn a_symlink_to_a_regular_database_still_works() {
  let (dir, db) = fixture();
  let link = dir.path().join("current.db");
  std::os::unix::fs::symlink(&db, &link).unwrap();
  let out = quick(&["query", "--db", &s(&link), "SELECT count(*) FROM people"]);
  assert_eq!(out.json()["rows"], json!([[3]]));
  let dangling = dir.path().join("dangling.db");
  std::os::unix::fs::symlink(dir.path().join("gone.db"), &dangling).unwrap();
  let out = quick(&["query", "--db", &s(&dangling), "select 1"]);
  assert_eq!((out.code, out.tool_code().as_str()), (4, "SQLITE_CANTOPEN"));
}

// Finding 4: the scrubber garbled messages for short paths.
#[test]
fn short_paths_do_not_garble_messages() {
  let dir = tempfile::tempdir().unwrap();
  for path in ["a", ".db", "data", "e", "not"] {
    let out = run_with(
      &["query", "--db", path, "select 1"],
      &[],
      Duration::from_secs(10),
    );
    let message = out.json()["error"]["message"].as_str().unwrap().to_string();
    assert_eq!(
      (out.code, message.as_str()),
      (4, "the database file does not exist"),
      "--db {path:?}"
    );
  }
  // A real path inside an SQLite message is still replaced.
  let missing_dir = dir.path().join("no-such-dir").join("x.db");
  let out = quick(&[
    "exec",
    "--db",
    &s(&missing_dir),
    "create table t(a)",
    "--confirm",
    "--create",
  ]);
  let text = out.text();
  assert_ne!(out.code, 0);
  assert!(!text.contains("no-such-dir"), "{text}");
  let _ = Path::new("");
}

// Finding 6a: the busy wait shares the call's single deadline.
#[test]
fn a_held_lock_bounds_the_whole_call_by_the_timeout() {
  let (_d, db) = fixture();
  let holder = rusqlite::Connection::open(&db).unwrap();
  holder.execute_batch("BEGIN EXCLUSIVE").unwrap();
  for op in [
    vec![
      "query",
      "--db",
      &s(&db),
      "SELECT count(*) FROM tags",
      "--timeout-ms",
      "400",
    ],
    vec!["describe", "--db", &s(&db), "tags", "--timeout-ms", "400"],
    vec![
      "exec",
      "--db",
      &s(&db),
      "DELETE FROM tags",
      "--timeout-ms",
      "400",
      "--confirm",
    ],
  ] {
    let out = quick(&op);
    assert_eq!(
      (out.code, out.tool_code().as_str()),
      (5, "SQLITE_BUSY"),
      "{op:?}"
    );
    assert!(
      out.elapsed < Duration::from_millis(1200),
      "{op:?}: {:?}",
      out.elapsed
    );
  }
  holder.execute_batch("ROLLBACK").unwrap();
}

// Finding 6b: load_extension() inside a hostile view is a refusal like the authorizer's.
#[test]
fn load_extension_inside_a_view_is_refused_like_the_authorizer() {
  let (_d, db) = fixture();
  rusqlite::Connection::open(&db)
    .unwrap()
    .execute_batch("CREATE VIEW evil AS SELECT load_extension('/nonexistent/evil') AS x")
    .unwrap();
  let path = s(&db);
  for argv in [
    vec!["query", "--db", path.as_str(), "SELECT * FROM evil"],
    vec![
      "exec",
      "--db",
      path.as_str(),
      "SELECT * FROM evil",
      "--confirm",
    ],
  ] {
    let out = quick(&argv);
    assert_eq!(
      (out.code, out.tool_code().as_str()),
      (6, "SQLITE_AUTH"),
      "{argv:?}: {}",
      out.text()
    );
  }
}
