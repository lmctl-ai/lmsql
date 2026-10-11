//! The operations on PostgreSQL, open mode: run any SQL over an autocommit session.
//!
//! A single row-returning statement is read through a server cursor in batches and streamed into
//! the [`Sink`] (inline, or a file for a big result), so even millions of rows use bounded memory.
//! Anything else (writes, DDL, `RETURNING`, utility statements, multi-statement text) runs over the
//! simple query protocol; the driver returns that response as a whole, so a big result from such
//! text is held in memory once before it is written (documented). Values come back in
//! PostgreSQL's text format and are typed by the column's type where lmsql knows it (always on the
//! cursor path; by the prepared statement on the RETURNING fallback; not for free-form
//! multi-statement text, whose values are strings).

use super::values::to_json;
use super::{connect, Postgres};
use crate::backend::{
  Backend, Column, ColumnInfo, DryRun, Planned, RunStats, TableDescription, TableEntry,
};
use crate::error::DbError;
use crate::output::Sink;
use crate::values::Limits;
use postgres::{SimpleQueryMessage, Transaction};
use serde_json::Value;

const CURSOR: &str = "lmsql_cursor";

/// Cursor batches grow from 64 rows to this.
const MAX_BATCH: usize = 2048;

fn columns(stmt: &postgres::Statement) -> (Vec<Column>, Vec<String>) {
  let cols = stmt
    .columns()
    .iter()
    .map(|c| Column {
      name: c.name().to_string(),
      decl_type: Some(c.type_().name().to_string()),
    })
    .collect();
  let types = stmt
    .columns()
    .iter()
    .map(|c| c.type_().name().to_string())
    .collect();
  (cols, types)
}

/// One text row typed by `types` (missing types read as text).
fn typed(row: &postgres::SimpleQueryRow, types: &[String]) -> Vec<Value> {
  (0..row.len())
    .map(|i| {
      to_json(
        types.get(i).map(String::as_str).unwrap_or("text"),
        row.get(i),
      )
    })
    .collect()
}

/// Read the declared cursor to the end in growing batches (text values), row by row.
fn fetch_cursor(
  tx: &mut Transaction<'_>,
  types: &[String],
  sink: &mut Sink,
  err: &dyn Fn(postgres::Error) -> DbError,
) -> Result<(), DbError> {
  let mut batch = 64usize;
  loop {
    let messages = tx
      .simple_query(&format!("FETCH FORWARD {batch} FROM {CURSOR}"))
      .map_err(err)?;
    let mut got = 0;
    for message in messages {
      if let SimpleQueryMessage::Row(row) = message {
        got += 1;
        sink.row(typed(&row, types))?;
      }
    }
    if got < batch {
      return Ok(());
    }
    batch = (batch * 2).min(MAX_BATCH);
  }
}

/// Feed a simple-query response to the sink: each statement with rows is a new result set, each
/// without rows an empty one. Returns the last command tag count. `types` types the columns when
/// they are known (the single-statement RETURNING fallback).
fn feed(
  messages: Vec<SimpleQueryMessage>,
  types: Option<(&[Column], &[String])>,
  sink: &mut Sink,
) -> Result<u64, DbError> {
  let mut affected = 0u64;
  let mut described = false;
  let mut current: Vec<String> = Vec::new();
  for message in messages {
    match message {
      SimpleQueryMessage::RowDescription(desc) => {
        described = true;
        let (cols, tys) = match types {
          Some((cols, tys)) if cols.len() == desc.len() => (cols.to_vec(), tys.to_vec()),
          _ => (
            desc
              .iter()
              .map(|c| Column {
                name: c.name().to_string(),
                decl_type: None,
              })
              .collect(),
            vec!["text".to_string(); desc.len()],
          ),
        };
        current = tys;
        sink.begin(cols)?;
      }
      SimpleQueryMessage::Row(row) => sink.row(typed(&row, &current))?,
      SimpleQueryMessage::CommandComplete(n) => {
        affected = n;
        if !described {
          sink.begin(Vec::new())?;
        }
        described = false;
      }
      _ => {}
    }
  }
  Ok(affected)
}

