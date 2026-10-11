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

## Slice 2 (0.3.0)

- **Table rules (SQLite)**: the authorizer (`install_table_rules`) checks the table of every
  Read / write / DDL action; `sqlite_*` bookkeeping is exempt. Installed only when a table rule
  differs from the default; the handle is read-only only when no table allows writes; the
  connection is refused outright only when nothing allows access (`TableRules::max`). Denials
  name the table and the key. `tables` hides `none` tables; `describe` refuses them up front.
- **Dry run** (owner's definition: estimate the blast radius, change nothing; no
  execute-then-roll-back): reuses lmrust's dryrun global. SQLite: `sqlite3_stmt_readonly` on every
  prepared statement; any write => none is stepped. PostgreSQL: no classifier — the text runs in a
  READ ONLY session; SQLSTATE 25006 => would change; plan via `EXPLAIN (FORMAT JSON)` on the
  extended protocol (one statement, no ANALYZE); `estimated_rows` from the ModifyTable's input
  node (the ModifyTable itself always estimates 0). No out/spill file. A write that the
  connection level forbids stays a denial.
- **Bypass**: a per-op boolean param (flag `--dangerously-bypass-permission-settings`), gated by
  `allow_bypass` in the operator's file because the LLM can see and pass the flag. Skips
  levels, table rules, the direct gate and output.dir; not the limits; one stderr line.
- **output.dir**: resolved at load (`~/`, relative to the settings file). `contain()` compares
  canonical paths (dangling symlinks followed too; a `..` in the not-yet-existing tail is refused).
  Spills go there. The slice-1 warning is gone.
- Mutation check: PS1–PS9 (fail closed x2, connection gate, table authorizer, direct gate, bypass
  gate, containment, dry run x2).

## Later slices (not started)

The TTY-only `lmsql config` helper (slice 3).
