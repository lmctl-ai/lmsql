//! The lmapi surface: the published document and its channel files, mode detection (lmapi
//! protocol 6.1), policy for the write op, and the small legacy surface.
//!
//! Regenerate the dev channel file after changing an operation:
//! `UPDATE_LMAPI_SCHEMA=1 cargo test --test lmapi schema_dev_file`.

mod support;

use serde_json::Value;
use std::path::PathBuf;
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
  assert_eq!(out.json()["access"], "dynamic");
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

#[test]
fn open_mode_runs_writes_with_no_confirmation_and_ignores_readonly() {
  let (_d, db) = fresh();
  // No --confirm needed; the write just runs (access is "dynamic").
  let out = run(&["exec", "--db", &db, "DELETE FROM tags"]);
  assert_eq!(out.code, 0, "{}", out.text());
  assert_eq!(out.json()["rows_affected"], 3);
  // --readonly is still an accepted global but does not block a write in open mode.
  let (_d, db) = fresh();
  let out = run(&["exec", "--db", &db, "DELETE FROM tags", "--readonly"]);
  assert_eq!(out.code, 0, "{}", out.text());
  assert_eq!(out.json()["rows_affected"], 3);
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
