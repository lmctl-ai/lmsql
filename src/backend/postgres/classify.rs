//! A PostgreSQL-aware tokenizer and the statement rules built on it (layer 1 of the Postgres
//! safety model; the server's READ ONLY transaction is layer 2).
//!
//! The tokenizer knows what PostgreSQL treats as one token: `--` and nested `/* */` comments,
//! `'...'` strings (`''` escape), `E'...'` strings (backslash escapes), `U&'...'`/`B'...'`/`X'...'`,
//! dollar quotes (`$$...$$`, `$tag$...$tag$`), quoted identifiers (`"..."`, `""` escape) and
//! `$1` parameters. A `;` inside any of them is not a separator.

use crate::error::DbError;
use lmrust::server::ErrorCode;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tok {
  /// An identifier or keyword: lower-cased unless quoted (quoted names keep their case).
  Word {
    text: String,
    quoted: bool,
  },
  Str,
  Num,
  Param,
  Punct(char),
}

/// The shape of a statement string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shape {
  /// Tokens with their byte offsets (start, end).
  pub tokens: Vec<(Tok, usize, usize)>,
  /// Non-empty statements separated by top-level `;`.
  pub statements: usize,
  /// Parentheses balance and never close more than they opened.
  pub balanced: bool,
  /// A string, quoted identifier, dollar quote or block comment is not closed.
  pub unterminated: bool,
}

impl Shape {
  /// Unquoted words in order, with whether `(` follows (a function call).
  pub fn words(&self) -> impl Iterator<Item = (usize, &str, bool)> {
    self
      .tokens
      .iter()
      .enumerate()
      .filter_map(move |(i, (t, _, _))| match t {
        Tok::Word {
          text,
          quoted: false,
        } => {
          let call = matches!(self.tokens.get(i + 1), Some((Tok::Punct('('), _, _)));
          Some((i, text.as_str(), call))
        }
        _ => None,
      })
  }

  /// The first unquoted word (after any opening parentheses), lower-cased.
  pub fn first_word(&self) -> Option<&str> {
    self
      .tokens
      .iter()
      .find_map(|(t, _, _)| match t {
        Tok::Word {
          text,
          quoted: false,
        } => Some(text.as_str()),
        Tok::Punct('(') => None,
        _ => Some(""),
      })
      .filter(|w| !w.is_empty())
  }

  /// Byte span of the statement text without a trailing `;` and comments.
  pub fn body(&self) -> (usize, usize) {
    let mut toks = self.tokens.iter().filter(|(t, _, _)| *t != Tok::Punct(';'));
    let start = toks.next().map(|(_, s, _)| *s).unwrap_or(0);
    let end = self
      .tokens
      .iter()
      .rev()
      .find(|(t, _, _)| *t != Tok::Punct(';'))
      .map(|(_, _, e)| *e)
      .unwrap_or(0);
    (start, end.max(start))
  }
}

fn ident_start(c: char) -> bool {
  c.is_alphabetic() || c == '_' || !c.is_ascii()
}

fn ident_continue(c: char) -> bool {
  c.is_alphanumeric() || c == '_' || c == '$' || !c.is_ascii()
}

