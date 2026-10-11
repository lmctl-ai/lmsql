//! Big results go to a file: the spill thresholds, explicit `out` in every format, atomic writes,
//! permissions, paths, statelessness and bounded memory, for SQLite and (with
//! `LMSQL_TEST_PG_URL`) PostgreSQL.

mod support;

use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use support::*;

/// A sandbox: a database path, a spill directory and a private TMPDIR, so every file a call
/// writes is visible to the test.
struct Sandbox {
  dir: tempfile::TempDir,
}

impl Sandbox {
  fn new() -> Sandbox {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("tmp")).unwrap();
    Sandbox { dir }
  }
  fn path(&self, rel: &str) -> PathBuf {
    self.dir.path().join(rel)
  }
  fn db(&self) -> String {
    s(&self.path("s.db"))
  }
  fn spill(&self) -> PathBuf {
    self.path("spill")
  }
  fn env(&self) -> Vec<(String, String)> {
    vec![
      ("LMSQL_OUT_DIR".into(), s(&self.spill())),
      ("TMPDIR".into(), s(&self.path("tmp"))),
    ]
  }
  fn run(&self, args: &[&str]) -> Out {
    self.run_env(args, &[])
  }
  fn run_env(&self, args: &[&str], extra: &[(&str, &str)]) -> Out {
    let mut env = self.env();
    for (k, v) in extra {
      env.retain(|(name, _)| name != k);
      env.push((k.to_string(), v.to_string()));
    }
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    run_with(args, &env, Duration::from_secs(120))
  }
}

fn count(n: u64) -> String {
  format!(
    "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c LIMIT {n}) SELECT x FROM c"
  )
}

fn names(dir: &Path) -> Vec<String> {
  let mut out: Vec<String> = match std::fs::read_dir(dir) {
    Ok(entries) => entries
      .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
      .collect(),
    Err(_) => Vec::new(),
  };
  out.sort();
  out
}

