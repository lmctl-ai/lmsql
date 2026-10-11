# lmsql memory

Short decision notes; the code and README are the reference.

- [open-mode.md](open-mode.md): the CURRENT design — v1 runs any SQL; the policy seam; where the
  guards went. Start here.
- [big-results.md](big-results.md): stateless calls; a big result is written to a file (out /
  spill), never truncated or paged.
- [decisions.md](decisions.md): phase-1 SQLite design decisions (much still applies: type mapping,
  caps, connections).
- [safety.md](safety.md): HISTORICAL — the read-only guards and their mutation check, removed in
  open mode (in git history, planned to return behind config).
- [phase2.md](phase2.md): HISTORICAL — the original Postgres safety plan.
