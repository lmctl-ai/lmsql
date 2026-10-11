//! The safety model, end to end through the binary: read operations cannot write by any
//! statement, exactly one statement per call, caps and timeouts hold, secrets never leak.

mod support;

use serde_json::{json, Value};
use std::time::Duration;
use support::*;

/// Statements that try to write through `query`; each must be refused and leave the file
/// byte-identical. (code, tool_code) of the refusal.
const WRITES_THROUGH_QUERY: &[(&str, &str, &str)] = &[
  (
    "INSERT INTO people(name) VALUES ('x')",
    "FORBIDDEN",
    "NOT_READ_ONLY",
  ),
  ("UPDATE people SET score = 0", "FORBIDDEN", "NOT_READ_ONLY"),
  ("DELETE FROM people", "FORBIDDEN", "NOT_READ_ONLY"),
  (
    "REPLACE INTO people(id, name) VALUES (1, 'x')",
    "FORBIDDEN",
    "NOT_READ_ONLY",
  ),
  (
    "WITH x(n) AS (SELECT 'y') INSERT INTO people(name) SELECT n FROM x",
    "FORBIDDEN",
    "NOT_READ_ONLY",
  ),
  (
    "INSERT INTO people(name) VALUES ('x') RETURNING id",
    "FORBIDDEN",
    "NOT_READ_ONLY",
  ),
  ("DROP TABLE people", "FORBIDDEN", "NOT_READ_ONLY"),
  ("CREATE TABLE evil(x)", "FORBIDDEN", "NOT_READ_ONLY"),
  ("CREATE TEMP TABLE evil(x)", "FORBIDDEN", "NOT_READ_ONLY"),
  (
    "ALTER TABLE people ADD COLUMN evil TEXT",
    "FORBIDDEN",
    "NOT_READ_ONLY",
  ),
  (
    "CREATE TRIGGER t AFTER INSERT ON people BEGIN DELETE FROM tags; END",
    "FORBIDDEN",
    "NOT_READ_ONLY",
  ),
  ("DROP INDEX people_score", "FORBIDDEN", "NOT_READ_ONLY"),
  ("ANALYZE", "FORBIDDEN", "NOT_READ_ONLY"),
  ("REINDEX", "FORBIDDEN", "NOT_READ_ONLY"),
  ("VACUUM", "FORBIDDEN", "NOT_READ_ONLY"),
  ("PRAGMA query_only = OFF", "FORBIDDEN", "SQLITE_AUTH"),
  ("PRAGMA writable_schema = 1", "FORBIDDEN", "SQLITE_AUTH"),
  ("PRAGMA writable_schema = RESET", "FORBIDDEN", "SQLITE_AUTH"),
  ("PRAGMA user_version = 42", "FORBIDDEN", "SQLITE_AUTH"),
  ("PRAGMA journal_mode = DELETE", "FORBIDDEN", "SQLITE_AUTH"),
  ("PRAGMA foreign_keys = OFF", "FORBIDDEN", "SQLITE_AUTH"),
  (
    "SELECT load_extension('/nonexistent/evil')",
    "FORBIDDEN",
    "SQLITE_AUTH",
  ),
  (
    "SELECT writefile('/tmp/lmsql-evil', 'x')",
    "VALIDATION_ERROR",
    "SQLITE_ERROR",
  ),
];

#[test]
fn query_can_never_write() {
  let (dir, db) = fixture();
  let before = snapshot(&db);
  for (sql, code, tool) in WRITES_THROUGH_QUERY {
    let out = run(&["query", "--db", &s(&db), sql]);
    let doc = out.json();
    assert_eq!(
      (doc["error"]["code"].as_str(), out.tool_code().as_str()),
      (Some(*code), *tool),
      "{sql}: {doc}"
    );
    assert_ne!(out.code, 0, "{sql}");
    assert!(snapshot(&db) == before, "{sql} changed the database");
  }
  // Nothing else appeared next to the database (journals, attached files).
  let names: Vec<String> = std::fs::read_dir(dir.path())
    .unwrap()
    .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
    .collect();
  assert_eq!(names, ["fixture.db"]);
}

#[test]
fn attach_and_vacuum_into_cannot_create_files_through_any_op() {
  let (dir, db) = fixture();
  let target = dir.path().join("stolen.db");
  let attach = format!("ATTACH DATABASE '{}' AS o", target.display());
  let vacuum = format!("VACUUM INTO '{}'", target.display());
  for (op, sql) in [
    ("query", &attach),
    ("query", &vacuum),
    ("exec", &attach),
    ("exec", &vacuum),
  ] {
    let out = run(&[op, "--db", &s(&db), sql, "--confirm"]);
    assert_ne!(out.code, 0, "{op} {sql}: {}", out.text());
    assert!(!target.exists(), "{op} {sql} created a file");
  }
}