fn mode(path: &Path) -> u32 {
  std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn ok(out: &Out) -> Value {
  assert_eq!(out.code, 0, "{}{}", out.text(), out.stderr);
  out.json()
}

fn ndjson(path: &Path) -> Vec<Value> {
  std::fs::read_to_string(path)
    .unwrap()
    .lines()
    .map(|l| serde_json::from_str(l).unwrap())
    .collect()
}

fn is_root() -> bool {
  // SAFETY: no preconditions.
  unsafe { libc::geteuid() == 0 }
}

// ---- spill thresholds --------------------------------------------------------------------------

#[test]
fn rows_exactly_at_max_rows_stay_inline_one_more_spills() {
  let sb = Sandbox::new();
  let db = sb.db();
  let at = ok(&sb.run(&["query", "--db", &db, &count(10), "--max-rows", "10"]));
  assert_eq!(
    (at["spilled"].clone(), at["row_count"].clone()),
    (json!(false), json!(10))
  );
  assert_eq!(at["rows"].as_array().unwrap().len(), 10);
  assert!(at.get("file").is_none());
  assert!(!sb.spill().exists(), "no spill directory without a spill");

  let over = ok(&sb.run(&["query", "--db", &db, &count(11), "--max-rows", "10"]));
  assert_eq!(over["spilled"], true);
  assert_eq!(over["truncated"], false);
  assert_eq!(over["row_count"], 11);
  assert!(over.get("rows").is_none());
  assert_eq!(
    over["preview"],
    json!([[1], [2], [3], [4], [5], [6], [7], [8], [9], [10]])
  );
  let file = PathBuf::from(over["file"]["path"].as_str().unwrap());
  assert_eq!(file.parent().unwrap(), sb.spill());
  let name = file.file_name().unwrap().to_str().unwrap();
  // lmsql-<yyyymmddThhmmssZ>-<8 hex>.ndjson
  assert!(
    name.starts_with("lmsql-") && name.ends_with(".ndjson"),
    "{name}"
  );
  let stamp = &name[6..22];
  assert!(
    stamp.chars().enumerate().all(|(i, c)| match i {
      8 => c == 'T',
      15 => c == 'Z',
      _ => c.is_ascii_digit(),
    }),
    "{stamp}"
  );
  assert!(
    name[23..31].chars().all(|c| c.is_ascii_hexdigit()),
    "{name}"
  );
  assert_eq!(over["file"]["format"], "ndjson");
  assert_eq!(over["file"]["rows"], 11);
  assert_eq!(
    over["file"]["bytes"],
    std::fs::metadata(&file).unwrap().len()
  );
  let rows = ndjson(&file);
  assert_eq!(rows.len(), 11);
  assert_eq!(rows[10], json!({"x": 11}));
  assert_eq!((mode(&file), mode(&sb.spill())), (0o600, 0o700));
}

#[test]
fn bytes_exactly_at_max_bytes_stay_inline_one_less_spills() {
  let sb = Sandbox::new();
  let db = sb.db();
  // rows [[1],[2],...,[10]] serialize to exactly 42 bytes.
  let at = ok(&sb.run(&["query", "--db", &db, &count(10), "--max-bytes", "42"]));
  assert_eq!(at["spilled"], false);
  assert_eq!(serde_json::to_string(&at["rows"]).unwrap().len(), 42);
  let over = ok(&sb.run(&["query", "--db", &db, &count(10), "--max-bytes", "41"]));
  assert_eq!(over["spilled"], true);
  assert_eq!(over["file"]["rows"], 10);
  // The preview also fits the byte threshold: it stops before the row that would not fit.
  assert!(serde_json::to_string(&over["preview"]).unwrap().len() <= 41);
}

// ---- explicit out ------------------------------------------------------------------------------

fn people_expected() -> Vec<Value> {
  vec![
    json!({"id": 1, "name": "ada", "score": 0.9, "avatar": {"$blob_base64": "AP8Q"}, "note": "first"}),
    json!({"id": 2, "name": "bo", "score": 0.4, "avatar": null, "note": null}),
    json!({"id": 3, "name": "chi ☃", "score": null, "avatar": {"$blob_base64": ""}, "note": "tab\there"}),
  ]
}

const PEOPLE: &str = "SELECT id, name, score, avatar, note FROM people ORDER BY id";

#[test]
fn out_in_all_three_formats_sqlite() {
  let (_d, db) = fixture();
  let sb = Sandbox::new();
  for (file, format) in [("p.ndjson", "ndjson"), ("p.csv", "csv"), ("p.json", "json")] {
    let target = sb.path(file);
    let doc = ok(&sb.run(&["query", "--db", &s(&db), PEOPLE, "--out", &s(&target)]));
    assert_eq!(doc["file"]["format"], format);
    assert_eq!(doc["file"]["path"], s(&target));
    assert_eq!(
      (doc["row_count"].clone(), doc["file"]["rows"].clone()),
      (json!(3), json!(3))
    );
    assert_eq!(
      (doc["spilled"].clone(), doc["overwrote"].clone()),
      (json!(false), json!(false))
    );
    assert_eq!(doc["preview"].as_array().unwrap().len(), 3);
    assert_eq!(
      doc["file"]["bytes"],
      std::fs::metadata(&target).unwrap().len()
    );
    assert_eq!(mode(&target), 0o600);
    let text = std::fs::read_to_string(&target).unwrap();
    match format {
      "ndjson" => assert_eq!(ndjson(&target), people_expected()),
      "json" => assert_eq!(
        serde_json::from_str::<Value>(&text).unwrap(),
        json!(people_expected())
      ),
      _ => assert_eq!(
        text,
        "id,name,score,avatar,note\r\n\
         1,ada,0.9,\"{\"\"$blob_base64\"\":\"\"AP8Q\"\"}\",first\r\n\
         2,bo,0.4,,\r\n\
         3,chi ☃,,\"{\"\"$blob_base64\"\":\"\"\"\"}\",tab\there\r\n"
      ),
    }
  }
  // outFormat overrides the extension.
  let target = sb.path("p.txt");
  let doc = ok(&sb.run(&[
    "query",
    "--db",
    &s(&db),
    PEOPLE,
    "--out",
    &s(&target),
    "--out-format",
    "csv",
  ]));
  assert_eq!(doc["file"]["format"], "csv");
  assert!(std::fs::read_to_string(&target)
    .unwrap()
    .starts_with("id,name"));
  assert_eq!(
    names(sb.dir.path()),
    ["p.csv", "p.json", "p.ndjson", "p.txt", "tmp"]
  );
}

#[test]
fn csv_edge_cases() {
  let sb = Sandbox::new();
  let target = sb.path("e.csv");
  let sql = "SELECT 'a,b' AS c1, 'say \"hi\"' AS c2, 'two' || char(10) || 'lines' AS c3, \
             '☃ ü' AS c4, NULL AS c5, '' AS c6, 'x' || char(13) || char(10) || 'y' AS c7, \
             42 AS c8, 1.5 AS c9";
  ok(&sb.run(&["query", "--db", &sb.db(), sql, "--out", &s(&target)]));
  assert_eq!(
    std::fs::read_to_string(&target).unwrap(),
    "c1,c2,c3,c4,c5,c6,c7,c8,c9\r\n\
     \"a,b\",\"say \"\"hi\"\"\",\"two\nlines\",☃ ü,,\"\",\"x\r\ny\",42,1.5\r\n"
  );
}

#[test]
fn preview_overwrite_multi_statement_and_empty_results() {
  let sb = Sandbox::new();
  let db = sb.db();
  let target = sb.path("r.ndjson");
  let t = s(&target);
  let doc = ok(&sb.run(&["query", "--db", &db, &count(100), "--out", &t]));
  assert_eq!(
    doc["preview"].as_array().unwrap().len(),
    20,
    "default preview with out"
  );
  assert_eq!(doc["overwrote"], false);
  let doc = ok(&sb.run(&[
    "query",
    "--db",
    &db,
    &count(100),
    "--out",
    &t,
    "--max-rows",
    "5",
  ]));
  assert_eq!(doc["preview"].as_array().unwrap().len(), 5);
  assert_eq!(doc["overwrote"], true);
  // The last statement's rows are the file's rows.
  let target = sb.path("m.csv");
  let doc = ok(&sb.run(&[
    "exec",
    "--db",
    &db,
    "SELECT 1 AS a; SELECT 2 AS b, 3 AS c",
    "--out",
    &s(&target),
  ]));
  assert_eq!(doc["row_count"], 1);
  assert_eq!(std::fs::read_to_string(&target).unwrap(), "b,c\r\n2,3\r\n");
  // A statement without rows still writes the file.
  let target = sb.path("e.json");
  let doc = ok(&sb.run(&[
    "exec",
    "--db",
    &db,
    "CREATE TABLE q(a)",
    "--out",
    &s(&target),
  ]));
  assert_eq!(doc["file"]["rows"], 0);
  assert_eq!(std::fs::read_to_string(&target).unwrap(), "[\n]\n");
  // exec with RETURNING streams the returned rows.
  let target = sb.path("ins.ndjson");
  let doc = ok(&sb.run(&[
    "exec",
    "--db",
    &db,
    "INSERT INTO q VALUES (7), (8) RETURNING a",
    "--out",
    &s(&target),
  ]));
  assert_eq!(doc["rows_affected"], 2);
  assert_eq!(ndjson(&target), [json!({"a": 7}), json!({"a": 8})]);
  // No temporary files are left next to the targets.
  assert!(
    names(sb.dir.path()).iter().all(|n| !n.contains(".lmsql-")),
    "{:?}",
    names(sb.dir.path())
  );
}

#[test]
fn relative_and_nested_out_paths() {
  let sb = Sandbox::new();
  let env = sb.env();
  let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
  let (out, _) = run_full(
    &["query", "--db", "s.db", &count(3), "--out", "sub/dir/r.csv"],
    &env,
    Some(sb.dir.path()),
    Duration::from_secs(60),
  );
  let doc = ok(&out);
  let expected = sb.path("sub/dir/r.csv");
  assert_eq!(
    doc["file"]["path"],
    s(&expected),
    "absolute path in the response"
  );
  assert_eq!(
    std::fs::read_to_string(&expected).unwrap(),
    "x\r\n1\r\n2\r\n3\r\n"
  );
}

// ---- atomicity ---------------------------------------------------------------------------------

/// Fails with an integer overflow at row 1000 of 2000.
const FAILS_AT_1000: &str =
  "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c LIMIT 2000) \
  SELECT CASE WHEN x = 1000 THEN abs(-9223372036854775807 - 1) ELSE x END AS v FROM c";

#[test]
fn an_error_mid_stream_leaves_no_partial_file() {
  let sb = Sandbox::new();
  let db = sb.db();
  let out_dir = sb.path("out");
  std::fs::create_dir(&out_dir).unwrap();
  // A new target: absent afterwards, and no temporary file left.
  let target = out_dir.join("new.ndjson");
  let out = sb.run(&["query", "--db", &db, FAILS_AT_1000, "--out", &s(&target)]);
  assert_ne!(out.code, 0, "{}", out.text());
  assert_eq!(out.tool_code(), "SQLITE_ERROR");
  assert_eq!(names(&out_dir), Vec::<String>::new());
  // An existing target: unchanged.
  let target = out_dir.join("old.ndjson");
  std::fs::write(&target, "old\n").unwrap();
  let out = sb.run(&["query", "--db", &db, FAILS_AT_1000, "--out", &s(&target)]);
  assert_ne!(out.code, 0);
  assert_eq!(std::fs::read_to_string(&target).unwrap(), "old\n");
  assert_eq!(names(&out_dir), ["old.ndjson"]);
  // A spill that fails: the spill directory stays empty.
  let out = sb.run(&["query", "--db", &db, FAILS_AT_1000, "--max-rows", "10"]);
  assert_ne!(out.code, 0);
  assert_eq!(names(&sb.spill()), Vec::<String>::new());
}

// ---- errors ------------------------------------------------------------------------------------

#[test]
fn unwritable_out_is_refused_before_the_sql_runs() {
  if is_root() {
    eprintln!("SKIPPED: permission checks do not apply to root");
    return;
  }
  let sb = Sandbox::new();
  let db = sb.db();
  let locked = sb.path("locked");
  std::fs::create_dir(&locked).unwrap();
  std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
  let target = locked.join("r.ndjson");
  let out = sb.run(&[
    "exec",
    "--db",
    &db,
    "CREATE TABLE z(a)",
    "--out",
    &s(&target),
  ]);
  std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
  assert_eq!(
    (out.code, out.tool_code().as_str()),
    (6, "OUT_NOT_WRITABLE"),
    "{}",
    out.text()
  );
  let message = out.json()["error"]["message"].as_str().unwrap().to_string();
  // The caller's own out path is shown as given (lmsql's messages are not scrubbed); the
  // database path is not part of the message.
  assert!(message.contains(&s(&target)), "{message}");
  assert!(
    !message.contains("s.db") && !message.contains("<db>"),
    "{message}"
  );
  // The statement never ran.
  let tables = ok(&sb.run(&["tables", "--db", &db]));
  assert_eq!(tables["tables"], json!([]));
  // An existing directory as out.
  let out = sb.run(&[
    "query",
    "--db",
    &db,
    "SELECT 1",
    "--out",
    &s(&sb.path("tmp")),
  ]);
  assert_eq!(out.tool_code(), "OUT_NOT_WRITABLE");
  // outFormat without out, and an unknown format.
  let out = sb.run(&["query", "--db", &db, "SELECT 1", "--out-format", "csv"]);
  assert_eq!(
    (out.code, out.tool_code().as_str()),
    (2, "OUT_FORMAT_WITHOUT_OUT")
  );
  let out = sb.run(&[
    "query",
    "--db",
    &db,
    "SELECT 1",
    "--out",
    &s(&sb.path("x")),
    "--out-format",
    "xml",
  ]);
  assert_eq!(out.code, 2, "{}", out.text());
}

#[test]
fn the_default_spill_directory_must_be_private() {
  let sb = Sandbox::new();
  let db = sb.db();
  let tmp = sb.path("tmp");
  let no_dir = [("LMSQL_OUT_DIR", "")];
  // Created 0700 on first spill under TMPDIR.
  let doc = ok(&sb.run_env(
    &["query", "--db", &db, &count(5), "--max-rows", "2"],
    &no_dir,
  ));
  assert_eq!(doc["spilled"], true);
  assert_eq!(mode(&tmp.join("lmsql")), 0o700);
  assert!(doc["file"]["path"]
    .as_str()
    .unwrap()
    .starts_with(&s(&tmp.join("lmsql"))));
  // Opened up to others: refused.
  std::fs::set_permissions(tmp.join("lmsql"), std::fs::Permissions::from_mode(0o777)).unwrap();
  let out = sb.run_env(
    &["query", "--db", &db, &count(5), "--max-rows", "2"],
    &no_dir,
  );
  assert_eq!(out.tool_code(), "OUT_NOT_WRITABLE", "{}", out.text());
  // A symlink in its place: refused.
  std::fs::remove_dir_all(tmp.join("lmsql")).unwrap();
  std::os::unix::fs::symlink(sb.path("spill-elsewhere"), tmp.join("lmsql")).unwrap();
  std::fs::create_dir(sb.path("spill-elsewhere")).unwrap();
  let out = sb.run_env(
    &["query", "--db", &db, &count(5), "--max-rows", "2"],
    &no_dir,
  );
  assert_eq!(out.tool_code(), "OUT_NOT_WRITABLE", "{}", out.text());
  assert_eq!(names(&sb.path("spill-elsewhere")), Vec::<String>::new());
}

// ---- statelessness -----------------------------------------------------------------------------

#[test]
fn consecutive_calls_share_nothing_and_leave_no_stray_files() {
  let sb = Sandbox::new();
  let db = sb.db();
  let doc = ok(&sb.run(&[
    "query",
    "--db",
    &db,
    "CREATE TEMP TABLE tt(x); INSERT INTO tt VALUES (1); SELECT x FROM tt",
  ]));
  assert_eq!(doc["rows"], json!([[1]]));
  let out = sb.run(&["query", "--db", &db, "SELECT x FROM tt"]);
  assert_eq!(
    out.tool_code(),
    "SQLITE_ERROR",
    "a temp table must not survive the call"
  );
  ok(&sb.run(&["query", "--db", &db, &count(3)]));
  // Only the database file; nothing in TMPDIR, no spill directory, no journal.
  assert_eq!(names(sb.dir.path()), ["s.db", "tmp"]);
  assert_eq!(names(&sb.path("tmp")), Vec::<String>::new());
}

// ---- bounded memory ----------------------------------------------------------------------------

/// Peak RSS allowed for a multi-million-row call (it measures ~5 MiB).
const RSS_LIMIT_KIB: u64 = 48 * 1024;

#[test]
fn five_million_sqlite_rows_stream_with_flat_memory() {
  let sb = Sandbox::new();
  let env = sb.env();
  let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
  let target = sb.path("big.ndjson");
  let sql = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c LIMIT 5000000) \
             SELECT x, 'row ' || x AS s FROM c";
  for out_arg in [Some(s(&target)), None] {
    let mut args = vec!["query", "--db", "s.db", sql, "--timeout-ms", "300000"];
    if let Some(t) = &out_arg {
      args.extend(["--out", t.as_str()]);
    }
    let (out, rss) = run_full(&args, &env, Some(sb.dir.path()), Duration::from_secs(300));
    let doc = ok(&out);
    assert_eq!(doc["row_count"], 5_000_000);
    assert_eq!(doc["spilled"], out_arg.is_none());
    let file = PathBuf::from(doc["file"]["path"].as_str().unwrap());
    assert_eq!(
      doc["file"]["bytes"],
      std::fs::metadata(&file).unwrap().len()
    );
    assert!(rss < RSS_LIMIT_KIB, "peak RSS {rss} KiB");
    eprintln!(
      "sqlite 5M rows (out={}): {:?}, peak RSS {rss} KiB",
      out_arg.is_some(),
      out.elapsed
    );
    std::fs::remove_file(file).unwrap();
  }
}

