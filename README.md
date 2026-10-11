# lmsql

One SQL statement in, structured JSON out. `lmsql` is a deliberately small database CLI for
LLMs and the programs that drive them: no shell, no cursors, no sessions, no stored procedures.
Every call opens the database, runs exactly one statement under hard limits, prints one JSON
document and exits.

It is a native [lmapi](https://github.com/lmctl-ai/lmapi) tool (`cli.schema.v1`), built on the
Rust SDK [lmrust](https://github.com/lmctl-ai/lmrust): `lmsql --schema` publishes the contract,
and an agent can call it through any lmapi client with validated flags or JSON.

Phase 1 supports **SQLite** (bundled; no system library needed). Postgres is planned.

```sh
lmsql query --db app.sqlite "SELECT id, name FROM users ORDER BY id LIMIT 5"
lmsql tables --db app.sqlite
lmsql describe --db app.sqlite users
lmsql exec --db app.sqlite "UPDATE users SET active = 0 WHERE id = 7" --confirm
```

## Why

An LLM that can run SQL needs three guarantees a normal client does not give: a read can never
write, one call is one statement, and the answer is bounded and machine-readable. lmsql makes
those properties of the tool, not of the prompt.

## Safety model

| Guarantee | How |
| --- | --- |
| `query`, `tables`, `describe` cannot write | four independent guards, each enough alone: the file is opened `SQLITE_OPEN_READ_ONLY`; `PRAGMA query_only = ON`; `query` refuses statements `sqlite3_stmt_readonly` does not call read-only (`NOT_READ_ONLY`); an authorizer refuses ATTACH/DETACH, PRAGMA assignments and `load_extension` |
| No other file is touched | `SQLITE_LIMIT_ATTACHED = 0` on every connection (no `ATTACH`, no `VACUUM INTO`); `file:` names are literal paths, never URIs |
| No schema corruption, no untrusted schema code | `SQLITE_DBCONFIG_DEFENSIVE` on, `SQLITE_DBCONFIG_TRUSTED_SCHEMA` off |
| No extensions | extension loading is never enabled, and the `load_extension` function is denied |
| Exactly one statement | prepared with `sqlite3_prepare_v2`; anything in its tail other than whitespace and comments is `TOO_MANY_STATEMENTS` (nothing runs); empty input is `EMPTY_STATEMENT`; a NUL byte is `NUL_IN_STATEMENT` |
| Bounded answers | `maxRows` (default 200, hard cap 10 000), `maxBytes` (default 1 MiB, hard cap 16 MiB): when hit, `truncated: true` and `truncated_by` say so; nothing is dropped silently. Values above a hard cap are refused, not clamped |
| Bounded time | `timeoutMs` (default 10 s, cap 300 s) is the lock wait (busy timeout) and the deadline of a progress handler that interrupts the statement (`TIMEOUT`, exit 7) |
| Writes need intent | `exec` is the only write path; it is `access: write` and dangerous, so it needs `--confirm`; `--dryrun` and `LMSQL_DRYRUN=1` simulate it, `--readonly` and `LMSQL_READONLY=1` refuse it |
| No secret echo | database paths and connection values never appear in results or errors (`<db>` replaces them) |

Every guard has its own test, and a mutation check (each guard removed in turn) fails a test for
every one of them; see `durable-memory/safety.md`.

## Operations

All operations take the database as `--db PATH` or `--connection NAME` (exactly one), and the
`--input-json '{...}'` / `--input-file` forms of lmapi. Errors are lmapi envelopes on stdout
(`{"success": false, "error": {"code", "message", "data": {"tool_code", ...}}}`) with lmapi exit
codes.

### `query` (read)

One read-only statement: `SELECT`, `WITH ... SELECT`, `VALUES`, read `PRAGMA`s (`table_info`,
`index_list`, `user_version`, ...), `EXPLAIN`.

```sh
$ lmsql query --db app.sqlite "SELECT id, name, avatar FROM users WHERE id < 3" --max-rows 50
{
  "columns": [{"name": "id", "type": "INTEGER"}, {"name": "name", "type": "TEXT"}, {"name": "avatar", "type": "BLOB"}],
  "row_count": 2,
  "rows": [[1, "ada", {"$blob_base64": "AP8Q"}], [2, "bo", null]],
  "truncated": false,
  "truncated_by": null
}
```

`type` is the declared column type (`null` for expressions). Params: `statement` (positional),
`maxRows`, `maxBytes`, `timeoutMs`.

### `exec` (write, needs `--confirm`)

One statement of any kind (`INSERT`, `UPDATE`, `DELETE`, `CREATE`, `PRAGMA x = y`, ...).

```sh
$ lmsql exec --db app.sqlite "INSERT INTO users(name) VALUES ('cy') RETURNING id" --confirm
{"last_insert_rowid": 3, "returning": {"columns": [...], "rows": [[3]], ...}, "rows_affected": 1}
```

`returning` appears when the statement produces rows (`RETURNING`, a reporting `PRAGMA`), capped
like `query`. `--create` creates a missing database file (otherwise a missing file is
`NOT_FOUND`).

### `tables` (read)

```sh
$ lmsql tables --db app.sqlite
{"tables": [{"name": "users", "type": "table"}, {"name": "active_users", "type": "view"}], "truncated": false}
```

### `describe` (read)

```sh
$ lmsql describe --db app.sqlite users
{"columns": [{"cid": 0, "default": null, "name": "id", "notnull": false, "pk": 1, "type": "INTEGER"}, ...],
 "table": "users", "type": "table"}
```

The name is matched case-insensitively; `pk` is the 1-based position in the primary key (0 if
not part of it). A missing table is `NOT_FOUND` / `TABLE_NOT_FOUND`.

## Type mapping

| SQLite value | JSON |
| --- | --- |
| NULL | `null` |
| INTEGER | number (exact 64-bit; JavaScript clients lose precision above 2^53) |
| REAL | number; NaN and +/-Infinity become `null` (JSON has no such numbers) |
| TEXT (valid UTF-8) | string (NUL bytes kept, as `\u0000`) |
| TEXT (invalid UTF-8) | `{"$text_base64": "..."}` (exact bytes) |
| BLOB | `{"$blob_base64": "..."}` (standard alphabet, padded) |

## Connections

`--db PATH` opens a SQLite file. `--connection NAME` reads the environment variable
`LMSQL_<NAME>` (name upper-cased; letters, digits, `_`), whose value is a path, `sqlite:PATH` or
`sqlite://PATH`. Postgres DSNs (`postgres://...`) are recognised and refused with
`UNSUPPORTED_BACKEND` until phase 2. The value is never printed, in results or errors.

```sh
export LMSQL_APP=/srv/data/app.sqlite
lmsql query --connection app "SELECT count(*) FROM users"
```

## Errors

| `error.code` (exit) | `data.tool_code` examples |
| --- | --- |
| `VALIDATION_ERROR` (2) | `SQLITE_ERROR` (syntax, unknown table/column/function; `data.offset` when known), `TOO_MANY_STATEMENTS`, `EMPTY_STATEMENT`, `NUL_IN_STATEMENT`, `LIMIT_OUT_OF_RANGE`, `CONNECTION_REQUIRED`, `CONNECTION_CONFLICT`, `UNSUPPORTED_BACKEND` |
| `NOT_FOUND` (4) | `SQLITE_CANTOPEN` (missing database), `TABLE_NOT_FOUND`, `CONNECTION_NOT_FOUND` |
| `CONFLICT` (5) | `SQLITE_CONSTRAINT_UNIQUE` (and other constraints), `SQLITE_BUSY`, `SQLITE_LOCKED` |
| `FORBIDDEN` (6) | `NOT_READ_ONLY`, `SQLITE_READONLY`, `SQLITE_AUTH` |
| `TIMEOUT` (7) | `TIMEOUT` |
| `COMMAND_FAILED` (1) | `SQLITE_NOTADB`, `SQLITE_CORRUPT`, `SQLITE_IOERR`, ... |

Validation of the call itself (unknown flag, wrong type, missing statement) is lmapi's
`VALIDATION_ERROR` with `data.issues`, before the database is opened. Policy refusals are
`MISSING_CONFIRMATION` (9) and `READONLY_VIOLATION` (6).

## lmapi and the human CLI

`lmsql --schema` prints the published document (also in `cli.schema.dev.json`;
`cli.schema.latest.json` is the last released contract). The flags form *is* the human CLI:
`lmsql query --db x "select 1"` is an lmapi invocation and prints JSON. Without an operation,
`lmsql`, `lmsql --help` and `lmsql --version` print help and the version; there are no other
verbs, so nothing can collide with lmapi detection.

## Build

```sh
cargo build --release          # target/release/lmsql
cargo test                     # unit + integration (real temp SQLite files)
```

MSRV: Rust 1.79 (same as lmrust).

## License

MIT, see [LICENSE](LICENSE). Copyright Lattice Systems LLC.
