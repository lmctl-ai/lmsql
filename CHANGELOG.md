# Changelog

## 0.3.0 (unreleased)

Permission settings, slice 1.

- A settings file (`$LMSQL_CONFIG`, else `$XDG_CONFIG_HOME/lmsql/settings.json`, else
  `~/.config/lmsql/settings.json`) that only restricts; without it lmsql is open as in 0.2.0.
  Invalid files fail closed (`SETTINGS_INVALID`); a group/other-writable file is warned about.
- With a settings file, `--connection` resolves through it (sqlite path, or postgres logins via
  `dsn_env` / `dsn`), `--db` needs `allow_direct` (`DIRECT_NOT_ALLOWED`), and paths / DSN values
  from the file join the scrub set.
- Connection levels `none` / `readonly` / `readwrite`: SQLite readonly opens read-only with
  `query_only`; PostgreSQL readonly uses the readonly login (else the readwrite login) with every
  transaction READ ONLY. Denials are `FORBIDDEN` / `PERMISSION_DENIED` with
  `error.data.manual_command` for the operator.
- `--help` text updated (it still described the 0.1 confirm/read-only model).
- SQLite per-table levels (`permissions.tables`) through the SQLite authorizer: `readonly` tables
  refuse writes/DDL, `none` tables refuse any access and are hidden from `tables`.
- Dry run (lmrust's `--dry-run` / `LMSQL_DRYRUN`): reads run normally; anything that would change
  something is not executed and is described (`executed: false, would_change: true`, the
  database's plan, PostgreSQL `estimated_rows`); no file is written (`would_write`).
- `--dangerously-bypass-permission-settings` on all operations, effective only with
  `"allow_bypass": true` (else `BYPASS_NOT_ALLOWED`); skips the permission rules, not the limits.
- `output.dir` containment for `out` and spills (canonical paths; `PERMISSION_DENIED` outside).

## 0.2.0 (unreleased)

Open mode, a PostgreSQL backend, and big results to a file.

- **Big results go to a file, never truncated** (lmsql is stateless: no paging between calls).
  New params `out` and `outFormat` (ndjson | csv | json): with `out`, the full result is streamed
  to that file and the response is a summary (`file {path, format, bytes, rows}`, `preview`,
  `overwrote`). Without `out`, a result past `maxRows` / `maxBytes` (now inline thresholds) spills
  in full to `$LMSQL_OUT_DIR` (default `<temp>/lmsql/`, 0700, must be private) as
  `lmsql-<UTC stamp>-<random>.ndjson`, with `spilled: true`. Rows are streamed with bounded memory
  (5M rows at ~5 MiB peak RSS, both backends); files are written to a 0600 temp name and renamed
  on success, removed on error; `out` is checked before the SQL runs (`OUT_NOT_WRITABLE`).
  `truncated` remains as a legacy field (always false for `query`/`exec`). Spill files are never
  deleted by lmsql. `maxRows` no longer declares a default in the schema (still 200).

- **Open mode (owner decision):** lmsql runs any SQL the database accepts. `query` and `exec`
  are equivalent (lmapi access `dynamic`, no `--confirm`). The read-only guards, keyword
  allowlist, single-statement rule, ATTACH/pragma/size limits and transaction-control refusal
  are removed; restrict access with the database (a read-only role, file permissions). The
  previous hardening is in git history (branch `phase2-postgres`, commit `0a3877f`) and is
  planned to return behind config. A `Policy` seam (`src/policy.rs`, reads `LMSQL_CONFIG`)
  currently allows everything.
- **PostgreSQL:** `postgres://` / `postgresql://` DSNs via `LMSQL_<NAME>` (`--connection`) or
  directly via `--db`. rustls TLS (ring provider, bundled Mozilla roots); `sslmode`
  disable/prefer/require/verify-ca/verify-full, default `prefer` local / `verify-full` remote.
  A single row-returning statement is read through a server cursor so a huge result stops at the
  cap; errors map from SQLSTATE (`PG_<code>`); connection failures are one `CONNECT_FAILED` with
  no host/user/password; `tables`/`describe` take a `schema` param.
- **One result shape** for `query`/`exec`: `{columns, rows, row_count, rows_affected,
  last_insert_rowid, truncated, truncated_by}`.
- Kept: `--max-rows` / `--max-bytes` / `--timeout-ms`, `tool_code`s, secret scrubbing of DSNs in
  error text. Builds on Rust 1.79.

## 0.1.0 (unreleased)

First version (phase 1, SQLite):

- lmapi operations `query` (read), `exec` (write, dangerous), `tables`, `describe`; flags and
  JSON inputs; `cli.schema.dev.json` / `cli.schema.latest.json` with a drift test.
- Read operations cannot write (four independent guards), one statement per call
  (`sqlite3_prepare_v2` tail), no ATTACH / `VACUUM INTO` / extensions, defensive mode.
- `maxRows`, `maxBytes` (explicit `truncated`/`truncated_by`) and `timeoutMs` (busy timeout and
  interrupt); documented JSON type mapping (base64 blobs, non-finite reals as `null`).
- `--db PATH` or `--connection NAME` from `LMSQL_<NAME>`; paths and DSNs are never echoed.
- Review fixes (FableReviewer): size limits on every connection (values 32 MiB, statements
  16 MiB, expression depth, compound SELECT, LIKE patterns) and a 256 MiB hard SQLite heap
  limit, so one builtin can no longer run or allocate unbounded; one deadline shared by the lock
  wait and the progress handler; `--db` must be an existing regular file (no fifo hang, no
  devices or directories), `:memory:` and blank names refused; reads of WAL databases documented
  (they create `-wal`/`-shm`) with `DIRECTORY_READONLY` for read-only directories; path
  scrubbing on whole tokens of SQLite messages only (no garbled messages); `load_extension()`
  inside a view is `SQLITE_AUTH`.
