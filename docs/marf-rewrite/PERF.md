# Performance evidence (fourth review)

This document answers why the rewrite outperforms the optimized reference
on committed reads, and whether that holds for reads during block
construction and for real Clarity execution. Everything here is reproducible
from checked-in scripts; raw logs are committed under
`contrib/marf-differential/results/` and `contrib/clarity-bench/results/`.

Vocabulary: **measured** = read off a log in this round; **inferred** = a
conclusion the measurements support but do not state directly; **open** = a
hypothesis the experiments here do not settle.

## 1. Targets, builds, environment

| target | source | how it is built |
|---|---|---|
| upstream | `stacks-marf` at `ad5c57786bb51b58d4bdb4992ad534f762d8cf03`, in this tree | `contrib/marf-differential/scripts/build-rewrite.sh` (oracle "base" of the in-tree harness) |
| reference | `stacks-marf` at `29ef093da5dd58c1fa484624d94a9d972356c629` | `contrib/marf-differential/scripts/build-reference.sh`: a worktree of that commit, this tree's `crates/stacks-marf-next` and `contrib/marf-differential` copied in, `contrib/marf-differential/reference/*.patch` applied (adapter differences, production options, counters; see `reference/README.md`) |
| rewrite | `crates/stacks-marf-next` at the final tested commit (section 8) | in-tree, "next" of both harness binaries |

Options. Upstream: `MARFOpenOpts::new(Deferred, external).with_compression(c)`
(it has no mmap or caches). Reference: its production settings from
`Config::get_marf_opts` on that branch, set explicitly by the harness
(`with_mmap(true)`, `root_node_cache = false`,
`resolved_patch_cache_capacity = 64`, `result_cache_capacity = 2048`); the
bare constructor's defaults (16-entry patch cache, no mmap) are not used.
Rewrite: mmap always, `patch_cache_capacity = 4096` (production), no result
cache; the ablation sets the capacity explicitly through the new
`MARFOpenOpts::with_patch_cache_capacity`.

Build profiles and instrumentation. All timing binaries are `--release`
with no instrumentation: the counting allocator (`alloc-count` feature of
the harness) and the read counters (`counters` feature; for the reference
this switches on its own `marf-read-bench-counters` feature, otherwise
configured out) are separate binaries used only for the runs whose logs say
so. Every log starts with the build and machine line the harness prints.

Machine: one container, 4 vCPU Intel Xeon 2.80 GHz, 15 GB, Linux 6.18,
`rustc 1.98.0`. Nothing else ran during the measurement batches (the
scripts record the load average before and after). Cold is defined per
experiment: "process-cold" = a fresh `MARF` open in the same process (every
process-local cache empty, the blobs mapping created by that open), with the
OS page cache warm from building the database; "page-cold" = the same after
`DROP_CACHES=1` wrote 3 to `/proc/sys/vm/drop_caches` (root).

Repetitions: `REPEAT=5` in-process repetitions on fresh databases, medians
reported with min-max; where a binary runs both implementations (`alloc`)
their order alternates between repetitions; across binaries the run scripts
alternate the order between passes. The 1M-key `big` ablation workload runs
fewer repetitions where noted.

## 2. Findings fixed (section 1 of the review)

* `PruneStats.blob_size`: `Store::write_shared_blob_with` returned the
  blob's starting offset and `prune_to_path` reported it as the length. It
  now returns `(offset, length)`. Regression test
  `pruning::tests::prune_stats_report_the_blob_length`: the reported size
  equals the `external_length` published in `marf_data` and, for a fresh
  destination, the blobs file length; reads and the root hash match the
  source, and proofs are refused as on every pruned database.
* Result-cache explanation: the reference's result cache serves only the
  active mutable trie (`cached_result`/`cache_result` in its `storage.rs`),
  so the committed-read working-set patterns never involved it. The
  harness comments and `STATUS.md` now describe hot64/fit/overflow as
  locality variations; the cache's real behaviour is measured during block
  construction (section 5), with counters.

## 3. Experiment matrix

Status of every configuration the review asked for. "done" means a
complete log exists under the path given in each section.

