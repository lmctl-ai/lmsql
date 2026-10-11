//! The four operations end to end against a fixture database, with golden outputs
//! (`tests/goldens/*.json`; regenerate with `UPDATE_GOLDENS=1 cargo test --test ops`).

mod support;

use serde_json::json;
use std::path::Path;
use support::*;

fn golden(name: &str, out: &Out) {
  let file = Path::new(env!("CARGO_MANIFEST_DIR"))
    .join("tests/goldens")
    .join(format!("{name}.json"));
  let actual = format!(
    "{{\"exit\": {}, \"stdout\": {}}}\n",
    out.code,
    out.text().trim_end()
  );
  // Normalize through serde to keep the files stable and readable.
  let value: serde_json::Value = serde_json::from_str(&actual).unwrap();
  let text = serde_json::to_string_pretty(&value).unwrap() + "\n";
  if std::env::var_os("UPDATE_GOLDENS").is_some() {
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, &text).unwrap();
  }
  let expected = std::fs::read_to_string(&file)
    .unwrap_or_else(|_| panic!("missing golden {} (UPDATE_GOLDENS=1)", file.display()));
  assert_eq!(text, expected, "golden {name}");
}

#[test]
fn goldens() {
  let (_d, db) = fixture();
  let db = s(&db);
  let cases: Vec<(&str, Vec<&str>)> = vec![
    ("query_people", vec!["query", "--db", &db, "SELECT * FROM people ORDER BY id"]),
    ("query_view", vec!["query", "--db", &db, "SELECT * FROM top"]),
    ("query_join", vec!["query", "--db", &db, "SELECT p.name, group_concat(t.tag, ',') AS tags FROM people p LEFT JOIN tags t ON t.person_id = p.id GROUP BY p.id ORDER BY p.id"]),
    ("query_empty", vec!["query", "--db", &db, "SELECT * FROM people WHERE id < 0"]),
    ("query_pragma_read", vec!["query", "--db", &db, "PRAGMA table_info(tags)"]),
    ("query_explain", vec!["query", "--db", &db, "EXPLAIN QUERY PLAN SELECT * FROM people WHERE score > 0.5"]),
    ("tables", vec!["tables", "--db", &db]),
    ("describe_people", vec!["describe", "--db", &db, "people"]),
    ("describe_view_case_insensitive", vec!["describe", "--db", &db, "TOP"]),
    ("describe_missing", vec!["describe", "--db", &db, "nope"]),
    ("error_syntax", vec!["query", "--db", &db, "SELEC 1"]),
    ("error_no_such_table", vec!["query", "--db", &db, "SELECT * FROM nope"]),
    ("error_not_read_only", vec!["query", "--db", &db, "DELETE FROM tags"]),
    ("error_no_connection", vec!["query", "select 1"]),
  ];
  for (name, args) in cases {
    golden(name, &run(&args));
  }
}

#[test]
fn exec_changes_data_and_reports_counts() {
  let (_d, db) = fixture();
  let db = s(&db);
  let out = run(&[
    "exec",
    "--db",
    &db,
    "INSERT INTO tags VALUES (3, 'snow'), (3, 'ice')",
    "--confirm",
  ]);
  assert_eq!(
    out.json(),
    json!({"rows_affected": 2, "last_insert_rowid": 5})
  );
  let out = run(&[
    "exec",
    "--db",
    &db,
    "UPDATE people SET score = score + 1 WHERE score < 1",
    "--confirm",
  ]);
  assert_eq!(out.json()["rows_affected"], 2);
  let out = run(&[
    "exec",
    "--db",
    &db,
    "INSERT INTO people(name) VALUES ('dee') RETURNING id, name, score",
    "--confirm",
  ]);
  let doc = out.json();
  assert_eq!(doc["rows_affected"], 1);
  assert_eq!(doc["last_insert_rowid"], 4);
  assert_eq!(doc["returning"]["rows"], json!([[4, "dee", 0.5]]));
  // DDL.
  let out = run(&[
    "exec",
    "--db",
    &db,
    "CREATE TABLE notes(id INTEGER PRIMARY KEY, body TEXT)",
    "--confirm",
  ]);
  assert_eq!(
    out.json(),
    json!({"rows_affected": 0, "last_insert_rowid": 0})
  );
  let out = run(&["tables", "--db", &db]);
  assert!(out.text().contains("\"notes\""));
  // Write-mode pragmas are allowed through exec.
  let out = run(&["exec", "--db", &db, "PRAGMA user_version = 7", "--confirm"]);
  assert_eq!(out.code, 0, "{}", out.text());
  let out = run(&["query", "--db", &db, "PRAGMA user_version"]);
  assert_eq!(out.json()["rows"], json!([[7]]));
}

