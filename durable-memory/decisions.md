# Phase 1 decisions

- **No legacy verbs.** The flags form of the published ops is the human CLI
  (`lmsql query --db x "select 1"` is lmapi-shaped and prints JSON). The legacy parser
  (`src/legacy.rs`) only handles no args, `--help`, `--version` and unknown words. A separate
  legacy `query` verb would collide with lmapi detection (spec 6.1: a published op name makes argv
  lmapi-shaped), and two outputs for one verb would confuse agents.
- **Op names** `query`, `exec`, `tables`, `describe` (flat, no prefix); inputs `flags` + `json`;
  the default lmapi globals are kept (lmsql owns no `--format`).
- **lmrust without default features**: sorted-key JSON output, deterministic goldens.
- **rusqlite 0.32** (as lmprobe) with `bundled`, `hooks` (authorizer, progress handler),
  `limits` (attach limit), `column_decltype`. Builds on Rust 1.79.
- **Statement tail via `rusqlite::Batch`**, which walks `sqlite3_prepare_v2` tails and skips
  comment-only remainders. A second statement that does not even prepare (references a table the
  first would create, or is garbage) still counts as a second statement. Nothing runs before the
  check.
- **NUL in a statement is refused**: SQLite stops reading at NUL, so text after it would be
  silently ignored.
- **`file:` paths**: the bundled SQLite is compiled with `SQLITE_USE_URI`, so `SQLITE_OPEN_URI`
  is not the switch; `literal_path` prefixes `./` to any name starting with `file:`.
- **Errors**: SQL errors (syntax, unknown objects) are `VALIDATION_ERROR` (the request is at
  fault); constraints and locks `CONFLICT`; refusals `FORBIDDEN`; interrupt = `TIMEOUT` (the only
  interrupt source is the deadline). "not authorized" prepare errors (function denials, disabled
  extension loading) are reported as `SQLITE_AUTH`.
- **Caps**: one extra row is read so `truncated` is exact; `maxBytes` counts the serialized
  `rows` array, and a lower bound is checked before encoding so a huge blob is never copied.
  Limits above the hard cap are refused (`LIMIT_OUT_OF_RANGE`), never clamped.
- **`exec`**: dangerous (needs `--confirm`); the dry-run result omits the command (it would carry
  the statement and nothing about the database is echoed). Statements that produce rows return
  them under `returning`. No create unless `create: true`.
- **`describe`** matches names case-insensitively (SQLite semantics) and reads
  `pragma_table_info` with a bound parameter.
- **Integer precision**: 64-bit integers are emitted exactly; JS clients lose precision above 2^53.
  Not encoded as strings, to keep rows natural for the common case (documented).