| experiment | configuration | status |
|---|---|---|
| committed reads (`alloc`) | external compressed, external plain, SQLite plain; upstream and reference binaries, rewrite in each | done (6 logs) |
| allocation and peak heap | external compressed, counting-allocator binary | done |
| patch-cache ablation `std` (40k keys) | capacities 0 / 64 / 4,096 / production; rewrite, reference, upstream | done |
| ablation, plain-encoding control | `std` workload uncompressed; all three | done |
| ablation, patch-free control | first block's keys at the first tip, in every ablation run | done |
| ablation, page-cold | `DROP_CACHES=1`; rewrite, reference | done |
| ablation, counters | `std` and `big`; rewrite and reference counter binaries | done |
| ablation `big` (1M keys, 6,200 distinct patched nodes at the tip) | capacities 0 / 64 / 4,096 / production; rewrite, reference | done (upstream not run on `big`: at its 0.1-1.3 ms per lookup the run would take hours and it has no cache to ablate) |
| working sets by distinct patched nodes | 32, 64, 1,024 (`std`); 32, 64, 1,024, 4,096, 6,200 (`big`) | done |
| reads during block construction | rewrite; reference with result cache 2,048 and 0; upstream | done |
| construction counters | rewrite; reference with result cache 2,048 and 0 | done |
| Clarity execution | rewrite; upstream; optimized MARF under upstream's consumer code; two passes in alternating order | done (6 logs; equivalence lines identical) |
| Clarity, whole optimized stack (optional) | worktree of 29ef093d as committed | not built in this round (the variant builder has the switch; its stackslib API differs and it is reported separately when it is) |
| validation | crate + differential tests; all 132 hand-off boundaries both ways; consumer prune tests; counters build | VALIDATION_STATUS |

Raw logs: `contrib/marf-differential/results/r4/` (one file per run; `ENV.txt`
has the revision, toolchain, machine and load before and after) and
`contrib/clarity-bench/results/r4/`.

## 4. Committed reads: patch-cache ablation

`marf-differential ablate <impl> <std|big> [reads] [plain]`, logs
`results/r4/ablate-*.log`. One implementation per process; the database is
built once per process, key sets are chosen by walk shape with the rewrite's
profiler on the shared files (the profiler also records which patched
records each walk meets, so a set's number of distinct patched nodes is
known), and for every capacity the database is reopened fresh and each set
is read once cold and then 5 warm passes, 5 repetitions, the capacity order
rotating between repetitions. Values are collected inside the timed loop and
verified against the model after it. `n keys` are distinct keys. Counters
come from separate binaries (`ablate-*-counters-*.log`).

The reference at capacity 0 bypasses its `SharedLru` (its `enabled()`
check); the rewrite at 0 skips lookup and admission and keeps only the one
scratch slot a resolved chain is returned from. Both verified by counters:
`patch_cache_lookups`, `hits` and `misses` are all zero at capacity 0 in
every counter log.

**Workload `std`** (warm median µs per lookup, 5 passes x 5 repetitions; cold = first pass after a fresh open, in parentheses for the production column):

| set | n keys | distinct patched nodes | hops | patched/walk | depth | upstream (no cache) | ref 0 | ref 64 (prod) | ref 4096 | rw 0 | rw 64 | rw 4096 (prod) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| patchfree@tip1 | 2,000 | 0 | 0.00 | 0.00 | 0.00 | 20.9 (22.1) | 7.8 | 7.8 (7.9) | 7.7 | 1.9 | 1.9 | 2.0 (2.2) |
| bulk tip (random keys) | 5,000 | 677 | 1.00 | 2.05 | 4.00 | 1305.6 (1279.2) | 33.8 | 11.3 (11.3) | 8.1 | 37.9 | 6.2 | 1.6 (2.2) |
| tip hops=2 depth=3-4 | 5,000 | 365 | 2.00 | 2.05 | 3.50 | 163.2 (150.5) | 19.2 | 9.6 (9.5) | 8.0 | 13.6 | 5.0 | 1.3 (1.8) |
| tip hops=2 depth=1-2 | 5,000 | 438 | 2.00 | 1.71 | 2.00 | 122.8 (112.1) | 15.0 | 7.6 (7.5) | 11.0 | 10.6 | 3.3 | 2.0 (1.9) |
| tip hops=1 depth=1-2 | 5,000 | 336 | 1.00 | 1.63 | 2.00 | 115.9 (116.2) | 14.4 | 6.8 (7.0) | 11.1 | 10.2 | 2.0 | 1.8 (2.0) |
| patched(nodes<=32) | 8,192 | 32 | 1.75 | 1.35 | 2.21 | 119.9 (118.7) | 14.3 | 6.8 (8.8) | 11.6 | 10.5 | 2.2 | 2.3 (2.4) |
| patched(nodes<=64) | 8,192 | 64 | 1.78 | 1.53 | 2.38 | 122.8 (137.8) | 15.3 | 6.7 (6.7) | 11.5 | 11.0 | 2.1 | 2.0 (2.1) |
| patched(nodes<=1024) | 8,192 | 995 | 1.86 | 1.92 | 2.71 | 135.1 (133.8) | 16.1 | 11.5 (10.9) | 11.2 | 13.6 | 5.1 | 1.8 (1.9) |