/// A `json` column read as its text (the plan of `EXPLAIN (FORMAT JSON)`).
struct JsonText(String);

impl<'a> postgres::types::FromSql<'a> for JsonText {
  fn from_sql(
    _: &postgres::types::Type,
    raw: &'a [u8],
  ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
    Ok(JsonText(String::from_utf8(raw.to_vec())?))
  }
  fn accepts(ty: &postgres::types::Type) -> bool {
    *ty == postgres::types::Type::JSON
  }
}

impl Postgres {
  /// The plan of one statement, best effort: `EXPLAIN (FORMAT JSON)` without ANALYZE plans and
  /// never executes, and the extended protocol accepts exactly one statement. `null` when the
  /// text cannot be explained (DDL, multi-statement text).
  fn plan(&self, sql: &str, limits: &Limits) -> (Option<Value>, Option<f64>) {
    let Ok(mut session) = connect(&self.dsn, limits) else {
      return (None, None);
    };
    if session.apply_settings(true).is_err() {
      return (None, None);
    }
    let explained = session
      .client
      .query_one(&format!("EXPLAIN (FORMAT JSON) {sql}") as &str, &[])
      .ok()
      .and_then(|row| row.try_get::<_, JsonText>(0).ok())
      .and_then(|text| serde_json::from_str::<Value>(&text.0).ok());
    match explained {
      Some(plan) => {
        // A write's top node (ModifyTable) estimates 0 output rows; the rows it would touch are
        // the estimate of its input node.
        let top = &plan[0]["Plan"];
        let node = if top["Node Type"] == "ModifyTable" {
          &top["Plans"][0]
        } else {
          top
        };
        let rows = node["Plan Rows"]
          .as_f64()
          .or_else(|| top["Plan Rows"].as_f64());
        (Some(plan), rows)
      }
      None => (None, None),
    }
  }
}

/// The first word of `sql` after whitespace, comments (`--`, nested `/* */`) and opening
/// parentheses, lower-cased (empty when there is none).
fn first_keyword(sql: &str) -> String {
  let b = sql.as_bytes();
  let mut i = 0;
  loop {
    while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b'(') {
      i += 1;
    }
    if b[i..].starts_with(b"--") {
      match b[i..].iter().position(|&c| c == b'\n') {
        Some(n) => i += n + 1,
        None => return String::new(),
      }
    } else if b[i..].starts_with(b"/*") {
      let mut depth = 0usize;
      while i < b.len() {
        if b[i..].starts_with(b"/*") {
          depth += 1;
          i += 2;
        } else if b[i..].starts_with(b"*/") {
          depth -= 1;
          i += 2;
          if depth == 0 {
            break;
          }
        } else {
          i += 1;
        }
      }
    } else {
      break;
    }
  }
  b[i..]
    .iter()
    .take_while(|c| c.is_ascii_alphabetic())
    .map(|c| c.to_ascii_lowercase() as char)
    .collect()
}

fn relkind(kind: &str) -> &'static str {
  match kind {
    "v" => "view",
    "m" => "materialized view",
    "f" => "foreign table",
    _ => "table",
  }
}