#[test]
fn transaction_control_through_query_is_harmless() {
  // sqlite3_stmt_readonly calls BEGIN/COMMIT read-only; each call has its own connection, so
  // they change nothing and cannot open a write transaction for a later statement.
  let (_d, db) = fixture();
  let before = snapshot(&db);
  for sql in [
    "BEGIN",
    "BEGIN IMMEDIATE",
    "COMMIT",
    "SAVEPOINT a",
    "ROLLBACK",
  ] {
    let out = run(&["query", "--db", &s(&db), sql]);
    assert!(
      out.code == 0 || out.code == 2 || out.code == 6,
      "{sql}: {}",
      out.text()
    );
  }
  assert!(snapshot(&db) == before);
}

#[test]
fn uri_file_names_are_plain_paths() {
  // Without SQLITE_OPEN_URI, `file:...?mode=rwc` is a (missing) file name, not a mode change.
  let (dir, db) = fixture();
  let uri = format!("file:{}?mode=rwc", db.display());
  let out = run(&["query", "--db", &uri, "select 1"]);
  assert_eq!(
    (out.code, out.tool_code().as_str()),
    (4, "SQLITE_CANTOPEN"),
    "{}",
    out.text()
  );
  let out = run(&["exec", "--db", &uri, "create table x(a)", "--confirm"]);
  assert_eq!(out.code, 4, "{}", out.text());
  assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn exactly_one_statement_per_call() {
  let (_d, db) = fixture();
  let before = snapshot(&db);
  for (op, sql, tool) in [
    ("query", "select 1; select 2", "TOO_MANY_STATEMENTS"),
    (
      "query",
      "select 1; DROP TABLE people",
      "TOO_MANY_STATEMENTS",
    ),
    (
      "query",
      "select ';'; delete from people",
      "TOO_MANY_STATEMENTS",
    ),
    (
      "exec",
      "delete from tags; delete from people",
      "TOO_MANY_STATEMENTS",
    ),
    (
      "exec",
      "insert into tags values (3, 'x'); not even sql",
      "TOO_MANY_STATEMENTS",
    ),
    ("query", "", "EMPTY_STATEMENT"),
    ("query", "  \n\t ", "EMPTY_STATEMENT"),
    ("exec", ";", "EMPTY_STATEMENT"),
  ] {
    let out = run(&[op, "--db", &s(&db), sql, "--confirm"]);
    assert_eq!(
      (out.code, out.tool_code()),
      (2, tool.to_string()),
      "{op} {sql:?}: {}",
      out.text()
    );
  }
  // A NUL byte (only possible through JSON): SQLite would ignore everything after it.
  let body = json!({"db": s(&db), "statement": "select 1\u{0}; drop table people"}).to_string();
  let out = run(&["query", "--input-json", &body]);
  assert_eq!(
    (out.code, out.tool_code().as_str()),
    (2, "NUL_IN_STATEMENT"),
    "{}",
    out.text()
  );
  // A comment-only statement (sent as JSON: as an argv token it would read as a flag).
  let body = json!({"db": s(&db), "statement": "-- just a comment"}).to_string();
  let out = run(&["query", "--input-json", &body]);
  assert_eq!(
    (out.code, out.tool_code().as_str()),
    (2, "EMPTY_STATEMENT"),
    "{}",
    out.text()
  );
  assert!(
    snapshot(&db) == before,
    "a rejected multi-statement call ran its first statement"
  );
  // A trailing semicolon and trailing comments are one statement.
  for sql in [
    "select 1;",
    "select 1; -- done",
    "select 1 /* c */ ; /* d */",
  ] {
    let out = run(&["query", "--db", &s(&db), sql]);
    assert_eq!(out.code, 0, "{sql}: {}", out.text());
  }
}

#[test]
fn a_million_row_select_is_capped_fast_and_marked_truncated() {
  let (_d, db) = fixture();
  let sql =
    "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c LIMIT 1000000) SELECT x FROM c";
  let out = run(&["query", "--db", &s(&db), sql]);
  let doc = out.json();
  assert_eq!(out.code, 0);
  assert_eq!(
    (doc["row_count"].clone(), doc["truncated"].clone()),
    (json!(200), json!(true))
  );
  assert_eq!(doc["truncated_by"], "max_rows");
  assert_eq!(doc["rows"][199], json!([200]));
  assert!(out.elapsed < Duration::from_secs(5), "{:?}", out.elapsed);
  // The hard cap.
  let out = run(&["query", "--db", &s(&db), sql, "--max-rows", "10000"]);
  assert_eq!(out.json()["row_count"], 10000);
  let out = run(&["query", "--db", &s(&db), sql, "--max-rows", "10001"]);
  assert_eq!(
    (out.code, out.tool_code().as_str()),
    (2, "LIMIT_OUT_OF_RANGE")
  );
}

#[test]
fn exactly_max_rows_rows_is_not_truncated() {
  let (_d, db) = fixture();
  let out = run(&[
    "query",
    "--db",
    &s(&db),
    "select id from people",
    "--max-rows",
    "3",
  ]);
  assert_eq!(out.json()["truncated"], false);
  let out = run(&[
    "query",
    "--db",
    &s(&db),
    "select id from people",
    "--max-rows",
    "2",
  ]);
  let doc = out.json();
  assert_eq!(
    (doc["row_count"].clone(), doc["truncated_by"].clone()),
    (json!(2), json!("max_rows"))
  );
}

#[test]
fn max_bytes_caps_the_rows_without_dropping_silently() {
  let (_d, db) = fixture();
  let sql = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c LIMIT 1000) SELECT x, printf('%.100c', 'z') FROM c";
  let out = run(&[
    "query",
    "--db",
    &s(&db),
    sql,
    "--max-rows",
    "1000",
    "--max-bytes",
    "2000",
  ]);
  let doc = out.json();
  assert_eq!(
    (doc["truncated"].clone(), doc["truncated_by"].clone()),
    (json!(true), json!("max_bytes"))
  );
  let rows_len = serde_json::to_string(&doc["rows"]).unwrap().len();
  assert!(rows_len <= 2000, "{rows_len}");
  assert!(doc["row_count"].as_u64().unwrap() >= 10);
}

