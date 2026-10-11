# lmsql

One SQL request in, structured JSON out. `lmsql` is a small, **stateless** database CLI for LLMs
and the programs that drive them: every call is one process, one connection and one statement
batch — no sessions, no cursors kept open, no pagination tokens, nothing remembered between calls.
A small result comes back inline; a big one is written in full to a file and the response says
where it is.

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

## Permissions (settings file)

lmsql's own permission layer **only restricts further**: it never grants more than the database
allows. The real boundary is the database role (PostgreSQL) or the file permissions (SQLite);
connect as a read-only role when nothing should be written.

Without a settings file lmsql is open, as described above. With one, it decides which
connections exist and how much lmsql may do with each. The file is `$LMSQL_CONFIG`, else
`$XDG_CONFIG_HOME/lmsql/settings.json`, else `~/.config/lmsql/settings.json`; keep it mode 0600
(lmsql warns on stderr if group or others can write it).

```json
{
  "version": 1,
  "allow_direct": false,
  "connections": {
    "app":  {"driver": "sqlite", "path": "~/data/app.sqlite",
             "permissions": {"default": "readonly"}},
    "prod": {"driver": "postgres",
             "logins": {"readonly":  {"dsn_env": "PROD_RO_DSN"},
                        "readwrite": {"dsn_env": "PROD_RW_DSN"}},
             "permissions": {"default": "readonly"}}
  }
}
```

- **Fail closed.** A file that exists but cannot be read, is not valid JSON, has an unknown key,
  a wrong type or an unknown level is an error (`SETTINGS_INVALID`, naming the file and the key);
  lmsql never falls back to open.
- **Connections.** `--connection NAME` resolves only through the file (the `LMSQL_<NAME>`
  fallback applies only when there is no file). A SQLite `path` may start with `~/`; a relative
  path is relative to the settings file. A PostgreSQL login names an environment variable holding
  the DSN (`dsn_env`, preferred) or holds it literally (`dsn`). The caller only ever needs the
  connection name; paths and DSNs are never echoed.
- **Direct access.** `--db PATH|DSN` is refused (`DIRECT_NOT_ALLOWED`) unless `"allow_direct":
  true`.
- **Levels** (`permissions.default`, required): `none` — every operation is refused; `readonly`
  — SQLite is opened read-only with `query_only` (the file is never created); PostgreSQL uses the
  `readonly` login if there is one, else the `readwrite` login, and every transaction of the
  session starts READ ONLY (`default_transaction_read_only`) — SQL could change that setting, so
  without a read-only login this is weaker than a real read-only role; `readwrite` — as without
  settings (the `readwrite` login, or the only one).
- **Denials** are one short error: `FORBIDDEN` (exit 6) with `tool_code` `PERMISSION_DENIED`,
  naming the connection, the rule and the settings file. `error.data.manual_command` is the
  equivalent command for the operator to run in their own shell with the original client, e.g.
  `sqlite3 ~/'data/app.sqlite' 'DELETE FROM t'` or `psql "$PROD_RW_DSN" -c 'DELETE FROM t'` (the
  environment variable name only, never its value).
- **Per-table levels (SQLite)** — `"permissions": {"default": "readwrite", "tables": {"audit_log":
  "readonly", "secrets": "none"}}`. Enforced by the SQLite authorizer, which reports the table of
  every read, write and DDL action whatever the SQL looks like (subqueries, views, `main.` names):
  a `readonly` table refuses writes and DDL on it, a `none` table refuses any access, and is
  hidden from `tables` and refused by `describe`. Unlisted tables follow the default. Names
  compare case-insensitively. For PostgreSQL a `tables` block is rejected: use grants and roles.
- **Dry run** — lmapi's `--dry-run` (or `LMSQL_DRYRUN=1`) estimates the blast radius and changes
  nothing. Reads run normally (`dry_run: true, executed: true, would_change: false`). If the SQL
  would change anything, **nothing is executed**; the response is `{"dry_run": true, "executed":
  false, "would_change": true, "statement": "...", "plan": ...}`:
  - SQLite asks `sqlite3_stmt_readonly` for every statement (preparing runs nothing); `plan` is
    the `EXPLAIN QUERY PLAN` lines (null where SQLite has none, e.g. DDL); multi-statement text
    lists each statement with its own `would_change` and plan, and none of them runs.
  - PostgreSQL runs the text in a session where every transaction is READ ONLY; a statement the
    server refuses there (SQLSTATE 25006) would change something. `plan` is `EXPLAIN (FORMAT
    JSON)` (no ANALYZE, so nothing executes; null for DDL or multi-statement text) and
    `estimated_rows` is the planner's estimate of the rows the write would touch.
  - No file is written: `file: null` with `would_write: {path, format}` when `out` was given, or
    a note when the result would have spilled. Permissions still apply (denied stays denied).
  - It is an estimate: on PostgreSQL the read-only session refuses writes such as `nextval`, but
    functions a read calls still run.
