//! The per-tool settings file: which connections exist and how much lmsql may do with each.
//!
//! The file only ever **restricts**: without it lmsql is open (any `--db`, `LMSQL_<NAME>`
//! connections, full access as the database allows). With it, connections come from the file,
//! `--db` needs `"allow_direct": true`, and each connection has a permission level. The database
//! role / file permissions stay the real boundary; this layer never grants more than they do.
//!
//! Location: `$LMSQL_CONFIG`, else `$XDG_CONFIG_HOME/lmsql/settings.json`, else
//! `~/.config/lmsql/settings.json`. A file that exists but cannot be read or does not match the
//! schema below exactly fails closed (`SETTINGS_INVALID`); lmsql never falls back to open.
//!
//! ```json
//! {"version": 1, "allow_direct": false, "allow_bypass": false,
//!  "output": {"dir": "~/lmsql-out"},
//!  "connections": {
//!    "app": {"driver": "sqlite", "path": "~/data/app.sqlite",
//!            "permissions": {"default": "readonly", "tables": {"audit_log": "none"}}},
//!    "prod": {"driver": "postgres",
//!             "logins": {"readonly": {"dsn_env": "PROD_RO_DSN"}, "readwrite": {"dsn_env": "PROD_RW_DSN"}},
//!             "permissions": {"default": "readonly"}}}}
//! ```

use crate::error::DbError;
use lmrust::server::ErrorCode;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Environment variable naming the settings file.
pub const CONFIG_ENV: &str = "LMSQL_CONFIG";

/// What lmsql may do on a connection (or, later, a table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Level {
  #[default]
  None,
  ReadOnly,
  ReadWrite,
}

impl Level {
  pub fn parse(name: &str) -> Option<Level> {
    match name {
      "none" => Some(Level::None),
      "readonly" => Some(Level::ReadOnly),
      "readwrite" => Some(Level::ReadWrite),
      _ => None,
    }
  }

  pub fn as_str(self) -> &'static str {
    match self {
      Level::None => "none",
      Level::ReadOnly => "readonly",
      Level::ReadWrite => "readwrite",
    }
  }
}

/// Per-table levels of a SQLite connection: listed tables (names compared case-insensitively)
/// get their own level, every other table the connection default.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TableRules {
  pub default: Level,
  /// Lower-cased table name -> level.
  pub tables: BTreeMap<String, Level>,
}

impl TableRules {
  pub fn new(default: Level, tables: &BTreeMap<String, Level>) -> TableRules {
    TableRules {
      default,
      tables: tables.iter().map(|(t, l)| (t.to_lowercase(), *l)).collect(),
    }
  }

  pub fn level(&self, table: &str) -> Level {
    self
      .tables
      .get(&table.to_lowercase())
      .copied()
      .unwrap_or(self.default)
  }

  /// The most any table allows: what the database handle must permit.
  pub fn max(&self) -> Level {
    self.tables.values().copied().fold(self.default, Level::max)
  }

  /// Some table differs from the default: the authorizer has work to do.
  pub fn any_rule(&self) -> bool {
    self.tables.values().any(|l| *l != self.default)
  }
}

