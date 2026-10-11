//! Shared helpers: run the built binary with a hard timeout, fixture databases, file hashes.

#![allow(dead_code)]

use rusqlite::Connection;
use serde_json::Value;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub struct Out {
  pub code: i32,
  pub stdout: Vec<u8>,
  pub stderr: String,
  pub elapsed: Duration,
}

impl Out {
  pub fn json(&self) -> Value {
    serde_json::from_slice(&self.stdout).unwrap_or_else(|e| {
      panic!(
        "stdout is not one JSON document ({e}): {}\nstderr: {}",
        String::from_utf8_lossy(&self.stdout),
        self.stderr
      )
    })
  }
  pub fn text(&self) -> String {
    String::from_utf8_lossy(&self.stdout).into_owned()
  }
  /// `error.data.tool_code` of an error envelope.
  pub fn tool_code(&self) -> String {
    self.json()["error"]["data"]["tool_code"]
      .as_str()
      .unwrap_or("<none>")
      .to_string()
  }
}

pub fn bin() -> PathBuf {
  PathBuf::from(env!("CARGO_BIN_EXE_lmsql"))
}

/// Run lmsql; kills it and panics after `limit`.
pub fn run_with(args: &[&str], env: &[(&str, &str)], limit: Duration) -> Out {
  let mut cmd = Command::new(bin());
  cmd
    .args(args)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .env_remove("LMSQL_READONLY")
    .env_remove("LMSQL_DRYRUN");
  for (k, v) in env {
    cmd.env(k, v);
  }
  let start = Instant::now();
  let mut child = cmd.spawn().expect("spawn lmsql");
  let mut so = child.stdout.take().unwrap();
  let mut se = child.stderr.take().unwrap();
  let t1 = std::thread::spawn(move || {
    let mut v = Vec::new();
    let _ = so.read_to_end(&mut v);
    v
  });
  let t2 = std::thread::spawn(move || {
    let mut v = Vec::new();
    let _ = se.read_to_end(&mut v);
    v
  });
  let status = loop {
    if let Some(s) = child.try_wait().unwrap() {
      break s;
    }
    if start.elapsed() > limit {
      let _ = child.kill();
      let _ = child.wait();
      panic!("lmsql {args:?} did not finish within {limit:?}");
    }
    std::thread::sleep(Duration::from_millis(5));
  };
  Out {
    code: status.code().expect("exit code"),
    stdout: t1.join().unwrap(),
    stderr: String::from_utf8_lossy(&t2.join().unwrap()).into_owned(),
    elapsed: start.elapsed(),
  }
}

pub fn run(args: &[&str]) -> Out {
  run_with(args, &[], Duration::from_secs(30))
}

/// A database with `people` (id, name, score, avatar, note), `tags`, a view and an index.
pub fn fixture() -> (tempfile::TempDir, PathBuf) {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("fixture.db");
  let conn = Connection::open(&path).unwrap();
  conn
    .execute_batch(
      "CREATE TABLE people (
         id INTEGER PRIMARY KEY,
         name TEXT NOT NULL UNIQUE,
         score REAL DEFAULT 0.5,
         avatar BLOB,
         note TEXT
       );
       CREATE TABLE tags (person_id INTEGER REFERENCES people(id), tag TEXT, PRIMARY KEY (person_id, tag));
       CREATE INDEX people_score ON people(score);
       CREATE VIEW top AS SELECT name, score FROM people WHERE score > 0.7;
       INSERT INTO people (name, score, avatar, note) VALUES
         ('ada', 0.9, x'00FF10', 'first'),
         ('bo', 0.4, NULL, NULL),
         ('chi ☃', 1e308 * 10, x'', 'tab\there');
       INSERT INTO tags VALUES (1, 'math'), (1, 'engines'), (2, 'art');",
    )
    .unwrap();
  (dir, path)
}

/// The exact bytes of a file (fixtures are small).
pub fn snapshot(path: &Path) -> Vec<u8> {
  std::fs::read(path).unwrap()
}

pub fn s(path: &Path) -> String {
  path.display().to_string()
}

/// The superuser URL of the test PostgreSQL server (`LMSQL_TEST_PG_URL`). Without it the
/// Postgres tests skip with a message, unless `LMSQL_REQUIRE_PG=1` (CI), which makes a missing
/// server a failure.
pub fn pg_url() -> Option<String> {
  match std::env::var("LMSQL_TEST_PG_URL") {
    Ok(url) if !url.trim().is_empty() => Some(url),
    _ => {
      if std::env::var("LMSQL_REQUIRE_PG").as_deref() == Ok("1") {
        panic!("LMSQL_REQUIRE_PG=1 but LMSQL_TEST_PG_URL is not set");
      }
      eprintln!("SKIPPED: set LMSQL_TEST_PG_URL to run the PostgreSQL tests");
      None
    }
  }
}

/// A direct superuser client for test setup.
pub fn pg_admin(url: &str) -> postgres::Client {
  postgres::Client::connect(url, postgres::NoTls).expect("connect to the test server")
}

/// `url` with another user and password (`postgres://user:pw@rest`).
pub fn pg_url_as(url: &str, user: &str, password: &str) -> String {
  let (scheme, rest) = url.split_once("://").unwrap();
  let rest = rest.rsplit_once('@').map(|(_, r)| r).unwrap_or(rest);
  format!("{scheme}://{user}:{password}@{rest}")
}

/// A fresh schema (and its name) for one test; dropped on drop.
pub struct PgSchema {
  pub url: String,
  pub name: String,
}

impl PgSchema {
  pub fn new(url: &str, label: &str) -> PgSchema {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = format!("lmsql_{label}_{}_{n}", std::process::id());
    pg_admin(url)
      .batch_execute(&format!(
        "DROP SCHEMA IF EXISTS {name} CASCADE; CREATE SCHEMA {name}"
      ))
      .unwrap();
    PgSchema {
      url: url.to_string(),
      name,
    }
  }
}

impl Drop for PgSchema {
  fn drop(&mut self) {
    if let Ok(mut c) = postgres::Client::connect(&self.url, postgres::NoTls) {
      let _ = c.batch_execute(&format!("DROP SCHEMA IF EXISTS {} CASCADE", self.name));
    }
  }
}

/// Run lmsql with `LMSQL_PG` set to `dsn` (`--connection pg`).
pub fn pg_run(dsn: &str, args: &[&str]) -> Out {
  let mut full = vec![args[0], "--connection", "pg"];
  full.extend(&args[1..]);
  run_with(&full, &[("LMSQL_PG", dsn)], Duration::from_secs(30))
}
