//! Functional end-to-end tests of the SQLite backend in open mode (through the built binary).
//! `query` and `exec` are equivalent: both run any statement and return one shape.

mod support;

use serde_json::{json, Value};
use std::time::Duration;
use support::*;

/// Run `statement` via `op` against `db` and return the JSON document.
fn op(op: &str, db: &str, statement: &str) -> Out {
  run(&[op, "--db", db, statement])
}

#[test]
fn query_and_exec_are_equivalent_and_run_any_statement() {
  let (_d, path) = fixture();
  let db = s(&path);
  // A write through `query`; a read through `exec`. Both work.
  let w = op("query", &db, "INSERT INTO tags VALUES (3, 'x'), (3, 'y')");
  assert_eq!(w.code, 0, "{}", w.text());
  assert_eq!(w.json()["rows_affected"], 2);
  let r = op("exec", &db, "SELECT count(*) AS n FROM tags");
  assert_eq!(r.code, 0, "{}", r.text());
  assert_eq!(r.json()["rows"], json!([[5]]));
  assert_eq!(r.json()["columns"], json!([{"name": "n", "type": null}]));
}

#[test]
fn the_shape_has_the_unified_fields() {
  let (_d, path) = fixture();
  let doc = op(
    "query",
    &s(&path),
    "SELECT id, name FROM people ORDER BY id LIMIT 1",
  )
  .json();
  for key in [
    "columns",
    "rows",
    "row_count",
    "rows_affected",
    "last_insert_rowid",
    "truncated",
  ] {
    assert!(doc.get(key).is_some(), "missing {key}: {doc}");
  }
  assert_eq!(doc["row_count"], 1);
  assert_eq!(doc["rows"], json!([[1, "ada"]]));
}

#[test]
fn ddl_dml_returning_and_multi_statement() {
  let dir = tempfile::tempdir().unwrap();
  let db = s(&dir.path().join("w.db")); // created on first use
  assert_eq!(
    op(
      "exec",
      &db,
      "CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT)"
    )
    .code,
    0
  );
  let ins = op(
    "exec",
    &db,
    "INSERT INTO t(v) VALUES ('a'), ('b') RETURNING id, v",
  );
  assert_eq!(ins.json()["rows"], json!([[1, "a"], [2, "b"]]));
  assert_eq!(ins.json()["rows_affected"], 2);
  assert_eq!(ins.json()["last_insert_rowid"], 2);
  assert_eq!(
    op("exec", &db, "UPDATE t SET v = 'z' WHERE id = 1").json()["rows_affected"],
    1
  );
  // Multi-statement text: the last statement's rows come back.
  let multi = op(
    "query",
    &db,
    "DELETE FROM t WHERE id = 2; SELECT id, v FROM t ORDER BY id",
  );
  assert_eq!(multi.code, 0, "{}", multi.text());
  assert_eq!(multi.json()["rows"], json!([[1, "z"]]));
}

#[test]
fn type_mapping_blob_null_and_nonfinite() {
  let (_d, path) = fixture();
  let doc = op(
    "query",
    &s(&path),
    "SELECT NULL, 42, 1.5, 'txt', x'00FF10', 1e308 * 10 AS inf",
  )
  .json();
  assert_eq!(
    doc["rows"][0],
    json!([null, 42, 1.5, "txt", {"$blob_base64": "AP8Q"}, null])
  );
}

#[test]
fn caps_truncate_without_materializing_everything() {
  let (_d, path) = fixture();
  let sql = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) SELECT x FROM c";
  let out = run(&["query", "--db", &s(&path), sql, "--max-rows", "10"]);
  assert_eq!(out.code, 0, "{}", out.text());
  let doc = out.json();
  assert_eq!(doc["row_count"], 10);
  assert_eq!(
    (doc["truncated"].clone(), doc["truncated_by"].clone()),
    (json!(true), json!("max_rows"))
  );
  assert!(out.elapsed < Duration::from_secs(5), "{:?}", out.elapsed);
}

#[test]
fn timeout_interrupts_a_runaway_statement() {
  let (_d, path) = fixture();
  let sql =
    "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) SELECT count(*) FROM c";
  let out = run_with(
    &["query", "--db", &s(&path), sql, "--timeout-ms", "300"],
    &[],
    Duration::from_secs(20),
  );
  assert_eq!(
    (out.code, out.tool_code().as_str()),
    (7, "TIMEOUT"),
    "{}",
    out.text()
  );
  assert!(out.elapsed < Duration::from_secs(5), "{:?}", out.elapsed);
}

