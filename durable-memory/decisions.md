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

## Review round 1 (FableReviewer, 2026-10-11)

- **Limits, not timeouts, bound a single builtin.** The progress handler runs between VM steps;
  `printf('%*c', 8e8)`, `randomblob(3e8)` or `zeroblob(3e8)` is one step. `SQLITE_LIMIT_LENGTH`
  = 32 MiB (twice the hard `maxBytes`, so any returnable value fits) makes them fail with
  `SQLITE_TOOBIG` before allocating (SQLite's `printf()` returns NULL instead of an error: its
  documented behaviour). Also: statements 16 MiB, expression depth 500, compound SELECT 200,
  function args pinned at 127, LIKE pattern 10 000. A process-wide `sqlite3_hard_heap_limit64`
  of 256 MiB backs them (`SQLITE_NOMEM`, mapped to `VALIDATION_ERROR` like `SQLITE_TOOBIG`: the
  request exceeds lmsql's limits). Values above 32 MiB stored in a database cannot be read
  (they could not be returned anyway).
- **One deadline per call.** `busy_timeout` restarts for every lock wait; a busy handler now
  stops at the call's deadline (thread-local, set at open; lmsql runs one call per process), so
  lock waits and statement work together stay within `timeoutMs` plus a few milliseconds.
- **WAL side files are documented, not avoided.** SQLite reads a WAL database without creating
  `-wal`/`-shm` only when it is opened `immutable`, which skips locking and can return garbage
  next to a live writer (and needs the URI form the design avoids). Reads therefore may create
  those files; the database file itself is never changed. `SQLITE_READONLY_DIRECTORY` /
  `SQLITE_READONLY_CANTINIT` become `FORBIDDEN` / `DIRECTORY_READONLY` with a message that
  explains WAL and journals (a write in a read-only directory gets the same code).
- **No `SQLITE_OPEN_NOFOLLOW`.** Symlinked databases (`current.db -> releases/v42.db`) are
  common and legitimate, and the path is the caller's own choice; lmsql follows symlinks and
  checks that the target is a regular file.
- **Path checks before open.** `check_path` refuses blank names, `:memory:` (also for
  `exec --create`, which used to "succeed" with nothing persisted) and anything that is not a
  regular file (fifo: the open would block forever; devices; directories). The check and the
  open are not atomic: a file swapped for a fifo in between can still block (accepted; the
  caller controls the path).
- **Scrubbing.** Only messages that come from SQLite/rusqlite are scrubbed (`DbError::foreign`),
  and only where a secret is a whole token (not preceded/followed by a path character). lmsql's
  own messages never contain paths, so `--db a` or `--db not` can no longer garble them.
- **Pre-encoding check is tested.** A test-only counter of base64-encoded bytes proves a value
  over the byte cap is refused without being encoded (the claim above stays).
