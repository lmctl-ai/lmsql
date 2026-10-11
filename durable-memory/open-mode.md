# Open mode (v1) — the current design

Owner decision (Mike Ma): the first working lmsql is a wide-open SQL tool. It runs any statement
the database accepts; usage limits come later as configuration. It is the owner's tool on the
owner's databases, so the earlier in-tool guards are postponed, not abandoned.

## What this means

- `query` and `exec` are the same operation in two names: both run reads, writes and DDL and
  return one shape (inline rows, or a file summary for a big result; `big-results.md`). lmapi
  access is `dynamic`, so no `--confirm`.
- SQLite opens read-write and creates the file if missing; multiple statements run
  (`execute_batch`), and the last statement's rows are returned.
- PostgreSQL runs autocommit over the simple protocol, except a single row-returning statement
  goes through a server cursor so the row cap can stop a huge result without materializing.
- No read-only enforcement, keyword allowlist, single-statement rule, ATTACH/pragma/size limit
  or transaction-control refusal. **Restrict access with the database**: a read-only PostgreSQL
  role (README has the SQL), or SQLite file permissions.

## The seam for later

`src/policy.rs` holds `Policy`. Every request passes through `Policy::allow(op, sql)`; v1 returns
`Ok(())` for everything and records `LMSQL_CONFIG` without reading it. Future limits (allow/deny
lists, read-only mode, per-connection rules loaded from a config file) belong here, so the
backends and op handlers do not change.

## Where the guards went

The phase-1/phase-2 hardening (read-only open, `query_only`, `sqlite3_stmt_readonly`, the SQLite
authorizer and size/heap limits, the PostgreSQL READ ONLY transaction, the statement classifier
and the `single_statement` rule) is in git history on branch `phase2-postgres` and at commit
`0a3877f`. `safety.md` and `phase2.md` describe that design and are kept for when it returns.

## What is still enforced (operational, not security)

Big results go to a file instead of being truncated (`big-results.md`), `--timeout-ms` (one
deadline per call), secret scrubbing of database paths and DSNs in error messages, and
`tool_code`s on errors. `scripts/mutation-check.py` covers these: 8 mutants, each killed by a
dedicated test.