#[test]
fn errors_carry_a_tool_code() {
  let (_d, path) = fixture();
  let db = s(&path);
  let out = op("query", &db, "SELECT * FROM nope");
  assert_eq!(
    (out.code, out.tool_code().as_str()),
    (2, "SQLITE_ERROR"),
    "{}",
    out.text()
  );
  let out = op("query", &db, "SELEC 1");
  assert_eq!(out.tool_code(), "SQLITE_ERROR");
  assert!(out.json()["error"]["data"]["offset"].is_number());
}

#[test]
fn tables_and_describe() {
  let (_d, path) = fixture();
  let db = s(&path);
  let t = run(&["tables", "--db", &db]).json();
  let names: Vec<&str> = t["tables"]
    .as_array()
    .unwrap()
    .iter()
    .map(|e| e["name"].as_str().unwrap())
    .collect();
  assert!(names.contains(&"people") && names.contains(&"top"));
  // SQL injection in a table name is just a (missing) name, not executed.
  let out = run(&["describe", "--db", &db, "t' OR 1=1 --"]);
  assert_eq!(
    (out.code, out.tool_code().as_str()),
    (4, "TABLE_NOT_FOUND"),
    "{}",
    out.text()
  );
  let d = run(&["describe", "--db", &db, "people"]).json();
  assert_eq!(d["table"], "people");
  let cols: Vec<&str> = d["columns"]
    .as_array()
    .unwrap()
    .iter()
    .map(|c| c["name"].as_str().unwrap())
    .collect();
  assert_eq!(cols, ["id", "name", "score", "avatar", "note"]);
}

#[test]
fn json_and_flags_forms_agree() {
  let (_d, path) = fixture();
  let db = s(&path);
  let flags = run(&[
    "query",
    "--db",
    &db,
    "SELECT name FROM people ORDER BY id",
    "--max-rows",
    "2",
  ]);
  let body =
    json!({"db": db, "statement": "SELECT name FROM people ORDER BY id", "maxRows": 2}).to_string();
  let json_form = run(&["query", "--input-json", &body]);
  assert_eq!(flags.stdout, json_form.stdout);
  assert_eq!(flags.json()["rows"], json!([["ada"], ["bo"]]));
}

#[test]
fn a_sqlite_error_message_is_scrubbed_of_the_path() {
  // Point at a directory: SQLite reports "unable to open database file" with the path.
  let dir = tempfile::tempdir().unwrap();
  let secret = dir.path().join("very-secret-name");
  std::fs::create_dir(&secret).unwrap();
  let out = op("query", &s(&secret), "SELECT 1");
  assert_ne!(out.code, 0);
  assert!(!out.text().contains("very-secret-name"), "{}", out.text());
  assert!(!out.stderr.contains("very-secret-name"), "{}", out.stderr);
}

/// A wrong-password PostgreSQL connection never echoes the DSN, user, host or password.
#[test]
fn postgres_connection_failure_is_scrubbed() {
  let Some(url) = pg_url() else { return };
  let bad = pg_url_as(&url, "postgres", "DEFINITELY-WRONG-xyz");
  let out = pg_run(&bad, &["query", "select 1"]);
  assert_eq!(
    (out.code, out.tool_code().as_str()),
    (8, "CONNECT_FAILED"),
    "{}",
    out.text()
  );
  let all = format!("{}{}", out.text(), out.stderr);
  for leak in ["DEFINITELY-WRONG-xyz", "postgres://", "password"] {
    assert!(!all.contains(leak), "leaked {leak:?}: {all}");
  }
  let _ = Value::Null;
}

// ---- PostgreSQL (real server via LMSQL_TEST_PG_URL) -------------------------------------------

/// Run `op`/`statement` against the test server.
fn pg(url: &str, op: &str, statement: &str) -> Out {
  pg_run(url, &[op, statement])
}

#[test]
fn pg_type_mapping_table() {
  let Some(url) = pg_url() else { return };
  let doc = pg(
    &url,
    "query",
    "SELECT 1::int2 a, 9223372036854775807::int8 b, 1.5::float8 c, true d, 'hi'::text e, \
     '\\x00ff10'::bytea f, '{\"k\":[1,null]}'::jsonb g, \
     '2026-01-02 03:04:05+00'::timestamptz h, '2026-01-02'::date i, 'P1DT2H'::interval j, \
     '{1,2,NULL}'::int[] k, 'NaN'::numeric l, '12345678901234567890.1'::numeric m, \
     '11111111-1111-1111-1111-111111111111'::uuid n, '10.0.0.1'::inet o, 'Infinity'::float8 p, null q",
  )
  .json();
  assert_eq!(
    doc["rows"][0],
    json!([
      1, 9223372036854775807i64, 1.5, true, "hi",
      {"$blob_base64": "AP8Q"}, {"k": [1, null]},
      "2026-01-02T03:04:05Z", "2026-01-02", "P1DT2H",
      "{1,2,NULL}", "NaN", "12345678901234567890.1",
      "11111111-1111-1111-1111-111111111111", "10.0.0.1", null, null
    ])
  );
}

