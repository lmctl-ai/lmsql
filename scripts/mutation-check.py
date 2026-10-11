#!/usr/bin/env python3
"""Mutation check of lmsql's open-mode invariants: big results go to a file (spill, atomic write,
private permissions, out checked before the SQL runs, bounded memory), the timeout, and secret
scrubbing. Apply each mutation alone, run `cargo test --no-fail-fast`, restore the file. Every
mutation must make at least one test fail (exit 1 lists survivors).

The PostgreSQL mutant is only exercised when LMSQL_TEST_PG_URL is set (otherwise the pg tests skip
and it survives); set it (and keep a throwaway server up) to check that one too.

Usage: python3 scripts/mutation-check.py   (from anywhere; takes a few minutes)
"""
import pathlib, subprocess, re, sys, os
os.chdir(pathlib.Path(__file__).resolve().parent.parent)
S = 'src/backend/sqlite.rs'
P = 'src/backend/postgres/ops.rs'
E = 'src/error.rs'
O = 'src/output.rs'

mutations = [
  # A result past the inline threshold stays inline instead of spilling to a file.
  ("OM1 spill disabled", O,
   "    if self.inline.len() < self.config.inline_rows",
   "    if true || self.inline.len() < self.config.inline_rows"),
  # SQLite --timeout-ms disabled: a runaway statement is never interrupted.
  ("OM2 sqlite timeout handler disabled", S,
   "Some(move || Instant::now() >= deadline)",
   "Some(move || { let _ = deadline; false })"),
  # A failed call leaves its partial temporary file behind.
  ("OM3 temp file kept on error", O,
   "      let _ = std::fs::remove_file(temp);",
   "      let _ = temp;"),
  # Secret scrubbing disabled: a database path / DSN leaks into an error message.
  ("OM4 scrubber disabled", E,
   'result.push_str(if whole { "<db>" } else { secret });',
   "result.push_str(secret);"),
  # The shared default spill directory is used even when others can write to it.
  ("OM5 spill dir privacy check removed", O,
   "    if meta.uid() != me || meta.mode() & 0o077 != 0 {",
   "    if false && (meta.uid() != me || meta.mode() & 0o077 != 0) {"),
  # out is not checked before the SQL runs: a write commits, then the file fails.
  ("OM6 out preflight removed", O,
   "        preflight(&path)?;",
   "        let _ = &path;"),
  # Result files readable by everyone.
  ("OM7 file mode not 0600", O,
   "    options.mode(0o600);",
   "    options.mode(0o644);"),
  # PostgreSQL fetches the whole result at once (memory grows with the result).
  ("OM8 pg cursor fetches everything at once", P,
   'format!("FETCH FORWARD {batch} FROM {CURSOR}")',
   'format!("FETCH ALL FROM {CURSOR}")'),
]

files = {S, P, E, O}
originals = {f: pathlib.Path(f).read_text() for f in files}
for name, f, a, b in mutations:
  assert originals[f].count(a) == 1, (name, originals[f].count(a))

survivors = []
try:
  for name, f, a, b in mutations:
    pathlib.Path(f).write_text(originals[f].replace(a, b))
    r = subprocess.run(["cargo", "test", "--no-fail-fast"], capture_output=True, text=True)
    failed = sorted(set(re.findall(r"^test (\S+) \.\.\. FAILED", r.stdout, re.M)))
    compiled = "error[" not in r.stderr
    print(f"{name}: exit={r.returncode} compiled={compiled} failing={failed}", flush=True)
    if not (compiled and failed):
      survivors.append(name)
    pathlib.Path(f).write_text(originals[f])
finally:
  for f, t in originals.items():
    pathlib.Path(f).write_text(t)
print("SURVIVORS:", survivors)
sys.exit(1 if survivors else 0)