// ---- PostgreSQL --------------------------------------------------------------------------------

fn pg_env(sb: &Sandbox, url: &str) -> Vec<(String, String)> {
  let mut env = sb.env();
  env.push(("LMSQL_PG".into(), url.to_string()));
  env
}

fn pg_call(sb: &Sandbox, url: &str, args: &[&str]) -> (Out, u64) {
  let mut full = vec![args[0], "--connection", "pg"];
  full.extend(&args[1..]);
  let env = pg_env(sb, url);
  let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
  run_full(&full, &env, None, Duration::from_secs(300))
}

const PG_TYPES: &str =
  "SELECT g AS id, 'a,b \"q\"' AS t, (g * 1.5)::numeric AS n, '\\x00ff'::bytea AS b, \
  '{\"k\": [1, null]}'::jsonb AS j, NULL::text AS z FROM generate_series(1, 2) g";

#[test]
fn pg_out_in_all_three_formats() {
  let Some(url) = pg_url() else { return };
  let sb = Sandbox::new();
  let expected = vec![
    json!({"id": 1, "t": "a,b \"q\"", "n": "1.5", "b": {"$blob_base64": "AP8="}, "j": {"k": [1, null]}, "z": null}),
    json!({"id": 2, "t": "a,b \"q\"", "n": "3.0", "b": {"$blob_base64": "AP8="}, "j": {"k": [1, null]}, "z": null}),
  ];
  for file in ["p.ndjson", "p.json", "p.csv"] {
    let target = sb.path(file);
    let (out, _) = pg_call(&sb, &url, &["query", PG_TYPES, "--out", &s(&target)]);
    let doc = ok(&out);
    assert_eq!(doc["file"]["rows"], 2);
    assert_eq!(doc["columns"][2], json!({"name": "n", "type": "numeric"}));
    assert_eq!(mode(&target), 0o600);
    let text = std::fs::read_to_string(&target).unwrap();
    match file {
      "p.ndjson" => assert_eq!(ndjson(&target), expected),
      "p.json" => assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), json!(expected)),
      _ => assert_eq!(
        text,
        "id,t,n,b,j,z\r\n\
         1,\"a,b \"\"q\"\"\",1.5,\"{\"\"$blob_base64\"\":\"\"AP8=\"\"}\",\"{\"\"k\"\":[1,null]}\",\r\n\
         2,\"a,b \"\"q\"\"\",3.0,\"{\"\"$blob_base64\"\":\"\"AP8=\"\"}\",\"{\"\"k\"\":[1,null]}\",\r\n"
      ),
    }
  }
}

