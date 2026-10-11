#!/usr/bin/env python3
"""Mutation check of lmsql's guards: apply each mutation alone, run `cargo test --no-fail-fast`,
restore the file. Every mutation must make at least one test fail (exit 1 lists survivors).

Usage: python3 scripts/mutation-check.py   (from anywhere; takes a few minutes)
"""
import pathlib, subprocess, re, sys, os
os.chdir(pathlib.Path(__file__).resolve().parent.parent)
S = 'src/backend/sqlite.rs'; E = 'src/error.rs'
mutations = [
  ("M1 read-only open flag -> READ_WRITE", S, "    Mode::Read => base | OpenFlags::SQLITE_OPEN_READ_ONLY,", "    Mode::Read => base | OpenFlags::SQLITE_OPEN_READ_WRITE,"),
  ("M2 query_only not set", S, '  conn.pragma_update(None, "query_only", true)?;', '  let _ = conn;'),
  ("M3 stmt_readonly check removed", S, "    if !stmt.readonly() {", "    if false {"),
  ("M4 authorizer allows everything", S, "    authorize(mode, &ctx.action)\n  }));", "    let _ = (mode, ctx);\n    Authorization::Allow\n  }));"),
  ("M5 attach limit removed", S, "  conn.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0);\n  conn.set_limit(Limit::SQLITE_LIMIT_LENGTH", "  conn.set_limit(Limit::SQLITE_LIMIT_LENGTH"),
  ("M6 statement tail ignored", S, "    Ok(None) => Ok(stmt),", "    _ => Ok(stmt),"),
  ("M7 file: names not made literal", S, "  if looks_like_uri {", "  if looks_like_uri && false {"),
  ("M8 length limit removed", S, "  conn.set_limit(Limit::SQLITE_LIMIT_LENGTH, MAX_VALUE_BYTES);\n  conn.set_limit(Limit::SQLITE_LIMIT_SQL_LENGTH", "  conn.set_limit(Limit::SQLITE_LIMIT_SQL_LENGTH"),
  ("M9 heap limit not set", S, "    rusqlite::ffi::sqlite3_hard_heap_limit64(HEAP_LIMIT_BYTES);", "    let _ = HEAP_LIMIT_BYTES;"),
  ("M10 is_file check removed", S, "    Ok(meta) if meta.is_file() => Ok(()),", "    Ok(_) => Ok(()),"),
  ("M11 blank/:memory: refusal removed", S, '  if text.trim().is_empty() || text == ":memory:" {', "  if false {"),
  ("M12 pre-encoding size check removed", S, "    if bytes + lower_bound + 1 > limits.max_bytes {", "    if false {"),
  ("M13 busy wait not sharing the deadline", S, "  conn.busy_handler(Some(wait_until_deadline))?;", "  conn.busy_timeout(limits.timeout)?;"),
  ("M14 scrub not whole-token", E, "      let whole = !before.is_some_and(path_char) && !after.is_some_and(path_char);", "      let whole = { let _ = (before, after); true };"),
  ("M15 own messages scrubbed too", E, "    let message = if self.foreign {", "    let message = if true {"),
  ("M16 'unsafe use of' not a refusal", E, '(message.starts_with("not authorized") || message.starts_with("unsafe use of"))', '(message.starts_with("not authorized"))'),
  ("M17 read-only directory not mapped", E, "        if e.extended_code == ffi::SQLITE_READONLY_DIRECTORY\n          || e.extended_code == ffi::SQLITE_READONLY_CANTINIT =>", "        if false =>"),
]
originals = {f: pathlib.Path(f).read_text() for f in {S, E}}
for name, f, a, b in mutations:
  assert originals[f].count(a) == 1, (name, originals[f].count(a))
survivors = []
try:
  for name, f, a, b in mutations:
    pathlib.Path(f).write_text(originals[f].replace(a, b))
    p = subprocess.run(["cargo", "test", "--no-fail-fast"], capture_output=True, text=True)
    failed = sorted(set(re.findall(r"^test (\S+) \.\.\. FAILED", p.stdout, re.M)))
    compiled = "error[" not in p.stderr
    print(f"{name}: exit={p.returncode} compiled={compiled} failing={failed}", flush=True)
    if not (compiled and failed):
      survivors.append(name)
    pathlib.Path(f).write_text(originals[f])
finally:
  for f, t in originals.items():
    pathlib.Path(f).write_text(t)
print("SURVIVORS:", survivors)
sys.exit(1 if survivors else 0)
