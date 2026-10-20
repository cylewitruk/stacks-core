# Round-3 originals (historical evidence)

Logs behind the tables in `docs/marf-rewrite/STATUS.md` ("Allocation and
time per operation", "Lookups qualified by walk shape", "Pruning memory"),
produced on 2026-10-20 by the `alloc` command of `marf-differential` at
revisions before the fourth review:

* `v2-upstream-*.log`, `v2-reference-*.log`: `REPEAT=3`, best-of-three
  timing (the harness of that round kept only the best), three storage
  configurations; `v2-reference-*` built in a worktree of the reference
  with the harness copied in by hand (the overlay procedure was scripted
  only in round 4).
* `v2-big-new.log`, `v2-before-prune-default.log`, `v2-big-before-prune.log`:
  the pruning-memory comparison (pre-fix binary built at `b35cf463b4`).
* `qualified-upstream.log`, `qualified-reference.log`: the first
  walk-shape bucket runs, made while a test build ran on the same machine
  (their caveat is in the STATUS history); superseded by `v2-*`.

These files were recovered from the session's scratch directory: they were
written outside the repository and never committed in round 3 (the report
of that round cited them without publishing them). Individual timing
samples do not exist for them: the harness of that round recorded only
aggregates. The counting allocator was compiled into every binary of that
round, so its timings carry that overhead.
