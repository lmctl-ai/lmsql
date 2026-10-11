//! The published lmapi operations (`query`, `exec`, `tables`, `describe`) and their handlers.

use crate::backend::{Backend, Target};
use crate::connection;
use crate::error::DbError;
use crate::values::{
  Limits, DEFAULT_MAX_BYTES, DEFAULT_MAX_ROWS, DEFAULT_TIMEOUT_MS, HARD_MAX_BYTES, HARD_MAX_ROWS,
  HARD_TIMEOUT_MS,
};
use lmrust::server::{check_policy, CheckContext, CliError, CliSpec, OpDef, Policy};
use lmrust::{Access, InputForm, OperationSchema, ParamSpec, ParamType, Record, ReturnType};
use serde_json::{json, Value};

pub const TOOL: &str = "lmsql";

fn db_param() -> ParamSpec {
  ParamSpec::new("db", ParamType::String)
    .optional()
    .describe("Path of the SQLite database file (or use connection)")
}

fn connection_param() -> ParamSpec {
  ParamSpec::new("connection", ParamType::String)
    .optional()
    .describe("Name of a configured connection: the value of the environment variable LMSQL_<NAME>")
}

fn statement_param(what: &str) -> ParamSpec {
  ParamSpec::new("statement", ParamType::String)
    .positional()
    .describe(format!("Exactly one SQL statement ({what})"))
}

fn max_rows_param() -> ParamSpec {
  ParamSpec::new("maxRows", ParamType::Number)
    .optional()
    .with_default(json!(DEFAULT_MAX_ROWS))
    .describe(format!(
      "Return at most this many rows (1..{HARD_MAX_ROWS}); more set truncated: true"
    ))
}

fn max_bytes_param() -> ParamSpec {
  ParamSpec::new("maxBytes", ParamType::Number)
    .optional()
    .with_default(json!(DEFAULT_MAX_BYTES))
    .describe(format!(
      "Cap on the serialized rows in bytes (1..{HARD_MAX_BYTES}); hitting it sets truncated: true"
    ))
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
      Access::Read,
      "Run one read-only SQL statement and return columns and rows as JSON",
      vec![
        statement_param("read-only: SELECT, WITH ... SELECT, read PRAGMA, EXPLAIN"),
        db_param(),
        connection_param(),
        max_rows_param(),
        max_bytes_param(),
        timeout_param(),
      ],
    ),
    schema(
      "exec",
      Access::Write,
      "Run one SQL statement that changes data or schema; returns rows_affected and last_insert_rowid",
      vec![
        statement_param("INSERT, UPDATE, DELETE, CREATE, ...; RETURNING rows are returned"),
        db_param(),
        connection_param(),
        ParamSpec::new("create", ParamType::Boolean)
          .optional()
          .describe("Create the SQLite database file if it does not exist"),
        max_rows_param(),
        max_bytes_param(),
        timeout_param(),
      ],
    ),
    schema(
      "tables",
      Access::Read,
      "List tables and views (internal sqlite_* objects excluded)",
      vec![db_param(), connection_param(), max_rows_param(), timeout_param()],
    ),
    schema(
      "describe",
      Access::Read,
      "Describe one table or view: columns with type, notnull, default and primary-key position",
      vec![
        ParamSpec::new("table", ParamType::String)
          .positional()
          .describe("Table or view name (case-insensitive)"),
        db_param(),
        connection_param(),
        timeout_param(),
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

fn target(params: &Record) -> Result<Target, DbError> {
  connection::resolve(text(params, "db"), text(params, "connection"), |k| {
    std::env::var(k).ok()
  })
}

fn limits(params: &Record) -> Result<Limits, DbError> {
  Limits::new(
    number(params, "maxRows"),
    number(params, "maxBytes"),
    number(params, "timeoutMs"),
  )
}

/// Run `f` against the resolved backend; errors and results never carry the path or DSN.
fn with_backend(
  params: &Record,
  f: impl FnOnce(&dyn Backend, &Limits) -> Result<Value, DbError>,
) -> Result<Value, CliError> {
  let target = target(params).map_err(|e| e.into_cli_error(&[]))?;
  let secrets = target.secrets();
  let limits = limits(params).map_err(|e| e.into_cli_error(&secrets))?;
  let backend = target.backend();
  // Results carry user data only (rows, names, counts); error messages are scrubbed.
  f(backend.as_ref(), &limits).map_err(|e| e.into_cli_error(&secrets))
}

fn statement(params: &Record) -> &str {
  text(params, "statement").unwrap_or_default()
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
    .description("One SQL statement in, structured JSON out (SQLite)")
    .inputs([InputForm::Flags, InputForm::Json])
    .op(
      OpDef::new("query", Access::Read, op("query").description.unwrap_or_default(), |params, _| {
        with_backend(params, |b, l| b.query(statement(params), l).map(|r| r.to_json()))
      })
      .schema(op("query")),
    )
    .op(
      OpDef::new("exec", Access::Write, op("exec").description.unwrap_or_default(), |params, policy: &Policy| {
        // The statement is not echoed into the policy result: it may hold literals the caller
        // treats as sensitive, and the caller already has it.
        let ctx = CheckContext::new("exec").dangerous(true);
        if let Some(simulated) = check_policy(Access::Write, policy, &ctx)? {
          return Ok(Value::Object(simulated));
        }
        let create = params.get("create") == Some(&Value::Bool(true));
        with_backend(params, |b, l| b.exec(statement(params), l, create).map(|r| r.to_json()))
      })
      .schema(op("exec")),
    )
    .op(
      OpDef::new("tables", Access::Read, op("tables").description.unwrap_or_default(), |params, _| {
        with_backend(params, |b, l| {
          let (tables, truncated) = b.tables(l)?;
          Ok(json!({
            "tables": tables.iter().map(|t| json!({"name": t.name, "type": t.kind})).collect::<Vec<_>>(),
            "truncated": truncated,
          }))
        })
      })
      .schema(op("tables")),
    )
    .op(
      OpDef::new("describe", Access::Read, op("describe").description.unwrap_or_default(), |params, _| {
        let table = text(params, "table").unwrap_or_default();
        with_backend(params, |b, l| b.describe(table, l).map(|d| d.to_json()))
      })
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
        ("query", "read"),
        ("exec", "write"),
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
