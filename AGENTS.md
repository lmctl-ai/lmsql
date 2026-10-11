# lmsql development

One SQL request in, structured JSON out: a native lmapi tool (lmrust `v0.1.2`, default features
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
- **Operational caps.** `--max-rows` / `--max-bytes` stop fetching (a single row-returning
  statement uses a cursor so a huge result does not materialize); `--timeout-ms` is one deadline
  per call. After changing a cap, run `python3 scripts/mutation-check.py` (every mutant must fail a
  test).
- **No secret echo.** Database paths and DSNs (user/host/password) never reach stdout/stderr;
  errors go through `DbError::into_cli_error`, which scrubs them. One generic `CONNECT_FAILED` for
  every PostgreSQL connection failure.
- **One result shape** for `query`/`exec`: `{columns, rows, row_count, rows_affected,
  last_insert_rowid, truncated, truncated_by}`.
- **Contract files.** After changing an operation:
  `cargo run -- --schema > cli.schema.dev.json` (the drift test compares byte for byte); update
  `cli.schema.latest.json` in a release.

## Checks

`cargo fmt --all --check`, `cargo clippy --locked --all-targets -- -D warnings`,
`cargo test --locked`, `cargo +1.79 test --locked` (MSRV). The PostgreSQL tests skip unless
`LMSQL_TEST_PG_URL` is set (a throwaway server), or are required by `LMSQL_REQUIRE_PG=1` (CI).

Commits: stage explicitly (never `git add -A`); intent line, context, trailers.