#[test]
fn huge_blobs_are_truncated_or_refused_never_returned_whole() {
  let (_d, db) = fixture();
  // 20 MB (under the 32 MiB value limit): the default 1 MiB cap stops it with zero rows.
  let out = run(&[
    "query",
    "--db",
    &s(&db),
    "SELECT zeroblob(20000000) AS b, 1",
  ]);
  let doc = out.json();
  assert_eq!(out.code, 0);
  assert_eq!(
    (doc["row_count"].clone(), doc["truncated_by"].clone()),
    (json!(0), json!("max_bytes"))
  );
  assert!(out.stdout.len() < 4096);
  // 50 MB is over the value limit: SQLite refuses to build it.
  let out = run(&[
    "query",
    "--db",
    &s(&db),
    "SELECT zeroblob(50000000) AS b, 1",
  ]);
  assert_eq!((out.code, out.tool_code().as_str()), (2, "SQLITE_TOOBIG"));
  // Under the cap it comes back as base64.
  let out = run(&["query", "--db", &s(&db), "SELECT zeroblob(3)"]);
  assert_eq!(out.json()["rows"][0][0], json!({"$blob_base64": "AAAA"}));
}

#[test]
fn an_infinite_recursive_cte_is_interrupted_at_the_timeout() {
  let (_d, db) = fixture();
  let sql =
    "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) SELECT count(*) FROM c";
  let out = run_with(
    &["query", "--db", &s(&db), sql, "--timeout-ms", "300"],
    &[],
    Duration::from_secs(20),
  );
  assert_eq!(
    (out.code, out.tool_code().as_str()),
    (7, "TIMEOUT"),
    "{}",
    out.text()
  );
  assert_eq!(out.json()["error"]["code"], "TIMEOUT");
  assert!(out.elapsed < Duration::from_secs(5), "{:?}", out.elapsed);
  // exec too.
  let out = run_with(
    &[
      "exec",
      "--db",
      &s(&db),
      "INSERT INTO tags(person_id, tag) SELECT 9, x FROM (WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) SELECT count(*) AS x FROM c)",
      "--timeout-ms",
      "300",
      "--confirm",
    ],
    &[],
    Duration::from_secs(20),
  );
  assert_eq!(out.code, 7, "{}", out.text());
}

#[test]
fn a_locked_database_is_a_conflict_after_the_busy_timeout() {
  let (_d, db) = fixture();
  let holder = rusqlite::Connection::open(&db).unwrap();
  holder.execute_batch("BEGIN EXCLUSIVE").unwrap();
  let out = run(&[
    "exec",
    "--db",
    &s(&db),
    "delete from tags",
    "--timeout-ms",
    "200",
    "--confirm",
  ]);
  assert_eq!(
    (out.code, out.tool_code().as_str()),
    (5, "SQLITE_BUSY"),
    "{}",
    out.text()
  );
  let out = run(&[
    "query",
    "--db",
    &s(&db),
    "select count(*) from tags",
    "--timeout-ms",
    "200",
  ]);
  assert_eq!(
    (out.code, out.tool_code().as_str()),
    (5, "SQLITE_BUSY"),
    "{}",
    out.text()
  );
  holder.execute_batch("ROLLBACK").unwrap();
  let out = run(&["query", "--db", &s(&db), "select count(*) from tags"]);
  assert_eq!(out.json()["rows"], json!([[3]]));
}