/// Tokenize `sql`.
pub fn shape(sql: &str) -> Shape {
  let chars: Vec<(usize, char)> = sql.char_indices().collect();
  let at = |i: usize| chars.get(i).map(|(_, c)| *c);
  let off = |i: usize| chars.get(i).map(|(o, _)| *o).unwrap_or(sql.len());
  let mut tokens = Vec::new();
  let mut unterminated = false;
  let mut depth: i64 = 0;
  let mut balanced = true;
  let mut i = 0;
  // A quoted string starting at `i` (the opening quote); returns the index after it.
  let string_end = |mut i: usize, backslash: bool, unterminated: &mut bool| -> usize {
    i += 1;
    loop {
      match at(i) {
        None => {
          *unterminated = true;
          return i;
        }
        Some('\\') if backslash => i += 2,
        Some('\'') if at(i + 1) == Some('\'') => i += 2,
        Some('\'') => return i + 1,
        Some(_) => i += 1,
      }
    }
  };
  while let Some(c) = at(i) {
    let start = i;
    if c.is_whitespace() {
      i += 1;
      continue;
    }
    if c == '-' && at(i + 1) == Some('-') {
      while at(i).is_some_and(|c| c != '\n') {
        i += 1;
      }
      continue;
    }
    if c == '/' && at(i + 1) == Some('*') {
      let mut nest = 0;
      loop {
        match (at(i), at(i + 1)) {
          (None, _) => {
            unterminated = true;
            break;
          }
          (Some('/'), Some('*')) => {
            nest += 1;
            i += 2;
          }
          (Some('*'), Some('/')) => {
            nest -= 1;
            i += 2;
            if nest == 0 {
              break;
            }
          }
          _ => i += 1,
        }
      }
      continue;
    }
    if c == '\'' {
      i = string_end(i, false, &mut unterminated);
      tokens.push((Tok::Str, off(start), off(i)));
      continue;
    }
    if c == '"' {
      let mut text = String::new();
      i += 1;
      loop {
        match at(i) {
          None => {
            unterminated = true;
            break;
          }
          Some('"') if at(i + 1) == Some('"') => {
            text.push('"');
            i += 2;
          }
          Some('"') => {
            i += 1;
            break;
          }
          Some(c) => {
            text.push(c);
            i += 1;
          }
        }
      }
      tokens.push((Tok::Word { text, quoted: true }, off(start), off(i)));
      continue;
    }
    if c == '$' {
      // `$1` parameter, or `$tag$ ... $tag$` dollar quote.
      if at(i + 1).is_some_and(|c| c.is_ascii_digit()) {
        i += 1;
        while at(i).is_some_and(|c| c.is_ascii_digit()) {
          i += 1;
        }
        tokens.push((Tok::Param, off(start), off(i)));
        continue;
      }
      let mut j = i + 1;
      while at(j).is_some_and(|c| {
        if j == i + 1 {
          ident_start(c)
        } else {
          ident_continue(c) && c != '$'
        }
      }) {
        j += 1;
      }
      if at(j) == Some('$') {
        let tag: String = chars[i..=j].iter().map(|(_, c)| *c).collect();
        let tag_len = j - i + 1;
        let mut k = j + 1;
        loop {
          if k >= chars.len() {
            unterminated = true;
            i = chars.len();
            break;
          }
          let candidate: String = chars[k..(k + tag_len).min(chars.len())]
            .iter()
            .map(|(_, c)| *c)
            .collect();
          if candidate == tag {
            i = k + tag_len;
            break;
          }
          k += 1;
        }
        tokens.push((Tok::Str, off(start), off(i)));
        continue;
      }
      tokens.push((Tok::Punct('$'), off(start), off(i + 1)));
      i += 1;
      continue;
    }
    if ident_start(c) {
      // String prefixes: E'..' (escapes), U&'..', B'..', X'..', N'..'.
      let next = at(i + 1);
      if matches!(c, 'e' | 'E') && next == Some('\'') {
        i = string_end(i + 1, true, &mut unterminated);
        tokens.push((Tok::Str, off(start), off(i)));
        continue;
      }
      if matches!(c, 'b' | 'B' | 'x' | 'X' | 'n' | 'N') && next == Some('\'') {
        i = string_end(i + 1, false, &mut unterminated);
        tokens.push((Tok::Str, off(start), off(i)));
        continue;
      }
      if matches!(c, 'u' | 'U') && next == Some('&') && matches!(at(i + 2), Some('\'') | Some('"'))
      {
        if at(i + 2) == Some('\'') {
          i = string_end(i + 2, false, &mut unterminated);
          tokens.push((Tok::Str, off(start), off(i)));
        } else {
          i += 2; // the quoted identifier follows as its own token
        }
        continue;
      }
      while at(i).is_some_and(ident_continue) {
        i += 1;
      }
      let text = sql[off(start)..off(i)].to_lowercase();
      tokens.push((
        Tok::Word {
          text,
          quoted: false,
        },
        off(start),
        off(i),
      ));
      continue;
    }
    if c.is_ascii_digit() || (c == '.' && at(i + 1).is_some_and(|c| c.is_ascii_digit())) {
      while at(i).is_some_and(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_') {
        i += 1;
      }
      tokens.push((Tok::Num, off(start), off(i)));
      continue;
    }
    match c {
      '(' => depth += 1,
      ')' => {
        depth -= 1;
        if depth < 0 {
          balanced = false;
        }
      }
      _ => {}
    }
    tokens.push((Tok::Punct(c), off(start), off(i + 1)));
    i += 1;
  }
  if depth != 0 {
    balanced = false;
  }
  let mut statements = 0;
  let mut in_statement = false;
  for (t, _, _) in &tokens {
    if *t == Tok::Punct(';') {
      in_statement = false;
    } else if !in_statement {
      in_statement = true;
      statements += 1;
    }
  }
  Shape {
    tokens,
    statements,
    balanced,
    unterminated,
  }
}

