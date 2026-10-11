# Phase 2: Postgres (not implemented)

Seams: `backend::Target` (add `Postgres(dsn)`), `backend::Backend` (implement in
`backend/postgres.rs`), `connection::parse_dsn` (currently refuses `postgres://` with
`UNSUPPORTED_BACKEND`), `Target::secrets` (the DSN, and its password, must be scrubbed).

Open questions:

- Driver: `postgres` (sync, matches the no-async design) vs `tokio-postgres`; TLS stack
  (rustls) and its MSRV versus 1.79.
- Read-only: `SET default_transaction_read_only = on` + `BEGIN READ ONLY` per call, and a role
  with only SELECT grants recommended in the docs; there is no `sqlite3_stmt_readonly`, so
  "exactly one statement" and "read-only statement" need the server (simple vs extended query
  protocol: the extended protocol refuses multiple statements).
- Timeouts: `statement_timeout` and `lock_timeout` per call; connection timeout.
- Type mapping: numeric/decimal (string to keep precision?), timestamps (RFC 3339), json/jsonb
  (embed as JSON?), arrays, uuid, bytea (`$blob_base64`), intervals.
- `tables`/`describe`: schemas other than `public`; `information_schema` vs `pg_catalog`.
- DSN secrets in error messages from the driver (scrub user/password/host).
