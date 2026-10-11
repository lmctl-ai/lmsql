//! The published lmapi operations (`query`, `exec`, `tables`, `describe`) and their handlers.

use crate::backend::Backend;
use crate::backend::DryRun;
use crate::connection;
use crate::connection::{Access as Granted, Intent};
use crate::error::DbError;
use crate::output::{Format, OutputConfig, Sink, DEFAULT_PREVIEW_ROWS};
use crate::policy::Policy as OpenPolicy;
use crate::settings::{Level, Settings};
use crate::values::{
  Limits, DEFAULT_MAX_BYTES, DEFAULT_MAX_ROWS, DEFAULT_TIMEOUT_MS, HARD_MAX_BYTES, HARD_MAX_ROWS,
  HARD_TIMEOUT_MS,
};
use lmrust::server::{CliError, CliSpec, ErrorCode, OpDef, Policy};
use lmrust::{Access, InputForm, OperationSchema, ParamSpec, ParamType, Record, ReturnType};
use serde_json::{json, Value};

pub const TOOL: &str = "lmsql";

fn db_param() -> ParamSpec {
  ParamSpec::new("db", ParamType::String)
    .optional()
    .describe("Path of a SQLite database file, or a postgres:// DSN (prefer connection: argv is visible to other users). With a settings file, allowed only if it sets allow_direct")
}

fn connection_param() -> ParamSpec {
  ParamSpec::new("connection", ParamType::String)
    .optional()
    .describe("Name of a configured connection: from the settings file if there is one (which also sets its permission level), else the environment variable LMSQL_<NAME> holding a SQLite path or a postgres:// DSN")
}

fn schema_param() -> ParamSpec {
  ParamSpec::new("schema", ParamType::String)
    .optional()
    .describe("Schema to look in (PostgreSQL; SQLite has only main)")
}

fn statement_param(what: &str) -> ParamSpec {
  ParamSpec::new("statement", ParamType::String)
    .positional()
    .describe(format!("SQL to run ({what})"))
}

fn max_rows_param() -> ParamSpec {
  // No declared default: whether maxRows was given decides the preview size of a file response.
  ParamSpec::new("maxRows", ParamType::Number)
    .optional()
    .describe(format!(
      "Inline threshold (1..{HARD_MAX_ROWS}, default {DEFAULT_MAX_ROWS}): a result with more rows is written in full to a file (spilled: true) instead of inline; in a file response, the preview size (default {DEFAULT_PREVIEW_ROWS})"
    ))
}

fn max_bytes_param() -> ParamSpec {
  ParamSpec::new("maxBytes", ParamType::Number)
    .optional()
    .with_default(json!(DEFAULT_MAX_BYTES))
    .describe(format!(
      "Inline threshold in serialized bytes (1..{HARD_MAX_BYTES}): a bigger result is written in full to a file instead of inline"
    ))
}

fn list_max_rows_param() -> ParamSpec {
  ParamSpec::new("maxRows", ParamType::Number)
    .optional()
    .with_default(json!(DEFAULT_MAX_ROWS))
    .describe(format!(
      "List at most this many tables (1..{HARD_MAX_ROWS}); more set truncated: true"
    ))
}

fn out_param() -> ParamSpec {
  ParamSpec::new("out", ParamType::String)
    .optional()
    .describe("Write the full result to this file (streamed; replaced atomically; parent directories created) and return a summary with a preview")
}

fn out_format_param() -> ParamSpec {
  ParamSpec::new("outFormat", ParamType::String)
    .optional()
    .with_enum(Format::NAMES.iter().map(|n| json!(n)))
    .describe("Format of the out file: ndjson (one JSON object per row), csv (RFC 4180, header row) or json (one array); default from the out extension, else ndjson")
}

/// `--dangerously-bypass-permission-settings`: on every operation.
pub const BYPASS_PARAM: &str = "dangerouslyBypassPermissionSettings";