fn invalid(tool: &str, message: impl Into<String>) -> DbError {
  DbError::new(ErrorCode::ValidationError, tool, message)
}

/// Exactly one complete statement (both ops). The server's extended protocol refuses a second
/// statement too; this check runs first and also catches what the protocol would not see
/// (unbalanced parentheses that could escape lmsql's own wrapping).
pub fn single_statement(sql: &str) -> Result<Shape, DbError> {
  if sql.contains('\0') {
    return Err(invalid(
      "NUL_IN_STATEMENT",
      "the statement contains a NUL byte",
    ));
  }
  let shape = shape(sql);
  if shape.unterminated {
    return Err(invalid(
      "UNTERMINATED_TOKEN",
      "a string, quoted identifier, dollar quote or comment is not closed",
    ));
  }
  match shape.statements {
    0 => Err(invalid(
      "EMPTY_STATEMENT",
      "the statement is empty (only whitespace or comments)",
    )),
    1 if !shape.balanced => Err(invalid(
      "UNBALANCED_PARENTHESES",
      "the statement's parentheses do not balance",
    )),
    1 => Ok(shape),
    _ => Err(invalid(
      "TOO_MANY_STATEMENTS",
      "exactly one SQL statement per call; remove everything after the first statement",
    )),
  }
}

/// Statements `query` may run (first keyword).
pub const QUERY_FIRST_WORDS: [&str; 6] = ["select", "with", "values", "table", "explain", "show"];

/// Words that make a statement a write wherever they appear (unquoted). Conservative: an
/// unquoted column named `update` is refused too (quote it).
pub const WRITE_WORDS: [&str; 5] = ["insert", "update", "delete", "merge", "into"];

/// Functions with effects outside the data a READ ONLY transaction protects (server files,
/// other sessions, WAL, locks, notifications, settings, nested SQL that escapes this check).
pub const DENIED_FUNCTIONS: [&str; 17] = [
  "set_config",
  "pg_notify",
  "pg_terminate_backend",
  "pg_cancel_backend",
  "pg_reload_conf",
  "pg_rotate_logfile",
  "pg_promote",
  "pg_stat_file",
  "pg_stat_reset",
  "pg_stat_statements_reset",
  "pg_import_system_collations",
  "pg_log_backend_memory_contexts",
  "query_to_xml",
  "query_to_xml_and_xmlschema",
  "query_to_xmlschema",
  "nextval",
  "setval",
];

/// Function-name prefixes denied for `query` (families).
pub const DENIED_FUNCTION_PREFIXES: [&str; 19] = [
  "dblink",
  "pg_advisory_",
  "pg_try_advisory_",
  "pg_replication_",
  "pg_logical_",
  "pg_create_",
  "pg_drop_",
  "pg_copy_",
  "pg_stat_reset_",
  "pg_wal_replay_",
  "pg_switch_",
  "pg_backup_",
  "pg_start_backup",
  "pg_stop_backup",
  "pg_read_",
  "pg_ls_",
  "pg_file_",
  "lo_",
  "cursor_to_xml",
];

fn not_read_only(message: String) -> DbError {
  DbError::new(ErrorCode::Forbidden, "NOT_READ_ONLY", message)
}

