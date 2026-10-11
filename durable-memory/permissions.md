# Permissions: the settings file (owner's model)

Settled model: permissions live in each tool's own settings file. A missing file = wide open
(the 0.2 behavior, unchanged). The tool layer only **restricts further**; it never grants more
than the database/OS allows, and the database role / file permission stays the real boundary.
No prompts; the CLI stays a stateless one-shot. A future harness only lists which lm* tools may
run; psql/sqlite3 are not on that list, so only the operator uses them (hence `manual_command`).

## Slice 1 (0.3.0, this branch)

- `src/settings.rs`: strict manual parsing of the version-1 schema; any problem is
  `SETTINGS_INVALID` naming the file and the key path (never fall back to open). Unknown keys are
  errors so a typo cannot silently drop a restriction. `permissions.default` is required (no
  implicit level). `~/` expands to `$HOME`; a relative sqlite path is relative to the settings file.
  `tables` on postgres is rejected (grants/roles are the tool there). `output.dir` and
  `allow_bypass` are parsed now; `output.dir` warns that it is not enforced yet.
- `connection::resolve_with`: file connections only (no `LMSQL_<NAME>` fallback once a file
  exists); `--db` gated by `allow_direct`.
- Levels: `none` refuses before connecting (and needs no DSN); `readonly` = SQLite
  `SQLITE_OPEN_READ_ONLY` + `query_only` (both: `PRAGMA query_only = OFF` still cannot write the
  read-only handle), PostgreSQL readonly login else readwrite login, and always
  `default_transaction_read_only = on` (weaker than a read-only role without the readonly login;
  documented). Writes refused that way (`SQLITE_READONLY`, PG `25006`/`42501`) are re-reported as
  the denial.
- Denial: `FORBIDDEN`/exit 6, `PERMISSION_DENIED`, one sentence (connection, rule, settings path),
  `error.data.manual_command` = `sqlite3 <path as written> <sql>` / `psql "$VAR" -c <sql>`
  (POSIX single-quoting; a `~/` prefix stays outside the quotes so the shell expands it; a literal
  DSN gets a placeholder, never the value).
- Tests run with `LMSQL_CONFIG` pinned to a nonexistent file (hermetic against a developer's own
  settings); `tests/settings.rs` is the functional matrix.

## Later slices (not started)

SQLite per-table levels via the authorizer; `--dry-run` (rollback) unified with lmrust's dryrun
global / `LMSQL_DRYRUN`; `--dangerously-bypass-permission-settings` gated by `allow_bypass`;
`output.dir` containment; the TTY-only `lmsql config` helper; mutation-check entries for the
settings gates.