**Workload `big`** (warm median µs per lookup, 5 passes x 5 repetitions; cold = first pass after a fresh open, in parentheses for the production column):

| set | n keys | distinct patched nodes | hops | patched/walk | depth | upstream (no cache) | ref 0 | ref 64 (prod) | ref 4096 | rw 0 | rw 64 | rw 4096 (prod) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| patchfree@tip1 | 4,000 | 0 | 0.00 | 0.00 | 0.00 | - | 8.8 | 8.6 (8.4) | 8.5 | 2.2 | 2.2 | 2.2 (2.4) |
| bulk tip (random keys) | 5,000 | 3,770 | 1.87 | 2.60 | 4.00 | - | 37.6 | 14.7 (16.6) | 25.7 | 42.3 | 9.1 | 2.6 (6.2) |
| tip hops=2 depth=3-4 | 5,000 | 3,846 | 2.00 | 2.57 | 4.00 | - | 37.3 | 14.3 (16.3) | 26.3 | 40.0 | 8.0 | 2.6 (5.4) |
| tip hops=3+ depth=3-4 | 5,000 | 2,745 | 3.00 | 2.62 | 4.00 | - | 32.6 | 10.1 (10.4) | 19.7 | 39.6 | 5.5 | 2.2 (3.9) |
| tip hops=1 depth=3-4 | 5,000 | 3,970 | 1.00 | 2.61 | 4.00 | - | 32.5 | 13.7 (13.3) | 25.8 | 39.2 | 7.9 | 2.5 (4.9) |
| patched(nodes<=32) | 8,192 | 32 | 2.03 | 1.26 | 4.00 | - | 31.0 | 9.8 (11.1) | 30.0 | 37.4 | 2.9 | 3.4 (3.4) |
| patched(nodes<=64) | 8,192 | 64 | 2.03 | 1.44 | 4.00 | - | 33.1 | 9.8 (9.5) | 29.6 | 38.3 | 2.9 | 2.8 (3.0) |
| patched(nodes<=1024) | 8,192 | 1,024 | 2.01 | 1.95 | 4.00 | - | 34.0 | 12.7 (12.9) | 22.1 | 40.0 | 7.2 | 2.6 (3.4) |
| patched(nodes<=4096) | 8,192 | 4,096 | 2.02 | 2.38 | 4.00 | - | 33.9 | 13.9 (14.4) | 24.8 | 39.7 | 8.0 | 4.4 (4.1) |
| patched all | 8,192 | 6,200 | 2.02 | 2.59 | 4.00 | - | 33.8 | 14.4 (17.3) | 34.4 | 40.9 | 8.6 | 4.8 (4.6) |

Counters (warm passes, first repetition), `std` workload, bulk-tip set
(677 distinct patched nodes, 25,000 lookups per 5 passes):

| capacity | implementation | node visits | patch-cache lookups | hits | misses (resolutions) | patch applications | record reads |
|---|---|---|---|---|---|---|---|
| 0 | rewrite | 86,440 | 0 | 0 | 51,280 | 164,880 | 337,760 |
| 0 | reference | 86,440 | 0 | 0 | (resolved inline) | 164,880 | 251,320 |
| 64 | rewrite | 86,440 | 86,440 | 27,600 | 23,680 (370 clears) | 58,600 | 176,280 |
| 64 | reference | 86,440 | 51,280 | 30,610 | 20,670 | 49,940 | 105,770 |
| 4,096 | rewrite | 86,440 | 86,440 | 51,280 | 0 | 0 | 70,320 |
| 4,096 | reference | 86,440 | 51,280 | 51,280 | 0 | 0 | 35,160 |

`big` workload, widest sets (warm, 40,960 lookups per 5 passes):

| set | capacity | implementation | patch-cache hits | misses (resolutions) | hit rate |
|---|---|---|---|---|---|
| 4,096 distinct nodes | 64 | rewrite | 43,196 | 54,249 (848 clears) | 44% |
| 4,096 distinct nodes | 64 | reference | 46,985 | 50,460 | 48% |
| 4,096 distinct nodes | 4,096 | rewrite | 76,965 | 20,480 (5 clears) | 79% |
| 4,096 distinct nodes | 4,096 | reference | 97,445 | 0 | 100% |
| 6,200 distinct nodes | 64 | rewrite | 42,323 | 63,667 (995 clears) | 40% |
| 6,200 distinct nodes | 64 | reference | 45,810 | 60,180 | 43% |
| 6,200 distinct nodes | 4,096 | rewrite | 73,704 | 32,286 (8 clears) | 70% |
| 6,200 distinct nodes | 4,096 | reference | 76,570 | 29,420 | 72% |

What this establishes (measured unless marked):

