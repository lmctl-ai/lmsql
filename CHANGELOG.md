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
