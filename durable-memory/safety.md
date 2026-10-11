# Safety: guards and their tests

Read operations are read-only by four independent guards (`src/backend/sqlite.rs`):

| Guard | Isolated test (`backend::sqlite::tests`) | What the test does without the other guards |
| --- | --- | --- |
| 1 `SQLITE_OPEN_READ_ONLY` | `guard1_read_only_open_refuses_writes_without_the_other_guards` | bare connection with the read flags: INSERT -> `SQLITE_READONLY` |
| 2 `PRAGMA query_only = ON` | `guard2_query_only_refuses_writes_on_a_writable_connection` | read-write connection + query_only: INSERT -> `SQLITE_READONLY`; `open` sets it |
| 3 `sqlite3_stmt_readonly` | `guard3_query_refuses_statements_sqlite_does_not_call_read_only` | INSERT, WITH ... INSERT through `query` -> `NOT_READ_ONLY` (not the step-time error) |
| 4 authorizer | `guard4_...`, `harden_installs_the_authorizer_in_every_mode` | PRAGMA assignments, ATTACH, load_extension -> `SQLITE_AUTH`; `harden` installs it |

Shared hardening: `harden_sets_the_limit_and_db_config_in_every_mode` (attach limit 0, defensive
on, trusted_schema off), `attach_limit_blocks_new_files_even_for_exec`, `file_uris_are_literal_paths`,
`one_statement_exactly`. End to end: `tests/safety.rs` (23 write attempts through `query` leave the
file byte-identical, attach/VACUUM INTO through every op, multi-statement, caps, timeout, busy,
secret echo, injection shapes).

## Mutation check (2026-10-11)

Each mutation applied alone, `cargo test --no-fail-fast`, then reverted:

| Mutation | Failing tests |
| --- | --- |
| M1 read open flag -> READ_WRITE | guard1 |
| M2 `query_only` not set | guard2 |
| M3 `stmt.readonly()` check removed | guard3, `query_can_never_write`, `goldens` |
| M4 authorizer allows everything | `harden_installs_the_authorizer_in_every_mode`, `query_can_never_write` |
| M5 attach limit removed | `harden_sets_the_limit_and_db_config_in_every_mode` |
| M6 statement tail ignored | `one_statement_exactly`, `exactly_one_statement_per_call` |
| M7 `file:` names not made literal | `file_uris_are_literal_paths`, `uri_file_names_are_plain_paths` |

M4 and M5 were first caught by no test (or only end to end): defense in depth hid them (the
authorizer also stops ATTACH; the limit also stops VACUUM INTO). The two `harden_*` tests were
added so each guard fails its own test.
