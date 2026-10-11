//! `--dry-run` (lmrust's dryrun global, also `LMSQL_DRYRUN=1`): reads run normally; anything that
//! would change something is not executed and is described instead. No file is written.

mod support;

use serde_json::{json, Value};
use std::time::Duration;
use support::*;

fn sqlite_db() -> (tempfile::TempDir, String) {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("d.db");
  rusqlite::Connection::open(&path)
    .unwrap()
    .execute_batch("CREATE TABLE t(x INTEGER); INSERT INTO t VALUES (1), (2), (3);")
    .unwrap();
  let db = s(&path);
  (dir, db)
}

fn snapshot_sqlite(db: &str) -> Vec<String> {
  let conn = rusqlite::Connection::open(db).unwrap();
  let mut stmt = conn
    .prepare("SELECT type || ' ' || name FROM sqlite_schema ORDER BY name")
    .unwrap();
  let mut out: Vec<String> = stmt
    .query_map([], |r| r.get(0))
    .unwrap()
    .map(Result::unwrap)
    .collect();
  let rows: String = conn
    .query_row("SELECT group_concat(x) FROM t", [], |r| r.get(0))
    .unwrap();
  out.push(rows);
  out
}

fn keys(doc: &Value) -> (Value, Value, Value) {
  (
    doc["dry_run"].clone(),
    doc["executed"].clone(),
    doc["would_change"].clone(),
  )
}

#[test]
fn sqlite_dry_run_reads_run_and_changes_are_only_described() {
  let (_d, db) = sqlite_db();
  let before = snapshot_sqlite(&db);
  // (sql, executed, plan expected)
  let cases: &[(&str, bool, bool)] = &[
    ("SELECT x FROM t ORDER BY x", true, false),
    ("DELETE FROM t WHERE x > 1", false, true),
    ("UPDATE t SET x = x + 10", false, true),
    // Nothing to scan: SQLite has no plan for a VALUES insert (best effort, null).
    ("INSERT INTO t VALUES (9)", false, false),
    ("CREATE TABLE z(a)", false, false),
    ("DROP TABLE t", false, false),
  ];
  for (sql, executed, has_plan) in cases {
    let out = run(&["query", "--db", &db, sql, "--dry-run"]);
    assert_eq!(out.code, 0, "{sql}: {}", out.text());
    let doc = out.json();
    assert_eq!(
      keys(&doc),
      (json!(true), json!(executed), json!(!executed)),
      "{sql}: {doc}"
    );
    if *executed {
      assert_eq!(doc["rows"], json!([[1], [2], [3]]));
    } else {
      assert_eq!(doc["statement"], *sql);
      assert_eq!(doc["plan"].is_array(), *has_plan, "{sql}: {doc}");
    }
  }
  // Multi-statement text with one write: nothing runs, each statement is described.
  let out = run(&[
    "exec",
    "--db",
    &db,
    "SELECT 1; DELETE FROM t; SELECT 2",
    "--dry-run",
  ]);
  let doc = out.json();
  assert_eq!(
    keys(&doc),
    (json!(true), json!(false), json!(true)),
    "{doc}"
  );
  let flags: Vec<Value> = doc["statements"]
    .as_array()
    .unwrap()
    .iter()
    .map(|s| json!([s["statement"], s["would_change"]]))
    .collect();
  assert_eq!(
    flags,
    [
      json!(["SELECT 1", false]),
      json!(["DELETE FROM t", true]),
      json!(["SELECT 2", false])
    ]
  );
  // A later statement that needs an earlier, unexecuted change is reported, not an error.
  let out = run(&[
    "exec",
    "--db",
    &db,
    "CREATE TABLE n(a); INSERT INTO n VALUES (1)",
    "--dry-run",
  ]);
  assert_eq!(out.code, 0, "{}", out.text());
  assert_eq!(out.json()["would_change"], true);
  // The environment variable works too.
  let out = run_with(
    &["exec", "--db", &db, "DELETE FROM t"],
    &[("LMSQL_DRYRUN", "1")],
    Duration::from_secs(30),
  );
  assert_eq!(out.json()["executed"], false, "{}", out.text());
  assert_eq!(
    snapshot_sqlite(&db),
    before,
    "a dry run changed the database"
  );
}

