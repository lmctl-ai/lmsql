//! PostgreSQL DSNs (`postgres://` / `postgresql://` URLs). lmsql handles `sslmode` (all five
//! libpq modes) and `sslrootcert` itself; every other parameter goes to the driver.
//!
//! Safe default: without `sslmode`, a local server (Unix socket, `localhost`, loopback address)
//! uses `prefer`, any other host `verify-full`. Plaintext to a non-local host needs an explicit
//! `sslmode=disable`; `sslmode=prefer` to a non-local host means `require` (no silent plaintext
//! fallback).

use crate::error::DbError;
use lmrust::server::ErrorCode;
use postgres::config::{Host, SslMode};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsMode {
  Disable,
  /// TLS if the server offers it, plaintext otherwise (local servers only).
  Prefer,
  /// TLS, server certificate not verified (libpq `require`).
  Require,
  /// TLS, certificate chain verified, host name not checked.
  VerifyCa,
  /// TLS, certificate chain and host name verified.
  VerifyFull,
}

#[derive(Debug, Clone)]
pub struct PgDsn {
  pub config: postgres::Config,
  pub tls: TlsMode,
  pub root_cert: Option<PathBuf>,
  /// Everything that identifies the server or the account: never echoed.
  pub secrets: Vec<String>,
}

pub fn is_postgres_dsn(value: &str) -> bool {
  let lower = value.trim_start().to_ascii_lowercase();
  lower.starts_with("postgres://") || lower.starts_with("postgresql://")
}

fn invalid(message: &str) -> DbError {
  DbError::new(ErrorCode::ValidationError, "INVALID_CONNECTION", message)
}

/// `%XX` decoding (libpq URIs do not treat `+` as a space).
fn percent_decode(text: &str) -> String {
  let bytes = text.as_bytes();
  let mut out = Vec::with_capacity(bytes.len());
  let mut i = 0;
  while i < bytes.len() {
    if bytes[i] == b'%'
      && i + 2 < bytes.len()
      && bytes[i + 1].is_ascii_hexdigit()
      && bytes[i + 2].is_ascii_hexdigit()
    {
      // Both are ASCII hex digits, so the slice is on char boundaries.
      if let Ok(b) = u8::from_str_radix(&text[i + 1..i + 3], 16) {
        out.push(b);
        i += 3;
        continue;
      }
    }
    out.push(bytes[i]);
    i += 1;
  }
  String::from_utf8_lossy(&out).into_owned()
}

fn is_local(config: &postgres::Config) -> bool {
  let hosts_local = !config.get_hosts().is_empty()
    && config.get_hosts().iter().all(|h| match h {
      Host::Unix(_) => true,
      Host::Tcp(name) => {
        let name = name.trim_start_matches('[').trim_end_matches(']');
        name.eq_ignore_ascii_case("localhost")
          || name
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
      }
    });
  hosts_local && config.get_hostaddrs().iter().all(|ip| ip.is_loopback())
}

