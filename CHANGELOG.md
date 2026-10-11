# Changelog

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
