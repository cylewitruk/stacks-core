# Reference overlay

`scripts/build-reference.sh` builds this harness against the optimization
reference (`stacks-marf` at `29ef093da5dd58c1fa484624d94a9d972356c629`) as
the oracle. The reference's crate is checked out in a git worktree and left
as committed; the current tree's `crates/stacks-marf-next` and
`contrib/marf-differential` are copied in, then these patches apply:

* `harness.patch`: the adapter differences. `Error::Patch(_)` arity and
  `open_block(tip, None)` in `baseline.rs`; in `alloc.rs` the reference's
  open options (`with_mmap(true)`, `root_node_cache = false`,
  `resolved_patch_cache_capacity` = the experiment's value or the production
  64, `result_cache_capacity` = the experiment's value or the default 2,048),
  `MarfConnection` in scope for `get_with_proof`, and `baseline_counters`
  reading the reference's `read_bench` counters when the harness is built
  with `counters`; in `Cargo.toml` that feature also switches on the
  reference's `marf-read-bench-counters`.
* `reference-counters.patch`: three counters added to the reference's
  `read_bench::Counters` (result-cache hits, misses, invalidations) and the
  gated hooks in `result_cache.rs` that feed them. Compiled only with the
  reference's own `marf-read-bench-counters` feature, so the timing binary
  is the reference's production code.

Regenerate after editing the harness: run the script, fix the worktree copy,
then `diff -u` the three harness files and `git diff` the two reference
files back into these patches (the script's comments say which).