* **Identical walks.** Node visits and back-pointer follows are the same
  for both implementations in every set (the trie is the same, read the
  same way), so the comparison is of implementation cost per visit, not of
  work avoided.
* **The patch-free control.** With no patched node on the path and no
  cache involved, the rewrite costs 1.9-2.2 µs per lookup, the reference
  7.7-8.8, upstream 21-22. That 4x against the reference is traversal and
  record access, not caching: the plain-encoding control says the same
  (final-tip walks: rewrite 3.5-3.9, reference 7.9-8.5, upstream 23-26;
  the rewrite is slower on plain than on compressed records because a
  plain `Node256` body is 2.6 KB and its slot list is skip-scanned).
* **Capacity equalized.** At 4,096 entries both caches reach 100% hits on
  every `std` set; the reference then costs 7.7-11.6 µs, the rewrite
  1.3-2.3. At 64 entries on sets wider than 64 patched nodes both caches
  thrash at similar rates (rewrite 44-54% hits with its clear-all policy,
  reference 48-60% with LRU) and the reference costs 6.7-14.7 µs, the
  rewrite 2.0-9.1. Equal capacities equalize hit rates to within a few
  points; they do not close the gap.
* **The reference's capacity is not a tunable left on the table.** Raised
  to 4,096 its lookups get slower, not faster, on `big` (22-34 µs against
  9-15 at 64, on sets with only 32 distinct patched nodes too), and on
  `std` it loses on every set but the two widest. Its production 64 is the
  right value for its implementation; the cost of its cache lookup grows
  with capacity (inferred from the timings; its `SmallLru` is scanned).
* **The rewrite's capacity does matter for the rewrite.** On sets wider
  than 64 patched nodes its 4,096 entries are worth 2.5-4x over 64 (`std`
  bulk tip: 1.6 vs 6.2 µs; `big` bulk tip: 2.6 vs 9.1). Above its capacity
  (6,200 distinct nodes) the clear-all policy keeps 70% hits and the cost
  is 4.8 µs; disabled, 34-42 µs. So the patch cache is necessary for the
  rewrite's patched-path numbers, but the comparison at equal capacity
  shows it is not what separates the two implementations.
* **Replacement policy.** The rewrite empties its map when full (no LRU
  order to maintain); the reference evicts LRU. At 4,096 on the
  4,096-node set this costs the rewrite 21 points of hit rate (79% against
  100%) and it is still 5.6x faster (4.4 vs 24.8 µs). Inferred: an LRU in
  the rewrite would help wide working sets slightly, but the per-hit cost
  is what matters.
* **Two record parses per plain node in the rewrite** (60,580 reads for
  30,290 visits on patch-free walks: the resolver re-parses the terminal
  record after its loop). A cheap inefficiency the reference does not have;
  left as is during this round so the measured binary matches the tested
  commit (listed under follow-ups).
* **Cold.** Process-cold first passes cost the rewrite 0.1-0.6 µs more
  than warm on `std` and up to 3.6 µs more on `big` (bulk tip: 6.2 cold vs
  2.6 warm: the patch cache fills); page-cold (`ablate-std-pagecold-*`)
  adds first-touch page faults on the mapping: rewrite bulk tip 5.3 µs on
  the first pass against 2.2 process-cold, then warm figures. The reference
  page-cold: 11.4-16.2 first pass against 8-11 process-cold. With a ~10 MB
  database the mapping is resident after one pass, so these say nothing
  about a mainnet-sized file (open).

## 5. Reads during block construction

`marf-differential construct <impl> [result-cache <n|off>]`, logs
`results/r4/construct-*.log`. A 20-block, 40,000-key base chain, then one
child block kept open while reads interleave with writes: a hot set of 64
inherited keys cycled, 5,000 distinct inherited keys once, 500 inserts then
reads of them (hot and all), 200 updates of inherited keys then reads
expecting the new values, 500 misses then insertion of those keys then
reads, one key updated 100 times with a read after each, 2,000 reads at
the parent tip and 2,000 at block 10's tip while the child is open, the
hot set again, seal, commit; then a rollback (begin on the child, 500
inserts, reads, `drop_current`, the block must be gone and the writes
invisible at the parent), the same block rebuilt and committed. Every read
result is collected inside the timed region and checked after it against a
model (exact values, absence for misses). Final roots of the child and of
the rebuilt block are identical across all four runs
(`f4ff08dc…`, `195883d9…`). 5 repetitions, medians with min-max.

