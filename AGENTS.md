# lmsql development

One SQL request in, structured JSON out — stateless, big results to a file: a native lmapi tool (lmrust `v0.1.2`, default features
off) over SQLite (rusqlite, bundled) and PostgreSQL (`postgres` + rustls). Read [README.md](README.md)
for the contract and [durable-memory/index.md](durable-memory/index.md) for decisions.

## Open mode (v1)

lmsql runs **any SQL the database accepts**. `query` and `exec` are equivalent (lmapi access
`dynamic`, no `--confirm`); SQLite opens read-write and creates the file; PostgreSQL runs
autocommit. There is no read-only guard, keyword allowlist, single-statement rule or
transaction-control refusal — restrict access with the database (a read-only role; file
permissions), not with lmsql. The earlier guard implementation is in git history (branch
`phase2-postgres`, commit `0a3877f`) and is planned to return behind config.

## Invariants (still enforced)

- **Policy seam.** Every request passes through `crate::policy::Policy` (`src/policy.rs`). v1
  allows everything and records `LMSQL_CONFIG`; put future limits here, not in the backends or the
  op handlers.
- **Settings only restrict, and fail closed.** `src/settings.rs`: no file = open; a present file
  that is not exactly valid = `SETTINGS_INVALID`, never open. Connection levels are enforced by
  the database handle (SQLite read-only + `query_only`; PostgreSQL READ ONLY session / readonly
  login), not by reading the SQL text. Denials are `PERMISSION_DENIED` with `manual_command`
  (never a secret value). SQLite table rules go through the authorizer; a dry run never
  executes a change (SQLite `stmt_readonly`, PostgreSQL READ ONLY session); the bypass flag works
  only with `allow_bypass`; `output.dir` contains result files. Tests pin `LMSQL_CONFIG` to a
  missing file (`durable-memory/permissions.md`).
- **Stateless.** One process, one connection, one statement batch; nothing persists between
  calls (no caches, lock files or leftover temp files). The only files lmsql writes are the
  declared `out` file and spill files (`src/output.rs`); tests check that a call leaves nothing
  else behind and that a TEMP table does not survive it.
- **Big results go to a file, never truncated.** Backends stream rows into `output::Sink` (SQLite
  row by row, PostgreSQL through a cursor); `--max-rows` / `--max-bytes` are inline thresholds,
  past them the full result spills to `$LMSQL_OUT_DIR`. Files: temp name (create_new, 0600) +
  rename on success, removed on error; spill dir 0700 and private. Memory must stay flat (the
  5M-row tests check peak RSS). `--timeout-ms` is one deadline per call. After changing any of
  this, run `python3 scripts/mutation-check.py` with `LMSQL_TEST_PG_URL` set (every mutant must
  fail a test).
- **No secret echo.** Database paths and DSNs (user/host/password) never reach stdout/stderr;
  errors go through `DbError::into_cli_error`, which scrubs them. One generic `CONNECT_FAILED` for
  every PostgreSQL connection failure.
- **Result shapes** for `query`/`exec`: inline `{columns, rows, row_count, rows_affected,
  last_insert_rowid, spilled: false, truncated, truncated_by}`, or a file summary with `file
  {path, format, bytes, rows}`, `preview`, `spilled`, `overwrote` instead of `rows`.
- **Contract files.** After changing an operation:
  `cargo run -- --schema > cli.schema.dev.json` (the drift test compares byte for byte); update
  `cli.schema.latest.json` in a release.

## Checks

`cargo fmt --all --check`, `cargo clippy --locked --all-targets -- -D warnings`,
`cargo test --locked`, `cargo +1.79 test --locked` (MSRV). The PostgreSQL tests skip unless
`LMSQL_TEST_PG_URL` is set (a throwaway server), or are required by `LMSQL_REQUIRE_PG=1` (CI).

Commits: stage explicitly (never `git add -A`); intent line, context, trailers.
