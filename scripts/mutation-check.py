#!/usr/bin/env python3
"""Mutation check of lmsql's open-mode operational caps (the protections that remain in v1 after
the SQL guards were removed): apply each mutation alone, run `cargo test --no-fail-fast`, restore
the file. Every mutation must make at least one test fail (exit 1 lists survivors).

The PostgreSQL mutant is only exercised when LMSQL_TEST_PG_URL is set (otherwise the pg tests skip
and it survives); set it (and keep a throwaway server up) to check that one too.

Usage: python3 scripts/mutation-check.py   (from anywhere; takes a few minutes)
"""
import pathlib, subprocess, re, sys, os
os.chdir(pathlib.Path(__file__).resolve().parent.parent)
S = 'src/backend/sqlite.rs'
P = 'src/backend/postgres/ops.rs'
E = 'src/error.rs'

mutations = [
  # SQLite --max-rows cap removed: the fetch would not stop at the cap.
  ("OM1 sqlite row cap removed", S,
   "    if rows.len() == limits.max_rows {",
   "    if false {"),
  # SQLite --timeout-ms disabled: a runaway statement is never interrupted.
  ("OM2 sqlite timeout handler disabled", S,
   "Some(move || Instant::now() >= deadline)",
   "Some(move || { let _ = deadline; false })"),
  # PostgreSQL cursor cap ignored: a huge SELECT keeps fetching instead of stopping at the cap.
  ("OM3 pg cursor cap ignored", P,
   "        if !sink.push(&texts) {\n          return Ok(());\n        }",
   "        {\n          let _ = sink.push(&texts);\n        }"),
  # Secret scrubbing disabled: a database path / DSN leaks into an error message.
  ("OM4 scrubber disabled", E,
   'result.push_str(if whole { "<db>" } else { secret });',
   "result.push_str(secret);"),
]

files = {S, P, E}
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
