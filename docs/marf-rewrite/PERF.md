# Performance evidence (fifth review)

This document answers why the rewrite (`crates/stacks-marf-next`) outperforms
the optimized reference MARF on committed reads, whether that holds for reads
during block construction and for Clarity execution, and what the evidence
does not cover. Every number is generated from published machine-readable
samples by a checked-in script (`contrib/marf-differential/scripts/tables.py`),
every binary is identified by revision and hash, and every build is from a
checked-in script that was validated from a fresh checkout.

Vocabulary: **measured** = read off a published sample in this round;
**inferred** = a conclusion the measurements support but do not state
directly; **open** = a hypothesis the experiments here do not settle.
**Reused** = a round-4 result cited for context only; its defects are named
where it is cited and nothing in sections 6-10 rests on it.

## 1. Requirement matrix

Status of each part of the fifth review. "completed" means the artifact is
in the pushed tree at the path given; "partial" names what is missing;
"unavailable" means the machine or the code cannot produce it, with the
reason; "not run" means it was deliberately left out, with the reason.

| # | requirement | status | where |
|---|---|---|---|
| 1a | round-3 and round-4 logs the `.gitignore` `*.log` rule excluded, recovered and published unchanged | completed | `contrib/marf-differential/results/r3/` (11 logs), `results/r4/` (28 logs + `validation/`), `contrib/clarity-bench/results/r4/` (6 logs); `.gitignore` exception scoped to those two `results/` trees |
| 1b | original individual samples for rounds 3 and 4 | unavailable: never existed | the harnesses of those rounds printed aggregates only (`results/r3/README.md`, `results/r4/README.md`); their logs are the only originals |
| 1c | machine-readable output with individual samples, configuration, workload and seeds, provider, instrumentation mode, counters | completed | every round-5 run writes `<experiment>-<provider>-<detail>-rep<N>.json` (`contrib/marf-differential/results/r5/`, Clarity runs included), schema in section 4 |
| 1d | source revisions and binary hashes in a manifest | completed | `target/bench-bin/MANIFEST.{upstream,reference,clarity}` copied into `results/r5/ENV.txt`; each JSON carries its own binary's sha256 and the build-time revision |
| 1e | tables generated from published data by a checked-in script | completed | `scripts/tables.py ablate|construct|clarity|check <dir>`; sections 5-8 are its output |
| 1f | fresh checkout contains every artifact | completed | section 11 (`git ls-tree` of the pushed tip from a fresh clone) |
| 2a | `build-reference.sh` works from a fresh checkout: directories, registration, overlays, revision check, identifiable binaries, identities recorded | completed | `scripts/build-reference.sh`; fresh-worktree evidence `results/r5/evidence/build-reference-fresh-worktree-at-5420f1c1.log`, failing original `...-at-e96c041a.log` |
| 2b | fresh and repeat invocation validated | completed | fresh: the log above (script-created `target/reference-worktree`); repeat: `results/r5/evidence/build-reference-repeat-at-d6fd444c54.log` |
| 2c | Clarity variant builder from a fresh checkout | completed | `contrib/clarity-bench/scripts/build-variants.sh` (`CLEAN=1` removes the worktree targets, every variant rebuilt from scratch), `MANIFEST.clarity`; log `contrib/clarity-bench/results/r5/evidence/build-variants-clean-at-8692a74f.log`, then a repeat invocation whose manifest is in `results/r5/ENV.txt` |
| 3 | cold-read isolation: fresh handle per provider x capacity x set x repetition, warm after own cold, occupancy at entry/exit, page-cold per cell with hard failure, mixed history separate, definitions | completed | `src/ablate.rs` (`isolated` default, `ABLATE_MODE=shared` separate), section 3 definitions, section 5 tables |
| 4 | counter semantics repaired: probes/hits/plain misses/admissions, resolutions and applications with the cache disabled, materializations, evictions/clears/occupancy, SQL or unavailable; sanity checks; instrumentation out of timing binaries | completed | `src/alloc.rs` `Counters` (every field `Option`), reference overlay `reference/reference.patch`, `tests/counters.rs` (both providers), section 3 table; counter binaries are separate (`-counters`) |
| 5 | residual read-path overhead investigated and separated: lookup implementation, failed plain probes, traversal | completed | late-probe diagnostic (`reference/README.md`), callgrind profiles `results/r5/profile/`, section 6; the control is the smallest valid one (one `cfg` block moving the probe after the record read, results unchanged) and it is not a production setting |
| 6 | affected experiments rerun with alternating provider order, individual repetitions, medians and variation from uninstrumented release runs | completed | `scripts/run-r5.sh` (one process per repetition, order rotated), sections 5-8; reused vs rerun in section 4 |
| 7a | "distinct once" label / distinct keys | completed | `construct.rs` generates distinct wide keys (shuffled index prefix) |
| 7b | failed-begin check implemented | completed | `Direct::try_begin`; the construction run asserts a second `begin` on the committed child fails and leaves the root unchanged |
| 7c | every returned read verified; cross-provider correctness records exported with a failing comparison script | completed | per-phase read digests in `construct-*.json`, per-workload `values_digest`, costs and roots in `clarity-*.json`; `tables.py check` exits 1 on any disagreement (`results/r5/CHECK.txt`) |
| 7d | both Clarity passes checked; harness work separated from timing | completed | 5 repetitions per variant in rotated order (each its own process), `harness` time reported apart from `exec` |
| 8a | nextest suites | completed | section 9, logs `results/r5/validation/` |
| 8b | GPL headers on the four new Rust files | completed | `ablate.rs`, `construct.rs`, `stats.rs`, `clarity-bench/src/main.rs` |
| 8c | requirement matrix, conclusions, six questions, revisions, fresh-clone verification | completed | this section, sections 10 and 11 |
| - | the reference's whole optimized stack (its `stackslib`) as a Clarity variant | not run | out of scope of this round's questions (the optmarf variant isolates the MARF crate under identical consumer code); the builder keeps the switch |
| - | mainnet-shaped database, memory at scale | not run | no such database on this machine; section 10 question 6 states the limits |

## 2. Targets, builds, identities

