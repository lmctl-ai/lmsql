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

## Mutation check, round 2 (after the review fixes, 2026-10-11)

`scripts/mutation-check.py` (each mutation alone, `cargo test --no-fail-fast`, file
restored after each): 17 mutations, 0 survivors.

| Mutation | Failing tests |
| --- | --- |
| M1 read open flag -> READ_WRITE | guard1, `reading_a_wal_database_leaves_the_database_file_unchanged` |
| M2 `query_only` not set | guard2 |
| M3 `stmt.readonly()` removed | guard3, `query_can_never_write`, `goldens` |
| M4 authorizer allows everything | `harden_installs_the_authorizer_in_every_mode`, `query_can_never_write` |
| M5 attach limit removed | `harden_sets_the_limit_and_db_config_in_every_mode` |
| M6 statement tail ignored | `one_statement_exactly`, `exactly_one_statement_per_call` |
| M7 `file:` names not literal | `file_uris_are_literal_paths` |
| M8 length limit removed | `harden_sets_size_and_shape_limits_in_every_mode`, `oversized_values_fail_fast_with_sqlite_toobig`, `huge_blobs_are_truncated_or_refused_never_returned_whole` |
| M9 heap limit not set | `opening_sets_the_hard_heap_limit` |
| M10 `is_file` check removed | `non_regular_paths_and_special_names_are_refused_without_opening` |
| M11 blank/`:memory:` refusal removed | `non_regular_paths_and_special_names_are_refused_without_opening` |
| M12 pre-encoding size check removed | `values_over_the_byte_cap_are_refused_before_they_are_encoded` |
| M13 busy wait without the shared deadline | `the_busy_wait_shares_the_call_deadline` |
| M14 scrub not whole-token | `scrub_replaces_whole_tokens_only` |
| M15 lmsql's own messages scrubbed | `short_paths_do_not_garble_messages` |
| M16 "unsafe use of" not a refusal | `load_extension_inside_a_view_is_refused_like_the_authorizer` |
| M17 read-only directory not mapped | `a_wal_database_in_a_read_only_directory_gets_a_clear_error` |

M7 is now also stopped end to end by `check_path` (a `file:...` name is not an existing file),
so only its unit test fails; the unit test still pins `literal_path` for `exec --create`.
