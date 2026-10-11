//! The lmapi surface: the published document and its channel files, mode detection (lmapi
//! protocol 6.1), policy for the write op, and the small legacy surface.
//!
//! Regenerate the dev channel file after changing an operation:
//! `UPDATE_LMAPI_SCHEMA=1 cargo test --test lmapi schema_dev_file`.

mod support;

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use support::*;

fn root() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn channel(name: &str) -> Value {
  serde_json::from_str(&std::fs::read_to_string(root().join(name)).unwrap()).unwrap()
}

#[test]
fn schema_dev_file_matches_lmsql_schema_byte_for_byte() {
  let out = run(&["--schema"]);
  assert_eq!(out.code, 0);
  let file = root().join("cli.schema.dev.json");
  if std::env::var_os("UPDATE_LMAPI_SCHEMA").is_some() {
    std::fs::write(&file, &out.stdout).unwrap();
  }
  assert!(
    std::fs::read(&file).unwrap() == out.stdout,
    "cli.schema.dev.json is stale: run UPDATE_LMAPI_SCHEMA=1 cargo test --test lmapi schema_dev_file"
  );
}

#[test]
fn latest_is_an_additive_subset_of_dev() {
  let dev = channel("cli.schema.dev.json");
  let latest = channel("cli.schema.latest.json");
  assert_eq!(latest["tool"], dev["tool"]);
  for op in latest["operations"].as_array().unwrap() {
    let name = &op["name"];
    let dev_op = dev["operations"]
      .as_array()
      .unwrap()
      .iter()
      .find(|d| &d["name"] == name)
      .unwrap_or_else(|| panic!("{name} was removed from dev"));
    assert_eq!(dev_op["access"], op["access"], "access of {name} changed");
    for param in op["params"].as_array().unwrap() {
      if matches!(param["occurs"].as_str(), None | Some("1") | Some("+")) {
        let dev_param = dev_op["params"]
          .as_array()
          .unwrap()
          .iter()
          .find(|p| p["name"] == param["name"])
          .unwrap_or_else(|| panic!("required param {} of {name} removed", param["name"]));
        assert_eq!(dev_param["type"], param["type"]);
      }
    }
  }
}

#[test]
fn channel_files_pass_the_lmrust_publish_check() {
  for name in ["cli.schema.dev.json", "cli.schema.latest.json"] {
    lmrust::parse_published_schema(&channel(name)).unwrap_or_else(|i| panic!("{name}: {i:?}"));
  }
  let raw = std::fs::read_to_string(root().join("cli.schema.dev.json")).unwrap();
  for needle in ["/home/", "/Users/", "/tmp/", "LMSQL_PROD"] {
    assert!(!raw.contains(needle), "{needle}");
  }
  assert!(raw.len() < 64 * 1024, "the document is {} bytes", raw.len());
}

#[test]
fn list_explain_and_op_help() {
  let out = run(&["--list"]);
  let names: Vec<String> = out
    .json()
    .as_array()
    .unwrap()
    .iter()
    .map(|o| o["name"].as_str().unwrap().to_string())
    .collect();
  assert_eq!(names, ["query", "exec", "tables", "describe"]);
  let out = run(&["--explain", "exec"]);
  assert_eq!(out.json()["access"], "write");
  let out = run(&["query", "--help"]);
  assert_eq!(out.code, 0);
  assert!(out.text().starts_with("lmsql query"), "{}", out.text());
}