/// Layer 1 for `query`: a conservative allowlist of read statements.
pub fn check_read_only(shape: &Shape) -> Result<(), DbError> {
  let first = shape.first_word().unwrap_or_default();
  if !QUERY_FIRST_WORDS.contains(&first) {
    return Err(not_read_only(format!(
      "query runs read statements only (SELECT, WITH ... SELECT, VALUES, TABLE, SHOW, EXPLAIN); \
       {} is not one; use exec to change data",
      first.to_uppercase()
    )));
  }
  let words: Vec<(usize, &str, bool)> = shape.words().collect();
  if first == "explain" {
    // EXPLAIN ANALYZE executes the statement.
    if words
      .iter()
      .any(|(_, w, _)| *w == "analyze" || *w == "analyse")
    {
      return Err(not_read_only(
        "EXPLAIN ANALYZE executes its statement; query allows plain EXPLAIN only".into(),
      ));
    }
  }
  for (_, word, call) in &words {
    if WRITE_WORDS.contains(word) {
      return Err(not_read_only(format!(
        "{} changes data; use exec (quote an identifier that is named {word})",
        word.to_uppercase()
      )));
    }
    if *call && denied_function(word) {
      return Err(not_read_only(format!(
        "{word}() has effects beyond reading and is not allowed in query"
      )));
    }
  }
  // Schema-qualified calls (`pg_catalog.pg_read_file(...)`) are words followed by `(` too; quoted
  // function names are checked case-sensitively as written.
  for (i, (tok, _, _)) in shape.tokens.iter().enumerate() {
    if let Tok::Word { text, quoted: true } = tok {
      let call = matches!(shape.tokens.get(i + 1), Some((Tok::Punct('('), _, _)));
      if call && denied_function(text) {
        return Err(not_read_only(format!(
          "{text}() has effects beyond reading and is not allowed in query"
        )));
      }
    }
  }
  Ok(())
}

