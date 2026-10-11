//! The policy seam.
//!
//! Planned configuration items (not implemented): allow/deny lists, a read-only mode,
//! per-connection rules, and a **disk-space cap** for `out` / spill files (today a big result is
//! written in full; see `crate::output`).
//!
//! Open mode (v1) runs any SQL the database accepts: this policy allows everything. It is the
//! single place every request's SQL passes through, so later versions can load limits here
//! (allow/deny lists, read-only enforcement, per-connection rules) from a config file named by
//! `LMSQL_CONFIG` without touching the backends or the op handlers. The earlier guard/refusal
//! implementation is in git history (branch `phase2-postgres`, and at commit 0a3877f).

use crate::error::DbError;

/// The effective policy for a run. v1 holds no rules.
#[derive(Debug, Clone, Default)]
pub struct Policy {
  /// The path from `LMSQL_CONFIG`, remembered for later versions; its contents are not parsed.
  config_path: Option<String>,
}

impl Policy {
  /// Load the policy. In v1 this only records `LMSQL_CONFIG` (if set); nothing is read from it.
  pub fn load() -> Policy {
    Policy {
      config_path: std::env::var("LMSQL_CONFIG").ok().filter(|p| !p.is_empty()),
    }
  }

  /// The configured config path, if any (unused in v1).
  pub fn config_path(&self) -> Option<&str> {
    self.config_path.as_deref()
  }

  /// Decide whether a request's SQL may run. v1 allows every statement.
  pub fn allow(&self, _op: &str, _sql: &str) -> Result<(), DbError> {
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn open_mode_allows_everything() {
    let policy = Policy::load();
    for sql in ["select 1", "drop table t", "attach database 'x' as y", ""] {
      assert!(policy.allow("query", sql).is_ok(), "{sql}");
      assert!(policy.allow("exec", sql).is_ok(), "{sql}");
    }
  }
}