| phase | kind | ops | rewrite µs/op | reference, result cache 2,048 | reference, result cache 0 | upstream |
|---|---|---|---|---|---|---|
| begin | write | 1 | 0.7 ms | 0.5 ms | 0.4 ms | 4.1 ms |
| inherited hot64 (cycled) | read | 4,992 | 0.96 (0.90-2.04) | 0.69 (0.64-0.95) | 2.07 (1.99-3.87) | 57.90 (53.78-68.04) |
| inherited wide (distinct once) | read | 5,000 | 2.95 (2.35-3.91) | 8.97 (8.78-11.21) | 8.43 (8.05-14.13) | 54.72 (48.80-56.59) |
| insert 500 new | write | 500 | 2.42 (2.01-3.09) | 9.91 (9.33-14.17) | 9.73 (8.85-18.93) | 30.23 (27.81-31.69) |
| inserted hot64 (cycled) | read | 4,992 | 0.56 (0.54-1.10) | 0.64 (0.52-0.74) | 0.94 (0.88-1.02) | 3.39 (3.26-3.86) |
| inserted all 500 (x10) | read | 5,000 | 0.75 (0.57-1.32) | 0.66 (0.61-1.09) | 1.02 (0.91-1.06) | 3.46 (3.31-4.88) |
| update 200 inherited | write | 200 | 2.65 (2.13-3.49) | 7.87 (7.03-13.35) | 6.97 (6.83-10.22) | 19.52 (18.13-20.45) |
| updated 200 (x25) | read | 5,000 | 0.64 (0.54-1.16) | 0.59 (0.54-0.92) | 0.90 (0.89-0.99) | 3.37 (3.29-4.70) |
| misses 500 (x10) | read | 5,000 | 0.78 (0.63-1.41) | 0.72 (0.66-1.16) | 1.08 (0.99-1.92) | 9.11 (7.89-12.52) |
| insert the 500 missed | write | 500 | 2.38 (1.63-3.56) | 6.10 (5.32-11.59) | 5.18 (4.71-11.04) | 12.92 (11.17-14.70) |
| formerly missed 500 (x10) | read | 5,000 | 0.83 (0.58-1.25) | 0.63 (0.60-1.02) | 0.96 (0.91-2.05) | 3.60 (3.42-3.62) |
| successive updates x100: writes | write | 100 | 6.08 (5.78-11.31) | 15.01 (13.75-29.29) | 15.14 (14.14-28.08) | 21.17 (19.94-21.46) |
| successive updates x100: reads | read | 100 | 0.69 (0.58-1.13) | 1.20 (1.07-2.06) | 1.01 (0.98-1.99) | 3.23 (3.11-3.43) |
| ancestor reads: parent tip 2000 | read | 2,000 | 2.07 (1.88-3.23) | 8.73 (8.06-12.70) | 8.65 (8.42-15.15) | 1260.86 (1151.01-1280.06) |
| ancestor reads: block-10 tip 2000 | read | 2,000 | 1.94 (1.85-3.69) | 6.72 (6.48-7.24) | 6.56 (6.52-10.89) | 1271.76 (1251.89-1721.70) |
| inherited hot64 after ancestor reads | read | 4,992 | 0.77 (0.71-1.56) | 0.55 (0.54-0.66) | 1.40 (1.35-2.89) | 8.67 (7.95-10.40) |
| seal | write | 1 | 16.6 ms | 17.5 ms | 17.2 ms | 17.2 ms |
| commit | write | 1 | 4.7 ms | 6.4 ms | 5.8 ms | 16.1 ms |
| whole construction (begin..commit) | total | 1 | 85.4 ms | 139.1 ms | 156.9 ms | 5882.1 ms |
| after commit: updated 200 | read | 200 | 8.67 (7.62-13.18) | 11.96 (11.67-14.01) | 12.27 (11.22-23.87) | 56.16 (50.99-62.46) |
| rollback: reads before drop | read | 500 | 1.05 (0.71-1.80) | 1.36 (1.31-1.50) | 1.12 (1.05-2.06) | 2.50 (2.38-3.69) |
| rollback: begin+insert+drop | write | 500 | 7.29 (5.26-8.90) | 12.99 (12.25-13.61) | 12.64 (11.87-20.41) | 34.19 (31.67-36.09) |
| rebuilt after rollback: reads | read | 500 | 4.73 (4.64-7.50) | 7.56 (6.65-10.40) | 7.22 (6.59-11.16) | 49.52 (46.37-67.84) |

`rewrite`: sum of medians: reads 53.910 ms, writes 29.200 ms (whole construction above includes both)

`ref rc2048`: sum of medians: reads 105.084 ms, writes 41.973 ms (whole construction above includes both)

`ref rc0`: sum of medians: reads 121.096 ms, writes 40.113 ms (whole construction above includes both)

`upstream`: sum of medians: reads 5823.299 ms, writes 82.005 ms (whole construction above includes both)

