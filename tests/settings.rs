//! The settings file through the binary: loading (missing / valid / invalid / permission warning),
//! connection resolution, the direct-access gate, and connection-level permissions on SQLite and
//! (with `LMSQL_TEST_PG_URL`) PostgreSQL.

mod support;

use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;
use support::*;

/// A directory with `app.db` (table `t` with one row) and a settings file.
struct Setup {
  dir: tempfile::TempDir,
}

impl Setup {
  fn new() -> Setup {
    let dir = tempfile::tempdir().unwrap();
    rusqlite::Connection::open(dir.path().join("app.db"))
      .unwrap()
      .execute_batch("CREATE TABLE t(x TEXT); INSERT INTO t VALUES ('one');")
      .unwrap();
    Setup { dir }
  }
  fn path(&self, rel: &str) -> PathBuf {
    self.dir.path().join(rel)
  }
  fn settings(&self) -> PathBuf {
    self.path("settings.json")
  }
  /// Write the settings file (mode 0600).
  fn write(&self, doc: &Value) {
    self.write_raw(&doc.to_string(), 0o600);
  }
  fn write_raw(&self, text: &str, mode: u32) {
    std::fs::write(self.settings(), text).unwrap();
    std::fs::set_permissions(self.settings(), std::fs::Permissions::from_mode(mode)).unwrap();
  }
  fn run(&self, args: &[&str], env: &[(&str, &str)]) -> Out {
    let config = s(&self.settings());
    let mut all = vec![("LMSQL_CONFIG", config.as_str())];
    all.extend_from_slice(env);
    run_with(args, &all, Duration::from_secs(60))
  }
  fn rows(&self) -> i64 {
    rusqlite::Connection::open(self.path("app.db"))
      .unwrap()
      .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
      .unwrap()
  }
}

fn sqlite_settings(level: &str) -> Value {
  json!({"version": 1, "connections": {
    "app": {"driver": "sqlite", "path": "app.db", "permissions": {"default": level}}}})
}

fn code(out: &Out) -> (i32, String) {
  (out.code, out.tool_code())
}

// ---- loading -----------------------------------------------------------------------------------

#[test]
fn a_missing_settings_file_keeps_lmsql_open() {
  let st = Setup::new();
  let db = s(&st.path("app.db"));
  // No file at LMSQL_CONFIG: --db and LMSQL_<NAME> work as before.
  let out = st.run(&["exec", "--db", &db, "INSERT INTO t VALUES ('two')"], &[]);
  assert_eq!(out.code, 0, "{}", out.text());
  let out = st.run(
    &["query", "--connection", "app", "SELECT count(*) FROM t"],
    &[("LMSQL_APP", &db)],
  );
  assert_eq!(out.json()["rows"], json!([[2]]));
  assert!(out.stderr.is_empty(), "{}", out.stderr);
}