fn denied_function(name: &str) -> bool {
  DENIED_FUNCTIONS.contains(&name) || DENIED_FUNCTION_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// Transaction control `exec` refuses: lmsql owns the transaction (COMMIT only on success).
pub fn check_exec(shape: &Shape) -> Result<(), DbError> {
  let first = shape.first_word().unwrap_or_default();
  let second = shape.words().nth(1).map(|(_, w, _)| w).unwrap_or_default();
  let control = matches!(
    first,
    "begin" | "start" | "commit" | "end" | "rollback" | "abort" | "savepoint" | "release"
  ) || (first == "prepare" && second == "transaction");
  if control {
    return Err(DbError::new(
      ErrorCode::Forbidden,
      "TRANSACTION_CONTROL_REFUSED",
      format!(
        "{} is transaction control; lmsql runs each exec in its own transaction and commits it on success",
        first.to_uppercase()
      ),
    ));
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn count(sql: &str) -> usize {
    shape(sql).statements
  }

  #[test]
  fn statements_are_counted_outside_strings_comments_and_quotes() {
    for (sql, n) in [
      ("select 1", 1),
      ("select 1;", 1),
      ("select 1; select 2", 2),
      ("select 1;;", 1),
      (";", 0),
      ("", 0),
      ("-- only\n", 0),
      ("/* a /* nested */ still */", 0),
      ("select ';'", 1),
      ("select 'it''s; fine'", 1),
      ("select E'\\'; still a string'", 1),
      ("select $$ ; $$", 1),
      ("select $tag$ ; $notit$ ; $tag$", 1),
      ("select \"a;b\" from t", 1),
      ("select 1 -- ; select 2", 1),
      ("select 1 /* ; */", 1),
      ("select 1 /* ; */; select 2", 2),
      ("select U&'d\\0061t;a'", 1),
      ("select $1; select $2", 2),
      ("select x'0A' ; select b'1'", 2),
    ] {
      assert_eq!(count(sql), n, "{sql:?}");
    }
  }

  #[test]
  fn single_statement_errors() {
    let code = |sql: &str| single_statement(sql).unwrap_err().tool_code;
    assert_eq!(code(""), "EMPTY_STATEMENT");
    assert_eq!(code("  -- c\n /* d */ "), "EMPTY_STATEMENT");
    assert_eq!(code("select 1; select 2"), "TOO_MANY_STATEMENTS");
    assert_eq!(code("select 1; drop table t"), "TOO_MANY_STATEMENTS");
    assert_eq!(code("select 'unterminated"), "UNTERMINATED_TOKEN");
    assert_eq!(code("select $$ open"), "UNTERMINATED_TOKEN");
    assert_eq!(code("select /* open"), "UNTERMINATED_TOKEN");
    assert_eq!(code("select \"open"), "UNTERMINATED_TOKEN");
    assert_eq!(code("select (1"), "UNBALANCED_PARENTHESES");
    assert_eq!(
      code("select 1) , x as (delete from t"),
      "UNBALANCED_PARENTHESES"
    );
    assert_eq!(code("select 1\0"), "NUL_IN_STATEMENT");
    assert!(single_statement("select 1; -- done").is_ok());
  }

  #[test]
  fn body_drops_trailing_semicolon_and_comments() {
    let sql = "  select 1 ; -- done";
    let s = shape(sql);
    let (a, b) = s.body();
    assert_eq!(&sql[a..b], "select 1");
    let sql = "(select 1)";
    let (a, b) = shape(sql).body();
    assert_eq!(&sql[a..b], "(select 1)");
  }

  #[test]
  fn query_allowlist() {
    let ok = |sql: &str| check_read_only(&shape(sql)).is_ok();
    for sql in [
      "select 1",
      "SELECT * FROM t WHERE note = 'insert into x'",
      "with x as (select 1) select * from x",
      "values (1), (2)",
      "table t",
      "show search_path",
      "explain select 1",
      "explain (costs off) select 1",
      "(select 1) union (select 2)",
      "select \"update\" from t",
      "select pg_sleep(0.01)",
      "select count(*) from pg_catalog.pg_class",
      "select $$delete from t$$",
    ] {
      assert!(ok(sql), "{sql}");
    }
    for sql in [
      "insert into t values (1)",
      "update t set a = 1",
      "delete from t",
      "merge into t using s on true when matched then delete",
      "with x as (insert into t values (1) returning 1) select * from x",
      "with x as (delete from t returning 1) select 1",
      "select * from t for update",
      "select * from t for no key update",
      "select 1 into newtab",
      "explain analyze select 1",
      "explain (analyze) delete from t",
      "explain (analyse true) select 1",
      "copy (select 1) to program 'id'",
      "copy t to '/tmp/x'",
      "do $$ begin end $$",
      "call p()",
      "listen ch",
      "notify ch",
      "set search_path = evil",
      "set role postgres",
      "set session authorization postgres",
      "reset role",
      "set transaction read write",
      "begin",
      "commit",
      "rollback",
      "end",
      "savepoint a",
      "release a",
      "prepare p as select 1",
      "execute p",
      "deallocate p",
      "vacuum",
      "analyze",
      "create temp table x(a int)",
      "lock table t",
      "select set_config('transaction_read_only', 'off', true)",
      "select pg_catalog.set_config('search_path', 'evil', false)",
      "select \"set_config\"('a', 'b', true)",
      "select nextval('s')",
      "select setval('s', 1)",
      "select pg_advisory_lock(1)",
      "select pg_try_advisory_xact_lock(1)",
      "select lo_import('/etc/passwd')",
      "select lo_export(1, '/tmp/x')",
      "select pg_read_file('/etc/passwd')",
      "select pg_ls_dir('/')",
      "select pg_notify('c', 'x')",
      "select * from dblink('host=x', 'select 1') as t(a int)",
      "select dblink_exec('host=x', 'delete from t')",
      "select pg_terminate_backend(1)",
      "select pg_logical_emit_message(true, 'p', 'x')",
      "select query_to_xml('select pg_terminate_backend(1)', true, true, '')",
      "select pg_switch_wal()",
    ] {
      assert!(!ok(sql), "{sql}");
    }
  }

  #[test]
  fn exec_refuses_transaction_control_only() {
    let refused = |sql: &str| check_exec(&shape(sql)).is_err();
    for sql in [
      "begin",
      "BEGIN TRANSACTION",
      "start transaction",
      "commit",
      "end",
      "rollback",
      "abort",
      "savepoint s",
      "release savepoint s",
      "prepare transaction 'x'",
      "commit prepared 'x'",
      "rollback prepared 'x'",
    ] {
      assert!(refused(sql), "{sql}");
    }
    for sql in [
      "insert into t values (1)",
      "prepare p as select 1",
      "create table x(a int)",
      "set search_path = public",
    ] {
      assert!(!refused(sql), "{sql}");
    }
  }
}
