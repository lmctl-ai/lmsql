//! The four operations on PostgreSQL.

use super::classify::{self, Shape};
use super::values::{min_json_len, to_json};
use super::{begin, connect, Guards, Postgres, Session};
use crate::backend::{
  Backend, Column, ColumnInfo, ExecOutcome, Rows, TableDescription, TableEntry,
};
use crate::error::DbError;
use crate::values::Limits;
use fallible_iterator::FallibleIterator;
use lmrust::server::ErrorCode;
use postgres::types::{FromSql, ToSql, Type};
use postgres::{SimpleQueryMessage, Transaction};
use serde_json::Value;

const CURSOR: &str = "lmsql_cursor";

/// Cursor batches grow from 8 rows to this: a cap is overshot by at most one batch.
pub const MAX_BATCH: usize = 256;

/// Collects rows under the caps; `push` returns false when the caller must stop reading.
struct Sink<'l> {
  types: Vec<String>,
  rows: Vec<Vec<Value>>,
  bytes: usize,
  limits: &'l Limits,
  truncated_by: Option<&'static str>,
}

impl<'l> Sink<'l> {
  fn new(types: Vec<String>, limits: &'l Limits) -> Self {
    Sink {
      types,
      rows: Vec::new(),
      bytes: 2,
      limits,
      truncated_by: None,
    }
  }

  fn push(&mut self, texts: &[Option<&str>]) -> bool {
    if self.rows.len() == self.limits.max_rows {
      self.truncated_by = Some("max_rows");
      return false;
    }
    let width = texts.len();
    let mut lower = 2 + width.saturating_sub(1);
    for (ty, t) in self.types.iter().zip(texts) {
      lower += min_json_len(ty, *t);
    }
    if self.bytes + lower + 1 > self.limits.max_bytes {
      self.truncated_by = Some("max_bytes");
      return false;
    }
    let values: Vec<Value> = self
      .types
      .iter()
      .zip(texts)
      .map(|(ty, t)| to_json(ty, *t))
      .collect();
    let size = serde_json::to_string(&values)
      .map(|s| s.len())
      .unwrap_or(usize::MAX)
      + 1;
    if self.bytes + size > self.limits.max_bytes {
      self.truncated_by = Some("max_bytes");
      return false;
    }
    self.bytes += size;
    self.rows.push(values);
    true
  }

  fn finish(self, columns: Vec<Column>) -> Rows {
    Rows {
      columns,
      rows: self.rows,
      truncated_by: self.truncated_by,
    }
  }
}

/// A text-like column read from the extended protocol (EXPLAIN, SHOW, `::text` casts).
struct RawText(String);

impl<'a> FromSql<'a> for RawText {
  fn from_sql(ty: &Type, raw: &'a [u8]) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
    // jsonb's binary form is a version byte followed by the JSON text.
    let raw = if *ty == Type::JSONB {
      raw.get(1..).unwrap_or_default()
    } else {
      raw
    };
    Ok(RawText(String::from_utf8(raw.to_vec())?))
  }
  fn accepts(ty: &Type) -> bool {
    matches!(
      *ty,
      Type::TEXT
        | Type::VARCHAR
        | Type::BPCHAR
        | Type::NAME
        | Type::UNKNOWN
        | Type::JSON
        | Type::JSONB
        | Type::XML
    )
  }
}

fn columns_of(stmt: &postgres::Statement) -> Vec<Column> {
  stmt
    .columns()
    .iter()
    .map(|c| Column {
      name: c.name().to_string(),
      decl_type: Some(c.type_().name().to_string()),
    })
    .collect()
}

fn no_params() -> std::iter::Empty<&'static (dyn ToSql + Sync)> {
  std::iter::empty()
}

/// Read a declared cursor in growing batches over the simple protocol (text values).
fn fetch_cursor(tx: &mut Transaction<'_>, sink: &mut Sink<'_>) -> Result<(), postgres::Error> {
  let mut batch = 8usize;
  loop {
    let want = (sink.limits.max_rows + 1 - sink.rows.len().min(sink.limits.max_rows)).min(batch);
    let messages = tx.simple_query(&format!("FETCH FORWARD {want} FROM {CURSOR}"))?;
    let mut got = 0;
    for message in &messages {
      if let SimpleQueryMessage::Row(row) = message {
        got += 1;
        let texts: Vec<Option<&str>> = (0..row.len()).map(|i| row.get(i)).collect();
        if !sink.push(&texts) {
          return Ok(());
        }
      }
    }
    if got < want {
      return Ok(());
    }
    batch = (batch * 2).min(MAX_BATCH);
  }
}

