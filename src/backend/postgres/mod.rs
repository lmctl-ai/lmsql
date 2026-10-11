//! PostgreSQL backend (sync `postgres` driver, rustls TLS).
//!
//! PostgreSQL has no per-statement read-only flag, so `query`/`tables`/`describe` are protected by
//! layers, each tested on its own (`durable-memory/postgres.md`):
//!
//! 1. **Statement rules** ([`classify`]): exactly one statement, balanced parentheses, and for
//!    `query` an allowlist of read statements, no write keywords, no side-effect functions.
//! 2. **A READ ONLY transaction**, locked in by a first query before the user's statement (so a
//!    later `SET TRANSACTION READ WRITE` is refused), always rolled back.
//! 3. **The server's own checks**: the extended protocol refuses a second statement, `DECLARE
//!    CURSOR` accepts only SELECT/VALUES, and the role's privileges (a read-only role is the real
//!    safeguard; README).
//!
//! Time: one deadline per call. `connect_timeout`, `statement_timeout`, `lock_timeout` and
//! `idle_in_transaction_session_timeout` come from the remaining time, and a watchdog thread
//! sends the server a cancel request when the deadline passes.

pub mod classify;
pub mod dsn;
pub mod errors;
pub mod tls;
pub mod values;

mod ops;

use crate::error::DbError;
use crate::values::Limits;
use postgres::{Client, NoTls, Transaction};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tokio_postgres_rustls::MakeRustlsConnect;

pub struct Postgres {
  dsn: String,
}

impl Postgres {
  pub fn new(dsn: String) -> Self {
    Postgres { dsn }
  }
}

/// Which layers run. Production always uses [`Guards::ALL`]; tests switch one off to show which
/// layer stops an attack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Guards {
  pub statement_rules: bool,
  pub read_only_txn: bool,
}

impl Guards {
  pub const ALL: Guards = Guards {
    statement_rules: true,
    read_only_txn: true,
  };
}

/// A connected session with its deadline; dropping it stops the watchdog.
pub struct Session {
  pub client: Client,
  pub deadline: Instant,
  watchdog: Option<mpsc::Sender<()>>,
}

impl Drop for Session {
  fn drop(&mut self) {
    if let Some(done) = self.watchdog.take() {
      let _ = done.send(());
    }
  }
}

impl Session {
  pub fn deadline_passed(&self) -> bool {
    Instant::now() >= self.deadline
  }

  /// Map a driver error from this session.
  pub fn err(&self, error: postgres::Error) -> DbError {
    errors::from_pg(error, false, self.deadline_passed())
  }

  /// Milliseconds left before the deadline (at least 1).
  pub fn remaining_ms(&self) -> u128 {
    self
      .deadline
      .saturating_duration_since(Instant::now())
      .as_millis()
      .max(1)
  }
}

/// Connect with the DSN's TLS policy; the deadline covers the connection attempt.
pub fn connect(dsn: &str, limits: &Limits) -> Result<Session, DbError> {
  let deadline = Instant::now() + limits.timeout;
  let parsed = dsn::parse(dsn)?;
  let connector = tls::connector(parsed.tls, parsed.root_cert.as_deref())?;
  let mut config = parsed.config.clone();
  config.connect_timeout(limits.timeout.max(Duration::from_millis(1)));
  let connected = match connector.clone() {
    Some(tls) => config.connect(tls),
    None => config.connect(NoTls),
  };
  let client = connected.map_err(|e| errors::from_pg(e, true, Instant::now() >= deadline))?;
  let watchdog = start_watchdog(&client, connector, deadline);
  Ok(Session {
    client,
    deadline,
    watchdog: Some(watchdog),
  })
}

/// At the deadline, ask the server to cancel whatever the session is running.
fn start_watchdog(
  client: &Client,
  connector: Option<MakeRustlsConnect>,
  deadline: Instant,
) -> mpsc::Sender<()> {
  let token = client.cancel_token();
  let (done, finished) = mpsc::channel::<()>();
  std::thread::spawn(move || {
    let wait = deadline.saturating_duration_since(Instant::now());
    if finished.recv_timeout(wait) == Err(mpsc::RecvTimeoutError::Timeout) {
      let _ = match connector {
        Some(tls) => token.cancel_query(tls),
        None => token.cancel_query(NoTls),
      };
    }
  });
  done
}

/// Start the call's transaction. Read calls are READ ONLY (unless a test switches layer 2 off);
/// the settings query right after BEGIN is the transaction's first query, so its read-only mode
/// can no longer be changed. Timeouts come from the remaining time; output formats are pinned so
/// values parse the same on every server.
pub fn begin<'a>(
  session: &'a mut Session,
  read: bool,
  guards: Guards,
) -> Result<Transaction<'a>, DbError> {
  let read_only = read && guards.read_only_txn;
  let ms = session.remaining_ms();
  let deadline_passed = session.deadline_passed();
  let mut tx = session
    .client
    .build_transaction()
    .read_only(read_only)
    .start()
    .map_err(|e| errors::from_pg(e, false, deadline_passed))?;
  let mut setup = String::new();
  if read_only {
    setup.push_str("SET TRANSACTION READ ONLY; ");
  }
  setup.push_str(&format!(
    "SELECT pg_catalog.set_config('statement_timeout', '{ms}', true), \
     pg_catalog.set_config('lock_timeout', '{ms}', true), \
     pg_catalog.set_config('idle_in_transaction_session_timeout', '{ms}', true), \
     pg_catalog.set_config('TimeZone', 'UTC', true), \
     pg_catalog.set_config('DateStyle', 'ISO, YMD', true), \
     pg_catalog.set_config('IntervalStyle', 'iso_8601', true), \
     pg_catalog.set_config('bytea_output', 'hex', true), \
     pg_catalog.set_config('extra_float_digits', '3', true)"
  ));
  tx.batch_execute(&setup)
    .map_err(|e| errors::from_pg(e, false, deadline_passed))?;
  Ok(tx)
}
