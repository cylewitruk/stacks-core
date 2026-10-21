# Round-6 results (bounded reruns after the sixth review)

`IMPACT.md` is the impact matrix written before any rerun. Everything here
was produced on the binaries of `scripts/build-rewrite.sh` and
`scripts/build-reference.sh` at tree revision `e745a61b70` (manifests in
`ENV.txt`), on an idle machine, with no build or test running during the
timing batch.

* `ablate-<provider>-std-rep<N>.json/.log`: `scripts/run-r6.sh`, the normal
  reference (`reference`), the repaired late-probe diagnostic
  (`reference-lateprobe`) and the rewrite rotated within each of 5
  repetitions (one process each), capacities 64 and 4,096, isolated cells,
  `std` workload; every cold and warm pass verified against the model
  outside its timed region (the round-5 rows verified only the final warm
  pass). `ablate-*-counters.json/.log`: the counter binaries, one process
  each; the diagnostic's `late_probe_extra_parses` counts the probes made
  after a mapped patch record was parsed. `TABLES-ablate.md`: complete
  output of `scripts/tables.py ablate` over this directory.
* `profile/`: scoped callgrind profiles (`scripts/profile.sh`): collection
  restricted to the harness's timed-pass function of one phase (warm or
  cold), one key set and one capacity per process; raw `.callgrind.out.xz`,
  complete `callgrind_annotate` outputs (inclusive and self), harness and
  valgrind logs, `MANIFEST` (binary hashes, valgrind version, the exact
  command of every profile). `SUMMARY.md` is the output of
  `scripts/profile-summary.py profile/`.
* `validation/`: nextest logs for the round-6 changes.
* `../../../clarity-bench/results/r6/evidence/clarity-provenance.log`: the
  Clarity variant builder run from a fresh clone into a new empty `WORK`.

Round-5 results (`../r5/`) are unchanged; their ablation rows keep their
original provenance (final warm pass verified).