#[test]
fn detection_lmapi_versus_legacy() {
  // lmapi-shaped: a published op or a root lmapi global -> one JSON document on stdout.
  for args in [
    vec!["--schema"],
    vec!["--list"],
    vec!["query", "select 1"],
    vec!["--input-json", r#"{"op":"tables","params":{}}"#],
    vec!["tables", "--readonly"],
  ] {
    let out = run(&args);
    serde_json::from_slice::<Value>(&out.stdout).unwrap_or_else(|e| panic!("{args:?}: {e}"));
  }
  // Legacy: help text, version, unknown words.
  let out = run(&[]);
  assert_eq!(out.code, 0);
  assert!(out.text().starts_with("lmsql: one SQL statement"));
  let out = run(&["--help"]);
  assert!(out.text().starts_with("lmsql: one SQL statement"));
  let out = run(&["--version"]);
  assert_eq!(out.text(), format!("lmsql {}\n", env!("CARGO_PKG_VERSION")));
  let out = run(&["select", "1"]);
  assert_eq!((out.code, out.stdout.len()), (2, 0));
  assert!(out.stderr.contains("unknown command"));
}

fn fresh() -> (tempfile::TempDir, String) {
  let (dir, db) = fixture();
  (dir, s(&db))
}

fn tag_count(db: &str) -> i64 {
  rusqlite::Connection::open(Path::new(db))
    .unwrap()
    .query_row("SELECT count(*) FROM tags", [], |r| r.get(0))
    .unwrap()
}

#[test]
fn exec_policy_confirm_dryrun_readonly() {
  let delete = "DELETE FROM tags";
  // (extra args, env, exit, envelope code or "ok"/"dry", rows left)
  type Case<'a> = (Vec<&'a str>, Vec<(&'a str, &'a str)>, i32, &'a str, i64);
  let cases: Vec<Case> = vec![
    (vec![], vec![], 9, "MISSING_CONFIRMATION", 3),
    (vec!["--confirm"], vec![], 0, "ok", 0),
    (vec!["--i-understand"], vec![], 0, "ok", 0),
    (
      vec!["--confirm=false"],
      vec![],
      9,
      "MISSING_CONFIRMATION",
      3,
    ),
    (vec!["--dryrun"], vec![], 0, "dry", 3),
    (vec!["--dryrun", "--confirm"], vec![], 0, "dry", 3),
    (
      vec!["--readonly", "--confirm"],
      vec![],
      6,
      "READONLY_VIOLATION",
      3,
    ),
    (
      vec!["--confirm"],
      vec![("LMSQL_READONLY", "1")],
      6,
      "READONLY_VIOLATION",
      3,
    ),
    (
      vec!["--confirm", "--readonly=false"],
      vec![("LMSQL_READONLY", "true")],
      6,
      "READONLY_VIOLATION",
      3,
    ),
    (vec!["--confirm"], vec![("LMSQL_DRYRUN", "1")], 0, "dry", 3),
  ];
  for (extra, env, exit, expect, left) in cases {
    let (_d, db) = fresh();
    let mut args = vec!["exec", "--db", db.as_str(), delete];
    args.extend(extra.iter());
    let out = run_with(&args, &env, std::time::Duration::from_secs(20));
    let doc = out.json();
    assert_eq!(out.code, exit, "{extra:?} {env:?}: {doc}");
    match expect {
      "ok" => assert_eq!(doc["rows_affected"], 3, "{doc}"),
      "dry" => assert_eq!(
        (doc["dryRun"].clone(), doc["simulated"].clone()),
        (json!(true), json!("exec"))
      ),
      code => assert_eq!(doc["error"]["code"], code, "{doc}"),
    }
    assert_eq!(tag_count(&db), left, "{extra:?} {env:?}");
    // The dry-run result never echoes the database path.
    assert!(!out.text().contains(&db), "{extra:?} leaked the path");
  }
}

#[test]
fn read_ops_run_under_readonly_and_dryrun() {
  let (_d, db) = fresh();
  for flag in ["--readonly", "--dryrun"] {
    let out = run(&["query", "--db", &db, "SELECT count(*) FROM tags", flag]);
    assert_eq!(out.json()["rows"], json!([[3]]), "{flag}");
    let out = run(&["tables", "--db", &db, flag]);
    assert_eq!(out.code, 0, "{flag}");
  }
}

#[test]
fn validation_errors_never_reach_the_database() {
  let (_d, db) = fresh();
  for (args, issue) in [
    (vec!["query", "--db", db.as_str()], "MISSING_REQUIRED"),
    (
      vec![
        "query",
        "--db",
        db.as_str(),
        "select 1",
        "--max-rows",
        "many",
      ],
      "TYPE_MISMATCH",
    ),
    (
      vec!["query", "--db", db.as_str(), "select 1", "--nope"],
      "UNKNOWN_PARAM",
    ),
    (vec!["describe", "--db", db.as_str()], "MISSING_REQUIRED"),
  ] {
    let out = run(&args);
    let doc = out.json();
    assert_eq!(
      (out.code, doc["error"]["data"]["issues"][0]["code"].as_str()),
      (2, Some(issue)),
      "{args:?}: {doc}"
    );
  }
  let out = run(&[
    "query",
    "--db",
    &db,
    "select 1",
    "--input-graphql",
    "{ query }",
  ]);
  assert_eq!(out.code, 2);
}
