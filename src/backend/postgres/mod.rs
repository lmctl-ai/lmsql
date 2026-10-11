//! PostgreSQL backend (sync `postgres` driver, rustls TLS), open mode.
//!
//! lmsql runs whatever SQL it is given over an autocommit session: no keyword allowlist, no
//! READ ONLY transaction, no transaction-control refusal. A read-only database role is the way to
//! restrict what a connection can do; the earlier in-tool guards are in git history (branch
//! `phase2-postgres`). Still enforced as operational caps: `max_rows`/`max_bytes` (a single
//! row-returning statement is read through a cursor so a huge result stops fast) and one deadline
//! per call (`connect_timeout`, `statement_timeout`, `lock_timeout`, plus a watchdog that sends
//! the server a cancel request when the deadline passes).

pub mod dsn;
pub mod errors;
pub mod tls;
pub mod values;

mod ops;

use crate::error::DbError;
use crate::values::Limits;
use postgres::{Client, NoTls};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tokio_postgres_rustls::MakeRustlsConnect;

pub struct Postgres {
  dsn: String,
  /// Settings level `readonly`: every transaction of the session starts READ ONLY.
  read_only: bool,
}

impl Postgres {
  pub fn new(dsn: String) -> Self {
    Postgres {
      dsn,
      read_only: false,
    }
  }

  /// A session whose transactions are all READ ONLY (`default_transaction_read_only`). SQL can
  /// change that setting again, so this is weaker than a read-only database role.
  pub fn read_only(dsn: String) -> Self {
    Postgres {
      dsn,
      read_only: true,
    }
  }
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

  /// Map a driver error from this session (after connecting).
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

  /// Pin the session's timeouts (from the remaining time) and output formats so values parse the
  /// same on every server. Session-level `SET` (not `SET LOCAL`): the process runs one call.
  pub fn apply_settings(&mut self, read_only: bool) -> Result<(), DbError> {
    let ms = self.remaining_ms();
    let mut setup = format!(
      "SET statement_timeout = {ms}; SET lock_timeout = {ms}; \
       SET idle_in_transaction_session_timeout = {ms}; \
       SET TimeZone = 'UTC'; SET DateStyle = 'ISO, YMD'; SET IntervalStyle = 'iso_8601'; \
       SET bytea_output = 'hex'; SET extra_float_digits = '3'"
    );
    if read_only {
      setup.push_str("; SET default_transaction_read_only = on");
    }
    let passed = self.deadline_passed();
    self
      .client
      .batch_execute(&setup)
      .map_err(|e| errors::from_pg(e, false, passed))
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