| phase | reference rc2048: result-cache hits / misses / invalidations | node visits (ref rc2048 / ref rc0 / rewrite) | patch-cache hits/misses (ref rc2048) | patch-cache hits/misses (rewrite) |
|---|---|---|---|---|
| begin | 0 / 0 / 0 | 4 / 4 / 4 | 3/7 | 3/7 |
| inherited hot64 (cycled) | 4928 / 64 / 0 | 219 / 17082 / 17082 | 7/60 | 5166/60 |
| inherited wide (distinct once) | 194 / 4806 / 0 | 16528 / 17203 / 17203 | 1034/4068 | 4382/924 |
| insert 500 new | 0 / 0 / 0 | 0 / 0 / 0 | 8/217 | 219/6 |
| inserted hot64 (cycled) | 4928 / 64 / 0 | 227 / 17706 / 17706 | 0/0 | 0/0 |
| inserted all 500 (x10) | 4564 / 436 / 0 | 1515 / 17420 / 17420 | 0/0 | 0/0 |
| update 200 inherited | 0 / 0 / 0 | 0 / 0 / 0 | 0/35 | 19/16 |
| updated 200 (x25) | 4801 / 199 / 0 | 683 / 17150 / 17150 | 0/0 | 0/0 |
| misses 500 (x10) | 4500 / 500 / 0 | 1239 / 12390 / 12390 | 22/23 | 443/7 |
| insert the 500 missed | 0 / 0 / 0 | 0 / 0 / 0 | 23/0 | 23/0 |
| formerly missed 500 (x10) | 4500 / 500 / 0 | 1743 / 17430 / 17430 | 0/0 | 0/0 |
| successive updates x100: writes | 0 / 100 / 0 | 300 / 300 / 300 | 0/0 | 0/0 |
| ancestor reads: parent tip 2000 | 0 / 0 / 0 | 6903 / 6903 / 6903 | 2421/1728 | 4007/142 |
| ancestor reads: block-10 tip 2000 | 0 / 0 / 0 | 6509 / 6509 / 6509 | 2456/1558 | 3718/296 |
| inherited hot64 after ancestor reads | 4931 / 61 / 0 | 210 / 17082 / 17082 | 0/5 | 387/3 |
| seal | 0 / 6 / 0 | 22 / 22 / 38 | 0/1 | 0/1 |
| commit | 0 / 0 / 0 | 0 / 0 / 0 | 0/247 | 249/0 |
| after commit: updated 200 | 0 / 0 / 0 | 686 / 686 / 686 | 30/216 | 41/205 |
| rollback: reads before drop | 0 / 500 / 0 | 1727 / 1727 / 1727 | 0/0 | 0/0 |
| rebuilt after rollback: reads | 0 / 0 / 0 | 1727 / 1727 / 1727 | 585/312 | 686/211 |

What this establishes:

* **The reference's result cache works where it applies.** With it on,
  repeated reads of the open block are answered without a walk (219 node
  visits for 4,992 hot reads, 98.7% hits) and cost 0.55-0.69 µs; off, the
  same reads walk (17,082 visits) and cost 0.94-2.07 µs. The cache is
  invalidated by every write (`result_cache_invalidations` is 0 because
  the writes evict by key and suspend reuse; the misses after each insert
  phase show the refill) and never serves inherited reads outside the hot
  set (194 hits in 5,000 wide reads), ancestor-tip reads (0 hits) or
  anything after commit.
* **The rewrite walks every read and is on par on the hot reads anyway**
  (0.56-0.96 µs against the reference's 0.55-0.69 with its cache, 0.94-2.07
  without): its walk through the in-memory arena plus one or two cached
  patched records costs about what the reference's cache hit costs.
* **Everything the result cache cannot serve stays 3-4x faster in the
  rewrite**: wide inherited reads 3.0 vs 9.0 µs, ancestor-tip reads 2.0 vs
  8.7, reads after commit 8.7 vs 12.0, reads of a rebuilt block 4.7 vs 7.6.
* **Writes are 2.5-4x faster** (insert 2.4 vs 9.9 µs, update 2.7 vs 7.9,
  successive updates 6.1 vs 15.0), commit 4.7 vs 6.4 ms, seal equal
  (16.6 vs 17.5 ms, SHA-512/256 bound).
* **Whole block**: 85 ms rewrite, 139 ms reference with its result cache,
  157 ms without, 5,882 ms upstream (its ancestor-tip reads cost 1.26 ms
  each). The rewrite's advantage during construction is 1.6x on this
  workload, not the 5x of committed reads: the result cache and the cheap
  arena reads narrow it, and seal, which is a quarter of the rewrite's
  block, is equal.
* **Rollback** behaves identically in all four: the dropped block has no
  root, its writes are invisible at the parent, and the rebuilt block
  commits to the same root everywhere.

## 6. Clarity execution