#[test]
fn dry_run_writes_no_file() {
  let (dir, db) = sqlite_db();
  let target = dir.path().join("sub/out.csv");
  let out = run(&[
    "query",
    "--db",
    &db,
    "SELECT x FROM t",
    "--out",
    &s(&target),
    "--dry-run",
  ]);
  let doc = out.json();
  assert_eq!(out.code, 0, "{doc}");
  assert_eq!(doc["file"], Value::Null);
  assert_eq!(
    doc["would_write"],
    json!({"path": s(&target), "format": "csv"})
  );
  assert_eq!(
    (doc["row_count"].clone(), doc["preview"].clone()),
    (json!(3), json!([[1], [2], [3]]))
  );
  assert!(!target.exists() && !dir.path().join("sub").exists());
  // Would have spilled: a note and a preview, no spill file.
  let spill = dir.path().join("spill");
  let out = run_with(
    &[
      "query",
      "--db",
      &db,
      "SELECT x FROM t",
      "--max-rows",
      "2",
      "--dry-run",
    ],
    &[("LMSQL_OUT_DIR", &s(&spill))],
    Duration::from_secs(30),
  );
  let doc = out.json();
  assert_eq!(doc["file"], Value::Null, "{doc}");
  assert!(doc["note"].as_str().unwrap().contains("spilled"), "{doc}");
  assert_eq!(doc["preview"], json!([[1], [2]]));
  assert!(!spill.exists());
}

#[test]
fn dry_run_keeps_permissions() {
  let (dir, db) = sqlite_db();
  let settings = dir.path().join("settings.json");
  let write = |default: &str| {
    let doc = json!({"version": 1, "connections": {"d": {"driver": "sqlite", "path": db,
      "permissions": {"default": default, "tables": {"hidden": "none"}}}}});
    std::fs::write(&settings, doc.to_string()).unwrap();
  };
  rusqlite::Connection::open(&db)
    .unwrap()
    .execute_batch("CREATE TABLE hidden(s)")
    .unwrap();
  let env = [("LMSQL_CONFIG", s(&settings))];
  let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
  // (default level, sql, denied)
  for (default, sql, denied) in [
    ("readonly", "SELECT x FROM t", false),
    ("readonly", "DELETE FROM t", true),
    ("readwrite", "DELETE FROM t", false),
    ("readwrite", "SELECT s FROM hidden", true),
  ] {
    write(default);
    let out = run_with(
      &["query", "--connection", "d", sql, "--dry-run"],
      &env,
      Duration::from_secs(30),
    );
    if denied {
      assert_eq!(
        out.tool_code(),
        "PERMISSION_DENIED",
        "{default} {sql}: {}",
        out.text()
      );
    } else {
      assert_eq!(
        out.json()["dry_run"],
        true,
        "{default} {sql}: {}",
        out.text()
      );
    }
  }
}

#[test]
fn pg_dry_run() {
  let Some(url) = pg_url() else { return };
  let schema = PgSchema::new(&url, "dryrun");
  let t = format!("{}.t", schema.name);
  pg_admin(&url)
    .batch_execute(&format!(
      "CREATE TABLE {t}(x int); INSERT INTO {t} SELECT generate_series(1, 1000); ANALYZE {t};"
    ))
    .unwrap();
  let count = || -> i64 {
    pg_admin(&url)
      .query_one(&format!("SELECT count(*) FROM {t}") as &str, &[])
      .unwrap()
      .get(0)
  };
  // (sql, executed, plan expected)
  let select = format!("SELECT count(*) FROM {t}");
  let delete = format!("DELETE FROM {t} WHERE x <= 100");
  let insert = format!("INSERT INTO {t} SELECT generate_series(1, 10)");
  let create = format!("CREATE TABLE {}.z(a int)", schema.name);
  let multi = format!("SELECT 1; DELETE FROM {t}");
  for (sql, executed, has_plan) in [
    (&select, true, false),
    (&delete, false, true),
    (&insert, false, true),
    (&create, false, false),
    (&multi, false, false),
  ] {
    let out = pg_run(&url, &["query", sql, "--dry-run"]);
    assert_eq!(out.code, 0, "{sql}: {}", out.text());
    let doc = out.json();
    assert_eq!(
      keys(&doc),
      (json!(true), json!(executed), json!(!executed)),
      "{sql}: {doc}"
    );
    if executed {
      assert_eq!(doc["rows"], json!([[1000]]));
    } else {
      assert_eq!(doc["plan"].is_array(), has_plan, "{sql}: {doc}");
    }
  }
  let doc = pg_run(&url, &["query", &delete, "--dry-run"]).json();
  assert_eq!(doc["estimated_rows"], 100.0, "{doc}");
  assert_eq!(count(), 1000, "a dry run changed the table");
  let exists: bool = pg_admin(&url)
    .query_one(
      "SELECT to_regclass($1) IS NOT NULL",
      &[&format!("{}.z", schema.name)],
    )
    .unwrap()
    .get(0);
  assert!(!exists);
}