fn bypass_param() -> ParamSpec {
  ParamSpec::new(BYPASS_PARAM, ParamType::Boolean)
    .optional()
    .describe("Skip the settings file's permission rules for this call (connection levels, table rules, the direct-access gate, output.dir); only works when the settings file sets allow_bypass: true. The operational limits still apply")
}

fn timeout_param() -> ParamSpec {
  ParamSpec::new("timeoutMs", ParamType::Number)
    .optional()
    .with_default(json!(DEFAULT_TIMEOUT_MS))
    .describe(format!(
      "Abort the statement after this many milliseconds (1..{HARD_TIMEOUT_MS}), also the lock wait"
    ))
}

fn schema(
  name: &str,
  access: Access,
  description: &str,
  params: Vec<ParamSpec>,
) -> OperationSchema {
  let mut s = OperationSchema::new(name)
    .access(access)
    .describe(description);
  s.returns = Some(ReturnType::Object);
  for p in params {
    s = s.param(p);
  }
  s
}

/// The operation schemas, in published order.
pub fn schemas() -> Vec<OperationSchema> {
  vec![
    schema(
      "query",
      Access::Dynamic,
      "Run SQL and return columns and rows as JSON (open mode: runs any statement the database accepts; identical to exec)",
      vec![
        statement_param("any SQL; multiple statements run in order and the last one's rows are returned"),
        db_param(),
        connection_param(),
        max_rows_param(),
        max_bytes_param(),
        out_param(),
        out_format_param(),
        timeout_param(),
        bypass_param(),
      ],
    ),
    schema(
      "exec",
      Access::Dynamic,
      "Run SQL and return rows plus rows_affected and last_insert_rowid (open mode: identical to query; both run any statement)",
      vec![
        statement_param("any SQL: INSERT, UPDATE, DELETE, CREATE, ...; RETURNING and SELECT rows are returned"),
        db_param(),
        connection_param(),
        max_rows_param(),
        max_bytes_param(),
        out_param(),
        out_format_param(),
        timeout_param(),
        bypass_param(),
      ],
    ),
    schema(
      "tables",
      Access::Read,
      "List tables and views (internal sqlite_* objects excluded)",
      vec![
        db_param(),
        connection_param(),
        schema_param(),
        list_max_rows_param(),
        timeout_param(),
        bypass_param(),
      ],
    ),
    schema(
      "describe",
      Access::Read,
      "Describe one table or view: columns with type, notnull, default and primary-key position",
      vec![
        ParamSpec::new("table", ParamType::String)
          .positional()
          .describe("Table or view name: SQLite matches case-insensitively; PostgreSQL follows SQL rules (schema.table, quoting), or the exact name when schema is given"),
        db_param(),
        connection_param(),
        schema_param(),
        timeout_param(),
        bypass_param(),
      ],
    ),
  ]
}

fn text<'a>(params: &'a Record, name: &str) -> Option<&'a str> {
  params.get(name).and_then(Value::as_str)
}

fn number(params: &Record, name: &str) -> Option<f64> {
  params.get(name).and_then(Value::as_f64)
}

fn limits(params: &Record) -> Result<Limits, DbError> {
  Limits::new(
    number(params, "maxRows"),
    number(params, "maxBytes"),
    number(params, "timeoutMs"),
  )
}

fn env(name: &str) -> Option<String> {
  std::env::var(name).ok()
}

