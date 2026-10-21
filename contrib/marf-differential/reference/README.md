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
    be a plain node, whether the record came from the mapped blob or from
    the SQLite path), `patch_resolutions` (chains actually resolved in
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
  * under `marf-diag-late-probe` (new feature, diagnostic control; repaired
    in the sixth review): for a record in the mapped blob,
    `read_node_with_state` reads the record first and probes the
    resolved-patch cache only when it is a patch, so a mapped plain record
    is never probed; a record on the SQLite fallback path is probed before
    its read, exactly as production. What the diagnostic changes, and what
    it does not: it removes the probes on mapped plain records (which can
    never hit: only resolved chains are admitted); it moves the probe for a
    mapped patch to after its first patch record has been parsed, so a hit
    costs one borrowed record parse the production order does not pay
    (counted as `late_probe_after_patch_parse`, exported by the harness as
    `late_probe_extra_parses`); successful hits still pay the `SharedLru`
    lock, the `SmallLru` linear scan and the recency rotation; admission
    and SQLite-path behaviour are those of production. Its timing
    difference against the production order is therefore the net effect of
    reordering the probe (plain probes removed, one extra parse per mapped
    hit), not an isolated measurement of failed probes. Results are
    unchanged; never a production setting. The harness's counter sanity
    tests assert `probes == hits + plain misses + admissions` for the
    reference as well as the rewrite; the identity holds for the diagnostic
    too (SQLite-path plain records count as plain misses).

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