impl Backend for Postgres {
  fn run(&self, sql: &str, limits: &Limits, sink: &mut Sink) -> Result<RunStats, DbError> {
    let mut session = connect(&self.dsn, limits)?;
    session.apply_settings(self.read_only)?;
    let deadline = session.deadline;
    let err = move |e: postgres::Error| {
      super::errors::from_pg(e, false, std::time::Instant::now() >= deadline)
    };
    let stats = |rows_affected| RunStats {
      rows_affected,
      last_insert_rowid: None,
    };

    // One statement the extended protocol accepts: prepare tells us its columns and types.
    match session.client.prepare(sql) {
      Ok(stmt) if stmt.columns().is_empty() => {
        let affected = session.client.execute(&stmt, &[]).map_err(err)?;
        sink.begin(Vec::new())?;
        Ok(stats(affected))
      }
      Ok(stmt) => {
        let (cols, types) = columns(&stmt);
        // A cursor streams a huge result; a data-modifying statement (INSERT ... RETURNING,
        // WITH ... INSERT) cannot be a cursor, so it falls back to the simple protocol.
        let mut tx = session.client.transaction().map_err(err)?;
        let declared = tx.batch_execute(&format!(
          "DECLARE {CURSOR} NO SCROLL CURSOR WITHOUT HOLD FOR {sql}"
        ));
        if declared.is_ok() {
          sink.begin(cols)?;
          let fetched = fetch_cursor(&mut tx, &types, sink, &err);
          let _ = tx.rollback();
          fetched?;
          return Ok(stats(0));
        }
        let _ = tx.rollback();
        let messages = session.client.simple_query(sql).map_err(err)?;
        Ok(stats(feed(messages, Some((&cols, &types)), sink)?))
      }
      // Multi-statement text or a utility statement the extended protocol rejects: run the whole
      // text over the simple protocol (autocommit, unless the text has its own BEGIN/COMMIT).
      Err(_) => {
        let messages = session.client.simple_query(sql).map_err(err)?;
        Ok(stats(feed(messages, None, sink)?))
      }
    }
  }

  /// A guardrail heuristic for honest mistakes, not a parser: only text whose first keyword is a
  /// read (SELECT, WITH, VALUES, TABLE, SHOW) is sent at all, and then in a session whose every
  /// transaction is READ ONLY; a statement the server still refuses as a write (SQLSTATE 25006)
  /// is reported, not run. Everything else is reported as a change without being sent. The plan
  /// is a best-effort extra.
  fn dry_run(&self, sql: &str, limits: &Limits, sink: &mut Sink) -> Result<DryRun, DbError> {
    let keyword = first_keyword(sql);
    // EXPLAIN without ANALYZE plans and never executes; only tried for plain DML.
    let plannable = matches!(
      keyword.as_str(),
      "insert" | "update" | "delete" | "merge" | "with"
    );
    let would_change = || {
      let (plan, estimated_rows) = if plannable {
        self.plan(sql, limits)
      } else {
        (None, None)
      };
      Ok(DryRun::WouldChange(vec![Planned {
        statement: sql.to_string(),
        would_change: true,
        plan,
        estimated_rows,
      }]))
    };
    if !matches!(
      keyword.as_str(),
      "select" | "with" | "values" | "table" | "show"
    ) {
      return would_change();
    }
    let read_only = Postgres {
      dsn: self.dsn.clone(),
      read_only: true,
    };
    match read_only.run(sql, limits, sink) {
      Ok(stats) => Ok(DryRun::Read(stats)),
      Err(e) if e.tool_code == "PG_25006" => would_change(),
      Err(e) => Err(e),
    }
  }

  fn tables(
    &self,
    schema: Option<&str>,
    limits: &Limits,
  ) -> Result<(Vec<TableEntry>, bool), DbError> {
    let mut session = connect(&self.dsn, limits)?;
    session.apply_settings(self.read_only)?;
    let deadline = session.deadline;
    let err =
      |e: postgres::Error| super::errors::from_pg(e, false, std::time::Instant::now() >= deadline);
    let cap = limits.max_rows as i64 + 1;
    let rows = session
      .client
      .query(
        "SELECT n.nspname, c.relname, c.relkind::text \
         FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f') \
           AND CASE WHEN $1::text IS NULL \
                    THEN n.nspname <> 'information_schema' AND n.nspname !~ '^pg_' \
                    ELSE n.nspname = $1::text END \
         ORDER BY 1, 2 LIMIT $2::int8",
        &[&schema, &cap],
      )
      .map_err(err)?;
    let truncated = rows.len() > limits.max_rows;
    let tables = rows
      .iter()
      .take(limits.max_rows)
      .map(|r| TableEntry {
        schema: Some(r.get(0)),
        name: r.get(1),
        kind: relkind(r.get(2)).to_string(),
      })
      .collect();
    Ok((tables, truncated))
  }