/// How a PostgreSQL login finds its DSN.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Login {
  /// The DSN is the value of this environment variable (only the name is ever shown).
  Env(String),
  /// The DSN is written in the settings file.
  Literal(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Driver {
  Sqlite {
    /// The path as written in the file (shown in `manual_command`).
    written: String,
    /// The path resolved (`~/` expanded; relative to the settings file's directory).
    path: PathBuf,
  },
  Postgres {
    readonly: Option<Login>,
    readwrite: Option<Login>,
  },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connection {
  pub name: String,
  pub driver: Driver,
  pub default: Level,
  /// Per-table levels (SQLite only; enforced in a later version).
  pub tables: BTreeMap<String, Level>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Settings {
  /// The file these settings came from; `None` = no file, lmsql is open.
  pub source: Option<PathBuf>,
  pub allow_direct: bool,
  pub allow_bypass: bool,
  pub output_dir: Option<String>,
  pub connections: BTreeMap<String, Connection>,
}

fn invalid(file: &Path, problem: impl std::fmt::Display) -> DbError {
  DbError::new(
    ErrorCode::ValidationError,
    "SETTINGS_INVALID",
    format!("settings file {}: {problem}", file.display()),
  )
}

/// `~/x` -> `$HOME/x`.
fn expand_home(path: &str, home: Option<&str>) -> Option<PathBuf> {
  match path.strip_prefix("~/") {
    Some(rest) => home.map(|h| Path::new(h).join(rest)),
    None if path == "~" => home.map(PathBuf::from),
    None => Some(PathBuf::from(path)),
  }
}

/// Where the settings file is looked for. `lookup` reads an environment variable.
pub fn settings_path(lookup: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
  let nonempty = |k: &str| lookup(k).filter(|v| !v.is_empty());
  if let Some(p) = nonempty(CONFIG_ENV) {
    return Some(PathBuf::from(p));
  }
  if let Some(xdg) = nonempty("XDG_CONFIG_HOME") {
    return Some(Path::new(&xdg).join("lmsql/settings.json"));
  }
  nonempty("HOME").map(|h| Path::new(&h).join(".config/lmsql/settings.json"))
}

impl Settings {
  /// No settings file: open, today's behavior.
  pub fn open() -> Settings {
    Settings::default()
  }

  pub fn is_open(&self) -> bool {
    self.source.is_none()
  }

  /// Load the settings of this call. Missing file => [`Settings::open`]; anything else wrong =>
  /// `SETTINGS_INVALID`. `warn` receives non-fatal warnings (a file others can write).
  pub fn load(
    lookup: &dyn Fn(&str) -> Option<String>,
    warn: &mut dyn FnMut(String),
  ) -> Result<Settings, DbError> {
    let Some(path) = settings_path(lookup) else {
      return Ok(Settings::open());
    };
    let text = match std::fs::read(&path) {
      Ok(bytes) => bytes,
      Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Settings::open()),
      Err(e) => return Err(invalid(&path, format!("cannot be read ({e})"))),
    };
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      if let Ok(meta) = std::fs::metadata(&path) {
        if meta.permissions().mode() & 0o022 != 0 {
          warn(format!(
            "lmsql: warning: settings file {} is writable by group or others; chmod 600 it",
            path.display()
          ));
        }
      }
    }
    let doc: Value = serde_json::from_slice(&text)
      .map_err(|e| invalid(&path, format!("is not valid JSON ({e})")))?;
    let home = lookup("HOME");
    let settings = parse(&doc, &path, home.as_deref())?;
    if settings.output_dir.is_some() {
      warn(
        "lmsql: warning: output.dir is not enforced by this lmsql version (accepted for later)"
          .to_string(),
      );
    }
    Ok(settings)
  }

  /// Strings that must not appear in output: paths and DSN values from the file and the
  /// environment variables it names.
  pub fn secrets(&self, lookup: &dyn Fn(&str) -> Option<String>) -> Vec<String> {
    let mut out = Vec::new();
    for conn in self.connections.values() {
      match &conn.driver {
        Driver::Sqlite { written, path } => {
          out.push(written.clone());
          out.push(path.display().to_string());
        }
        Driver::Postgres {
          readonly,
          readwrite,
        } => {
          for login in [readonly, readwrite].into_iter().flatten() {
            match login {
              Login::Env(var) => out.extend(lookup(var)),
              Login::Literal(dsn) => out.push(dsn.clone()),
            }
          }
        }
      }
    }
    out.retain(|s| !s.trim().is_empty());
    out
  }
}

/// A JSON object with exactly the allowed keys.
fn object<'a>(
  v: &'a Value,
  at: &str,
  allowed: &[&str],
  file: &Path,
) -> Result<&'a Map<String, Value>, DbError> {
  let map = v
    .as_object()
    .ok_or_else(|| invalid(file, format!("{at} must be an object")))?;
  for key in map.keys() {
    if !allowed.contains(&key.as_str()) {
      return Err(invalid(
        file,
        format!("unknown key {at}.{key} (allowed: {})", allowed.join(", ")),
      ));
    }
  }
  Ok(map)
}

