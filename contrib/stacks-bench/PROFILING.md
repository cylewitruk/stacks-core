# Storage and commit profiling

Normal builds include fixed-name wall/thread-CPU spans for sealing, trie serialization,
blob sync/mapping, value publication, SQLite commit, hash publication and advance-tip work.
The surrounding Segment spans distinguish Clarity from header-index commits without
file paths, block hashes or keys as profiler tags. These timings do not change durability,
read-ahead or database behavior.

Use a profiled historical replay with `--no-profiler-kv`. Do **not** pass
`--bench-spans-only` (alias `--no-profiler`) when investigating storage: that option
prunes storage spans from the database after collecting them. No
`commit-residency-diagnostics` feature or `STACKS_WRITEBACK_DIAGNOSTICS` environment
variable is needed for the main commit breakdown. Leave the latter unset to avoid
verbose per-transaction diagnostic logs.

For a commit-focused export, existing repeatable `--span` filters can retain:

```text
--span 'Seal:*' --span 'Flush:*' --span 'Commit:*'
--span 'Clarity commit:*' --span 'Values:*' --span 'Direct hash:*'
--span 'Headers index:*' --span 'Advance tip:*' --span 'Writeback: MARF insert batch' --span 'MARF:*'
```

Segment and Transaction spans are always retained. Filters reduce persistence volume;
they do not disable collection. A threshold can hide short but frequently called work,
so start without one when comparing totals. The global `--no-profiler-kv` switch also
omits attached counters, not span timing.

## Sampling and interpretation

- Persisted-node/patch reads: 1/256 calls.
- Persisted-hash reads, value-record reads and legacy SQLite value lookups: 1/64 calls.
- Optional direct-hash diagnostic lookups: 1/256 calls; cache hits use aggregate counters.
- Commit barriers and the main commit phases: every invocation, including rare slow syncs.

Sampled read spans contain **observed time and observed call counts**, not full totals.
Unsampled read calls do not allocate count-only rows or suppress nested probes.
Rare mapping-growth spans and optional diagnostic counters remain observable; children
of an unsampled read attach to the nearest active ancestor. Do not extrapolate them using
`call_count / sample_count`: both count only observed calls for these probes. Compare
sample averages/distributions alongside the fully timed parent. Borrowed mmap reads may
fault later when the caller accesses returned bytes; these spans are not fault counters.
Repeated calls at the same parent/callsite aggregate into one node; no per-node, per-key,
or per-value tags are emitted. Sampling reduces hot clock probes and sparse transaction
rows, but does not promise a 64x/256x reduction in database size.

Wall minus thread CPU is a wait indicator, not an I/O measurement. A long SQLite commit
or sync span identifies where to correlate OS blocked stacks and device latency; a long
read span can motivate page-fault tracing. Never add inclusive parents to their children.

`Segment: Setup` now covers the setup inside segment execution rather than only the
initial transaction classification; older versions of that span are not comparable.
The independent setup/execution/commit metric boundaries are unchanged. Index Commit
still includes builder cleanup, now visible as a child. Commit metadata covers the
burn-view, snapshot and reward preparation previously left between phase spans.
