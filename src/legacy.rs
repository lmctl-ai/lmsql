//! The non-lmapi surface: help, version and a clear error for anything else. lmsql has no
//! legacy verbs of its own; its human CLI is the lmapi flags form (`lmsql query --db x "..."`),
//! so no legacy word can ever collide with lmapi detection.

use std::io::Write;

pub const HELP: &str = "lmsql: one SQL statement in, structured JSON out (SQLite, PostgreSQL)

Usage:
  lmsql query    --connection NAME \"SELECT ...\"  [--max-rows N] [--max-bytes N] [--timeout-ms N]
  lmsql exec     --connection NAME \"INSERT ...\"  [--out FILE [--out-format ndjson|csv|json]]
  lmsql tables   --connection NAME
  lmsql describe --connection NAME TABLE
  lmsql <op> --db FILE|postgres://... ...   (direct; with a settings file only if allow_direct)
  lmsql <op> --input-json '{\"connection\": \"app\", \"statement\": \"select 1\"}'
  lmsql --schema | --list | --explain <op> | <op> --help
  lmsql --version

Every result and every error is one JSON document on stdout. query and exec run any SQL the
connection's permission level allows; a big result is written to a file. Connections and their
levels (none, readonly, readwrite) come from the settings file ($LMSQL_CONFIG, else
~/.config/lmsql/settings.json); without one, LMSQL_<NAME> names a connection and access is open.
";

/// Handle a non-lmapi invocation.
pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
  match args.first().map(String::as_str) {
    None | Some("--help") | Some("-h") | Some("help") => {
      let _ = out.write_all(HELP.as_bytes());
      0
    }
    Some("--version") | Some("-V") => {
      let _ = writeln!(out, "lmsql {}", env!("CARGO_PKG_VERSION"));
      0
    }
    Some(other) => {
      let _ = writeln!(
        err,
        "lmsql: unknown command {other:?}; operations are query, exec, tables, describe (lmsql --help)"
      );
      2
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn run_str(args: &[&str]) -> (i32, String, String) {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(&args, &mut out, &mut err);
    (
      code,
      String::from_utf8(out).unwrap(),
      String::from_utf8(err).unwrap(),
    )
  }

  #[test]
  fn help_version_and_unknown_words() {
    assert_eq!(run_str(&[]).0, 0);
    assert!(run_str(&["--help"])
      .1
      .starts_with("lmsql: one SQL statement"));
    assert_eq!(
      run_str(&["-V"]).1,
      format!("lmsql {}\n", env!("CARGO_PKG_VERSION"))
    );
    let (code, out, err) = run_str(&["select"]);
    assert_eq!((code, out.as_str()), (2, ""));
    assert!(err.contains("unknown command"));
  }
}
