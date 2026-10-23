# Round-4 originals (historical evidence)

Logs behind the tables of the fourth-review `docs/marf-rewrite/PERF.md`,
produced on 2026-10-20 by the checked-in scripts at revision `42c2750206`
(`ENV.txt`: revision, toolchain, machine, load before and after). They were
written outside the commits of that round (the repository's `*.log` ignore
rule) and are published here unchanged; the fifth review's corrected runs
are in `../r5/`.

* `alloc-*.log`: committed reads (`alloc` command), three storage
  configurations, upstream and reference binaries;
  `alloc-counts-upstream-external-compressed.log` is the counting-allocator
  binary.
* `ablate-std-*.log`, `ablate-big-*.log`: the patch-cache ablation. Known
  defects, corrected in round 5: one `MARF` handle per capacity was shared
  by every key set in sequence, so a set's "cold" pass inherited cache
  entries and page residency from the sets before it and the reference's
  figures at capacity 4,096 include the cost of entries left by earlier
  sets (`../r5/` measures isolated cells and keeps this scheme as the
  separately named `shared` experiment); the counter tables mapped the
  reference's `resolved_patch_cache_misses` (admissions) to "misses" and
  printed the rewrite-only SQL counters as 0 for the reference.
* `ablate-std-pagecold-*.log`: `DROP_CACHES=1`, same shared-handle scheme
  (the drop happened once per capacity, not per cell).
* `construct-*.log`: reads during block construction, four configurations;
  `construct-counters-*.log` the counter binaries. The "inherited wide
  (distinct once)" phase read keys that were not all distinct (fixed in
  round 5).
* `validation/`: the nextest logs of that round.

Individual samples do not exist for any of these runs: the harness printed
medians and min-max only. The round-5 runs (`../r5/*.json`) carry every
sample.