- **Bypass** — `--dangerously-bypass-permission-settings` skips the permission rules for one call
  (connection levels, table rules, the direct-access gate, `output.dir`) but not the operational
  limits (`--timeout-ms`, inline thresholds), and prints one line on stderr. It only works when
  the operator set `"allow_bypass": true`; otherwise it fails with `BYPASS_NOT_ALLOWED`. The flag
  is visible to the LLM, so the opt-in has to live in the operator's file, not on the command
  line. Without a settings file it is a no-op.
- **`output.dir`** — `"output": {"dir": "~/lmsql-out"}` confines result files: an `out` path must
  resolve inside it (canonical, symlinks and `..` followed; for a new file its nearest existing
  parent), else `PERMISSION_DENIED`; spills go there (created 0700) instead of `$LMSQL_OUT_DIR`.
  Checked before any SQL runs.
- **Coming next**: an operator-only `lmsql config` helper to edit the settings file.

## Big results go to a file

lmsql never truncates a result and never pages it. If it is small, the rows come back inline. If
it is big, **the full result is written to a file** and the response is a summary with a short
preview; the caller then reads or processes the file with its own tools.

```sh
# Explicit: stream everything to a file (format from the extension, or --out-format).
lmsql query --connection prod "SELECT * FROM events" --out /work/events.ndjson
lmsql query --db app.sqlite "SELECT * FROM users" --out users.csv

# Automatic: past --max-rows / --max-bytes the result spills to $LMSQL_OUT_DIR.
lmsql query --db app.sqlite "SELECT * FROM log"
```

A file response (both cases):

```json
{"columns": [{"name": "id", "type": "INTEGER"}, {"name": "msg", "type": "TEXT"}],
 "row_count": 5000000, "rows_affected": 0, "last_insert_rowid": 0,
 "file": {"path": "/tmp/lmsql/lmsql-20261011T160537Z-59b5c9e9.ndjson",
          "format": "ndjson", "bytes": 157777792, "rows": 5000000},
 "preview": [[1, "boot"], [2, "ready"]],
 "spilled": true, "overwrote": false, "truncated": false, "truncated_by": null}
```

How an LLM caller should follow up (the file is plain text; `row_count` is the exact number of
rows, already counted):

```sh
head -n 5 /tmp/lmsql/lmsql-...ndjson                     # look at a few rows
jq -c 'select(.status == "error")' file.ndjson | head    # filter (ndjson: one object per line)
jq -r '.msg' file.ndjson | sort | uniq -c | sort -rn     # aggregate
wc -l file.ndjson                                        # = rows (ndjson)
```

Often the better follow-up is a narrower query (`WHERE`, `GROUP BY`, `LIMIT`) — the database is
faster at it than a file scan.

**`out` given** — every row is streamed to that file and the response is the summary above
(`spilled: false`, `overwrote` says whether a file was replaced). `--out-format` is `ndjson`,
`csv` or `json`; by default it comes from the extension (`.csv`, `.json`, `.ndjson`/`.jsonl`),
else ndjson. A relative path is relative to the working directory; the response always has the
absolute path. Missing parent directories are created. The path is checked (a probe file is
created and removed) **before the SQL runs**, so an unwritable `out` (`OUT_NOT_WRITABLE`) never
follows a committed write. The preview is the first `--max-rows` rows if given, else 20 (and never
more than `--max-bytes`).