#[test]
fn paths_and_dsns_never_appear_in_output() {
  let (dir, db) = fixture();
  let secret_dir = dir.path().join("very-secret-dir");
  std::fs::create_dir(&secret_dir).unwrap();
  let missing = secret_dir.join("missing.db");
  // (argv, environment)
  type Case<'a> = (Vec<String>, Vec<(&'a str, String)>);
  let cases: Vec<Case> = vec![
    (
      vec![
        "query".into(),
        "--db".into(),
        s(&missing),
        "select 1".into(),
      ],
      vec![],
    ),
    (
      vec![
        "exec".into(),
        "--db".into(),
        s(&missing),
        "select 1".into(),
        "--confirm".into(),
      ],
      vec![],
    ),
    (
      vec![
        "query".into(),
        "--db".into(),
        s(&secret_dir),
        "select 1".into(),
      ],
      vec![],
    ),
    (
      vec!["describe".into(), "--db".into(), s(&db), "nope".into()],
      vec![],
    ),
    (
      vec![
        "query".into(),
        "--db".into(),
        s(&db),
        "select * from nope".into(),
      ],
      vec![],
    ),
    (
      vec![
        "query".into(),
        "--connection".into(),
        "prod".into(),
        "select 1".into(),
      ],
      vec![("LMSQL_PROD", s(&missing))],
    ),
    (
      vec![
        "query".into(),
        "--connection".into(),
        "pg".into(),
        "select 1".into(),
      ],
      vec![(
        "LMSQL_PG",
        "postgres://admin:hunter2@very-secret-dir.example/db".into(),
      )],
    ),
  ];
  for (args, env) in cases {
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let out = run_with(&argv, &env, Duration::from_secs(20));
    assert_ne!(out.code, 0, "{argv:?}");
    let all = format!("{}{}", out.text(), out.stderr);
    for secret in [
      "very-secret-dir",
      "hunter2",
      "admin",
      "missing.db",
      &s(dir.path()),
    ] {
      assert!(!all.contains(secret), "{argv:?} leaked {secret:?}: {all}");
    }
  }
}

#[test]
fn unicode_nul_and_very_long_statements() {
  let (_d, db) = fixture();
  let out = run(&[
    "query",
    "--db",
    &s(&db),
    "SELECT name FROM people WHERE name LIKE 'chi%'",
  ]);
  assert_eq!(out.json()["rows"], json!([["chi ☃"]]));
  let out = run(&[
    "query",
    "--db",
    &s(&db),
    "SELECT 'a' || char(0) || 'b', '日本語', '🦀'",
  ]);
  assert_eq!(out.json()["rows"], json!([["a\u{0}b", "日本語", "🦀"]]));
  // A long statement (a 20k-item IN list; deep expressions stop at SQLite's depth 1000).
  let long = format!(
    "SELECT count(*) FROM (SELECT 1) WHERE 1 IN ({})",
    vec!["1"; 20_000].join(",")
  );
  assert!(long.len() > 40_000);
  let file = _d.path().join("long.json");
  std::fs::write(&file, json!({"db": s(&db), "statement": long}).to_string()).unwrap();
  let out = run(&["query", "--input-file", &s(&file)]);
  assert_eq!(
    out.json()["rows"],
    json!([[1]]),
    "{}",
    &out.text()[..out.text().len().min(300)]
  );
  // Quotes and backslashes in values come back exactly.
  let out = run(&["query", "--db", &s(&db), r#"SELECT 'it''s', '\n', '"q"'"#]);
  assert_eq!(out.json()["rows"], json!([["it's", "\\n", "\"q\""]]));
}

#[test]
fn injection_shaped_input_is_just_one_statement() {
  let (_d, db) = fixture();
  let before = snapshot(&db);
  // Classic payloads: either one read-only statement (harmless) or rejected.
  for sql in [
    "SELECT * FROM people WHERE name = '' OR '1'='1'",
    "SELECT * FROM people WHERE name = 'x' UNION SELECT sql, 1, 1, 1, 1 FROM sqlite_schema",
    "SELECT * FROM people WHERE name = ''; DROP TABLE people; --'",
    "SELECT * FROM people -- '; DELETE FROM people",
    "SELECT * FROM people WHERE id = 1/*; DELETE FROM people */",
  ] {
    let out = run(&["query", "--db", &s(&db), sql]);
    assert!(out.code == 0 || out.code == 2, "{sql}: {}", out.text());
    serde_json::from_slice::<Value>(&out.stdout).expect("one JSON document");
  }
  assert!(snapshot(&db) == before);
}