#[test]
fn invalid_settings_fail_closed() {
  let st = Setup::new();
  let db = s(&st.path("app.db"));
  let cases: &[(&str, &str)] = &[
    ("{not json", "is not valid JSON"),
    (
      r#"{"version": 1, "allow_everything": true}"#,
      "unknown key (top level).allow_everything",
    ),
    (r#"{"version": "1"}"#, "version must be 1"),
    (
      r#"{"version": 1, "allow_direct": 1}"#,
      "allow_direct must be true or false",
    ),
    (
      r#"{"version": 1, "connections": {"app": {"driver": "sqlite", "path": "app.db", "permissions": {"default": "admin"}}}}"#,
      "permissions.default must be",
    ),
    (
      r#"{"version": 1, "connections": {"app": {"driver": "sqlite", "path": "app.db", "permissions": {"default": "readonly"}, "colour": "red"}}}"#,
      "unknown key connections.app.colour",
    ),
    (
      r#"{"version": 1, "connections": {"pg": {"driver": "postgres", "logins": {"readwrite": {"dsn_env": "X"}}, "permissions": {"default": "readonly", "tables": {"t": "none"}}}}}"#,
      "use database grants/roles",
    ),
  ];
  for (text, expect) in cases {
    st.write_raw(text, 0o600);
    // Every way in is refused, including the paths that need no connection from the file.
    for args in [
      vec!["query", "--db", db.as_str(), "SELECT 1"],
      vec!["query", "--connection", "app", "SELECT 1"],
      vec!["tables", "--connection", "app"],
    ] {
      let out = st.run(&args, &[("LMSQL_APP", &db)]);
      assert_eq!(
        code(&out),
        (2, "SETTINGS_INVALID".into()),
        "{text}: {}",
        out.text()
      );
      let message = out.json()["error"]["message"].as_str().unwrap().to_string();
      assert!(message.contains(&s(&st.settings())), "{message}");
      assert!(message.contains(expect), "{text}: {message}");
    }
  }
  // A directory where the file should be: unreadable, also fails closed.
  std::fs::remove_file(st.settings()).unwrap();
  std::fs::create_dir(st.settings()).unwrap();
  let out = st.run(&["query", "--db", &db, "SELECT 1"], &[]);
  assert_eq!(out.tool_code(), "SETTINGS_INVALID", "{}", out.text());
  assert_eq!(st.rows(), 1);
}

#[test]
fn a_settings_file_others_can_write_is_warned_about() {
  let st = Setup::new();
  for (mode, warned) in [(0o600, false), (0o644, false), (0o664, true), (0o666, true)] {
    st.write_raw(&sqlite_settings("readwrite").to_string(), mode);
    let out = st.run(&["query", "--connection", "app", "SELECT 1"], &[]);
    assert_eq!(out.code, 0, "{}", out.text());
    assert_eq!(
      out.stderr.contains("writable by group or others"),
      warned,
      "{mode:o}: {}",
      out.stderr
    );
  }
}

// ---- resolution and the direct gate ------------------------------------------------------------

#[test]
fn connections_resolve_through_the_file_and_direct_access_is_gated() {
  let st = Setup::new();
  let db = s(&st.path("app.db"));
  // (allow_direct, args, env, expected exit, expected tool code or "ok")
  type Case<'a> = (bool, Vec<&'a str>, Vec<(&'a str, &'a str)>, i32, &'a str);
  let cases: Vec<Case> = vec![
    (
      false,
      vec!["query", "--connection", "app", "SELECT 1"],
      vec![],
      0,
      "ok",
    ),
    // Only in the environment: no fallback once a settings file exists.
    (
      false,
      vec!["query", "--connection", "other", "SELECT 1"],
      vec![("LMSQL_OTHER", &db)],
      4,
      "CONNECTION_NOT_FOUND",
    ),
    (
      false,
      vec!["query", "--db", &db, "SELECT 1"],
      vec![],
      6,
      "DIRECT_NOT_ALLOWED",
    ),
    (
      false,
      vec!["query", "--db", "postgres://u:pw@127.0.0.1:1/x", "SELECT 1"],
      vec![],
      6,
      "DIRECT_NOT_ALLOWED",
    ),
    (
      true,
      vec!["query", "--db", &db, "SELECT 1"],
      vec![],
      0,
      "ok",
    ),
  ];
  for (allow_direct, args, env, exit, expect) in cases {
    let mut doc = sqlite_settings("readwrite");
    doc["allow_direct"] = json!(allow_direct);
    st.write(&doc);
    let out = st.run(&args, &env);
    assert_eq!(out.code, exit, "{args:?}: {}", out.text());
    if expect != "ok" {
      assert_eq!(out.tool_code(), expect, "{args:?}");
      // Neither the path nor a DSN password is echoed.
      assert!(
        !out.text().contains(&db) && !out.text().contains("pw@"),
        "{}",
        out.text()
      );
    }
  }
}

// ---- levels on SQLite --------------------------------------------------------------------------

#[test]
fn sqlite_connection_levels() {
  let st = Setup::new();
  // (level, op args after --connection app, allowed)
  let cases: &[(&str, &[&str], bool)] = &[
    ("readwrite", &["query", "SELECT x FROM t"], true),
    ("readwrite", &["exec", "INSERT INTO t VALUES ('w')"], true),
    ("readwrite", &["tables"], true),
    ("readonly", &["query", "SELECT x FROM t"], true),
    ("readonly", &["tables"], true),
    ("readonly", &["describe", "t"], true),
    ("readonly", &["exec", "INSERT INTO t VALUES ('w')"], false),
    ("readonly", &["query", "UPDATE t SET x = 'u'"], false),
    ("readonly", &["exec", "CREATE TABLE z(a)"], false),
    (
      "readonly",
      &["exec", "PRAGMA query_only = OFF; DELETE FROM t"],
      false,
    ),
    ("none", &["query", "SELECT x FROM t"], false),
    ("none", &["tables"], false),
    ("none", &["describe", "t"], false),
  ];
  for (level, op, allowed) in cases {
    st.write(&sqlite_settings(level));
    let before = st.rows();
    let mut args = vec![op[0], "--connection", "app"];
    args.extend(&op[1..]);
    let out = st.run(&args, &[]);
    if *allowed {
      assert_eq!(out.code, 0, "{level} {op:?}: {}", out.text());
    } else {
      assert_eq!(
        code(&out),
        (6, "PERMISSION_DENIED".into()),
        "{level} {op:?}: {}",
        out.text()
      );
      let doc = out.json();
      assert_eq!(doc["error"]["code"], "FORBIDDEN");
      let message = doc["error"]["message"].as_str().unwrap();
      assert!(
        message.contains("connection \"app\"") && message.contains(level),
        "{message}"
      );
      assert!(message.contains(&s(&st.settings())), "{message}");
      assert!(doc["error"]["data"]["manual_command"]
        .as_str()
        .unwrap()
        .starts_with("sqlite3 'app.db' "));
      assert_eq!(st.rows(), before, "{level} {op:?} changed the database");
    }
  }
  // readonly never creates a missing database file.
  let mut doc = sqlite_settings("readonly");
  doc["connections"]["app"]["path"] = json!("missing.db");
  st.write(&doc);
  let out = st.run(&["query", "--connection", "app", "SELECT 1"], &[]);
  assert_ne!(out.code, 0);
  assert!(!st.path("missing.db").exists());
}

#[test]
fn the_manual_command_is_shell_quoted_and_keeps_the_written_path() {
  let st = Setup::new();
  let mut doc = sqlite_settings("readonly");
  doc["connections"]["app"]["path"] = json!("~/data dir/app.db");
  st.write(&doc);
  let home = s(st.dir.path());
  std::fs::create_dir(st.path("data dir")).unwrap();
  std::fs::copy(st.path("app.db"), st.path("data dir/app.db")).unwrap();
  let out = st.run(
    &[
      "exec",
      "--connection",
      "app",
      "INSERT INTO t VALUES ('it''s')",
    ],
    &[("HOME", &home)],
  );
  assert_eq!(out.tool_code(), "PERMISSION_DENIED", "{}", out.text());
  assert_eq!(
    out.json()["error"]["data"]["manual_command"],
    r#"sqlite3 ~/'data dir/app.db' 'INSERT INTO t VALUES ('\''it'\'''\''s'\'')'"#
  );
  // The expanded path is a secret: it is not in the message.
  assert!(
    !out.text().contains(&s(&st.path("data dir/app.db"))),
    "{}",
    out.text()
  );
}

// ---- PostgreSQL --------------------------------------------------------------------------------

/// A schema with table `t` (one row), a login that can only read it, and a settings file whose
/// connection `pg` uses `logins` (the env var names are fixed; values are passed per call).
struct PgSetup {
  st: Setup,
  schema: PgSchema,
  rw: String,
  ro: String,
}

const RO_PASSWORD: &str = "ro-Secret-9182";

impl PgSetup {
  fn new(url: &str, label: &str) -> PgSetup {
    let schema = PgSchema::new(url, label);
    let s = &schema.name;
    let role = format!("{s}_ro");
    pg_admin(url)
      .batch_execute(&format!(
        "CREATE TABLE {s}.t(x text); INSERT INTO {s}.t VALUES ('one');
         DROP ROLE IF EXISTS {role};
         CREATE ROLE {role} LOGIN PASSWORD '{RO_PASSWORD}';
         GRANT USAGE ON SCHEMA {s} TO {role};
         GRANT SELECT ON {s}.t TO {role};"
      ))
      .unwrap();
    PgSetup {
      st: Setup::new(),
      rw: url.to_string(),
      ro: pg_url_as(url, &role, RO_PASSWORD),
      schema,
    }
  }
  fn write(&self, logins: Value, level: &str) {
    self.st.write(&json!({"version": 1, "connections": {
      "pg": {"driver": "postgres", "logins": logins, "permissions": {"default": level}}}}));
  }
  fn run(&self, op: &str, sql: &str) -> Out {
    let args: Vec<&str> = if sql.is_empty() {
      vec![op, "--connection", "pg"]
    } else {
      vec![op, "--connection", "pg", sql]
    };
    self
      .st
      .run(&args, &[("PG_RO_DSN", &self.ro), ("PG_RW_DSN", &self.rw)])
  }
  fn rows(&self) -> i64 {
    pg_admin(&self.rw)
      .query_one(
        &format!("SELECT count(*) FROM {}.t", self.schema.name) as &str,
        &[],
      )
      .unwrap()
      .get(0)
  }
}

impl Drop for PgSetup {
  fn drop(&mut self) {
    let _ = pg_admin(&self.rw).batch_execute(&format!(
      "DROP SCHEMA IF EXISTS {0} CASCADE; DROP ROLE IF EXISTS {0}_ro",
      self.schema.name
    ));
  }
}

#[test]
fn pg_connection_levels_and_logins() {
  let Some(url) = pg_url() else { return };
  let pg = PgSetup::new(&url, "levels");
  let t = format!("{}.t", pg.schema.name);
  let insert = format!("INSERT INTO {t} VALUES ('w')");
  let select = format!("SELECT x FROM {t}");
  let both = json!({"readonly": {"dsn_env": "PG_RO_DSN"}, "readwrite": {"dsn_env": "PG_RW_DSN"}});
  let rw_only = json!({"readwrite": {"dsn_env": "PG_RW_DSN"}});
  // (logins, level, op, sql, allowed)
  let cases: Vec<(&Value, &str, &str, &str, bool)> = vec![
    (&both, "readwrite", "exec", &insert, true),
    (&both, "readonly", "query", &select, true),
    (&both, "readonly", "tables", "", true),
    // The readonly login: refused by the database grants and the READ ONLY session.
    (&both, "readonly", "exec", &insert, false),
    // Only a readwrite login: the READ ONLY session refuses the write.
    (&rw_only, "readonly", "query", &select, true),
    (&rw_only, "readonly", "exec", &insert, false),
    (&rw_only, "readwrite", "exec", &insert, true),
    (&both, "none", "query", &select, false),
    (&both, "none", "tables", "", false),
  ];
  for (logins, level, op, sql, allowed) in cases {
    pg.write(logins.clone(), level);
    let before = pg.rows();
    let out = pg.run(op, sql);
    let text = format!("{}{}", out.text(), out.stderr);
    // No DSN value, password or host:port ever appears.
    for secret in [RO_PASSWORD, pg.rw.as_str(), pg.ro.as_str()] {
      assert!(!text.contains(secret), "{level} {op}: leaked: {text}");
    }
    if allowed {
      assert_eq!(out.code, 0, "{level} {op} {sql}: {text}");
    } else {
      assert_eq!(
        code(&out),
        (6, "PERMISSION_DENIED".into()),
        "{level} {op} {sql}: {text}"
      );
      let manual = out.json()["error"]["data"]["manual_command"]
        .as_str()
        .unwrap()
        .to_string();
      assert!(manual.starts_with("psql \"$PG_RW_DSN\" -c '"), "{manual}");
      assert_eq!(pg.rows(), before, "{level} {op} changed the database");
    }
  }
}

#[test]
fn pg_missing_login_variable_and_none_without_connecting() {
  let Some(url) = pg_url() else { return };
  let pg = PgSetup::new(&url, "envs");
  pg.write(
    json!({"readwrite": {"dsn_env": "PG_UNSET_DSN"}}),
    "readwrite",
  );
  let out = pg.run("query", "SELECT 1");
  assert_eq!(
    code(&out),
    (4, "CONNECTION_NOT_FOUND".into()),
    "{}",
    out.text()
  );
  assert!(out.text().contains("PG_UNSET_DSN"));
  // none never needs the DSN.
  pg.write(json!({"readwrite": {"dsn_env": "PG_UNSET_DSN"}}), "none");
  let out = pg.run("query", "SELECT 1");
  assert_eq!(out.tool_code(), "PERMISSION_DENIED", "{}", out.text());
  // A literal DSN in the file: the manual command is a placeholder, never the value.
  pg.write(json!({"readwrite": {"dsn": pg.rw}}), "none");
  let out = pg.run("query", "SELECT 1");
  let manual = out.json()["error"]["data"]["manual_command"]
    .as_str()
    .unwrap()
    .to_string();
  assert!(manual.contains("<the dsn of connection pg in "), "{manual}");
  assert!(!out.text().contains(&pg.rw), "{}", out.text());
}

// ---- SQLite per-table rules --------------------------------------------------------------------

#[test]
fn sqlite_table_rules() {
  let st = Setup::new();
  rusqlite::Connection::open(st.path("app.db"))
    .unwrap()
    .execute_batch(
      "CREATE TABLE audit(x TEXT); INSERT INTO audit VALUES ('a');
       CREATE TABLE secrets(x TEXT); INSERT INTO secrets VALUES ('s');
       CREATE VIEW leaky AS SELECT x FROM secrets;",
    )
    .unwrap();
  let settings = |default: &str| {
    json!({"version": 1, "connections": {"app": {"driver": "sqlite", "path": "app.db",
      "permissions": {"default": default, "tables": {"audit": "readonly", "Secrets": "none"}}}}})
  };
  // (default, op args after --connection app, denied table or "" when allowed)
  let cases: &[(&str, &[&str], &str)] = &[
    ("readwrite", &["query", "SELECT x FROM audit"], ""),
    ("readwrite", &["exec", "INSERT INTO t VALUES ('w')"], ""),
    (
      "readwrite",
      &["exec", "INSERT INTO audit VALUES ('w')"],
      "audit",
    ),
    ("readwrite", &["exec", "UPDATE AUDIT SET x = 'u'"], "audit"),
    ("readwrite", &["exec", "DROP TABLE audit"], "audit"),
    (
      "readwrite",
      &["exec", "CREATE INDEX i ON audit(x)"],
      "audit",
    ),
    ("readwrite", &["query", "SELECT x FROM secrets"], "secrets"),
    (
      "readwrite",
      &["query", "SELECT (SELECT count(*) FROM main.secrets)"],
      "secrets",
    ),
    ("readwrite", &["query", "SELECT x FROM leaky"], "secrets"),
    ("readwrite", &["describe", "secrets"], "secrets"),
    ("readwrite", &["describe", "audit"], ""),
    // Default readonly: an unlisted table follows the default.
    ("readonly", &["query", "SELECT x FROM t"], ""),
    ("readonly", &["exec", "INSERT INTO t VALUES ('w')"], "?"),
    // Default none, but some table readable: only that table.
    ("none", &["query", "SELECT x FROM audit"], ""),
    ("none", &["query", "SELECT x FROM t"], "t"),
  ];
  for (default, op, denied) in cases {
    st.write(&settings(default));
    let mut args = vec![op[0], "--connection", "app"];
    args.extend(&op[1..]);
    let out = st.run(&args, &[]);
    if denied.is_empty() {
      assert_eq!(out.code, 0, "{default} {op:?}: {}", out.text());
      continue;
    }
    assert_eq!(
      code(&out),
      (6, "PERMISSION_DENIED".into()),
      "{default} {op:?}: {}",
      out.text()
    );
    let message = out.json()["error"]["message"].as_str().unwrap().to_string();
    if *denied != "?" {
      assert!(
        message.contains(&format!("table {denied:?}")),
        "{default} {op:?}: {message}"
      );
    }
  }
  // Nothing was changed by the refused statements.
  let conn = rusqlite::Connection::open(st.path("app.db")).unwrap();
  let audit: i64 = conn
    .query_row("SELECT count(*) FROM audit", [], |r| r.get(0))
    .unwrap();
  assert_eq!(audit, 1);
  // `tables` hides `none` tables.
  st.write(&settings("readwrite"));
  let out = st.run(&["tables", "--connection", "app"], &[]);
  let names: Vec<String> = out.json()["tables"]
    .as_array()
    .unwrap()
    .iter()
    .map(|t| t["name"].as_str().unwrap().to_string())
    .collect();
  assert_eq!(names, ["audit", "leaky", "t"]);
}