`contrib/clarity-bench` (`scripts/build-variants.sh`, `scripts/run.sh`),
logs `contrib/clarity-bench/results/r4/clarity-<variant>-pass<1|2>.log`.
Real contracts through `stackslib`'s `ClarityInstance` and `MarfedKV` with
the clarity crate's unit-test header and burn databases (epoch 2.5,
unlimited block limit). One contract: a `uint -> uint` map, two data
variables, functions `set-many`/`read-many`/`update-then-read` over lists
of 50 keys, `incr`, and `fail-after-write` (writes then returns `(err u1)`).
Setup (untimed): genesis with the default cost contract plus `costs-3`,
contract deployment, then 40 blocks populating 20,000 keys. Workloads, 10
blocks each unless noted, every returned value checked against a model:

* **read-heavy**: 20 `read-many` calls over random keys plus 10 over one
  hot 50-key list per block (1,500 map reads per block).
* **write-heavy**: `set-round` then 20 `update-then-read` calls, each
  writing 50 random keys and reading them back in the same transaction.
* **mixed**: 30 calls per block cycling `incr`, `set-many` on a hot list,
  `read-many` on random keys.
* **inherited-after-history** (5 blocks): `read-many` as read-heavy, after
  the 70 blocks above, over keys written 40-70 blocks earlier.
* **abort** (5 blocks): 20 `fail-after-write` calls, each followed by a
  read proving the aborted write is invisible; then a whole block of
  writes rolled back with `rollback_block`, and a block proving they are
  gone.

Variants (section 1): rewrite; upstream (worktree of `ad5c5778` as
committed); optmarf (upstream's `stackslib` and `clarity-cli` over the
optimized `stacks-marf` of `29ef093d`, with its `crates/stacks-mmap`, two
workspace dependency entries and three `MarfCore` trait imports; the
optimized crate's production options set explicitly). Each variant runs
twice in alternating order (rewrite, upstream, optmarf; then the reverse),
`REPEAT=5` fresh databases per run; the tables give the median of the 5
repetitions for each pass. `exec` is every transaction of the 9 timed
blocks (the first block of each workload is reported apart), `seal` the
root computation, `commit` the serialized write.

Equivalence: for every workload the cost totals (`runtime`, `read_count`,
`read_length`, `write_count`, `write_length`) and the final block root are
identical in all six logs (`grep last_root` in `run.sh` prints one line
per workload, count 6).

| workload (9 timed blocks) | txs | rewrite exec ms (pass 1 / pass 2) | optmarf exec ms | upstream exec ms | seal ms (rw / opt / up) | commit ms (rw / opt / up) |
|---|---|---|---|---|---|---|
| read-heavy | 270 | 358 / 438 | 388 / 481 | 1,048 / 1,083 | 3.5 / 4.8 / 5.9 | 11.1 / 12.4 / 17.1 |
| write-heavy | 189 | 521 / 576 | 623 / 688 | 1,125 / 1,124 | 135 / 165 / 170 | 379 / 402 / 478 |
| mixed | 270 | 292 / 260 | 314 / 317 | 841 / 797 | 27 / 35 / 33 | 19 / 23 / 31 |
| inherited-after-history (4 timed) | 120 | 137 / 134 | 167 / 182 | 541 / 444 | 1.4 / 2.4 / 3.4 | 5.4 / 5.5 / 6.5 |
| abort (4 timed) | 160 | 26 / 22 | 21 / 21 | 98 / 108 | 1.6 / 1.9 / 2.6 | 4.2 / 4.7 / 6.2 |

First block of each workload (process caches cold for its keys), exec ms,
pass 1: read-heavy 52 / 40 / 150; write-heavy 52 / 67 / 88; mixed 31 / 34
/ 89; inherited 32 / 43 / 150; abort 6.8 / 5.5 / 25 (rewrite / optmarf /
upstream). Populate (40 blocks, untimed): 1.47 s / 1.54 s / 2.88 s.

What this establishes:

* **Against upstream the speedup survives Clarity execution** at 2-3x on
  execution time (read-heavy 2.5-2.9x, write-heavy 2.0-2.2x, mixed
  2.9-3.1x, inherited 3.3-4.0x, abort 3.8-5.0x) and 1.2-1.5x on commit.
* **Against the optimized MARF under the same consumer code the advantage
  is modest**: 5-25% on execution (read-heavy 8-9%, write-heavy 16%,
  mixed 7-18%, inherited 18-26%), 6-20% on seal and commit, and none on
  the abort workload (21-26 ms both ways). Pass-to-pass variation (read-
  heavy rewrite 358 vs 438 ms) is of the same order as the rewrite-optmarf
  difference on read-heavy, so on that workload the two are within noise;
  on write-heavy, mixed and inherited the ordering holds in both passes.
* **Inferred: the interpreter dominates.** 1,500 map reads per read-heavy
  block cost the rewrite about 40 ms of execution, roughly 27 µs per
  `map-get?`, against 1.7-3 µs for the MARF lookup underneath (section 4);
  the rest is Clarity's own work (cost tracking, value serialization, the
  `MarfedKV` layer's key hashing and metadata lookups). A 5x MARF-level
  gain therefore shows as a 5-25% end-to-end gain here, and the optimized
  crate's result cache serves the repeated hot-list reads inside a block.
* **Open**: which part of the per-read cost above the MARF is the
  `MarfedKV` layer (it issues two MARF lookups per map read, value and
  metadata) and whether the reference's whole optimized stack (its
  `stackslib` value stores, not built in this round) changes the picture.

