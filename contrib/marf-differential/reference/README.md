# Reference overlay

`scripts/build-reference.sh` builds this harness against the optimization
reference (`stacks-marf` at `29ef093da5dd58c1fa484624d94a9d972356c629`) as
the oracle, from a script-owned worktree: the worktree is created (or reset)
at that revision and verified, the current tree's `crates/stacks-marf-next`
and `contrib/marf-differential` are copied in and registered as workspace
members, then these patches apply:

* `harness.patch`: the adapter differences, generated as a diff from this
  tree's files to the adapted copies. `Error::Patch(_)` arity and
  `open_block(tip, None)` in `baseline.rs`; in `alloc.rs` the reference's
  open options (`with_mmap(true)`, `root_node_cache = false`,
  `resolved_patch_cache_capacity` = the experiment's value or the production
  64, `result_cache_capacity` = the experiment's value or the default 2,048),
  `MarfConnection` in scope for `get_with_proof`, `baseline_counters` reading
  the reference's `read_bench` counters and `baseline_occupancy` its cache
  length when the harness is built with `counters`; in `Cargo.toml` the
  `counters` feature switches on the reference's `marf-read-bench-counters`
  and `reference-lateprobe` its `marf-diag-late-probe`.
* `reference.patch`: changes to the reference crate itself, all behind
  features that are off in the timing binary, so the timing binary is the
  reference's production code:
  * under `marf-read-bench-counters` (its own opt-in counter module): new
    `read_bench::Counters` fields `patch_cache_probes` (every probe of the
    resolved-patch cache, made before the record is read),
    `patch_cache_probe_miss_plain` (probes whose record then turned out to
    be a plain node), `patch_resolutions` (chains actually resolved in
    `read_patched_persisted_node`, cache enabled or not),
    `owned_materializations` (resolved nodes and root-cache fills),
    `result_cache_hits` / `result_cache_misses` /
    `result_cache_invalidations` (hooks in `result_cache.rs`), plus
    `SharedLru::len`, `TrieStorageConnection::resolved_patch_cache_len` and
    `MARF::patch_cache_len` for occupancy. The pre-existing counters
    (`resolved_patch_cache_hits`, `resolved_patch_cache_misses` =
    admissions, `patch_records` = patch applications, `cursor_steps`,
    `backpointer_follows`, `mmap_node_returns`, `resolved_item_reads`) are
    used as they are.
  * under `marf-diag-late-probe` (new feature, diagnostic control for the
    fifth review): `read_node_with_state` reads the mapped record first and
    probes the resolved-patch cache only when that record is a patch,
    instead of probing by location before every read. A plain record's
    probe can never hit (only resolved chains are admitted), so the change
    preserves results; it removes the failed probes on plain records.
    External-blob storage only (the SQLite-stored path is unchanged and its
    cache is then not consulted); never a production setting.

The script builds `marf-differential-reference` (timing),
`marf-differential-reference-counters` (counters), runs the harness's
counter sanity tests (`tests/counters.rs`) in the worktree, builds
`marf-differential-reference-lateprobe` (the diagnostic control, timing) and
writes `target/bench-bin/MANIFEST.reference` (revisions, patch hashes,
binary hashes).

Regenerating after editing the harness: run the script, make the edits in
the worktree, then `diff -u` the three harness files (tree to worktree) into
`harness.patch` and `git -C <worktree> diff -- stacks-marf` into
`reference.patch`.
