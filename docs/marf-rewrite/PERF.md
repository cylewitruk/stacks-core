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

EXPERIMENT_MATRIX

## 4. Committed reads: patch-cache ablation

ABLATION_RESULTS

## 5. Reads during block construction

CONSTRUCTION_RESULTS

## 6. Clarity execution

CLARITY_RESULTS

## 7. What the experiments establish

CAUSES

## 8. Final tested commit and validation

VALIDATION