#[test]
fn pg_spill_threshold_and_mid_stream_error() {
  let Some(url) = pg_url() else { return };
  let sb = Sandbox::new();
  let q = |n: u32| format!("SELECT g FROM generate_series(1, {n}) g");
  let (at, _) = pg_call(&sb, &url, &["query", &q(10), "--max-rows", "10"]);
  assert_eq!(ok(&at)["spilled"], false);
  let (over, _) = pg_call(&sb, &url, &["query", &q(11), "--max-rows", "10"]);
  let doc = ok(&over);
  assert_eq!(
    (doc["spilled"].clone(), doc["row_count"].clone()),
    (json!(true), json!(11))
  );
  assert_eq!(
    ndjson(Path::new(doc["file"]["path"].as_str().unwrap())).len(),
    11
  );
  // Division by zero at row 1000 of a cursor read: no file, no temporary file.
  let target = sb.path("out/fail.ndjson");
  let (out, _) = pg_call(
    &sb,
    &url,
    &[
      "query",
      "SELECT CASE WHEN g = 1000 THEN 1 / 0 ELSE g END FROM generate_series(1, 5000) g",
      "--out",
      &s(&target),
    ],
  );
  assert_eq!(out.tool_code(), "PG_22012", "{}", out.text());
  assert_eq!(names(&sb.path("out")), Vec::<String>::new());
}

