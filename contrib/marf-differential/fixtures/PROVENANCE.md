# Fixture provenance

Every `*.json` here was recorded with `marf-differential record-all` by
driving the **upstream** `stacks-marf` crate (stacks-network/stacks-core
`main` at `ad5c57786bb51b58d4bdb4992ad534f762d8cf03`) through
`contrib/marf-differential/src/baseline.rs`. Expected observations never
come from the rewrite.

The same operation sequences were previously recorded from the optimized
reference (`29ef093da5dd58c1fa484624d94a9d972356c629`, cylewitruk/stacks-core
`perf/optimized-stacks-bench-integration`); the files produced by the two
oracles are byte-identical, which is itself a check that the optimized
reference preserves upstream behaviour on these sequences.

To re-record: `cargo run --release -p marf-differential -- record-all`
(then `git diff` must be empty).