/// Stream extended-protocol rows of text-like columns; returns how many rows the server sent.
fn stream_text_rows(
  rows: postgres::RowIter<'_>,
  sink: &mut Sink<'_>,
  drain: bool,
) -> Result<u64, postgres::Error> {
  let mut rows = rows;
  let mut count = 0u64;
  let mut open = true;
  while let Some(row) = rows.next()? {
    count += 1;
    if !open {
      continue; // drained for the row count, not stored
    }
    let texts: Vec<Option<String>> = (0..row.len())
      .map(|i| {
        row
          .try_get::<_, Option<RawText>>(i)
          .ok()
          .flatten()
          .map(|t| t.0)
      })
      .collect();
    let refs: Vec<Option<&str>> = texts.iter().map(|t| t.as_deref()).collect();
    if !sink.push(&refs) {
      open = false;
      if !drain {
        break;
      }
    }
  }
  Ok(count)
}

fn is_cursor_statement(shape: &Shape) -> bool {
  let first = shape.first_word().unwrap_or_default();
  let writes = shape
    .words()
    .any(|(_, w, _)| matches!(w, "insert" | "update" | "delete" | "merge"));
  matches!(first, "select" | "values" | "table") || (first == "with" && !writes)
}

impl Postgres {
  /// `query` with explicit layers (tests switch one off; production uses [`Guards::ALL`]).
  pub fn query_with(&self, sql: &str, limits: &Limits, guards: Guards) -> Result<Rows, DbError> {
    let shape = classify::single_statement(sql)?;
    if guards.statement_rules {
      classify::check_read_only(&shape)?;
    }
    let (a, b) = shape.body();
    let body = &sql[a..b];
    let mut session = connect(&self.dsn, limits)?;
    read_rows(&mut session, &shape, body, limits, guards)
  }
}

