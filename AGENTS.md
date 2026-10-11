# lmsql development

One SQL statement in, structured JSON out: a native lmapi tool (lmrust `v0.1.2`, default
features off) over SQLite (rusqlite, bundled). Read [README.md](README.md) for the contract and
[durable-memory/index.md](durable-memory/index.md) for decisions.

## Invariants

- **Read ops never write.** `query`/`tables`/`describe` keep all four guards in
  `src/backend/sqlite.rs` (read-only open, `query_only`, `sqlite3_stmt_readonly`, authorizer).
  Each guard has its own test; after changing a guard or a limit, run
  `python3 scripts/mutation-check.py` (every mutant must fail a test; `durable-memory/safety.md`).
- **Bounded resources.** Size limits and the heap limit in `harden`/`open`; one deadline per call
  shared by the busy and progress handlers; `--db` must be an existing regular file.
- **One statement per call**, found with the `sqlite3_prepare_v2` tail (`single_statement`),
  never by splitting text.
- **No secret echo.** Paths and connection values never reach stdout/stderr; errors go through
  `DbError::into_cli_error` (scrubbed).
- **No silent loss.** Caps set `truncated`/`truncated_by`; limits above a hard cap are refused.
- **Contract files.** After changing an operation: `UPDATE_LMAPI_SCHEMA=1 cargo test --test lmapi schema_dev_file`;
  `cli.schema.latest.json` changes only in a release and must stay an additive subset of dev.
- **Goldens.** `UPDATE_GOLDENS=1 cargo test --test ops` only when an output change is intended.

## Checks

`cargo fmt --all --check`, `cargo clippy --locked --all-targets -- -D warnings`,
`cargo test --locked`, `cargo +1.79 test --locked` (MSRV).

Commits: never `git add -A`; intent line, context, trailers.