/// Run `f` against the resolved backend under the settings file (if any); errors and results
/// never carry a path or DSN. `intent` describes the call for a permission denial.
fn with_backend(
  params: &Record,
  intent: Intent<'_>,
  f: impl FnOnce(&dyn Backend, &Limits, &Granted) -> Result<Value, DbError>,
) -> Result<Value, CliError> {
  let settings = Settings::load(&env, &mut |warning| eprintln!("{warning}"))
    .map_err(|e| e.into_cli_error(&[]))?;
  let mut secrets = settings.secrets(&env);
  let bypass = match (
    &settings.source,
    params.get(BYPASS_PARAM) == Some(&Value::Bool(true)),
  ) {
    (Some(file), true) if !settings.allow_bypass => {
      return Err(
        DbError::new(
          ErrorCode::Forbidden,
          "BYPASS_NOT_ALLOWED",
          format!(
            "--dangerously-bypass-permission-settings is not allowed: the operator must set \
             \"allow_bypass\": true in {}",
            file.display()
          ),
        )
        .into_cli_error(&secrets),
      );
    }
    (Some(file), true) => {
      eprintln!(
        "lmsql: permission settings bypassed for this call (allow_bypass is true in {})",
        file.display()
      );
      true
    }
    // No settings file: nothing to bypass.
    _ => false,
  };
  let (target, mut access) = connection::resolve_with(
    text(params, "db"),
    text(params, "connection"),
    &settings,
    bypass,
    env,
  )
  .map_err(|e| e.into_cli_error(&secrets))?;
  if !bypass {
    access.output_dir = settings.output_dir.clone();
  }
  secrets.extend(target.secrets());
  if access.max_level() == Level::None {
    return Err(access.denied(intent).into_cli_error(&secrets));
  }
  if let Intent::Describe(table) = intent {
    if access.rule.is_some() && access.level_of(table) == Level::None {
      return Err(access.denied_table(table, intent).into_cli_error(&secrets));
    }
  }
  let limits = limits(params).map_err(|e| e.into_cli_error(&secrets))?;
  let backend = target.backend(access.mode());
  // Results carry user data only (rows, names, counts); error messages are scrubbed.
  f(backend.as_ref(), &limits, &access)
    .map_err(|e| access.map_error(e, intent))
    .map_err(|e| e.into_cli_error(&secrets))
}

fn statement(params: &Record) -> &str {
  text(params, "statement").unwrap_or_default()
}

/// `query` and `exec` are identical in open mode: both pass the SQL through the policy seam
/// (which allows everything in v1) and run it, returning one shape.
fn run_sql(op: &str, params: &Record, dry_run: bool) -> Result<Value, CliError> {
  let sql = statement(params).to_string();
  let intent = Intent::Sql(statement(params));
  with_backend(params, intent, |b, l, access| {
    OpenPolicy::load().allow(op, &sql)?;
    // Checked before the SQL runs: a bad out path must not follow a committed write.
    let config = OutputConfig::new(
      text(params, "out"),
      text(params, "outFormat"),
      l.max_rows,
      l.max_bytes,
      params.contains_key("maxRows"),
      dry_run,
      access.output_dir.as_deref(),
    )?;
    let mut sink = Sink::new(config);
    if dry_run {
      return dry_run_result(b, l, &sql, sink, access, intent);
    }
    let stats = b.run(&sql, l, &mut sink)?;
    Ok(sink.finish()?.to_json(&stats))
  })
}

/// `--dry-run`: estimate without changing anything. Reads run normally; if anything would
/// change, nothing is executed and the response describes it. Permissions still apply.
fn dry_run_result(
  b: &dyn Backend,
  l: &Limits,
  sql: &str,
  mut sink: Sink,
  access: &Granted,
  intent: Intent<'_>,
) -> Result<Value, DbError> {
  match b.dry_run(sql, l, &mut sink)? {
    DryRun::Read(stats) => {
      let mut out = sink.finish()?.to_json(&stats);
      out["dry_run"] = json!(true);
      out["executed"] = json!(true);
      out["would_change"] = json!(false);
      Ok(out)
    }
    DryRun::WouldChange(planned) => {
      // A write the connection level does not allow stays denied in a dry run.
      if access.rule.is_some() && access.max_level() <= Level::ReadOnly {
        return Err(access.denied(intent));
      }
      let single = planned.len() == 1;
      let mut out = json!({
        "dry_run": true,
        "executed": false,
        "would_change": true,
        "statement": sql,
        "plan": if single { planned[0].plan.clone() } else { Value::Null },
      });
      if let Some(rows) = planned
        .first()
        .and_then(|p| p.estimated_rows)
        .filter(|_| single)
      {
        out["estimated_rows"] = json!(rows);
      }
      if !single {
        out["statements"] = Value::Array(
          planned
            .iter()
            .map(
              |p| json!({"statement": p.statement, "would_change": p.would_change, "plan": p.plan}),
            )
            .collect(),
        );
      }
      Ok(out)
    }
  }
}