| target | source | build | binaries (`target/bench-bin/`) |
|---|---|---|---|
| upstream | `stacks-marf` at `ad5c57786bb51b58d4bdb4992ad534f762d8cf03`, in this tree (untouched) | `contrib/marf-differential/scripts/build-rewrite.sh` | `marf-differential-upstream` (timing), `-upstream-counters`, `-upstream-alloc`; the rewrite is the "next" side of each |
| reference | `stacks-marf` at `29ef093da5dd58c1fa484624d94a9d972356c629` | `contrib/marf-differential/scripts/build-reference.sh`: script-owned detached worktree at that revision (created or reset, revision verified), this tree's `crates/stacks-marf-next` and `contrib/marf-differential` copied in and registered as workspace members, `reference/harness.patch` (adapter) and `reference/reference.patch` (feature-gated counters and the late-probe diagnostic) applied, counter sanity tests run in the worktree | `marf-differential-reference` (timing), `-reference-counters`, `-reference-lateprobe` (diagnostic, timing), `-reference-lateprobe-counters` |
| rewrite | `crates/stacks-marf-next` at the revision in `MANIFEST.*` | in-tree | as above |
| Clarity variants | rewrite (this tree); upstream (worktree of `ad5c5778` as committed); optmarf (upstream's `stackslib` over the optimized `stacks-marf` of `29ef093d`) | `contrib/clarity-bench/scripts/build-variants.sh` | `clarity-bench-{rewrite,upstream,optmarf}` |

Identities. Each binary embeds the tree revision (`-dirty` if the crates or
harness differed from HEAD), the oracle's source and the toolchain at build
time (`MARF_BENCH_TREE_REV`, `MARF_BENCH_PROVIDER`, `MARF_BENCH_RUSTC`) and
prints them in its environment line; every JSON record carries them with the
sha256 of the running binary (computed by the binary over its own file), the
feature set (`counters`, `alloc_count`, `reference_lateprobe`), the arguments
and the start time. The manifests written by the build scripts list the
sha256 of every binary and, for the reference, of the two overlay patches;
`run-r5.sh` copies them into `results/r5/ENV.txt`.

Fresh-checkout validation (requirement 2). The previous `build-reference.sh`
failed from a fresh worktree (`cp` into a directory the reference revision
does not have; the harness crate was never registered): the reproduced
failure is `results/r5/evidence/build-reference-fresh-worktree-at-e96c041a.log`.
The rewritten script was validated with no pre-existing worktree (it created
`target/reference-worktree`; log `...-at-5420f1c1.log`: revision verified,
both overlays applied, four binaries, counter tests `2 passed`, manifest) and
then with a repeat invocation on the same worktree (reset, clean, re-copy,
re-apply; log `build-reference-repeat-at-d6fd444c54.log`). The Clarity builder was
run with `CLEAN=1` (worktrees' `target/` removed, every variant rebuilt from
scratch) and again without it.

Options. Upstream: `MARFOpenOpts::new(Deferred, external).with_compression`.
Reference: the production settings of its `Config::get_marf_opts`
(`with_mmap(true)`, `root_node_cache = false`, `resolved_patch_cache_capacity
= 64`, `result_cache_capacity = 2048`), set explicitly by the adapter; the
ablation overrides only the patch-cache capacity. Rewrite: mmap, patch cache
4,096 (production), no result cache; the ablation sets the capacity through
`MARFOpenOpts::with_patch_cache_capacity`.

Machine: one container, 4 vCPU Intel Xeon 2.80 GHz, 15 GB, Linux
6.18.44, `rustc 1.98.0`. The measurement batch ran alone (load averages
before and after in `ENV.txt`); builds, tests and the profiles ran before
it. Timing binaries carry no instrumentation; counters and the counting
allocator are separate binaries whose samples feed only the counter tables.

## 3. Definitions

**Measurement cell** (`ablate`, default `isolated` mode): one
(provider, capacity, key set, repetition). A fresh `MARF` handle is opened
for the cell (every process-local cache empty, a new blobs mapping), cache
occupancy is recorded (`occupancy_entry`), the set is read once (the
**first-pass** sample), occupancy is recorded again, the set is read
`PASSES` = 5 more times (the **warmed** samples), occupancy is recorded at
exit and the handle is dropped. Nothing from another set or capacity is in
the caches of a cell. Each repetition is its own process (`REPEAT=1`), and
`run-r5.sh` rotates the provider order between repetitions, so provider A
is not systematically measured before provider B.

* **process-cold**: the first pass of a cell as above, with the OS page
  cache as left by building the database in the same process (warm).
* **page-cold**: process-cold plus `echo 3 > /proc/sys/vm/drop_caches`
  immediately before opening the cell's handle (`DROP_CACHES=1`, root). If
  the write fails the run aborts with exit status 3 before any cell is
  timed, so a page-cold file is never a warm run in disguise.
* **first pass**: the single read of each key of the set in the cold state.
* **warmed**: the following 5 passes of the same cell; the tables report
  their median and min-max over passes and repetitions.
* **mixed history** (`ABLATE_MODE=shared`, the round-4 scheme): one handle
  per capacity, sets read through it in order; the record names the
  preceding sets and the occupancy at entry. Reported only in section 5.6
  as a separately named experiment.

**Counters.** Each harness counter is an `Option`: `None` means the provider
has no equivalent, and a table prints "n/a" (never 0) for it. Equivalent
meanings, with the source counter in each provider:

| harness counter | meaning | rewrite (`stacks-marf-next` `counters`) | reference (`marf-read-bench-counters`, overlay) |
|---|---|---|---|
| node visits | records visited by the walk | `node_visits` | `cursor_steps` |
| back-pointer follows | jumps to an ancestor trie | `backptr_follows` | `backpointer_follows` |
| record reads | records read from storage, including a chain's patch records | `record_reads` | `resolved_item_reads + mmap_node_returns` |
| patch-cache probes | lookups made in the resolved-patch cache | `patch_cache_lookups` | `patch_cache_probes` (new, at the production probe) |
| hits | probes answered | `patch_cache_hits` | `resolved_patch_cache_hits` |
| plain misses | probes whose record turned out to be a plain node (can never hit) | `patch_cache_probe_miss_plain` (new) | `patch_cache_probe_miss_plain` (new; mapped or SQLite path) |
| admissions | resolved chains inserted into the cache | `patch_cache_misses` (its name in the crate) | `resolved_patch_cache_misses` |
| clears | whole-cache clears (the rewrite's replacement policy) | `patch_cache_clears` | n/a (LRU evicts one entry; evictions are not counted by the crate) |
| resolutions | patch chains actually resolved (cache enabled or not) | `patch_resolutions` | `patch_resolutions` (new) |
| applications | patch records applied during resolutions | `patch_applications` | `patch_records` |
| owned materializations | nodes decoded into an owned value | `owned_materializations` | `owned_materializations` (new: resolved nodes and root-cache fills) |
| location SQL, block-id SQL | SQLite round trips for record location and block id | `location_sql`, `block_id_sql` | n/a (the reference's row/block-id caches are not instrumented) |
| result-cache hits / misses / invalidations | its open-block result cache | n/a (no result cache) | `result_cache_*` (new hooks) |
| occupancy | entries retained in the patch cache | `patch_cache_len()` (scratch slot excluded) | `SharedLru::len` via `MARF::patch_cache_len` (new) |

Identity checked by `tests/counters.rs` for both providers (`cargo test
--features counters --test counters`, also run inside the reference
worktree by `build-reference.sh`): `probes == hits + plain misses +
admissions`; `plain misses == node visits - patched visits` (patched visits
from the profiler); at capacity 0 probes, hits and admissions are 0 while
resolutions equal the set's patched visits (bypass counts work); a cold
pass over a set that fits resolves exactly its distinct patched records
and leaves that many entries; warm passes over a fitting set resolve
nothing; every `None` is printed as unavailable, never as zero. Parsing is
distinguished from I/O by record reads (parses of mapped bytes) against
the SQL round-trip counters; with `with_mmap` and external blobs no record
read in these runs is a SQLite read.

## 4. Evidence: recovered, reused, rerun

**Recovered** (requirement 1). The round-3 and round-4 logs were written
outside the repository and the repository's `*.log` ignore rule kept them
out of the commits that cited them. They are now in
`contrib/marf-differential/results/r3/` and `results/r4/` and
`contrib/clarity-bench/results/r4/`, byte for byte as produced (the
`.gitignore` exception is limited to those two `results/` trees). Their
READMEs say what each file is and which revision produced it. The harnesses
of those rounds printed aggregates only, so no individual samples exist for
them; the recovered logs are the complete originals, and the two READMEs
say which revision and harness produced each file and what its known
defects are.

**Rerun** (requirement 6; everything in sections 5-8). Priorities as the
review listed them: the isolated ablation of the rewrite and the reference
at 0 / 64 / 4,096 / production on `std`, upstream at production (5
repetitions, rotated order); the plain-encoding control; strict page-cold
(3 repetitions); the late-probe control (5); the counter runs (isolated and
mixed history); the `big` workload (3 repetitions, 0 / 64 / 4,096); the
construction workload (4 configurations, 5 repetitions, rotated); Clarity
(3 variants, 5 repetitions, rotated). Medians and min-max come from the
uninstrumented release binaries only.

**Reused** (context only, defects named). The round-4 ablation tables
(`results/r4/`) were produced with one handle per capacity shared across
sets (mixed history), so their per-set figures for the reference at 4,096
include the cost of entries left by earlier sets; their counter columns
mapped the reference's `resolved_patch_cache_misses` to "misses" and the
rewrite's SQL counters to 0 for the reference. Section 5 replaces them.
The round-4 construction and Clarity figures were correct in method but had
no individual samples; section 7 and 8 replace them with sampled runs.

**Machine-readable samples.** One JSON file per process. `ablate-*.json`:
`manifest`, `workload` (name, blocks, inserts, patch blocks, rewrites),
`seeds`, `passes`, `capacities`, `describe` (the provider's open options as
printed), `cells[]` with provider, workload, `plain`, `mode`, `page_cold`,
`capacity`, set name and tip, `n_keys`, `distinct_patched_nodes`, walk
shape, `repetition`, `preceding_sets`, `occupancy_entry`,
`occupancy_after_cold`, `occupancy_exit`, `cold_ns`, `warm_ns[]`,
`counters_cold`, `counters_warm`, `peak_heap_bytes`. `construct-*.json`:
`manifest`, `config`, `phases[]` with name, kind, ops, `samples_ns[]` (one
per repetition), per-phase counters, and `digest` (a fold of every returned
value) per phase; `roots`. `clarity-*.json`: manifest, variant, per-workload
`exec`, `harness`, `seal`, `commit` samples (one per repetition), `costs`,
`last_root`, `values_digest`. `tables.py check` compares the digests, roots
and costs across providers and exits 1 on any disagreement.

## 5. Committed reads: isolated patch-cache ablation

`marf-differential ablate <impl> <std|big> [reads] [plain]`, 5 repetitions
(3 for `big` and page-cold), one process per repetition, provider order
rotated (`run-r5.sh`), samples `results/r5/ablate-*.json`, tables generated
by `tables.py ablate` (complete output, every counter table included, in
`results/r5/TABLES-ablate.md`). The database (`std`: 20 blocks x 2,000
inserts plus 8 patch blocks x 50 rewrites, 40,000 keys; `big`: 1M keys) is
built once per process; key sets are chosen by walk shape with the rewrite's
profiler on the shared files (identical trie for every implementation),
which also records the patched records each walk meets, so a set's number
of distinct patched nodes is known. Every returned value is verified
against the model after the timed loop. Capacity `prod` is the provider's
production value (reference 64, rewrite 4,096; upstream has no cache).

### 5.1 `std`, process-cold, isolated cells

Per cell: first-pass (cold) median / warmed median (warmed min-max) in µs per lookup; 5 repetitions x 5 warm passes.

| set | n keys | distinct patched nodes | rewrite cap prod | rewrite cap 0 | rewrite cap 64 | rewrite cap 4096 | reference cap prod | reference cap 0 | reference cap 64 | reference cap 4096 | reference-lateprobe cap prod | reference-lateprobe cap 0 | reference-lateprobe cap 64 | reference-lateprobe cap 4096 | upstream cap prod |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| patchfree@tip1 | 2,000 | 0 | 2.1 / 2.0 (1.8-2.9) | 2.2 / 2.0 (1.9-3.0) | 2.2 / 2.0 (1.8-3.0) | 2.1 / 1.9 (1.8-2.2) | 8.2 / 8.3 (7.4-10.9) | 8.1 / 7.9 (7.5-10.1) | 8.2 / 8.1 (7.5-11.6) | 8.1 / 8.0 (7.3-11.9) | 7.9 / 7.8 (7.4-8.7) | 7.9 / 7.7 (7.2-15.1) | 7.7 / 7.7 (7.4-8.8) | 8.4 / 8.6 (7.5-10.9) | 17.5 / 16.8 (15.9-26.5) |
| bulk tip (random keys) | 5,000 | 677 | 2.1 / 1.6 (1.4-2.5) | 42.4 / 41.3 (36.4-48.6) | 6.2 / 6.0 (5.8-8.3) | 2.0 / 1.5 (1.3-1.9) | 13.0 / 11.9 (11.0-17.2) | 34.7 / 33.1 (31.9-46.2) | 12.0 / 11.4 (11.0-16.8) | 8.4 / 8.9 (7.8-12.6) | 12.1 / 11.3 (10.5-16.2) | 32.1 / 33.0 (31.2-56.4) | 11.2 / 11.0 (10.4-16.7) | 7.0 / 6.4 (6.0-13.0) | 1239.7 / 1217.1 (1162.6-1370.7) |
| tip hops=2 depth=3-4 | 5,000 | 365 | 1.7 / 1.3 (1.2-2.3) | 14.0 / 13.7 (12.8-22.1) | 5.0 / 4.9 (4.7-5.3) | 1.6 / 1.3 (1.2-1.4) | 10.0 / 9.4 (9.1-14.9) | 19.0 / 20.0 (17.4-26.9) | 9.8 / 9.9 (9.3-11.0) | 6.6 / 6.4 (6.1-11.0) | 11.0 / 9.8 (9.1-14.3) | 18.4 / 19.4 (17.3-24.6) | 9.8 / 10.0 (9.2-17.8) | 6.0 / 6.0 (5.4-9.0) | 148.3 / 160.4 (141.8-188.7) |
| tip hops=2 depth=1-2 | 5,000 | 438 | 2.0 / 1.9 (1.8-4.6) | 11.9 / 11.3 (10.6-16.3) | 3.5 / 3.2 (3.1-4.6) | 2.1 / 1.9 (1.8-2.8) | 7.4 / 8.0 (7.1-15.2) | 14.0 / 14.0 (13.5-25.4) | 8.4 / 7.6 (6.9-11.2) | 8.0 / 8.0 (7.3-14.2) | 9.8 / 7.9 (6.9-12.8) | 17.3 / 14.4 (13.5-30.1) | 7.8 / 7.9 (7.1-11.4) | 7.0 / 7.0 (6.0-9.3) | 121.4 / 111.9 (102.7-149.1) |
| tip hops=1 depth=1-2 | 5,000 | 336 | 1.9 / 1.7 (1.5-3.0) | 11.2 / 10.6 (10.3-19.8) | 2.1 / 2.0 (1.9-3.3) | 2.0 / 1.7 (1.5-2.8) | 8.4 / 6.7 (6.3-11.9) | 13.8 / 13.7 (13.2-19.9) | 7.1 / 7.1 (6.4-12.2) | 7.5 / 7.4 (7.0-13.0) | 7.2 / 7.6 (6.8-11.1) | 17.4 / 15.7 (13.3-21.5) | 7.7 / 7.2 (6.6-11.2) | 8.5 / 8.1 (6.6-11.5) | 105.4 / 108.3 (95.8-129.5) |
| patched(nodes<=32) | 8,192 | 32 | 2.4 / 2.2 (2.1-3.4) | 10.5 / 10.7 (10.2-15.3) | 2.2 / 2.1 (2.1-2.6) | 2.3 / 2.2 (2.1-3.7) | 7.5 / 7.0 (6.6-11.1) | 13.6 / 14.3 (13.2-17.8) | 7.0 / 7.0 (6.6-10.5) | 6.7 / 6.6 (6.4-10.5) | 7.0 / 7.5 (6.4-9.7) | 14.9 / 13.8 (13.2-16.6) | 7.3 / 6.8 (6.5-10.7) | 9.2 / 6.9 (6.4-16.4) | 110.9 / 111.8 (99.6-126.6) |
| patched(nodes<=64) | 8,192 | 64 | 2.2 / 2.0 (1.9-2.2) | 11.0 / 11.6 (10.6-15.3) | 2.1 / 2.0 (2.0-2.2) | 2.1 / 2.0 (1.9-3.2) | 7.8 / 6.6 (6.2-10.3) | 14.9 / 15.0 (14.1-22.2) | 6.7 / 7.0 (6.2-10.1) | 6.6 / 6.5 (6.2-9.9) | 6.8 / 6.5 (6.1-11.9) | 14.6 / 15.0 (13.9-22.4) | 6.5 / 6.6 (6.2-8.9) | 7.7 / 7.2 (6.2-9.8) | 120.5 / 123.7 (109.6-164.8) |
| patched(nodes<=1024) | 8,192 | 995 | 2.1 / 1.8 (1.7-2.2) | 12.4 / 12.6 (11.6-17.7) | 5.2 / 5.0 (4.8-7.7) | 2.1 / 1.8 (1.7-2.7) | 11.5 / 11.8 (10.4-16.1) | 19.1 / 17.7 (15.8-22.6) | 12.3 / 11.7 (10.1-17.5) | 8.8 / 10.1 (9.1-15.0) | 12.0 / 11.1 (10.0-13.5) | 17.1 / 16.9 (15.4-21.4) | 10.5 / 11.4 (10.1-17.2) | 7.5 / 6.8 (6.5-9.3) | 138.7 / 138.1 (122.6-158.0) |

What this establishes (measured):

* **Patch-free control** (first row, no patched node on any path, every
  probe a plain miss): rewrite 2.0 µs, reference 8.0, upstream 16.8, at
  every capacity. The 4x against the reference is traversal and record
  access, not caching.
* **Equal capacity, equal hit rate.** At 4,096 both caches hold every
  distinct patched node of every `std` set (occupancy after the cold pass
  equals the set's distinct patched nodes: 677, 365, 438, 336, 32, 64, 995
  for both providers) and warm passes admit nothing (section 5.5). The
  reference then costs 6.4-10.1 µs, the rewrite 1.3-2.0. The cache is held
  constant and the gap is 4-5x.
* **The round-4 claim that the reference gets slower at 4,096 was an
  artifact of the shared handle.** In isolated cells the reference at 4,096
  is faster than at its production 64 on every set wider than 64 nodes
  (bulk tip 8.9 vs 11.4, hops=2 depth=3-4 6.4 vs 9.9, 995-node set 10.1 vs
  11.7) and equal on the narrow ones. Section 5.6 shows the mixed-history
  occupancy that produced the old figures.
* **Capacity matters for the rewrite** (its cache is cheap per probe,
  section 6): on sets wider than 64 nodes 4,096 entries are worth 2.5-4x
  over 64 (bulk tip 1.5 vs 6.0, 995-node set 1.8 vs 5.0); disabled, 10-41
  µs. The reference gains 1.2-1.5x from the same change on `std`.
* **Variation.** Warm min-max over 25 passes stays within about 10-30% of
  the median for the rewrite and the reference; the first pass of a cell
  costs the rewrite 0.1-0.5 µs more than warm (the cache fills), the
  reference 0.5-1.7 µs more.

### 5.2 Plain-encoding control (no patches anywhere)

Per cell: first-pass (cold) median / warmed median (warmed min-max) in µs per lookup; 5 repetitions x 5 warm passes.

| set | n keys | distinct patched nodes | rewrite cap prod | reference cap prod | upstream cap prod |
|---|---|---|---|---|---|
| patchfree@tip1 | 2,000 | 0 | 2.0 / 1.9 (1.7-2.7) | 7.5 / 7.3 (5.7-10.1) | 17.4 / 16.8 (15.3-22.6) |
| bulk tip (random keys) | 5,000 | 0 | 3.8 / 3.8 (3.6-5.3) | 8.4 / 8.8 (7.3-17.4) | 24.2 / 21.5 (19.7-44.8) |
| tip hops=2 depth=none | 5,000 | 0 | 4.0 / 3.8 (3.6-5.1) | 7.9 / 7.8 (7.4-12.8) | 25.1 / 22.1 (20.5-28.7) |
| tip hops=1 depth=none | 5,000 | 0 | 3.7 / 3.6 (3.4-4.0) | 7.6 / 8.0 (7.3-12.3) | 21.7 / 21.4 (19.7-26.7) |
| tip hops=3+ depth=none | 2,423 | 0 | 4.1 / 3.9 (3.8-4.2) | 9.1 / 9.2 (8.0-13.3) | 24.5 / 24.9 (23.1-35.8) |

No compressed record exists, so no probe can hit and no chain is resolved:
rewrite 1.9-3.9 µs, reference 7.3-9.2, upstream 16.8-24.9. The rewrite is
slower on plain final-tip walks than on compressed ones (3.6-3.9 vs
1.3-2.0) because a plain `Node256` body is 2.6 KB and its slot list is
skip-scanned; the 2x against the reference remains.

### 5.3 Page-cold (strict), production settings

Per cell: first-pass (cold) median / warmed median (warmed min-max) in µs per lookup; 3 repetitions x 5 warm passes.

| set | n keys | distinct patched nodes | rewrite cap prod | reference cap prod |
|---|---|---|---|---|
| patchfree@tip1 | 2,000 | 0 | 3.0 / 2.3 (1.9-3.0) | 13.9 / 9.8 (9.1-11.9) |
| bulk tip (random keys) | 5,000 | 677 | 7.6 / 1.8 (1.5-2.8) | 53.3 / 13.2 (11.1-15.0) |
| tip hops=2 depth=3-4 | 5,000 | 365 | 5.2 / 1.4 (1.3-2.1) | 41.7 / 12.2 (9.3-15.2) |
| tip hops=2 depth=1-2 | 5,000 | 438 | 5.2 / 1.9 (1.8-2.2) | 35.1 / 9.7 (7.5-11.9) |
| tip hops=1 depth=1-2 | 5,000 | 336 | 5.7 / 1.7 (1.6-2.9) | 23.2 / 7.3 (6.5-11.1) |
| patched(nodes<=32) | 8,192 | 32 | 4.5 / 2.4 (2.2-3.3) | 24.5 / 7.4 (6.7-10.8) |
| patched(nodes<=64) | 8,192 | 64 | 4.7 / 2.4 (2.0-3.1) | 25.0 / 7.1 (6.6-13.0) |
| patched(nodes<=1024) | 8,192 | 995 | 4.7 / 1.9 (1.7-2.7) | 69.7 / 11.8 (10.4-17.7) |

Page-cold is enforced per cell (`DROP_CACHES=1`; the drop is made before
each handle is opened and the run aborts if it fails). The first pass then
includes the mapping's first-touch faults: rewrite 3.0-7.6 µs against
1.7-2.4 process-cold, reference 13.9-69.7 against 6.6-13.0. The warmed
passes return to the process-cold figures. The database is about 10 MB and
resident after one pass, so this says nothing about a mainnet-sized file
(section 10, question 6).

### 5.4 `big` workload (1M keys), isolated cells, 3 repetitions

Per cell: first-pass (cold) median / warmed median (warmed min-max) in µs per lookup; 3 repetitions x 5 warm passes.

| set | n keys | distinct patched nodes | rewrite cap 0 | rewrite cap 64 | rewrite cap 4096 | reference cap 0 | reference cap 64 | reference cap 4096 |
|---|---|---|---|---|---|---|---|---|
| patchfree@tip1 | 4,000 | 0 | 2.2 / 2.3 (2.1-3.1) | 2.3 / 2.2 (2.1-3.1) | 2.3 / 2.2 (2.0-2.4) | 9.1 / 8.9 (7.7-16.6) | 12.8 / 12.4 (8.7-15.0) | 9.2 / 8.5 (8.0-11.3) |
| bulk tip (random keys) | 5,000 | 3,770 | 49.5 / 40.5 (37.7-49.1) | 9.6 / 9.5 (8.3-13.5) | 5.9 / 2.7 (2.4-3.6) | 37.7 / 33.8 (32.5-36.6) | 18.7 / 17.4 (13.7-21.0) | 29.7 / 25.5 (23.8-34.9) |
| tip hops=2 depth=3-4 | 5,000 | 3,846 | 37.6 / 36.3 (35.6-41.9) | 9.4 / 8.0 (7.8-11.5) | 7.7 / 2.6 (2.4-3.8) | 36.3 / 32.7 (30.8-38.5) | 20.6 / 18.4 (13.1-21.8) | 25.4 / 26.3 (22.2-33.4) |
| tip hops=3+ depth=3-4 | 5,000 | 2,745 | 38.4 / 38.1 (36.0-44.9) | 6.5 / 5.2 (5.0-8.3) | 5.0 / 2.1 (2.0-3.1) | 38.1 / 30.9 (29.0-40.6) | 11.4 / 11.3 (9.3-14.6) | 17.3 / 16.7 (15.4-21.2) |
| tip hops=1 depth=3-4 | 5,000 | 3,970 | 39.4 / 38.2 (36.7-45.3) | 8.8 / 8.0 (7.5-10.0) | 5.3 / 2.4 (2.3-3.6) | 32.6 / 30.3 (29.7-35.3) | 21.7 / 16.4 (13.2-18.7) | 25.2 / 27.0 (23.9-38.0) |
| patched(nodes<=32) | 8,192 | 32 | 36.0 / 35.9 (33.1-38.9) | 3.6 / 2.8 (2.7-3.3) | 4.7 / 3.1 (2.8-3.7) | 30.3 / 29.0 (27.8-33.8) | 10.9 / 9.9 (9.2-13.5) | 10.6 / 9.5 (8.8-14.5) |
| patched(nodes<=64) | 8,192 | 64 | 39.9 / 35.5 (34.0-39.9) | 3.6 / 2.8 (2.7-3.0) | 3.4 / 2.7 (2.6-4.9) | 31.8 / 30.2 (28.3-38.6) | 10.1 / 9.1 (8.6-13.0) | 9.7 / 8.7 (8.5-10.7) |
| patched(nodes<=1024) | 8,192 | 1,024 | 36.5 / 36.5 (35.2-39.7) | 7.3 / 6.7 (6.4-10.7) | 3.9 / 2.7 (2.5-3.8) | 32.4 / 32.8 (29.9-39.4) | 13.4 / 13.7 (11.4-17.7) | 13.7 / 12.1 (11.4-18.4) |
| patched(nodes<=4096) | 8,192 | 4,096 | 40.6 / 39.4 (37.3-43.9) | 9.7 / 7.8 (7.5-11.6) | 4.9 / 3.8 (3.7-5.5) | 33.7 / 31.5 (30.4-35.9) | 14.0 / 13.7 (12.7-18.0) | 24.3 / 24.2 (22.0-32.7) |
| patched all | 8,192 | 6,200 | 45.1 / 38.7 (35.9-43.4) | 9.4 / 8.6 (7.9-11.3) | 5.4 / 4.5 (4.2-6.8) | 34.0 / 36.7 (31.2-40.5) | 15.2 / 16.4 (12.6-21.8) | 33.3 / 35.0 (30.6-43.9) |

Here the working sets reach 2,745-6,200 distinct patched nodes and the
reference at 4,096 **is** slower than at 64 even in isolation (bulk tip
25.5 vs 17.4, 4,096-node set 24.2 vs 13.7, 6,200-node set 35.0 vs 16.4): its
cache then holds 3,770-4,096 entries and the linear scan per probe (section
6) costs more than the resolutions it saves. The rewrite at 4,096 costs
2.1-4.5 µs on the same sets (2.1-3.1 below its capacity, 3.8-4.5 above it
with its clear-all policy keeping 70-79% hits, section 5.5); at 64 it
thrashes like the reference (5.2-9.5 vs 9.1-18.4).

### 5.5 Counters (separate binaries; warm = the 5 warm passes of the cell)

Rows from `TABLES-ablate.md` (`std`, isolated). Visits and probes are the
same for both providers on every set (same trie, same walk); probes equal
visits because both probe before every record read; plain misses are the
probes on plain records.

| set | capacity | provider | node visits | probes | hits | plain misses | admissions | clears | resolutions | applications | record reads | occupancy entry / after cold / exit |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| bulk tip (677) | 64 | reference | 86,440 | 86,440 | 30,610 | 35,160 | 20,670 | n/a | 20,670 | 49,940 | 105,770 | 0 / 64 / 64 |
| bulk tip (677) | 64 | rewrite | 86,440 | 86,440 | 27,600 | 35,160 | 23,680 | 370 | 23,680 | 58,600 | 176,280 | 0 / 12 / 12 |
| bulk tip (677) | 4,096 | reference | 86,440 | 86,440 | 51,280 | 35,160 | 0 | n/a | 0 | 0 | 35,160 | 0 / 677 / 677 |
| bulk tip (677) | 4,096 | rewrite | 86,440 | 86,440 | 51,280 | 35,160 | 0 | 0 | 0 | 0 | 70,320 | 0 / 677 / 677 |
| patched(nodes<=64) | 64 | reference | 138,670 | 138,670 | 62,620 | 76,050 | 0 | n/a | 0 | 0 | 76,050 | 0 / 64 / 64 |
| patched(nodes<=64) | 64 | rewrite | 138,670 | 138,670 | 61,980 | 76,050 | 640 | 10 | 640 | 1,510 | 154,890 | 0 / 58 / 58 |
| patched(nodes<=1024) (995) | 64 | reference | 141,980 | 141,980 | 49,480 | 63,165 | 29,335 | n/a | 29,335 | 71,710 | 164,210 | 0 / 64 / 64 |
| patched(nodes<=1024) (995) | 64 | rewrite | 141,980 | 141,980 | 44,726 | 63,165 | 34,089 | 532 | 34,089 | 84,300 | 278,808 | 0 / 10 / 51 |
| patched(nodes<=1024) (995) | 4,096 | reference | 141,980 | 141,980 | 78,815 | 63,165 | 0 | n/a | 0 | 0 | 63,165 | 0 / 995 / 995 |
| patched(nodes<=1024) (995) | 4,096 | rewrite | 141,980 | 141,980 | 78,815 | 63,165 | 0 | 0 | 0 | 0 | 126,330 | 0 / 995 / 995 |
| patchfree@tip1 | any | both | 30,290 | 30,290 | 0 | 30,290 | 0 | 0 / n/a | 0 | 0 | 30,290 (ref) / 60,580 (rw) | 0 / 0 / 0 |

Sanity checks (all hold in every counter file, enforced by
`tests/counters.rs` for the small workload): `probes == hits + plain misses
+ admissions`; at capacity 0 probes, hits and admissions are 0 while
resolutions equal the patched visits (bulk tip: 51,280 resolutions,
164,880 applications, for both providers); a cold pass over a fitting set
admits exactly its distinct patched records and the occupancy after it is
that number; warm passes over a fitting set admit and resolve nothing;
`None` prints as n/a.

Two hit rates. On the patched probes only (the ones that can hit), both
caches at 4,096 are at 100% on every `std` set; on all probes the rate is
59% (bulk tip) to 45% (32-node set) because 41-60% of the probes are on
plain records and fail by construction. At 64 the reference's LRU keeps a
few points more than the rewrite's clear-all on wide sets (bulk tip: 60%
vs 54% of patched probes; 995-node set: 63% vs 57%), and on the 64-node set
the rewrite clears 10 times (its map clears when full before admitting, so
64 distinct nodes do not quite fit in 64 slots: 58 retained) where the
reference's LRU holds all 64. `big`, 4,096-node set at 4,096: reference 100%
of patched probes, rewrite 79% (20,480 admissions, 5 clears); 6,200-node
set: 72% vs 70%.

Record reads: the rewrite reads twice as many records as the reference on
patch-free visits (60,580 vs 30,290 on the patch-free set, 70,320 vs 35,160
on bulk tip at 4,096): its resolver re-parses the terminal record after its
loop. An identified inefficiency of the rewrite, unchanged this round so the
measured binary is the tested commit (follow-up in section 10).

### 5.6 Mixed history (`ABLATE_MODE=shared`, counters only)

One handle per capacity, the eight sets read in sequence through it
(`ablate-*-std-shared-counters.json`). The reference's occupancy at
4,096 grows across the sequence: 677 after bulk tip, 960, 1,280, 1,374,
1,374, 1,377, 1,683 at the end; every later set probes through that many
entries. At 64 the occupancy is 64 from the second set on and later sets
start with a cache full of another set's entries (the 32-node and 64-node
sets then show 0 admissions warm because their nodes displaced the
others during the cold pass). This is the scheme the round-4 tables were
measured under, and it is why they showed the reference slower at 4,096
on `std`. It is reported as mixed-history behaviour only and must not be
read as per-set cost.

## 6. Residual read-path overhead

Question: with the cache held constant, where do the reference's extra
6-8 µs per lookup go? Three suspects named by the review: the `SharedLru`
mutex, the `SmallLru` linear scan, and the failed probes on plain records.
Two instruments, both on the timing binaries' code: a diagnostic variant of
the reference (`marf-diag-late-probe`, overlay feature, never a production
setting; `reference/README.md`) that reads the mapped record first and
probes the cache only when the record is a patch, removing exactly the plain
probes and nothing else (results unchanged: a plain record's probe can
never hit); and bounded callgrind profiles of one isolated ablation process
per configuration (`results/r5/profile/`, instruction counts, deterministic).

### 6.1 Late-probe control, timings (`std`, isolated, warm medians µs; from the table in 5.1)

| set | reference 64 | late-probe 64 | reference 4,096 | late-probe 4,096 | rewrite 4,096 |
|---|---|---|---|---|---|
| patchfree@tip1 | 8.1 | 7.7 | 8.0 | 8.6 | 1.9 |
| bulk tip (677) | 11.4 | 11.0 | 8.9 | 6.4 | 1.5 |
| tip hops=2 depth=3-4 (365) | 9.9 | 10.0 | 6.4 | 6.0 | 1.3 |
| tip hops=2 depth=1-2 (438) | 7.6 | 7.9 | 8.0 | 7.0 | 1.9 |
| tip hops=1 depth=1-2 (336) | 7.1 | 7.2 | 7.4 | 8.1 | 1.7 |
| patched(nodes<=32) | 7.0 | 6.8 | 6.6 | 6.9 | 2.2 |
| patched(nodes<=64) | 7.0 | 6.6 | 6.5 | 7.2 | 2.0 |
| patched(nodes<=1024) (995) | 11.7 | 11.4 | 10.1 | 6.8 | 1.8 |

Counters confirm the control does what it says: at 4,096 its plain misses
are 0 and its probes equal the patched visits (bulk tip 51,280 probes,
51,280 hits). The few resolutions it still shows (90-3,325 per 5 passes)
are patch chains reached through the reference's SQLite path, which the
late probe does not cache; they are 0.1-4% of visits.

Measured: removing the plain probes changes nothing at 64 (the scan is over
at most 64 entries) and saves 1-3 µs at 4,096 on the sets whose occupancy
is in the hundreds (bulk tip 8.9 to 6.4, 995-node set 10.1 to 6.8). With
every failed probe removed and every patched probe a hit, the reference
still costs 6.0-8.6 µs against the rewrite's 1.3-2.2: the remaining 4.5-6.5
µs per lookup is not in the cache.

### 6.2 Instruction attribution (callgrind, whole `ablate std` process, one capacity each)

| profile | timed reads, Ir | `SharedLru::get` | `read_patched_persisted_node` (resolutions) | rewrite `resolve_in` |
|---|---|---|---|---|
| reference 64 | 7.68 G | 0.71 G (9%) | 1.98 G (26%) | - |
| reference 4,096 | 8.80 G | 3.89 G (44%) | 0.21 G (2%) | - |
| late-probe 64 | 7.51 G | 0.21 G (3%) | 2.09 G (28%) | - |
| late-probe 4,096 | 6.67 G | 1.09 G (16%) | 0.39 G (6%) | - |
| rewrite 64 | 4.80 G | - | - | 3.69 G (77%) |
| rewrite 4,096 | 2.56 G | - | - | 1.02 G (40%) |

(`results/r5/profile/README.md` has the exact counts and binary hashes.)
Inside `SharedLru::get` at 4,096: 1.64 G Ir comparing the
`(StacksBlockId, u64)` keys and 1.13 G iterating, i.e. `SmallLru`'s
`entries.iter().rposition(..)` over the retained entries on every record
read, followed by a `rotate_left` on a hit; the mutex itself does not
appear in the top self costs (uncontended, one lock per probe). The
rewrite's hashed map shows as SipHash at 0.15 G Ir.

What this establishes:

* **Measured.** The reference's cache lookup is a linear scan whose cost
  grows with occupancy, performed on every record read because the probe
  precedes the read: 9% of its read instructions at 64 entries, 44% at
  about 1,000 entries (isolated `std` at 4,096), and on `big` (3,770-4,096
  entries) it outweighs the resolutions it saves (section 5.4). Plain
  misses are about half of the probes and account for the difference
  between the production order and the late probe (3.89 G to 1.09 G).
* **Measured.** The rewrite executes 2.56 G instructions for the same
  reads the reference executes 8.80 G for at equal capacity (3.4x), and
  4.80 G vs 7.68 G at 64. With the reference's cache made as cheap as the
  diagnostic allows (6.67 G) the ratio is still 2.6x.
* **Inferred.** The residual 4.5-6.5 µs per lookup at equal, fully hit
  caches is per-visit cost in the reference's traversal and record access:
  its cursor walk with owned `TrieNodeType` values (`load_from_parts`,
  `ptrs_from_parts_into`, `TriePtr::from_slice_compressed` are 10-11% each
  of its read instructions), the `Arc` clone and `ReadTrieNode` rebuild per
  hit, and block-id resolution through SQLite rows
  (`get_block_identifier`, `rusqlite::Rows::advance`: about 7% each) where
  the rewrite's walk parses a borrowed record, selects the child in one
  skip-scan and keeps block ids in its own map. The profile names these
  functions; it does not time them, so the split among them is inferred,
  not measured.
* **Not inferred.** None of this says 64 is the reference's optimal
  capacity (isolated `std` shows 4,096 faster for it, `big` shows 64
  faster: the optimum depends on the working set), nor that a large cache
  is free for the rewrite: its clear-all policy costs it 21 points of hit
  rate on the 4,096-node set (79% vs 100%) and its 4,096 decoded branches
  can reach about 25 MB on a mainnet-shaped trie (section 10, question 6).

## 7. Reads during block construction

`marf-differential construct <impl> [result-cache <n|off>]`, 5 repetitions
per configuration, one process each, order rotated; samples
`results/r5/construct-*.json`, table by `tables.py construct`. A 20-block,
40,000-key base chain, then one child block kept open while reads
interleave with writes (hot set of 64 inherited keys cycled; 5,000 distinct
inherited keys once, now generated distinct; 500 inserts then reads; 200
updates then reads of the new values; 500 misses then their insertion and
reads; one key updated 100 times with a read after each; 2,000 reads at the
parent tip and 2,000 at block 10's tip while the child is open; the hot set
again; seal; commit), then a `begin` of the already committed child, which
must fail and leave the root unchanged (asserted), a rollback (begin on a
new child, 500 inserts, reads, `drop_current`, the block must be gone and
the writes invisible), and the same block rebuilt and committed. Every read
result is collected inside the timed region and checked after it; each
phase exports a digest of its returned values and the roots are exported,
and `tables.py check` found all 15 cross-provider records (13 phase
digests, 2 roots) identical across the 23 runs.

| phase | kind | ops | reference rc0 µs/op median (min-max, n) | reference rc2048 µs/op median (min-max, n) | rewrite µs/op median (min-max, n) | upstream µs/op median (min-max, n) |
|---|---|---|---|---|---|---|
| begin | write | 1 | 0.6 ms (0.4-0.7, n=5) | 0.7 ms (0.5-0.7, n=5) | 0.5 ms (0.5-0.7, n=5) | 3.2 ms (2.9-4.4, n=5) |
| inherited hot64 (cycled) | read | 4,992 | 2.61 (2.03-3.85, n=5) | 0.98 (0.76-1.07, n=5) | 0.94 (0.90-1.40, n=5) | 58.12 (53.34-58.39, n=5) |
| inherited wide (distinct once) | read | 5,000 | 10.64 (8.85-12.88, n=5) | 11.55 (9.88-14.66, n=5) | 2.78 (2.66-3.47, n=5) | 54.13 (49.79-64.43, n=5) |
| insert 500 new | write | 500 | 15.12 (10.52-19.60, n=5) | 10.71 (10.39-23.45, n=5) | 2.27 (2.13-3.36, n=5) | 28.95 (28.14-34.28, n=5) |
| inserted hot64 (cycled) | read | 4,992 | 1.32 (0.89-2.10, n=5) | 0.54 (0.53-0.79, n=5) | 0.55 (0.54-0.90, n=5) | 3.37 (3.18-3.59, n=5) |
| inserted all 500 (x10) | read | 5,000 | 1.61 (0.97-1.70, n=5) | 0.65 (0.62-1.08, n=5) | 0.64 (0.63-0.90, n=5) | 3.58 (3.44-3.84, n=5) |
| update 200 inherited | write | 200 | 9.12 (8.08-12.81, n=5) | 8.06 (7.79-17.56, n=5) | 3.22 (2.38-3.72, n=5) | 21.52 (20.83-22.73, n=5) |
| updated 200 (x25) | read | 5,000 | 0.97 (0.89-1.55, n=5) | 0.58 (0.57-0.91, n=5) | 0.65 (0.56-0.90, n=5) | 3.81 (3.38-4.65, n=5) |
| misses 500 (x10) | read | 5,000 | 1.23 (1.02-1.97, n=5) | 0.74 (0.67-1.16, n=5) | 0.73 (0.64-1.13, n=5) | 9.69 (7.07-11.80, n=5) |
| insert the 500 missed | write | 500 | 6.23 (5.09-9.44, n=5) | 6.03 (5.83-13.47, n=5) | 2.31 (1.80-3.65, n=5) | 15.38 (12.02-19.73, n=5) |
| formerly missed 500 (x10) | read | 5,000 | 1.02 (0.92-1.74, n=5) | 0.63 (0.60-1.05, n=5) | 0.76 (0.61-1.05, n=5) | 3.81 (3.67-6.07, n=5) |
| successive updates x100: writes | write | 100 | 15.80 (14.93-23.98, n=5) | 21.22 (14.55-34.08, n=5) | 7.02 (5.17-9.83, n=5) | 21.45 (20.68-33.77, n=5) |
| successive updates x100: reads | read | 100 | 1.06 (1.00-2.50, n=5) | 1.54 (1.09-2.78, n=5) | 0.81 (0.70-1.01, n=5) | 3.31 (3.14-6.05, n=5) |
| ancestor reads: parent tip 2000 | read | 2,000 | 8.76 (8.37-12.89, n=5) | 9.19 (8.56-14.51, n=5) | 2.75 (1.96-3.02, n=5) | 1219.49 (1147.47-1426.47, n=5) |
| ancestor reads: block-10 tip 2000 | read | 2,000 | 7.48 (6.74-10.22, n=5) | 7.07 (6.69-9.97, n=5) | 2.16 (2.01-3.25, n=5) | 1327.59 (1091.38-1395.76, n=5) |
| inherited hot64 after ancestor reads | read | 4,992 | 1.40 (1.38-2.41, n=5) | 0.58 (0.55-0.76, n=5) | 0.83 (0.73-1.22, n=5) | 8.38 (8.06-13.57, n=5) |
| seal | write | 1 | 18.5 ms (17.1-27.4, n=5) | 19.6 ms (17.2-25.4, n=5) | 16.8 ms (14.7-22.6, n=5) | 17.4 ms (16.5-27.6, n=5) |
| commit | write | 1 | 7.4 ms (6.9-11.5, n=5) | 7.4 ms (6.8-9.1, n=5) | 5.0 ms (4.6-6.1, n=5) | 16.0 ms (15.6-25.5, n=5) |
| whole construction (begin..commit) | total | 1 | 189.9 ms (162.0-261.2, n=5) | 158.9 ms (157.7-216.3, n=5) | 88.6 ms (76.4-114.6, n=5) | 5956.0 ms (5217.4-6299.7, n=5) |
| after commit: updated 200 | read | 200 | 13.03 (12.39-18.78, n=5) | 13.74 (12.05-19.08, n=5) | 11.30 (7.97-16.17, n=5) | 53.64 (52.91-82.14, n=5) |
| rollback: reads before drop | read | 500 | 1.29 (1.16-2.00, n=5) | 1.55 (1.30-2.53, n=5) | 1.11 (0.94-1.49, n=5) | 3.17 (2.34-4.89, n=5) |
| rollback: begin+insert+drop | write | 500 | 18.91 (13.11-24.50, n=5) | 17.21 (13.88-21.73, n=5) | 6.77 (5.37-8.46, n=5) | 37.27 (31.53-55.01, n=5) |
| rebuilt after rollback: reads | read | 500 | 10.75 (7.12-13.58, n=5) | 7.24 (6.88-10.86, n=5) | 5.36 (4.73-7.08, n=5) | 50.72 (46.38-74.07, n=5) |

What this establishes (measured):

* **The reference's result cache works where it applies**: hot reads of
  the open block cost 0.98 µs with it (219 node visits for 4,992 reads,
  4,928 hits) and 2.61 without; the rewrite walks every one of them in
  0.94. Inserted-key hot reads: 0.54 / 1.32 / 0.55.
* **Everything the result cache cannot serve is 3-4x faster in the
  rewrite**: wide inherited reads 2.78 vs 11.55 µs (now over 5,000 distinct
  keys), ancestor-tip reads 2.75 / 2.16 vs 9.19 / 7.07, rebuilt-block reads
  5.36 vs 7.24, reads after commit 11.3 vs 13.7 (the smallest gap: both
  reopen the committed trie).
* **Writes are 2.5-4.7x faster** (insert 2.27 vs 10.71, update 3.22 vs
  8.06, successive updates 7.0 vs 21.2, rollback inserts 6.8 vs 17.2),
  commit 5.0 vs 7.4 ms, seal 16.8 vs 19.6 / 18.5 / 17.4 ms (SHA-512/256
  bound; the rewrite's is lowest but the ranges overlap).
* **Whole block**: 88.6 ms rewrite, 158.9 ms reference with its result
  cache, 189.9 ms without, 5,956 ms upstream (its ancestor-tip reads cost
  1.2-1.3 ms each). 1.8x against the reference's production configuration,
  not the 4-5x of committed reads: the result cache and the cheap arena
  reads narrow it, and seal is a fifth of the rewrite's block.
* **Variation** is larger here than in section 5 (whole-block min-max
  76-115 ms for the rewrite, 158-216 for the reference), one reason the
  round-4 figures (85 / 139 / 157 ms) differ from these by up to 20%; the
  ordering is the same in every repetition.
* Rollback and the failed re-begin behave identically in all four
  configurations (asserted by the harness; identical roots).

## 8. Clarity execution

`contrib/clarity-bench` (`scripts/build-variants.sh`), real contracts
through `stackslib`'s `ClarityInstance` and `MarfedKV` with the MARF
provider switched; workloads as in the fourth review (read-heavy,
write-heavy, mixed, inherited-after-history, abort), 10 blocks per workload
(5 for the last two), the first block reported apart. 5 repetitions per
variant, each its own process, order rotated per repetition
(`run-r5.sh`), samples `results/r5/clarity-*.json`, table by `tables.py
clarity`. `exec` is every transaction of the timed blocks; `harness` is the
benchmark's own work inside the timed window (key selection, model
bookkeeping), reported apart and 1-2% of `exec`; `seal` and `commit` as
before. Equivalence: cost totals, final roots and a digest of every
returned value per workload are identical across all 15 runs (`tables.py
check`, 15 records).

**abort**

| variant | exec ms (median, min-max, n) | harness ms | seal ms | commit ms | first block exec ms |
|---|---|---|---|---|---|
| optmarf | 23.5 (21.3-31.2, n=5) | 0.1 (0.1-0.1, n=5) | 2.0 (1.8-2.4, n=5) | 5.0 (4.8-6.8, n=5) | 5.6 (5.2-9.2, n=5) |
| rewrite | 23.3 (22.3-34.3, n=5) | 0.1 (0.1-0.2, n=5) | 1.6 (1.5-2.2, n=5) | 5.1 (4.9-5.5, n=5) | 5.5 (5.2-7.9, n=5) |
| upstream | 96.5 (92.9-136.3, n=5) | 0.1 (0.1-0.2, n=5) | 2.5 (2.4-3.6, n=5) | 5.6 (4.4-6.5, n=5) | 25.3 (24.0-39.5, n=5) |

**inherited-after-history**

| variant | exec ms (median, min-max, n) | harness ms | seal ms | commit ms | first block exec ms |
|---|---|---|---|---|---|
| optmarf | 162.2 (156.5-216.2, n=5) | 2.7 (2.5-3.4, n=5) | 2.2 (2.0-2.7, n=5) | 5.2 (5.0-14.0, n=5) | 39.9 (37.4-50.8, n=5) |
| rewrite | 159.6 (135.8-168.0, n=5) | 3.2 (3.1-3.6, n=5) | 1.6 (1.5-1.7, n=5) | 5.6 (5.3-6.8, n=5) | 35.1 (33.7-53.5, n=5) |
| upstream | 521.5 (427.0-555.0, n=5) | 3.6 (3.3-3.9, n=5) | 2.8 (2.6-3.4, n=5) | 6.1 (4.7-31.9, n=5) | 122.7 (114.5-185.9, n=5) |

**mixed**

| variant | exec ms (median, min-max, n) | harness ms | seal ms | commit ms | first block exec ms |
|---|---|---|---|---|---|
| optmarf | 349.2 (331.3-381.4, n=5) | 4.2 (4.0-4.6, n=5) | 35.1 (30.7-42.9, n=5) | 24.2 (22.4-33.1, n=5) | 40.7 (31.7-56.1, n=5) |
| rewrite | 289.7 (258.2-430.6, n=5) | 5.3 (4.6-6.1, n=5) | 26.7 (25.5-35.5, n=5) | 21.2 (18.9-25.7, n=5) | 29.9 (27.4-54.5, n=5) |
| upstream | 872.6 (800.9-956.0, n=5) | 4.9 (4.6-5.6, n=5) | 34.2 (31.1-39.2, n=5) | 30.4 (27.4-37.0, n=5) | 100.6 (87.7-110.9, n=5) |

**read-heavy**

| variant | exec ms (median, min-max, n) | harness ms | seal ms | commit ms | first block exec ms |
|---|---|---|---|---|---|
| optmarf | 404.9 (369.0-471.5, n=5) | 6.4 (6.1-8.0, n=5) | 4.8 (4.7-4.9, n=5) | 13.0 (12.5-16.1, n=5) | 42.4 (39.1-48.5, n=5) |
| rewrite | 318.8 (303.8-444.3, n=5) | 7.2 (6.0-9.2, n=5) | 3.3 (3.0-3.9, n=5) | 12.2 (10.2-17.4, n=5) | 35.1 (34.0-59.3, n=5) |
| upstream | 1077.8 (1045.3-1254.2, n=5) | 7.9 (7.2-8.9, n=5) | 6.0 (5.5-6.9, n=5) | 14.7 (13.5-16.3, n=5) | 105.2 (98.0-141.7, n=5) |

**write-heavy**

| variant | exec ms (median, min-max, n) | harness ms | seal ms | commit ms | first block exec ms |
|---|---|---|---|---|---|
| optmarf | 590.6 (565.0-642.7, n=5) | 8.1 (7.3-8.7, n=5) | 163.4 (156.8-176.1, n=5) | 392.3 (362.9-405.3, n=5) | 75.2 (57.8-87.6, n=5) |
| rewrite | 585.3 (498.1-601.2, n=5) | 9.2 (8.9-9.7, n=5) | 138.5 (135.0-161.3, n=5) | 380.8 (347.6-416.8, n=5) | 53.8 (47.4-86.1, n=5) |
| upstream | 1111.4 (933.0-1156.4, n=5) | 9.1 (8.3-9.3, n=5) | 168.7 (137.7-182.1, n=5) | 473.6 (441.8-499.3, n=5) | 92.9 (76.7-109.8, n=5) |

Paired by repetition (same `rep` number, `exec` ms, rewrite / optmarf):
read-heavy 304/405, 350/441, 316/388, 319/369, 444/471 (rewrite faster
in 5 of 5, by 6-25%); write-heavy 498/565, 547/643, 590/591, 585/587,
601/639 (5 of 5, by 0-15%); mixed 277/343, 258/349, 290/359, 361/331,
431/381 (3 of 5); inherited 160/162, 136/216, 168/160, 155/157, 164/184
(4 of 5, three of them within 2%); abort 34/23, 22/31, 25/23, 23/21, 23/27
(2 of 5).

What this establishes:

* **Against upstream the speedup survives Clarity execution**: 3.4x
  (read-heavy), 1.9x (write-heavy), 3.0x (mixed), 3.3x (inherited), 4.1x
  (abort) on execution time; 1.1-1.3x on commit. Every repetition of every
  workload is faster than every upstream repetition.
* **Against the optimized MARF under identical consumer code the
  advantage is small and workload-dependent.** Read-heavy: 21% on the
  medians and the rewrite wins every paired repetition. Mixed: 17% on the
  medians but two of five pairs go the other way; write-heavy and
  inherited: medians within 1-2%, the pairs mostly favour the rewrite by
  single digits; abort: equal. Repetition-to-repetition variation
  (read-heavy rewrite 304-444 ms) is as large as the medians' difference,
  so the defensible statement is: never slower by more than noise, 15-25%
  faster on read-dominated blocks, equal elsewhere. The round-4 "5-25%" is
  replaced by this.
* **Seal is 15-30% faster in the rewrite in every workload** (3.3 vs 4.8,
  138.5 vs 163.4, 26.7 vs 35.1, 1.6 vs 2.2, 1.6 vs 2.0 ms); the same hash
  over the same nodes, so the difference is the hashing loop's buffer
  handling (inferred).
* **Inferred: the interpreter dominates.** 1,500 map reads per read-heavy
  block cost the rewrite about 35 ms of execution, roughly 23 µs per
  `map-get?`, against 1.5-2 µs for the MARF lookup underneath (section 5);
  a 4-5x MARF-level gain shows as 20% end to end. Which part of the per-read
  cost above the MARF is `MarfedKV` (two MARF lookups per map read, value
  and metadata, plus key hashing) stays open.

## 9. Validation

All `cargo nextest`, at `b553f53d` (the harness and crate sources of the
measured binaries), logs in `results/r5/validation/`, exit codes in its
`REV.txt`:

| suite | command | result |
|---|---|---|
| crate + differential | `cargo nextest run -p stacks-marf-next -p marf-differential --no-fail-fast` | 18 passed, 0 failed |
| crate counters build | `cargo nextest run -p stacks-marf-next --features counters` | 12 passed |
| harness counters build, incl. `tests/counters.rs` (rewrite and upstream providers; the upstream provider reports every counter unavailable, which the test accepts only as `None`, never as 0) | `cargo nextest run -p marf-differential --features counters --no-fail-fast` | 8 passed |
| reference counters, inside the reference worktree | `cargo test --release -p marf-differential --features counters --test counters` (run by `build-reference.sh`; `results/r5/evidence/build-reference-*.log`) | 2 passed (both providers consistent) |
| every hand-off boundary, both directions | `MARF_DIFF_ALL_HANDOFFS=1 cargo nextest run -p marf-differential -E 'test(/handoff/)'` | 2 passed (132 boundaries each way), 451 s |
| pruning and snapshot consumers | `cargo nextest run -p stackslib -p marf-squash -p clarity-cli --no-fail-fast -E 'test(/snapshot::tests::clarity/) \| test(/snapshot::tests::index/) \| package(marf-squash) \| package(clarity-cli)'` | 68 passed, 0 failed (2,203 of the stackslib binary's tests not selected) |
| benchmark correctness gates | every `ablate`, `construct` and `clarity-bench` process verifies every returned value against its model and panics otherwise; `tables.py check` compares the exported digests, roots and costs across providers | all processes exit 0; `results/r5/CHECK.txt` |

No test was skipped, disabled or quarantined. Not run this round, as
before: the full `stackslib` and `stacks-node` suites, a follower sync.

## 10. The six questions

Answers graded **measured** / **inferred** / **open**.

**1. Does the rewrite's speedup come from its larger patch-cache capacity?**
Measured: no. With no patches on the path and every probe a plain miss
the rewrite is 4x faster (2.0 vs 8.0 µs); with no patches in the database
(plain encoding) 2x; with both caches at 4,096 entries, every distinct
patched node resident and 100% of patched probes hitting, 4-5x (1.3-2.0
vs 6.4-10.1). Capacity matters for the rewrite itself (2.5-4x between 64
and 4,096 on wide sets, section 5.1) and for the reference in the
direction the working set decides (faster at 4,096 on `std`, slower on
`big`, sections 5.1 and 5.4); it does not explain the gap between them.
The round-4 observation that the reference slows down at 4,096 on `std`
was an artifact of shared-handle measurement (section 5.6) and is
withdrawn.

**2. How much does the cache lookup implementation itself cost, including
failed probes on plain records?** Measured: the reference probes before
every record read, so about half its probes are on plain records and fail
by construction (35,160 of 86,440 on bulk tip; 100% on patch-free walks);
its `SmallLru` scans its entries linearly per probe. Cost: 9% of its read
instructions at 64 entries, 44% at about 1,000, and on `big` the scan
outweighs the resolutions it saves. Removing the plain probes (late-probe
control) saves 1-3 µs per lookup at 4,096 on the wide sets and nothing at
64; removing them in instructions: 3.89 G to 1.09 G in `SharedLru::get`.
The rewrite's probe is a hash lookup (SipHash 0.15 G Ir over the same
reads) and it also probes on plain records. Inferred: an LRU with a hashed
index would remove most of the reference's lookup cost; it would not
close the gap (see 3).

**3. What is the residual traversal advantage?** Measured: with every
failed probe removed and every patched probe a hit the reference costs
6.0-8.6 µs per lookup, the rewrite 1.3-2.2, and executes 2.6x the
instructions for the same reads. Inferred (from the profile's function
attribution, not timed): the reference's cursor walk decodes owned
`TrieNodeType` values per visit (`load_from_parts`, `ptrs_from_parts_into`,
`from_slice_compressed`), rebuilds a `ReadTrieNode` and clones an `Arc`
per cache hit, and resolves block ids through SQLite rows, where the
rewrite parses a borrowed record view, selects the child in one skip-scan
without allocation and keeps block ids in a process map. Open: the exact
split of the residual among those functions.

**4. Does the advantage hold during block construction?** Measured: yes,
but smaller. 1.8x on the whole block against the reference's production
configuration (88.6 vs 158.9 ms), 2.1x without its result cache, 67x
against upstream. Hot reads of the open block are at parity (0.94 vs 0.98
µs; the reference's result cache answers them without a walk), every
other read is 3-4x faster, writes 2.5-4.7x, commit 1.5x, seal equal within
noise. Every value, phase digest and root is identical across providers;
the rollback and the failed re-begin behave identically.

**5. Does it survive Clarity execution, and what are the uncertainties and
regressions?** Measured: against upstream 1.9-4.1x on execution across
the five workloads, in every repetition. Against the optimized MARF under
identical consumer code: 21% on read-heavy blocks (5 of 5 pairs), equal
within noise on write-heavy, inherited-history and abort blocks, 17% on
the medians of mixed blocks with 2 of 5 pairs reversed. No workload is
slower by more than the repetition noise, which is the uncertainty: 10-40%
repetition-to-repetition on a 4-vCPU container. Seal is 15-30% faster in
every workload. No regression was measured against either baseline; the
known inefficiency of the rewrite (double record parse on patch-free
visits, section 5.5) is present in every measured binary.

**6. What is not measured: memory and workload limits.** Not measured:
mainnet-shaped tries (every database here is 40,000 or 1M keys, about
10 MB to 300 MB, resident in the page cache after one pass; page-cold
first passes are measured only at that size); the patch cache's memory at
scale (4,096 decoded `Node256` branches at about 6 KB each is about 25 MB
if a mainnet trie fills it, against the reference's 64 entries; measured
here only as peak heap on the small workloads, `STATUS.md`); concurrent
readers (every run is single-threaded; the reference's `SharedLru` is
built for sharing); the reference's whole optimized stack (its `stackslib`)
as a Clarity variant; workloads with more repeated reads per block than
these (where the reference's result cache, which the rewrite lacks, could
matter more); and the `MarfedKV`-level breakdown of the per-read cost above
the MARF. Follow-ups identified, not done: the double record parse; an LRU
or generation scheme in place of clear-all for working sets just above
capacity (bounded gain: 3.8 vs 2.7 µs at 79% vs 100% hits).

## 11. Revisions

| what | revision |
|---|---|
| sources of every measured binary (`crates/`, `contrib/`, `stackslib/`) | `b553f53df41dcbe3d0a530e77075b943a5e2a11d` (the commits after it up to the batch add results files only: `d6fd444c54`, `684d419ea0`) |
| build and measurement batch (`results/r5/ENV.txt`, manifests with every binary's sha256) | tree `684d419ea0277fadc1c3353365f2356fbb736463`; upstream oracle `ad5c57786bb51b58d4bdb4992ad534f762d8cf03` (in tree); reference `29ef093da5dd58c1fa484624d94a9d972356c629` |
| results and table script | `eb10e727b3` (`results/r5/`, `scripts/tables.py`) |
| validation (section 9) | `b553f53df4`, logs in `results/r5/validation/` |
| this document and `STATUS.md` | the commits after `eb10e727b3` on `claude/awesome-brown-xj309n`, documentation only |

Reproduction from a fresh clone of the branch: `contrib/marf-differential/scripts/build-rewrite.sh`,
`contrib/marf-differential/scripts/build-reference.sh`,
`contrib/clarity-bench/scripts/build-variants.sh` (root for the page-cold
cells), then `BIG=1 contrib/marf-differential/scripts/run-r5.sh <dir>` and
`python3 contrib/marf-differential/scripts/tables.py ablate|construct|clarity|check <dir>`.
The fresh-clone check of the pushed tip (every file of `results/r3`, `r4`,
`r5`, `contrib/clarity-bench/results`, the scripts and the overlay patches
present; the three build scripts run) is recorded in `results/r5/evidence/fresh-clone-check.log`.
