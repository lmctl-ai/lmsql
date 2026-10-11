# Big results go to a file (owner decision)

Principle: functionality first, and lmsql is a **stateless CLI** — one process, one connection,
one statement batch per call; no sessions, no server cursors kept between calls, no pagination
tokens, nothing remembered. So a big result cannot be "continued" by a later call. Instead of
truncating it, lmsql writes it **in full to a file** and tells the caller where it is; the caller
reads or processes the file with its own tools (head, jq, wc, a script) or runs a narrower query.

## Decisions

- `out` (+ `outFormat`: ndjson | csv | json, default from the extension, else ndjson): always
  stream the full result there; the response is a summary + preview.
- No `out`: inline up to `maxRows` / `maxBytes` (now thresholds, flag names kept). Past either,
  spill the full result to `$LMSQL_OUT_DIR` (default `<temp>/lmsql/`) as ndjson. `truncated` stays
  as a legacy field (false).
- Exact thresholds: a result with exactly `maxRows` rows, or whose serialized `rows` array is
  exactly `maxBytes`, stays inline; one more spills.
- Preview: the first `maxRows` rows if given, else 20 (that is why `maxRows` declares no schema
  default: lmrust would inject it and the "given" signal would be lost).
- Streaming, bounded memory: backends push rows into `output::Sink`. SQLite steps row by row;
  PostgreSQL reads a single row-returning statement through `DECLARE CURSOR` + `FETCH` batches
  (64 → 2048). The simple protocol (multi-statement text, RETURNING) is returned whole by the
  sync driver — documented limitation.
- Atomic files: a `create_new` 0600 temp file next to the target, renamed on success; a `Drop`
  guard removes it on any error. An existing target is replaced (`overwrote: true`).
- `out` is preflighted (parent created, probe file created + removed) **before** the SQL runs, so
  a bad path never follows a committed write. A spill can still fail after a PostgreSQL autocommit
  write (documented).
- The default spill directory lives in the shared temp dir, so it must be a real directory owned
  by the effective uid with no group/other permissions; `LMSQL_OUT_DIR` (the user's choice) must
  only be a real directory.
- No deletion of old spill files (stateless; cleanup is the caller's job). No disk-space cap yet:
  a future `Policy` config item.
- CSV: RFC 4180, CRLF, header row; NULL = empty field, empty string = `""`.
- Dev/test profile is optimized (opt-level 1, deps 2): the 5M-row tests take seconds, not minutes.

## Evidence

5M rows: SQLite ~5 s, PostgreSQL ~9 s, peak RSS ~5 MiB for every format and for spills (tests in
`tests/output.rs` read the child's RSS with `wait4`, limit 48 MiB). Mutation check: 8 mutants,
0 survivors.
