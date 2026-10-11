# lmsql

One SQL request in, structured JSON out. `lmsql` is a small database CLI for LLMs and the programs
that drive them: no shell, no sessions, no prompts. Every call opens the database, runs the SQL it
is given, prints one JSON document and exits.

It is a native [lmapi](https://github.com/lmctl-ai/lmapi) tool (`cli.schema.v1`), built on the Rust
SDK [lmrust](https://github.com/lmctl-ai/lmrust): `lmsql --schema` publishes the contract, and an
agent calls it through any lmapi client with validated flags or JSON.

Backends: **SQLite** (bundled; no system library) and **PostgreSQL** (rustls TLS).

```sh
lmsql query --db app.sqlite "SELECT id, name FROM users ORDER BY id LIMIT 5"
lmsql exec  --db app.sqlite "UPDATE users SET active = 0 WHERE id = 7"
lmsql tables   --db app.sqlite
lmsql describe --db app.sqlite users
export LMSQL_PROD="postgres://reader@db.internal/app?sslmode=verify-full"
lmsql query --connection prod "SELECT now()"
```

## Open mode (v1)

This version runs **any SQL the database accepts**. There is no in-tool restriction on what a
statement may do: `query` and `exec` are identical in power (both run reads, writes and DDL), and
SQLite is opened read-write (the file is created if missing). This is deliberate — lmsql v1 is the
owner's tool on the owner's databases, meant to work end to end; usage limits will come later as
configuration through the `Policy` seam (`src/policy.rs`, which today allows everything and records
`LMSQL_CONFIG` for the future).

**To restrict what a connection can do, use the database itself:** connect PostgreSQL as a
read-only role, or point `--db` at a SQLite file opened by a user without write permission. An
earlier version enforced read-only and single-statement rules inside lmsql; that code lives in git
history (branch `phase2-postgres`, commit `0a3877f`) and is planned to return behind config.

### A recommended read-only PostgreSQL role

```sql
CREATE ROLE lmsql_ro LOGIN PASSWORD '...';
GRANT CONNECT ON DATABASE app TO lmsql_ro;
GRANT USAGE ON SCHEMA public TO lmsql_ro;
GRANT SELECT ON ALL TABLES IN SCHEMA public TO lmsql_ro;
ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT ON TABLES TO lmsql_ro;
ALTER ROLE lmsql_ro SET default_transaction_read_only = on;
```

## What is still enforced: operational caps

These bound the *result and the time*, not what SQL may do (they protect the LLM's context and the
process, not the database):

| Cap | Default | Meaning |
| --- | --- | --- |
| `--max-rows` | 200 | stop fetching after this many rows; `truncated: true`, `truncated_by: "max_rows"`. A single row-returning statement is read through a cursor (SQLite) or a server cursor (PostgreSQL), so a 5-million-row `SELECT` stops in milliseconds without materializing. |
| `--max-bytes` | 1 MiB | stop when the serialized rows reach this size (`truncated_by: "max_bytes"`). |
| `--timeout-ms` | 10 s | one deadline for the call: SQLite busy-wait + a progress handler that interrupts a long statement (`TIMEOUT`, exit 7); PostgreSQL `statement_timeout`/`lock_timeout`/`connect_timeout` plus a watchdog that sends a cancel request at the deadline. |

Errors are lmapi envelopes on stdout (`{"success": false, "error": {"code", "message", "data":
{"tool_code", ...}}}`) with the backend's code in `tool_code` (`SQLITE_*`, `PG_<SQLSTATE>`, or
`TIMEOUT` / `CONNECT_FAILED`). A database path or DSN (user, host, password) never appears in a
result or an error message.

## Operations

All take the database as `--db` or `--connection NAME`, and accept the `--input-json` /
`--input-file` forms of lmapi. Every result has one shape:

```json
{"columns": [{"name": "...", "type": "..."}], "rows": [[...]],
 "row_count": 0, "rows_affected": 0, "last_insert_rowid": null,
 "truncated": false, "truncated_by": null}
```

- **`query`** and **`exec`** both run the SQL and return that shape. They are equivalent (lmapi
  access `dynamic`, no confirmation). Multiple statements run in order; the last statement's rows
  are returned. `last_insert_rowid` is SQLite's rowid (`null` on PostgreSQL).
- **`tables`** lists tables and views; `--schema` limits it (PostgreSQL schema; SQLite attached
  database name).
- **`describe`** returns a table's columns (name, type, notnull, default, primary-key position),
  with an optional `--schema`.

## Connections

- `--db PATH` opens a SQLite file. `--db postgres://...` connects to PostgreSQL directly.
- `--connection NAME` reads the environment variable `LMSQL_<NAME>`, whose value is a SQLite path
  or a `postgres://` / `postgresql://` DSN.

Prefer `LMSQL_<NAME>` for PostgreSQL: a DSN on the command line is visible to other users of the
machine through `ps` / `/proc`. A DSN on `--db` is accepted for convenience but carries that risk.

### TLS (PostgreSQL)

`sslmode` in the DSN is honored: `disable`, `prefer`, `require`, `verify-ca`, `verify-full`. The
default is `prefer` for a local server (loopback / Unix socket) and `verify-full` for any other
host. `sslrootcert=<pem>` supplies the trust roots (otherwise the bundled Mozilla roots). TLS uses
rustls with the ring provider — no system TLS library.

## Type mapping

SQLite: NULL→`null`, INTEGER→number, REAL→number (NaN/±Inf→`null`), TEXT→string (invalid UTF-8→
`{"$text_base64": "..."}`), BLOB→`{"$blob_base64": "..."}`.

PostgreSQL values come back in the server's text format, typed by the column: ints/float/bool→
number/bool, `numeric`→string (exact, incl. `"NaN"`), `json`/`jsonb`→the JSON value, `bytea`→
`{"$blob_base64": ...}`, `timestamptz`→RFC 3339 UTC string, and everything else (text, date,
interval, uuid, inet, arrays, enums, composites, …) as its text string. 64-bit integers are exact;
JavaScript clients lose precision above 2^53.

## Limitations

- **Multi-statement PostgreSQL text** runs over the simple query protocol, which carries no column
  types, so its values come back as strings. A single statement keeps its types.
- **An unresponsive server**: the deadline drives `statement_timeout` and a cancel request, but if
  a PostgreSQL server accepts the connection and then neither answers nor honors the cancel, the
  call can block on the socket past `--timeout-ms`. A future version may add a socket read deadline.
- **TLS verification**: `verify-ca` / `verify-full` are implemented and unit-tested, but there is
  no integration test against a self-signed-certificate server yet.

## Build

```sh
cargo build --release          # target/release/lmsql
cargo test                     # SQLite + lmapi; PostgreSQL tests skip unless LMSQL_TEST_PG_URL is set
```

Running the PostgreSQL tests:

```sh
LMSQL_TEST_PG_URL="postgres://postgres:pw@127.0.0.1:5432/postgres?sslmode=disable" cargo test
```

MSRV: Rust 1.79.

## License

MIT, see [LICENSE](LICENSE). Copyright Lattice Systems LLC.