fn read_rows(
  session: &mut Session,
  shape: &Shape,
  body: &str,
  limits: &Limits,
  guards: Guards,
) -> Result<Rows, DbError> {
  let deadline = session.deadline;
  let err =
    |e: postgres::Error| super::errors::from_pg(e, false, std::time::Instant::now() >= deadline);
  let mut tx = begin(session, true, guards)?;
  let stmt = tx.prepare(body).map_err(err)?;
  let columns = columns_of(&stmt);
  let types: Vec<String> = stmt
    .columns()
    .iter()
    .map(|c| c.type_().name().to_string())
    .collect();
  let mut sink = Sink::new(types, limits);
  if is_cursor_statement(shape) {
    tx.execute(
      &format!("DECLARE {CURSOR} NO SCROLL CURSOR FOR {body}") as &str,
      &[],
    )
    .map_err(err)?;
    fetch_cursor(&mut tx, &mut sink).map_err(err)?;
  } else {
    let rows = tx.query_raw(&stmt, no_params()).map_err(err)?;
    stream_text_rows(rows, &mut sink, false).map_err(err)?;
  }
  tx.rollback().map_err(err)?;
  Ok(sink.finish(columns))
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
  fn query(&self, sql: &str, limits: &Limits) -> Result<Rows, DbError> {
    self.query_with(sql, limits, Guards::ALL)
  }

  fn exec(&self, sql: &str, limits: &Limits, create: bool) -> Result<ExecOutcome, DbError> {
    if create {
      return Err(DbError::new(
        ErrorCode::ValidationError,
        "CREATE_NOT_SUPPORTED",
        "create applies to SQLite files; PostgreSQL databases are created on the server",
      ));
    }
    let shape = classify::single_statement(sql)?;
    classify::check_exec(&shape)?;
    let (a, b) = shape.body();
    let body = &sql[a..b];
    let mut session = connect(&self.dsn, limits)?;
    let deadline = session.deadline;
    let err =
      |e: postgres::Error| super::errors::from_pg(e, false, std::time::Instant::now() >= deadline);
    let mut tx = begin(&mut session, false, Guards::ALL)?;
    let stmt = tx.prepare(body).map_err(err)?;
    let first = shape.first_word().unwrap_or_default().to_string();
    let (rows_affected, returning) = if stmt.columns().is_empty() {
      (tx.execute(&stmt, &[]).map_err(err)?, None)
    } else {
      let columns = columns_of(&stmt);
      let types: Vec<String> = stmt
        .columns()
        .iter()
        .map(|c| c.type_().name().to_string())
        .collect();
      let mut sink = Sink::new(types, limits);
      let affected = if is_cursor_statement(&shape) {
        tx.execute(
          &format!("DECLARE {CURSOR} NO SCROLL CURSOR FOR {body}") as &str,
          &[],
        )
        .map_err(err)?;
        fetch_cursor(&mut tx, &mut sink).map_err(err)?;
        0
      } else if matches!(first.as_str(), "insert" | "update" | "delete" | "merge") {
        // RETURNING rows as text, streamed: the statement becomes a top-level data-modifying CTE.
        let aliases: Vec<String> = (1..=columns.len()).map(|i| format!("\"c{i}\"")).collect();
        let casts: Vec<String> = aliases
          .iter()
          .map(|a| format!("lmsql_w.{a}::text"))
          .collect();
        let wrapped = format!(
          "WITH lmsql_w({}) AS (\n{body}\n) SELECT {} FROM lmsql_w",
          aliases.join(", "),
          casts.join(", ")
        );
        let rows = tx.query_raw(&wrapped as &str, no_params()).map_err(err)?;
        stream_text_rows(rows, &mut sink, true).map_err(err)?
      } else if first == "with" {
        // A WITH holding a data-modifying statement cannot be nested in another WITH: the
        // simple protocol returns its rows as text (layer 1 and Parse both saw one statement).
        let mut affected = 0;
        for message in tx.simple_query(body).map_err(err)? {
          match message {
            SimpleQueryMessage::Row(row) => {
              let texts: Vec<Option<&str>> = (0..row.len()).map(|i| row.get(i)).collect();
              sink.push(&texts);
            }
            SimpleQueryMessage::CommandComplete(n) => affected = n,
            _ => {}
          }
        }
        affected
      } else {
        let rows = tx.query_raw(&stmt, no_params()).map_err(err)?;
        stream_text_rows(rows, &mut sink, true).map_err(err)?;
        0
      };
      (affected, Some(sink.finish(columns)))
    };
    tx.commit().map_err(err)?;
    Ok(ExecOutcome {
      rows_affected,
      last_insert_rowid: None,
      returning,
    })
  }

  fn tables(
    &self,
    schema: Option<&str>,
    limits: &Limits,
  ) -> Result<(Vec<TableEntry>, bool), DbError> {
    let mut session = connect(&self.dsn, limits)?;
    let deadline = session.deadline;
    let err =
      |e: postgres::Error| super::errors::from_pg(e, false, std::time::Instant::now() >= deadline);
    let mut tx = begin(&mut session, true, Guards::ALL)?;
    let cap = limits.max_rows as i64 + 1;
    let rows = tx
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
    tx.rollback().map_err(err)?;
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
        ErrorCode::NotFound,
        "TABLE_NOT_FOUND",
        format!("no table or view named {table:?}"),
      )
    };
    let mut session = connect(&self.dsn, limits)?;
    let deadline = session.deadline;
    let err =
      |e: postgres::Error| super::errors::from_pg(e, false, std::time::Instant::now() >= deadline);
    let mut tx = begin(&mut session, true, Guards::ALL)?;
    let found = match schema {
      Some(schema) => tx.query_opt(
        "SELECT c.oid, n.nspname, c.relname, c.relkind::text \
         FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = $1::text AND c.relname = $2::text AND c.relkind IN ('r', 'p', 'v', 'm', 'f')",
        &[&schema, &table],
      ),
      // to_regclass parses the name (schema.table, quoting, search_path) and returns NULL when
      // there is no such relation; an invalid name is an error, reported as not found.
      None => tx.query_opt(
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
    let rows = tx
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
    tx.rollback().map_err(err)?;
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