  fn describe(
    &self,
    schema: Option<&str>,
    table: &str,
    limits: &Limits,
  ) -> Result<TableDescription, DbError> {
    let not_found = || {
      DbError::new(
        lmrust::server::ErrorCode::NotFound,
        "TABLE_NOT_FOUND",
        format!("no table or view named {table:?}"),
      )
    };
    let mut session = connect(&self.dsn, limits)?;
    session.apply_settings(self.read_only)?;
    let deadline = session.deadline;
    let err =
      |e: postgres::Error| super::errors::from_pg(e, false, std::time::Instant::now() >= deadline);
    let found = match schema {
      Some(schema) => session.client.query_opt(
        "SELECT c.oid, n.nspname, c.relname, c.relkind::text \
         FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = $1::text AND c.relname = $2::text AND c.relkind IN ('r', 'p', 'v', 'm', 'f')",
        &[&schema, &table],
      ),
      None => session.client.query_opt(
        "SELECT c.oid, n.nspname, c.relname, c.relkind::text \
         FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.oid = pg_catalog.to_regclass($1::text) AND c.relkind IN ('r', 'p', 'v', 'm', 'f')",
        &[&table],
      ),
    };
    let found = match found {
      Ok(row) => row,
      Err(e)
        if e
          .code()
          .is_some_and(|c| c.code() == "42602" || c.code() == "3F000") =>
      {
        None
      }
      Err(e) => return Err(err(e)),
    };
    let Some(found) = found else {
      return Err(not_found());
    };
    let oid: postgres::types::Oid = found.get(0);
    let rows = session
      .client
      .query(
        "SELECT a.attname, pg_catalog.format_type(a.atttypid, a.atttypmod), a.attnotnull, \
                pg_catalog.pg_get_expr(d.adbin, d.adrelid), \
                COALESCE((SELECT u.ord FROM pg_catalog.pg_index i, \
                          LATERAL unnest(i.indkey::int2[]) WITH ORDINALITY AS u(attnum, ord) \
                          WHERE i.indrelid = a.attrelid AND i.indisprimary AND u.attnum = a.attnum), 0) \
         FROM pg_catalog.pg_attribute a \
         LEFT JOIN pg_catalog.pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
         WHERE a.attrelid = $1 AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum",
        &[&oid],
      )
      .map_err(err)?;
    let columns = rows
      .iter()
      .enumerate()
      .map(|(i, r)| ColumnInfo {
        cid: i as i64,
        name: r.get(0),
        decl_type: r.get(1),
        notnull: r.get(2),
        default: r.get(3),
        pk: r.get(4),
      })
      .collect();
    Ok(TableDescription {
      schema: Some(found.get(1)),
      name: found.get(2),
      kind: relkind(found.get(3)).to_string(),
      columns,
    })
  }
}

#[cfg(test)]
mod tests {
  use super::first_keyword;

  #[test]
  fn first_keyword_skips_whitespace_comments_and_parentheses() {
    for (sql, kw) in [
      ("SELECT 1", "select"),
      ("  \n\twith x as (select 1) select * from x", "with"),
      ("-- note\nDELETE FROM t", "delete"),
      ("/* a /* nested */ b */ (( VALUES (1)))", "values"),
      ("Show search_path", "show"),
      ("insert into t values (1)", "insert"),
      ("-- only a comment", ""),
      ("", ""),
      ("/* unterminated", ""),
      ("1 + 1", ""),
      ("\u{2603} select", ""),
    ] {
      assert_eq!(first_keyword(sql), kw, "{sql:?}");
    }
  }
}
