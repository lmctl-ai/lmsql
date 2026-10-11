//! The operations on PostgreSQL, open mode: run any SQL over an autocommit session.
//!
//! A single row-returning statement is read through a cursor so a huge result stops at the cap
//! without materializing. Anything else (writes, DDL, `RETURNING`, utility statements, or
//! multi-statement text) runs over the simple query protocol; its last statement's rows are
//! returned, capped. Values come back in PostgreSQL's text format and are typed by the column's
//! type where lmsql knows it (always on the cursor path; by the prepared statement on the
//! RETURNING fallback; by name only for free-form multi-statement text).

use super::values::{min_json_len, to_json};
use super::{connect, Postgres};
use crate::backend::{Backend, Column, ColumnInfo, Rows, RunOutcome, TableDescription, TableEntry};
use crate::error::DbError;
use crate::values::Limits;
use postgres::{SimpleQueryMessage, Transaction};
use serde_json::Value;

const CURSOR: &str = "lmsql_cursor";

/// Cursor batches grow from 8 rows to this: a cap is overshot by at most one batch.
const MAX_BATCH: usize = 256;

/// Collects text rows under the caps; `push` returns false when the caller should stop.
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
    for (i, t) in texts.iter().enumerate() {
      lower += min_json_len(self.type_at(i), *t);
    }
    if self.bytes + lower + 1 > self.limits.max_bytes {
      self.truncated_by = Some("max_bytes");
      return false;
    }
    let values: Vec<Value> = texts
      .iter()
      .enumerate()
      .map(|(i, t)| to_json(self.type_at(i), *t))
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

  fn type_at(&self, i: usize) -> &str {
    self.types.get(i).map(String::as_str).unwrap_or("text")
  }
}

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

/// Read a cursor in growing batches over the simple protocol (text values).
fn fetch_cursor(tx: &mut Transaction<'_>, sink: &mut Sink<'_>) -> Result<(), postgres::Error> {
  let mut batch = 8usize;
  loop {
    let messages = tx.simple_query(&format!("FETCH FORWARD {batch} FROM {CURSOR}"))?;
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
    if got < batch {
      return Ok(());
    }
    batch = (batch * 2).min(MAX_BATCH);
  }
}

/// The last statement's rows and the last command tag, from a simple-query batch. `types` is the
/// prepared column types when known (the single-RETURNING fallback); otherwise types come by name.
fn last_result(
  messages: &[SimpleQueryMessage],
  types: Option<&[String]>,
  limits: &Limits,
) -> (Rows, u64) {
  let mut best: Option<Rows> = None;
  let mut cur_cols: Vec<Column> = Vec::new();
  let mut sink: Option<Sink> = None;
  let mut affected = 0u64;
  for message in messages {
    match message {
      SimpleQueryMessage::RowDescription(desc) => {
        cur_cols = desc
          .iter()
          .map(|c| Column {
            name: c.name().to_string(),
            decl_type: None,
          })
          .collect();
        let cur_types: Vec<String> = match types {
          Some(t) => t.to_vec(),
          None => vec!["text".to_string(); desc.len()],
        };
        if let Some(t) = types {
          for (col, name) in cur_cols.iter_mut().zip(t) {
            col.decl_type = Some(name.clone());
          }
        }
        sink = Some(Sink::new(cur_types, limits));
      }
      SimpleQueryMessage::Row(row) => {
        if let Some(s) = sink.as_mut() {
          let texts: Vec<Option<&str>> = (0..row.len()).map(|i| row.get(i)).collect();
          s.push(&texts);
        }
      }
      SimpleQueryMessage::CommandComplete(n) => {
        affected = *n;
        if let Some(s) = sink.take() {
          best = Some(Rows {
            columns: cur_cols.clone(),
            rows: s.rows,
            truncated_by: s.truncated_by,
          });
        }
      }
      _ => {}
    }
  }
  let rows = best.unwrap_or(Rows {
    columns: Vec::new(),
    rows: Vec::new(),
    truncated_by: None,
  });
  // `affected` is the last statement's command tag: the row count for INSERT/UPDATE/DELETE
  // (including RETURNING), or the selected count for a trailing SELECT in multi-statement text.
  (rows, affected)
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
  fn run(&self, sql: &str, limits: &Limits) -> Result<RunOutcome, DbError> {
    let mut session = connect(&self.dsn, limits)?;
    session.apply_settings()?;
    let deadline = session.deadline;
    let err =
      |e: postgres::Error| super::errors::from_pg(e, false, std::time::Instant::now() >= deadline);

    // One statement the extended protocol accepts: prepare tells us its columns and types.
    match session.client.prepare(sql) {
      Ok(stmt) if stmt.columns().is_empty() => {
        let affected = session.client.execute(&stmt, &[]).map_err(err)?;
        Ok(RunOutcome {
          rows: Rows {
            columns: Vec::new(),
            rows: Vec::new(),
            truncated_by: None,
          },
          rows_affected: affected,
          last_insert_rowid: None,
        })
      }
      Ok(stmt) => {
        let (cols, types) = columns(&stmt);
        // Try a cursor so a huge result stops at the cap; a data-modifying statement
        // (INSERT ... RETURNING, WITH ... INSERT) cannot be a cursor, so fall back.
        let mut tx = session.client.transaction().map_err(err)?;
        let declared = tx.batch_execute(&format!(
          "DECLARE {CURSOR} NO SCROLL CURSOR WITHOUT HOLD FOR {sql}"
        ));
        match declared {
          Ok(()) => {
            let mut sink = Sink::new(types, limits);
            let fetch = fetch_cursor(&mut tx, &mut sink);
            let _ = tx.rollback();
            fetch.map_err(err)?;
            Ok(RunOutcome {
              rows: Rows {
                columns: cols,
                rows: sink.rows,
                truncated_by: sink.truncated_by,
              },
              rows_affected: 0,
              last_insert_rowid: None,
            })
          }
          Err(_) => {
            let _ = tx.rollback();
            // RETURNING or a data-modifying CTE: run it once, typed by the prepared columns.
            let messages = session.client.simple_query(sql).map_err(err)?;
            let (mut rows, affected) = last_result(&messages, Some(&types), limits);
            rows.columns = cols;
            Ok(RunOutcome {
              rows,
              rows_affected: affected,
              last_insert_rowid: None,
            })
          }
        }
      }
      // Multi-statement text or a utility statement the extended protocol rejects: run the whole
      // text over the simple protocol (autocommit, unless the text has its own BEGIN/COMMIT).
      Err(_) => {
        let messages = session.client.simple_query(sql).map_err(err)?;
        let (rows, affected) = last_result(&messages, None, limits);
        Ok(RunOutcome {
          rows,
          rows_affected: affected,
          last_insert_rowid: None,
        })
      }
    }
  }

  fn tables(
    &self,
    schema: Option<&str>,
    limits: &Limits,
  ) -> Result<(Vec<TableEntry>, bool), DbError> {
    let mut session = connect(&self.dsn, limits)?;
    session.apply_settings()?;
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
    session.apply_settings()?;
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