#[test]
fn exec_errors_map_to_standard_codes() {
  let (_d, db) = fixture();
  let db = s(&db);
  for (sql, exit, code, tool) in [
    (
      "INSERT INTO people(name) VALUES ('ada')",
      5,
      "CONFLICT",
      "SQLITE_CONSTRAINT_UNIQUE",
    ),
    (
      "INSERT INTO people(id, name) VALUES (1, 'z')",
      5,
      "CONFLICT",
      "SQLITE_CONSTRAINT_PRIMARYKEY",
    ),
    (
      "INSERT INTO people(name) VALUES (NULL)",
      5,
      "CONFLICT",
      "SQLITE_CONSTRAINT_NOTNULL",
    ),
    (
      "INSERT INTO nope VALUES (1)",
      2,
      "VALIDATION_ERROR",
      "SQLITE_ERROR",
    ),
    ("UPDATE people SET", 2, "VALIDATION_ERROR", "SQLITE_ERROR"),
  ] {
    let out = run(&["exec", "--db", &db, sql, "--confirm"]);
    let doc = out.json();
    assert_eq!(
      (
        out.code,
        doc["error"]["code"].as_str(),
        out.tool_code().as_str()
      ),
      (exit, Some(code), tool),
      "{sql}: {doc}"
    );
  }
  // A syntax error reports its byte offset (an incomplete statement has none).
  let out = run(&[
    "exec",
    "--db",
    &db,
    "UPDATE people SETT score = 1",
    "--confirm",
  ]);
  assert_eq!(out.json()["error"]["data"]["offset"], 14, "{}", out.text());
}

#[test]
fn exec_does_not_create_databases_unless_asked() {
  let dir = tempfile::tempdir().unwrap();
  let db = dir.path().join("new.db");
  let out = run(&["exec", "--db", &s(&db), "CREATE TABLE t(x)", "--confirm"]);
  assert_eq!((out.code, out.tool_code().as_str()), (4, "SQLITE_CANTOPEN"));
  assert!(!db.exists());
  let out = run(&[
    "exec",
    "--db",
    &s(&db),
    "CREATE TABLE t(x)",
    "--confirm",
    "--create",
  ]);
  assert_eq!(out.code, 0, "{}", out.text());
  assert!(db.exists());
  // Reads never create.
  let other = dir.path().join("other.db");
  let out = run(&["query", "--db", &s(&other), "select 1"]);
  assert_eq!((out.code, out.tool_code().as_str()), (4, "SQLITE_CANTOPEN"));
  assert!(!other.exists());
}

#[test]
fn type_mapping_through_sql() {
  let (_d, db) = fixture();
  let out = run(&[
    "query",
    "--db",
    &s(&db),
    "SELECT NULL, 42, -9223372036854775808, 9223372036854775807, 1.5, 1e308 * 10, -1e308 * 10, \
     0.0 / 0.0, 'text', CAST(x'C3A9' AS TEXT), CAST(x'FF00FE' AS TEXT), x'DEADBEEF', x'', zeroblob(2)",
  ]);
  let doc = out.json();
  assert_eq!(out.code, 0, "{doc}");
  assert_eq!(
    doc["rows"][0],
    json!([
      null,
      42,
      i64::MIN,
      i64::MAX,
      1.5,
      null,
      null,
      null,
      "text",
      "é",
      {"$text_base64": "/wD+"},
      {"$blob_base64": "3q2+7w=="},
      {"$blob_base64": ""},
      {"$blob_base64": "AAA="}
    ])
  );
  // Column types: declared types for table columns, null for expressions.
  let out = run(&[
    "query",
    "--db",
    &s(&db),
    "SELECT id, name, score + 1 AS s FROM people LIMIT 1",
  ]);
  assert_eq!(
    out.json()["columns"],
    json!([{"name": "id", "type": "INTEGER"}, {"name": "name", "type": "TEXT"}, {"name": "s", "type": null}])
  );
}

#[test]
fn json_and_flags_forms_agree() {
  let (_d, db) = fixture();
  let flags = run(&[
    "query",
    "--db",
    &s(&db),
    "SELECT name FROM people ORDER BY id",
    "--max-rows",
    "2",
  ]);
  let body =
    json!({"db": s(&db), "statement": "SELECT name FROM people ORDER BY id", "maxRows": 2})
      .to_string();
  let json_form = run(&["query", "--input-json", &body]);
  let envelope = json!({"op": "query", "params": {"db": s(&db), "statement": "SELECT name FROM people ORDER BY id", "maxRows": 2}}).to_string();
  let doc_form = run(&["--input-json", &envelope]);
  assert_eq!(flags.stdout, json_form.stdout);
  assert_eq!(flags.stdout, doc_form.stdout);
  assert_eq!(flags.json()["rows"], json!([["ada"], ["bo"]]));
}

#[test]
fn named_connections_from_the_environment() {
  let (_d, db) = fixture();
  let path = s(&db);
  let uri = format!("sqlite://{path}");
  for value in [path.as_str(), uri.as_str()] {
    let out = run_with(
      &[
        "query",
        "--connection",
        "main",
        "SELECT count(*) FROM people",
      ],
      &[("LMSQL_MAIN", value)],
      std::time::Duration::from_secs(20),
    );
    assert_eq!(out.json()["rows"], json!([[3]]), "{value}");
  }
  let out = run_with(
    &["query", "--connection", "pg", "select 1"],
    &[(
      "LMSQL_PG",
      "postgresql://u:p@h.invalid/db?connect_timeout=2",
    )],
    std::time::Duration::from_secs(20),
  );
  // A Postgres DSN is a Postgres connection now; this host does not exist.
  assert_eq!((out.code, out.tool_code().as_str()), (8, "CONNECT_FAILED"));
  let out = run(&["query", "--connection", "nope", "select 1"]);
  assert_eq!(
    (out.code, out.tool_code().as_str()),
    (4, "CONNECTION_NOT_FOUND")
  );
  let out = run(&["query", "--connection", "main", "--db", &path, "select 1"]);
  assert_eq!(
    (out.code, out.tool_code().as_str()),
    (2, "CONNECTION_CONFLICT")
  );
}