#[test]
fn pg_writes_returning_and_multi_statement() {
  let Some(url) = pg_url() else { return };
  let schema = PgSchema::new(&url, "writes");
  let t = format!("{}.t", schema.name);
  assert_eq!(
    pg(
      &url,
      "exec",
      &format!("CREATE TABLE {t}(id serial PRIMARY KEY, v text)")
    )
    .code,
    0
  );
  let ins = pg(
    &url,
    "exec",
    &format!("INSERT INTO {t}(v) VALUES ('a'),('b') RETURNING id, v"),
  );
  assert_eq!(ins.code, 0, "{}", ins.text());
  assert_eq!(ins.json()["rows"], json!([[1, "a"], [2, "b"]]));
  assert_eq!(ins.json()["rows_affected"], 2);
  let upd = pg(&url, "query", &format!("UPDATE {t} SET v='z' WHERE id=1"));
  assert_eq!(upd.json()["rows_affected"], 1);
  // Multi-statement text runs over the simple protocol, which carries no column types, so
  // values come back as strings (documented limitation). The last statement's rows come back.
  let multi = pg(
    &url,
    "exec",
    &format!("DELETE FROM {t} WHERE id=2; SELECT id, v FROM {t} ORDER BY id"),
  );
  assert_eq!(multi.code, 0, "{}", multi.text());
  assert_eq!(multi.json()["rows"], json!([["1", "z"]]));
}

#[test]
fn pg_tables_and_describe_with_schema() {
  let Some(url) = pg_url() else { return };
  let schema = PgSchema::new(&url, "meta");
  pg_admin(&url)
    .batch_execute(&format!(
      "CREATE TABLE {0}.item(id int PRIMARY KEY, name text NOT NULL); CREATE VIEW {0}.v AS SELECT 1 AS x",
      schema.name
    ))
    .unwrap();
  let t = pg_run(&url, &["tables", "--schema", &schema.name]).json();
  let names: Vec<&str> = t["tables"]
    .as_array()
    .unwrap()
    .iter()
    .map(|e| e["name"].as_str().unwrap())
    .collect();
  assert!(names.contains(&"item") && names.contains(&"v"), "{t}");
  // A quoted-looking table name is a parameter, not SQL.
  let miss = pg_run(
    &url,
    &["describe", "--schema", &schema.name, "x' OR 1=1 --"],
  );
  assert_eq!(
    (miss.code, miss.tool_code().as_str()),
    (4, "TABLE_NOT_FOUND"),
    "{}",
    miss.text()
  );
  let d = pg_run(&url, &["describe", "--schema", &schema.name, "item"]).json();
  assert_eq!(d["schema"], schema.name);
  let cols: Vec<&str> = d["columns"]
    .as_array()
    .unwrap()
    .iter()
    .map(|c| c["name"].as_str().unwrap())
    .collect();
  assert_eq!(cols, ["id", "name"]);
  assert_eq!(d["columns"][0]["pk"], 1);
  assert_eq!(d["columns"][1]["notnull"], true);
}

#[test]
fn pg_row_cap_stops_a_huge_select_fast() {
  let Some(url) = pg_url() else { return };
  let out = pg_run(
    &url,
    &[
      "query",
      "SELECT g FROM generate_series(1, 5000000) g",
      "--max-rows",
      "5",
    ],
  );
  assert_eq!(out.code, 0, "{}", out.text());
  let doc = out.json();
  assert_eq!(doc["row_count"], 5);
  assert_eq!(doc["truncated_by"], "max_rows");
  assert!(out.elapsed < Duration::from_secs(10), "{:?}", out.elapsed);
}

#[test]
fn pg_timeout_interrupts_pg_sleep() {
  let Some(url) = pg_url() else { return };
  let out = pg_run(
    &url,
    &["query", "SELECT pg_sleep(10)", "--timeout-ms", "500"],
  );
  assert_eq!(
    (out.code, out.tool_code().as_str()),
    (7, "TIMEOUT"),
    "{}",
    out.text()
  );
  assert!(out.elapsed < Duration::from_secs(5), "{:?}", out.elapsed);
}