**`out` not given** — rows stay inline (`rows`) while the result is within `--max-rows`
(default 200) **and** `--max-bytes` (default 1 MiB, the exact size of the serialized `rows`
array). The first row past either threshold switches the call to a spill: the full result goes to
`$LMSQL_OUT_DIR/lmsql-<UTC yyyymmddThhmmssZ>-<random>.ndjson` (default directory
`<system temp>/lmsql/`, created mode 0700; as it sits in a shared directory it must be a real
directory owned by you and closed to others, else `OUT_NOT_WRITABLE`) and the response is the
summary with `spilled: true`. So `--max-rows` / `--max-bytes` are the *inline thresholds* now
(the flag names are kept for compatibility); `truncated` is a legacy field and always `false`.

**Writing safely** — rows are written as they are fetched, never all held in memory: 5 million
rows stream at about 5 MiB peak memory (SQLite ~5 s, PostgreSQL ~9 s, debug build). Each file is
written to a new temporary name in the target directory (mode 0600) and renamed into place only
when the call succeeds; on any error (SQL error mid-stream, timeout, disk) the temporary file is
removed and an existing target is left untouched. An existing `out` file is replaced.

**Cleanup is the caller's job** — lmsql never deletes old spill files (it remembers nothing
between calls). Delete them when done, or point `LMSQL_OUT_DIR` at a directory you clean. There is
no disk-space cap yet (a planned `Policy` setting).

### File formats

Values follow the same type rules as inline rows (see *Type mapping*): BLOB / `bytea` →
`{"$blob_base64": "..."}`, PostgreSQL `numeric` → string, `json`/`jsonb` → the JSON value,
non-finite reals → `null`, NULL → `null`.

| Format | Shape |
| --- | --- |
| `ndjson` | one JSON object per line, keyed by column name (`{"id":1,"name":"ada"}`); a duplicate column name gets `_2`, `_3`, … |
| `json` | one JSON array of the same objects, written incrementally (`[` … `]`; an empty result is `[\n]`) |
| `csv` | RFC 4180: a header row of the (de-duplicated) column names, CRLF line ends, a field quoted when it contains `,` `"` CR or LF (quotes doubled). **NULL is an empty field; an empty string is `""`**, so they stay distinct. Numbers and booleans as JSON text; objects (blobs, json) as compact JSON text. A statement without columns writes an empty file. |

## Time limit

`--timeout-ms` (default 10 s, at most 300 s) is one deadline for the call: SQLite busy-wait plus
a progress handler that interrupts a long statement (`TIMEOUT`, exit 7); PostgreSQL
`statement_timeout` / `lock_timeout` / `connect_timeout` plus a watchdog that sends a cancel
request at the deadline. It includes the time spent writing a file.

Errors are lmapi envelopes on stdout (`{"success": false, "error": {"code", "message", "data":
{"tool_code", ...}}}`) with the backend's code in `tool_code` (`SQLITE_*`, `PG_<SQLSTATE>`, or
`TIMEOUT` / `CONNECT_FAILED` / `OUT_NOT_WRITABLE`). A database path or DSN (user, host, password)
never appears in a result or an error message.

## Operations

All take the database as `--db` or `--connection NAME`, and accept the `--input-json` /
`--input-file` forms of lmapi. An inline result:

```json
{"columns": [{"name": "...", "type": "..."}], "rows": [[...]],
 "row_count": 0, "rows_affected": 0, "last_insert_rowid": null,
 "spilled": false, "truncated": false, "truncated_by": null}
```

and a file result replaces `rows` with `file`, `preview` and `overwrote` (see above).

- **`query`** and **`exec`** both run the SQL and return that shape (params `out`, `outFormat`,
  `maxRows`, `maxBytes`, `timeoutMs`). They are equivalent (lmapi access `dynamic`, no
  confirmation). Multiple statements run in order; the last statement's rows are returned.
  `last_insert_rowid` is SQLite's rowid (`null` on PostgreSQL).
- **`tables`** lists tables and views; `--schema` limits it (PostgreSQL schema; SQLite attached
  database name); `--max-rows` caps the list (`truncated: true` past it).
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
  types, so its values come back as strings. A single statement keeps its types. The driver also
  returns a simple-protocol response as a whole, so a *huge* result from multi-statement text or
  `INSERT ... RETURNING` is held in memory once before it is written; a single `SELECT` streams
  through a cursor with bounded memory.
- **Side effects before a spill error**: `out` is checked before the SQL runs, but a spill (or a
  full disk) can still fail after a PostgreSQL autocommit statement has committed.
- **No disk-space cap** and **no cleanup** of spill files (see *Big results go to a file*).
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