#[test]
fn pg_temp_tables_do_not_survive_the_call() {
  let Some(url) = pg_url() else { return };
  let sb = Sandbox::new();
  let (out, _) = pg_call(
    &sb,
    &url,
    &[
      "query",
      "CREATE TEMP TABLE lmsql_tt(x int); INSERT INTO lmsql_tt VALUES (1); SELECT x FROM lmsql_tt",
    ],
  );
  assert_eq!(ok(&out)["rows"], json!([["1"]]));
  let (out, _) = pg_call(&sb, &url, &["query", "SELECT x FROM lmsql_tt"]);
  assert_eq!(out.tool_code(), "PG_42P01", "{}", out.text());
  assert_eq!(names(&sb.path("tmp")), Vec::<String>::new());
}

#[test]
fn pg_five_million_rows_stream_with_flat_memory() {
  let Some(url) = pg_url() else { return };
  let sb = Sandbox::new();
  let target = sb.path("big.ndjson");
  let (out, rss) = pg_call(
    &sb,
    &url,
    &[
      "query",
      "SELECT g, 'row ' || g AS s FROM generate_series(1, 5000000) g",
      "--out",
      &s(&target),
      "--timeout-ms",
      "300000",
    ],
  );
  let doc = ok(&out);
  assert_eq!(doc["file"]["rows"], 5_000_000);
  assert_eq!(
    doc["file"]["bytes"],
    std::fs::metadata(&target).unwrap().len()
  );
  assert!(rss < RSS_LIMIT_KIB, "peak RSS {rss} KiB");
  eprintln!("pg 5M rows: {:?}, peak RSS {rss} KiB", out.elapsed);
}