fn boolean(map: &Map<String, Value>, key: &str, at: &str, file: &Path) -> Result<bool, DbError> {
  match map.get(key) {
    None => Ok(false),
    Some(Value::Bool(b)) => Ok(*b),
    Some(_) => Err(invalid(file, format!("{at}.{key} must be true or false"))),
  }
}

fn string<'a>(
  map: &'a Map<String, Value>,
  key: &str,
  at: &str,
  file: &Path,
) -> Result<Option<&'a str>, DbError> {
  match map.get(key) {
    None => Ok(None),
    Some(Value::String(s)) if !s.is_empty() => Ok(Some(s)),
    Some(_) => Err(invalid(
      file,
      format!("{at}.{key} must be a non-empty string"),
    )),
  }
}

fn level(v: &Value, at: &str, file: &Path) -> Result<Level, DbError> {
  v.as_str().and_then(Level::parse).ok_or_else(|| {
    invalid(
      file,
      format!("{at} must be \"none\", \"readonly\" or \"readwrite\""),
    )
  })
}

fn valid_name(name: &str) -> bool {
  !name.is_empty()
    && name.len() <= 64
    && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn login(v: &Value, at: &str, file: &Path) -> Result<Login, DbError> {
  let map = object(v, at, &["dsn_env", "dsn"], file)?;
  match (
    string(map, "dsn_env", at, file)?,
    string(map, "dsn", at, file)?,
  ) {
    (Some(var), None) => Ok(Login::Env(var.to_string())),
    (None, Some(dsn)) => {
      if !crate::backend::postgres::dsn::is_postgres_dsn(dsn) {
        return Err(invalid(file, format!("{at}.dsn must be a postgres:// DSN")));
      }
      Ok(Login::Literal(dsn.to_string()))
    }
    _ => Err(invalid(
      file,
      format!("{at} needs exactly one of dsn_env or dsn"),
    )),
  }
}

fn connection(
  name: &str,
  v: &Value,
  file: &Path,
  home: Option<&str>,
) -> Result<Connection, DbError> {
  let at = format!("connections.{name}");
  if !valid_name(name) {
    return Err(invalid(
      file,
      format!("{at}: connection names use letters, digits and underscores (at most 64)"),
    ));
  }
  let map = object(v, &at, &["driver", "path", "logins", "permissions"], file)?;
  let driver = match string(map, "driver", &at, file)? {
    Some("sqlite") => {
      if map.contains_key("logins") {
        return Err(invalid(
          file,
          format!("{at}.logins applies to postgres connections"),
        ));
      }
      let written = string(map, "path", &at, file)?.ok_or_else(|| {
        invalid(
          file,
          format!("{at}.path is required for a sqlite connection"),
        )
      })?;
      let mut path = expand_home(written, home)
        .ok_or_else(|| invalid(file, format!("{at}.path uses ~ but HOME is not set")))?;
      if path.is_relative() {
        path = file.parent().unwrap_or(Path::new(".")).join(path);
      }
      Driver::Sqlite {
        written: written.to_string(),
        path,
      }
    }
    Some("postgres") => {
      if map.contains_key("path") {
        return Err(invalid(
          file,
          format!("{at}.path applies to sqlite connections"),
        ));
      }
      let logins_at = format!("{at}.logins");
      let logins = map.get("logins").ok_or_else(|| {
        invalid(
          file,
          format!("{logins_at} is required for a postgres connection"),
        )
      })?;
      let logins = object(logins, &logins_at, &["readonly", "readwrite"], file)?;
      let readonly = logins
        .get("readonly")
        .map(|v| login(v, &format!("{logins_at}.readonly"), file))
        .transpose()?;
      let readwrite = logins
        .get("readwrite")
        .map(|v| login(v, &format!("{logins_at}.readwrite"), file))
        .transpose()?;
      if readonly.is_none() && readwrite.is_none() {
        return Err(invalid(
          file,
          format!("{logins_at} needs a readonly or readwrite login"),
        ));
      }
      Driver::Postgres {
        readonly,
        readwrite,
      }
    }
    Some(other) => {
      return Err(invalid(
        file,
        format!("{at}.driver {other:?} is not supported (sqlite or postgres)"),
      ))
    }
    None => {
      return Err(invalid(
        file,
        format!("{at}.driver is required (sqlite or postgres)"),
      ))
    }
  };
  let perm_at = format!("{at}.permissions");
  let perms = map
    .get("permissions")
    .ok_or_else(|| invalid(file, format!("{perm_at}.default is required")))?;
  let perms = object(perms, &perm_at, &["default", "tables"], file)?;
  let default = level(
    perms
      .get("default")
      .ok_or_else(|| invalid(file, format!("{perm_at}.default is required")))?,
    &format!("{perm_at}.default"),
    file,
  )?;
  let mut tables = BTreeMap::new();
  if let Some(t) = perms.get("tables") {
    if matches!(driver, Driver::Postgres { .. }) {
      return Err(invalid(
        file,
        format!(
          "{perm_at}.tables is not supported for postgres connections; use database grants/roles \
           (GRANT/REVOKE on a dedicated login) instead"
        ),
      ));
    }
    let t_at = format!("{perm_at}.tables");
    let t = t
      .as_object()
      .ok_or_else(|| invalid(file, format!("{t_at} must be an object")))?;
    for (table, v) in t {
      tables.insert(table.clone(), level(v, &format!("{t_at}.{table}"), file)?);
    }
  }
  Ok(Connection {
    name: name.to_string(),
    driver,
    default,
    tables,
  })
}

/// Validate a settings document (schema version 1).
pub fn parse(doc: &Value, file: &Path, home: Option<&str>) -> Result<Settings, DbError> {
  let map = object(
    doc,
    "(top level)",
    &[
      "version",
      "allow_direct",
      "allow_bypass",
      "output",
      "connections",
    ],
    file,
  )?;
  match map.get("version") {
    Some(v) if v.as_u64() == Some(1) => {}
    Some(_) => return Err(invalid(file, "version must be 1")),
    None => return Err(invalid(file, "version is required (1)")),
  }
  let output_dir = match map.get("output") {
    None => None,
    Some(v) => {
      string(object(v, "output", &["dir"], file)?, "dir", "output", file)?.map(str::to_string)
    }
  };
  let mut connections = BTreeMap::new();
  if let Some(c) = map.get("connections") {
    let c = c
      .as_object()
      .ok_or_else(|| invalid(file, "connections must be an object"))?;
    for (name, v) in c {
      connections.insert(name.clone(), connection(name, v, file, home)?);
    }
  }
  Ok(Settings {
    source: Some(file.to_path_buf()),
    allow_direct: boolean(map, "allow_direct", "(top level)", file)?,
    allow_bypass: boolean(map, "allow_bypass", "(top level)", file)?,
    output_dir,
    connections,
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  fn p(doc: Value) -> Result<Settings, String> {
    parse(&doc, Path::new("/cfg/settings.json"), Some("/home/u")).map_err(|e| {
      assert_eq!(e.tool_code, "SETTINGS_INVALID");
      assert!(e.message.contains("/cfg/settings.json"), "{}", e.message);
      e.message
    })
  }

  #[test]
  fn a_valid_document() {
    let s = p(json!({
      "version": 1, "allow_direct": true,
      "connections": {
        "app": {"driver": "sqlite", "path": "~/data/app.sqlite",
                "permissions": {"default": "readonly", "tables": {"audit": "none"}}},
        "rel": {"driver": "sqlite", "path": "db/x.sqlite", "permissions": {"default": "readwrite"}},
        "pg": {"driver": "postgres", "logins": {"readonly": {"dsn_env": "RO"}},
               "permissions": {"default": "readonly"}}}}))
    .unwrap();
    assert!(s.allow_direct && !s.allow_bypass);
    let app = &s.connections["app"];
    assert_eq!(
      app.driver,
      Driver::Sqlite {
        written: "~/data/app.sqlite".into(),
        path: "/home/u/data/app.sqlite".into()
      }
    );
    assert_eq!(
      (app.default, app.tables["audit"]),
      (Level::ReadOnly, Level::None)
    );
    assert_eq!(
      s.connections["rel"].driver,
      Driver::Sqlite {
        written: "db/x.sqlite".into(),
        path: "/cfg/db/x.sqlite".into()
      }
    );
  }

  #[test]
  fn invalid_documents_name_the_problem() {
    let sqlite = |perms: Value| json!({"version": 1, "connections": {"a": {"driver": "sqlite", "path": "x", "permissions": perms}}});
    for (doc, expect) in [
      (json!([]), "must be an object"),
      (json!({"connections": {}}), "version is required"),
      (json!({"version": 2}), "version must be 1"),
      (
        json!({"version": 1, "extra": 1}),
        "unknown key (top level).extra",
      ),
      (
        json!({"version": 1, "allow_direct": "yes"}),
        "allow_direct must be true or false",
      ),
      (
        json!({"version": 1, "output": {"dir": 3}}),
        "output.dir must be a non-empty string",
      ),
      (
        json!({"version": 1, "output": {"dri": "x"}}),
        "unknown key output.dri",
      ),
      (
        sqlite(json!({"default": "admin"})),
        "permissions.default must be",
      ),
      (sqlite(json!({})), "permissions.default is required"),
      (
        sqlite(json!({"default": "none", "tables": {"t": "write"}})),
        "tables.t must be",
      ),
      (
        sqlite(json!({"default": "none", "extra": 1})),
        "unknown key connections.a.permissions.extra",
      ),
      (
        json!({"version": 1, "connections": {"a": {"driver": "sqlite", "permissions": {"default": "none"}}}}),
        "path is required",
      ),
      (
        json!({"version": 1, "connections": {"a": {"driver": "mysql", "path": "x", "permissions": {"default": "none"}}}}),
        "not supported",
      ),
      (
        json!({"version": 1, "connections": {"bad-name": {"driver": "sqlite", "path": "x", "permissions": {"default": "none"}}}}),
        "connection names",
      ),
      (
        json!({"version": 1, "connections": {"p": {"driver": "postgres", "logins": {}, "permissions": {"default": "none"}}}}),
        "needs a readonly or readwrite login",
      ),
      (
        json!({"version": 1, "connections": {"p": {"driver": "postgres", "logins": {"readonly": {"dsn_env": "A", "dsn": "postgres://x"}}, "permissions": {"default": "none"}}}}),
        "exactly one of dsn_env or dsn",
      ),
      (
        json!({"version": 1, "connections": {"p": {"driver": "postgres", "logins": {"readonly": {"dsn": "mysql://x"}}, "permissions": {"default": "none"}}}}),
        "must be a postgres:// DSN",
      ),
      (
        json!({"version": 1, "connections": {"p": {"driver": "postgres", "logins": {"readonly": {"dsn_env": "A"}}, "permissions": {"default": "readonly", "tables": {"t": "none"}}}}}),
        "use database grants/roles",
      ),
    ] {
      let message = p(doc.clone()).unwrap_err();
      assert!(message.contains(expect), "{doc}: {message}");
    }
  }

  #[test]
  fn settings_path_order() {
    let env = |pairs: &'static [(&'static str, &'static str)]| {
      move |k: &str| {
        pairs
          .iter()
          .find(|(n, _)| *n == k)
          .map(|(_, v)| v.to_string())
      }
    };
    let f = env(&[
      ("LMSQL_CONFIG", "/a.json"),
      ("XDG_CONFIG_HOME", "/x"),
      ("HOME", "/h"),
    ]);
    assert_eq!(settings_path(&f), Some("/a.json".into()));
    let f = env(&[("XDG_CONFIG_HOME", "/x"), ("HOME", "/h")]);
    assert_eq!(settings_path(&f), Some("/x/lmsql/settings.json".into()));
    let f = env(&[("HOME", "/h")]);
    assert_eq!(
      settings_path(&f),
      Some("/h/.config/lmsql/settings.json".into())
    );
  }
}