/// Parse a DSN. Errors never contain any part of it.
pub fn parse(raw: &str) -> Result<PgDsn, DbError> {
  let raw = raw.trim();
  if !is_postgres_dsn(raw) {
    return Err(invalid("not a postgres:// or postgresql:// DSN"));
  }
  let (base, query) = match raw.split_once('?') {
    Some((b, q)) => (b, q),
    None => (raw, ""),
  };
  let mut sslmode: Option<String> = None;
  let mut root_cert: Option<PathBuf> = None;
  let mut kept: Vec<&str> = Vec::new();
  for pair in query.split('&').filter(|p| !p.is_empty()) {
    let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
    match percent_decode(key).as_str() {
      "sslmode" => sslmode = Some(percent_decode(value)),
      "sslrootcert" => root_cert = Some(PathBuf::from(percent_decode(value))),
      _ => kept.push(pair),
    }
  }
  let rebuilt = if kept.is_empty() {
    base.to_string()
  } else {
    format!("{base}?{}", kept.join("&"))
  };
  let mut config: postgres::Config = rebuilt
    .parse()
    .map_err(|_| invalid("the connection's DSN is not a valid postgres:// URL"))?;
  let local = is_local(&config);
  let tls = match sslmode.as_deref() {
    None if local => TlsMode::Prefer,
    None => TlsMode::VerifyFull,
    Some("disable") => TlsMode::Disable,
    Some("allow") | Some("prefer") if local => TlsMode::Prefer,
    Some("allow") | Some("prefer") => TlsMode::Require,
    Some("require") => TlsMode::Require,
    Some("verify-ca") => TlsMode::VerifyCa,
    Some("verify-full") => TlsMode::VerifyFull,
    Some(_) => {
      return Err(invalid(
        "sslmode must be disable, prefer, require, verify-ca or verify-full",
      ))
    }
  };
  config.ssl_mode(match tls {
    TlsMode::Disable => SslMode::Disable,
    TlsMode::Prefer => SslMode::Prefer,
    TlsMode::Require | TlsMode::VerifyCa | TlsMode::VerifyFull => SslMode::Require,
  });
  config.application_name("lmsql");
  let mut secrets = vec![raw.to_string(), rebuilt.clone()];
  secrets.extend(config.get_user().map(str::to_string));
  secrets.extend(
    config
      .get_password()
      .map(|p| String::from_utf8_lossy(p).into_owned()),
  );
  secrets.extend(config.get_dbname().map(str::to_string));
  for host in config.get_hosts() {
    secrets.push(match host {
      Host::Tcp(name) => name.clone(),
      #[cfg(unix)]
      Host::Unix(path) => path.display().to_string(),
    });
  }
  secrets.extend(config.get_hostaddrs().iter().map(|ip| ip.to_string()));
  // The URL-encoded spellings of the user and password as they appear in the DSN.
  if let Some((userinfo, _)) = base
    .split_once("://")
    .map(|(_, rest)| rest)
    .and_then(|rest| rest.rsplit_once('@'))
  {
    secrets.extend(userinfo.split(':').map(str::to_string));
  }
  secrets.retain(|s| s.trim().len() >= 2);
  secrets.sort();
  secrets.dedup();
  Ok(PgDsn {
    config,
    tls,
    root_cert,
    secrets,
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn safe_tls_defaults() {
    let mode = |dsn: &str| parse(dsn).unwrap().tls;
    assert_eq!(mode("postgres://u:p@localhost/db"), TlsMode::Prefer);
    assert_eq!(mode("postgres://u:p@127.0.0.1:5433/db"), TlsMode::Prefer);
    assert_eq!(mode("postgres://u:p@[::1]/db"), TlsMode::Prefer);
    assert_eq!(
      mode("postgresql://u:p@db.example.com/db"),
      TlsMode::VerifyFull
    );
    assert_eq!(mode("postgres://u:p@10.0.0.5/db"), TlsMode::VerifyFull);
    assert_eq!(
      mode("postgres://u:p@db.example.com/db?sslmode=prefer"),
      TlsMode::Require
    );
    assert_eq!(
      mode("postgres://u:p@localhost/db?sslmode=prefer"),
      TlsMode::Prefer
    );
    assert_eq!(
      mode("postgres://u:p@db.example.com/db?sslmode=disable"),
      TlsMode::Disable
    );
    assert_eq!(
      mode("postgres://u:p@db.example.com/db?sslmode=require"),
      TlsMode::Require
    );
    assert_eq!(
      mode("postgres://u:p@db.example.com/db?sslmode=verify-ca"),
      TlsMode::VerifyCa
    );
    assert_eq!(
      mode("postgres://u:p@db.example.com/db?connect_timeout=3&sslmode=verify-full"),
      TlsMode::VerifyFull
    );
    let d =
      parse("postgres://u:p@h.example/db?sslrootcert=%2Fetc%2Fca.pem&sslmode=verify-ca").unwrap();
    assert_eq!(d.root_cert, Some(PathBuf::from("/etc/ca.pem")));
    assert_eq!(d.config.get_ssl_mode(), SslMode::Require);
    assert_eq!(
      parse("postgres://u@localhost/db?sslmode=disable")
        .unwrap()
        .config
        .get_ssl_mode(),
      SslMode::Disable
    );
  }

  #[test]
  fn errors_never_contain_the_dsn() {
    for dsn in [
      "postgres://user:hunter2@db.example.com/db?sslmode=bogus",
      "postgres://user:hunter2@db.example.com/db?nosuchparam=1",
      "postgres://user:hunter2@db.example.com:notaport/db",
      "mysql://user:hunter2@db.example.com/db",
    ] {
      let err = parse(dsn).unwrap_err();
      assert_eq!(err.tool_code, "INVALID_CONNECTION", "{dsn}");
      for part in ["user", "hunter2", "db.example.com", "bogus"] {
        assert!(!err.message.contains(part), "{dsn}: {}", err.message);
      }
    }
  }

  #[test]
  fn percent_decoding_never_panics() {
    assert_eq!(percent_decode("a%2Fb%zz%é%"), "a/b%zz%é%");
    assert_eq!(percent_decode("%41%42"), "AB");
  }

  #[test]
  fn secrets_cover_user_password_host_and_database() {
    let d = parse("postgres://al%40ice:p%3Ass@db.example.com:5432/sales?sslmode=require").unwrap();
    for s in [
      "al@ice",
      "p:ss",
      "db.example.com",
      "sales",
      "al%40ice",
      "p%3Ass",
    ] {
      assert!(
        d.secrets.iter().any(|x| x == s),
        "{s} not in {:?}",
        d.secrets
      );
    }
  }
}