## 7. What the experiments establish

Measured facts, the inferences they support, and what stays open.

**Measured.**

1. The three implementations perform the same walks (identical node-visit
   and back-pointer counts per key set, section 4) over the same bytes
   (identical roots everywhere, sections 5 and 6).
2. With no patched node on the path and no cache involved, one lookup
   costs about 2 µs in the rewrite, 8 µs in the reference, 21 µs upstream
   (compressed), and 3.5-3.9 / 8 / 23-26 µs on plain records.
3. With patch caches at equal capacity and equal (100%) hit rates, the
   reference costs 8-12 µs per lookup, the rewrite 1.3-2.3 µs.
4. The reference gets slower when its patch cache is enlarged to the
   rewrite's capacity; the rewrite gets 2.5-4x faster on wide working sets
   when its cache is enlarged from the reference's 64 to its 4,096.
5. During block construction the reference's result cache answers 98.7%
   of hot open-block reads without a walk and brings those reads to parity
   with the rewrite; the rewrite is 3-4x faster on every read the cache
   cannot serve and 2.5-4x faster on writes; the whole block is 1.6x
   faster in the rewrite.
6. In Clarity execution the rewrite is 2-3x faster than upstream and
   5-25% faster than the optimized MARF under the same consumer code, with
   pass-to-pass noise of the same order on the read-heavy workload.
7. Memory (section 4 of `STATUS.md`, same harness): the rewrite's
   per-lookup allocations are zero after warm-up against the reference's
   4-7 and upstream's 40-56; its peak live heap during inserts and commits
   is at or below the oracles'; its patch cache holds up to 4,096 decoded
   branches (a `Node256` branch is a 256-entry pointer vector, about 6 KB,
   so the cache can reach about 25 MB on a mainnet-shaped trie against the
   reference's 64 entries); its temporary file during pruning is the size
   of the output blob.

**Inferred.**

* The speedup on committed reads is per-visit cost in the read path, not
  cache behaviour: (2) and (3) hold the cache constant and the gap stays;
  the plain-encoding control removes patches entirely and the gap is
  still 2x against the reference. What differs per visit: the rewrite
  parses a borrowed record and selects a child in one skip-scan with no
  allocation and no synchronization; the reference's node read goes
  through its cursor, its `SharedLru` behind a mutex with an `Arc` clone
  per hit, and owned node values on resolution.
* The patch cache's role is to keep patched paths at the patch-free cost.
  It is necessary (disabled: 10-40 µs) and its 4,096 capacity is the
  right size for this implementation (its clear-all map costs nothing per
  hit beyond a hash lookup, so a large capacity is free), whereas the
  reference's structure makes 64 its practical ceiling.
* The advantage shrinks where the rewrite's walk is not on the critical
  path: hot reads of the open block (the reference's result cache answers
  them in the same 0.6 µs the rewrite's arena walk costs), seal (hashing
  bound, equal), the abort workload (equal), and whole Clarity blocks
  (interpreter bound: 5-25%).

**Open.**

* Mainnet-shaped tries: every workload here fits in the page cache and the
  patch-cache working sets are at most 6,200 nodes; first-touch costs and
  the cache's memory at scale are measured only on 1M keys.
* The reference's whole optimized stack (its `stackslib` value stores) was
  not run; the optmarf variant isolates the MARF crate only.
* How much of a Clarity map read above the MARF is the `MarfedKV` layer,
  and whether the reference's result cache or the rewrite's absence of one
  matters for node workloads with more repeated reads per block than
  these.
* Replacement policy: an LRU in the rewrite's patch cache would raise hit
  rates on working sets just above capacity (79% to 100% on the 4,096-node
  set); the gain is bounded by the measured 4.4 vs 2.6 µs.
* The double record parse on patch-free walks (section 4) is an
  identified, unmeasured inefficiency of the rewrite.

## 8. Final tested commit and validation

VALIDATION