pub fn spec() -> CliSpec<Policy> {
  let ops = schemas();
  let op = |name: &str| {
    ops
      .iter()
      .find(|s| s.name == name)
      .cloned()
      .expect("op schema")
  };
  CliSpec::with_policy(TOOL)
    .version(env!("CARGO_PKG_VERSION"))
    .description("One SQL statement in, structured JSON out (SQLite, PostgreSQL)")
    .inputs([InputForm::Flags, InputForm::Json])
    .op(
      OpDef::new(
        "query",
        Access::Dynamic,
        op("query").description.unwrap_or_default(),
        |params, policy: &Policy| run_sql("query", params, policy.dry_run),
      )
      .schema(op("query")),
    )
    .op(
      OpDef::new(
        "exec",
        Access::Dynamic,
        op("exec").description.unwrap_or_default(),
        |params, policy: &Policy| run_sql("exec", params, policy.dry_run),
      )
      .schema(op("exec")),
    )
    .op(
      OpDef::new(
        "tables",
        Access::Read,
        op("tables").description.unwrap_or_default(),
        |params, _| {
          with_backend(params, Intent::Tables, |b, l, access| {
            let (mut tables, truncated) = b.tables(text(params, "schema"), l)?;
            // A `none` table is hidden.
            tables.retain(|t| access.rule.is_none() || access.level_of(&t.name) != Level::None);
            Ok(json!({
              "tables": tables.iter().map(|t| t.to_json()).collect::<Vec<_>>(),
              "truncated": truncated,
            }))
          })
        },
      )
      .schema(op("tables")),
    )
    .op(
      OpDef::new(
        "describe",
        Access::Read,
        op("describe").description.unwrap_or_default(),
        |params, _| {
          let table = text(params, "table").unwrap_or_default();
          with_backend(params, Intent::Describe(table), |b, l, _| {
            b.describe(text(params, "schema"), table, l)
              .map(|d| d.to_json())
          })
        },
      )
      .schema(op("describe")),
    )
}

/// The published document (`lmsql --schema`), as a string, byte-identical to the CLI output.
pub fn schema_text() -> String {
  let mut out = Vec::new();
  let code = lmrust::server::run_cli_io(
    &spec(),
    &["--schema".to_string()],
    &mut out,
    &Default::default(),
  );
  assert_eq!(code, 0);
  String::from_utf8(out).expect("utf-8 schema")
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_document_passes_lmrust_publish_check() {
    let doc: Value = serde_json::from_str(&schema_text()).unwrap();
    lmrust::parse_published_schema(&doc).unwrap_or_else(|issues| panic!("{issues:?}"));
    assert_eq!(doc["tool"], TOOL);
    assert_eq!(doc["inputs"], json!(["flags", "json"]));
    assert!(doc.get("product").is_none(), "lmsql is its own product");
    let ops: Vec<(&str, &str)> = doc["operations"]
      .as_array()
      .unwrap()
      .iter()
      .map(|o| (o["name"].as_str().unwrap(), o["access"].as_str().unwrap()))
      .collect();
    assert_eq!(
      ops,
      [
        ("query", "dynamic"),
        ("exec", "dynamic"),
        ("tables", "read"),
        ("describe", "read")
      ]
    );
    for op in doc["operations"].as_array().unwrap() {
      assert_eq!(op["returns"], "object");
      assert!(op["description"].as_str().is_some_and(|d| d.len() > 20));
      for p in op["params"].as_array().unwrap() {
        assert!(
          p["description"].as_str().is_some_and(|d| !d.is_empty()),
          "{}",
          p["name"]
        );
      }
    }
  }

  #[test]
  fn the_document_is_deterministic() {
    assert_eq!(schema_text(), schema_text());
  }
}
