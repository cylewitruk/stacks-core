# MARF rewrite: status

Branch: `claude/awesome-brown-xj309n`.
Compatibility baseline: upstream `stacks-network/stacks-core` `main` at
`ad5c57786bb51b58d4bdb4992ad534f762d8cf03`; `stacks-marf/` at that commit is
the behavioural and storage oracle and is unchanged in this tree.
Optimization reference: `29ef093da5dd58c1fa484624d94a9d972356c629` (tag
`optimized-ref`, merge-base = the pinned upstream commit); used for ideas
and benchmarked against separately, not a compatibility requirement.
Rewrite: `crates/stacks-marf-next/` (crate `stacks-marf-next`).

Documents: `ANALYSIS.md` (behavioural contract and accidental complexity of
the original), `COMPAT.md` (what is read/written; the reference's storage
additions and their boundaries), `OPTIMIZATIONS.md` (every reference
optimization classified implemented / applicable / deferred),
`DECISIONS.md` (architecture log), `FUTURE.md` (format-changing
optimizations and where they plug in).

## Where things live

* `crates/stacks-marf-next/src/`: `lib` (ids, values, errors), `node`
  (logical nodes), `codec` (legacy encodings, borrowed record views), `hash`
  (consensus hashing), `trie` (arena for the trie being built, hashing,
  serialization), `storage` (SQLite + blobs file, memory map, caches), `marf`
  (forest API, views, transactions), `proofs`, `pruning` (`Prune` trait),
  `compat` (names other crates still import).
* `contrib/marf-differential/`: the behavioural oracle. Operation DSL,
  adapters calling the real APIs of both crates, seeded generators, 22
  recorded fixtures, replay and hand-off tests, the `alloc` measurement, CLI.

## Size

Production lines = non-blank lines that are not `//` comments, with trailing
`#[cfg(test)] mod` blocks removed, excluding `test/`, `tests/` and bench
modules (counted with a 10-line Python script applying these rules; re-run with
the same rules to compare).

| | Upstream `stacks-marf/src` at `ad5c5778` | Rewrite (`crates/stacks-marf-next/src`) |
|---|---|---|
| Production lines | 11,482 | 5,724 |
| Modules | 14 | 10 |
| Node representations | `TrieNode4/16/48/256`, `TrieLeaf`, `TrieNodeType`, packed/canonical/mapped variants | `Node::{Leaf, Branch}` plus borrowed `Record`/`BranchView`/`LeafView`/`PatchView` |
| Storage handle types | `TrieFileStorage`, `TrieStorageConnection`, `TrieStorageTransaction`, `ReopenedTrieStorageConnection`, `TrieRAM`, `TrieFile`, … | `MARF`, `MarfTransaction`, `View`, `ReopenedMarf`, `TrieFileStorage` (shim) |

The differential harness is another 2,617 lines (`contrib/marf-differential/src`).

## Tests and how to run them

```
cargo nextest run -p stacks-marf-next -p marf-differential --no-fail-fast
```

Latest result: 16 tests, 16 passed (10 unit tests in `stacks-marf-next`;
6 harness tests: `baseline_reproduces_fixtures`,
`rewrite_reproduces_fixtures`, `rewrite_reproduces_supported_fixture_ops`,
`storage_variants_agree_on_consensus_observations`,
`baseline_then_rewrite_handoff`, `rewrite_then_baseline_handoff`). The
hand-off tests split fixtures after a `Close` or a `Prune`, run the prefix
on one implementation and the suffix on the other over the same directory,
and assert every fixture contributes at least one split. Coverage per
direction: every `Prune` boundary plus a quarter of the `Close` boundaries
of each fixture, evenly spaced: 48 of the 132 eligible boundaries.
`MARF_DIFF_ALL_HANDOFFS=1` splits at all 132 (about eight minutes per
direction); run on this tip after the staged-pruning change, both directions
passed with no divergences.

Fixtures (`contrib/marf-differential/fixtures/*.json`, recorded from
upstream's `stacks-marf` at `ad5c5778` with `marf-differential record-all`;
see `fixtures/PROVENANCE.md`; the same sequences recorded from the optimized
reference produce byte-identical files), 22 in all. Suffixes name the storage variant
(`ext`/`sql` blobs, `comp`/`plain`, `_imm` = immediate hashing):
`small_*` (one seed, four variants), `dense_*`, `forks_ext_comp`,
`wide_ext_comp` (6,000 inserts per block), `patch_chain_ext_comp` (patch
chains to depth 4), `unconfirmed_*`, `prune_*` (pruning then extension),
`edge_cases_*` (error paths), `lifecycle_*` (rollbacks, reopen, drop paths),
`proofs_ext_comp` (proofs, and tampered proofs: flipped byte, truncation,
wrong value, wrong root, empty block map), `regress_*` (scenarios ported from
the original's `node_patch`/`marf_regression` tests: 1x256, 128x128, 2048x1,
8x256 batch).

`clippy --all-targets --no-deps` is clean for both crates.

## Implemented and verified against the original

* Opening/creating databases, schema 1-3 migration, internal and external
  blobs; schema-4 (format 6) databases are refused with an explicit error.
* `begin`, inserts, `seal`, `commit`, `commit_to`, `commit_mined`,
  `drop_current`, `drop_unconfirmed`, transactions (`begin_tx`, `commit_tx`,
  `rollback`, `into_sqlite_tx`), forks, reopen, read-only reopen.
* Exact root hashes (ancestor skip-list), block-height keys,
  `get_block_at_height`, `get_block_height`, `check_ancestor_block_hash`,
  `open_block`.
* Every legacy record encoding, read and written; patch generation and
  resolution.
* Unconfirmed tries, including resuming a persisted one.
* Merkle proofs: generation, wire format, verification, rejection of
  malformed proofs.
* `for_each_leaf`.
* Pruning (`Prune::prune_to_path`) and pruned-MARF semantics.

## Borrowed traversal (acceptance criterion from the second review)

Reads borrow records; the one owned node on the read path is a resolved
patch chain, produced on a cache miss and then cached:

* `codec::Record` borrows `hash || body` from storage; `Record::descend`
  compares the path and selects the child for the next key byte in one
  skip-scan over the slot list, reading two bytes per slot (type flags and
  character) to find where each occupied slot's pointer starts. This applies
  to every encoding, plain `Node256`/`Node48` included: pointer widths vary
  per slot, so a slot's position cannot be computed without the scan, and
  the `Node48` index array is not consulted on lookup. Nothing is allocated
  by the scan. No `Cow`; owned `Branch`/`Leaf` are produced only by
  `to_node`/`to_branch` for copy-on-write, proofs and pruning, and by patch
  resolution (next point).
* `storage::Blobs` memory-maps the blobs file lazily and re-maps only when a
  read reaches past the mapped length. SQLite-stored tries use one reusable
  buffer per store.
* A patch chain met cold is resolved by decoding its base record into an
  owned `Branch` (one allocation of the slot vector) and applying the
  patches; the result goes into a bounded per-store cache (`(block_id,
  offset)` -> `Branch`, 4,096 entries) and later reads borrow it, so a hot
  node that is patched in block after block is decoded once per cache
  lifetime. This is the allocation the zero-allocation lookup figures below
  do not show: they measure warm caches. Unconfirmed tries bypass the cache.
* Hashing collects each branch preimage into two reusable buffers and asks
  for every child's contribution once.
* `MarfTransaction` holds `Option<Transaction>`; rollback or an uncommitted
  drop invalidates block-id, location and patch caches.

### Allocation and time per operation

`marf-differential alloc <external|sqlite> <compressed|plain>` installs a
counting global allocator (allocations, bytes, and the peak of live heap
above where a phase started) and drives both crates through their public
APIs (no DSL, no observation encoding). Workload: 20 blocks x 2,000 fresh
keys, then 8 blocks rewriting 50 keys each (so lookups at the tip go through
patch chains), 5,000 reads per read phase, 500 proofs, then a prune of the
whole chain at the final tip. `REPEAT=3`: allocation counts and peaks from
the first run (deterministic), times the best of three, `--release`, this
container with nothing else running (all tables in this section come from
one sequential, uncontended batch). "next" is the rewrite in every table.

Two oracles, measured separately with their own binaries:

* **upstream** = `stacks-marf` at `ad5c5778`, `MARFOpenOpts::new(Deferred,
  external).with_compression(c)` (upstream has no mmap or cache options).
  Branch `marf-v3-upstream`.
* **reference** = `stacks-marf` at `29ef093d` with its production options
  (`Config::get_marf_opts`: `with_mmap(true)`, `root_node_cache = false`,
  `resolved_patch_cache_capacity = 64`, default 2,048-entry result cache).
  Built in a worktree of the reference branch with the identical rewrite and
  harness sources copied in. The "next" column of the reference binary's
  run agreed with the upstream binary's run to within 0.1-0.5 µs on lookups
  and within 15% on seal and commit (two runs of the same code); the tables
  show the upstream binary's "next" values.

**External blobs, compressed (the node's production storage):**

| phase | ops | upstream alloc/op | upstream µs/op | reference alloc/op | reference µs/op | next alloc/op | next µs/op |
|---|---|---|---|---|---|---|---|
| insert | 40,000 | 38.7 | 14.3 | 8.1 | 7.2 | 0.87 | 2.0 |
| seal (hashing) | 20 | 3,348 | 16,092 | 26.8 | 14,102 | 17.0 | 12,691 |
| commit (serialize + write) | 20 | 2,202 | 11,984 | 1,312 | 5,723 | 531.5 | 4,266 |
| warm_get | 5,000 | 44.3 | 1,200 | 4.3 | 12.9 | 0.02 | 1.7 |
| miss_get | 5,000 | 40.4 | 1,271 | 4.1 | 10.2 | 0.00 | 1.5 |
| patched_get | 5,000 | 41.6 | 127 | 3.9 | 11.0 | 0.00 | 1.7 |
| proof | 500 | 3,030 | 22,829 | 316 | 1,222 | 103.5 | 343 |
| prune (per node, 48,630 nodes) | 48,630 | 4.07 | 9.09 | 2.95 | 8.02 | 0.62 | 4.49 |

**External blobs, plain:**

| phase | ops | upstream alloc/op | upstream µs/op | reference alloc/op | reference µs/op | next alloc/op | next µs/op |
|---|---|---|---|---|---|---|---|
| insert | 40,000 | 19.0 | 8.5 | 7.7 | 6.9 | 0.70 | 1.8 |
| seal | 20 | 2,729 | 14,373 | 25.8 | 14,203 | 16.9 | 14,289 |
| commit | 20 | 33.4 | 5,489 | 33.9 | 5,562 | 25.5 | 4,937 |
| warm_get | 5,000 | 16.0 | 18.6 | 0 | 4.9 | 0.00 | 4.0 |
| miss_get | 5,000 | 13.1 | 13.1 | 0 | 3.4 | 0.00 | 3.3 |
| patched_get | 5,000 | 17.9 | 18.1 | 0 | 4.8 | 0.00 | 3.6 |
| proof | 500 | 1,798 | 2,449 | 214 | 910 | 126.3 | 512 |
| prune (per node) | 48,630 | 3.87 | 8.97 | 2.86 | 7.71 | 0.59 | 4.89 |

**SQLite-internal blobs, plain:**

| phase | ops | upstream alloc/op | upstream µs/op | reference alloc/op | reference µs/op | next alloc/op | next µs/op |
|---|---|---|---|---|---|---|---|
| insert | 40,000 | 19.0 | 15.2 | 7.7 | 14.6 | 0.70 | 7.0 |
| seal | 20 | 2,729 | 14,790 | 25.9 | 14,959 | 16.9 | 12,442 |
| commit | 20 | 30.5 | 11,040 | 30.6 | 10,073 | 25.5 | 9,253 |
| warm_get | 5,000 | 16.0 | 90.2 | 0 | 60.5 | 0.00 | 70.8 |
| miss_get | 5,000 | 13.1 | 51.1 | 0 | 35.4 | 0.00 | 37.2 |
| patched_get | 5,000 | 17.9 | 30.7 | 0 | 21.3 | 0.00 | 25.0 |
| proof | 500 | 1,798 | 8,131 | 214 | 8,016 | 126.3 | 7,467 |
| prune (per node) | 48,630 | 3.87 | 14.6 | 2.86 | 21.6 | 0.59 | 13.3 |

**Peak live heap during a phase** (KiB above the phase's starting point;
external compressed; the reference's `next` column is the same code, shown
to give the run-to-run spread):

| phase | upstream | reference | next | next (reference run) |
|---|---|---|---|---|
| insert (one 2,000-key block) | 2,538 | 2,611 | 2,500 | 2,500 |
| commit (one block) | 1,979 | 2,059 | 648 | 648 |
| proof (500 proofs) | 488 | 356 | 914 | 914 |
| prune (48,630 nodes) | 5,863 | 5,919 | 4,504 | 4,504 |

### Pruning memory (third review, major finding)

The first version of `prune_to_path` collected every decoded node into a
`Vec<Node>` and built the whole blob in memory, which upstream's squash
does not do (it stages nodes in a temporary file and streams the output).
ADR-10 describes the replacement: nodes are staged on disk in the record
encoding, hashed children-first with one 32-byte hash per node in memory,
and streamed into the blobs file children-first with the root in a reserved
slot (upstream's layout). Measured with the harness's `prune` phase, peak
live heap during the prune, same binary for upstream and the rewrite:

| workload | nodes collected | rewrite before (`b35cf463b4`) | rewrite now | upstream | reference |
|---|---|---|---|---|---|
| 28 blocks, 40,400 keys (default) | 48,630 | 15,878 KiB | 4,504 KiB | 5,863 KiB | 5,919 KiB |
| 58 blocks, 200,400 keys | 254,505 | 81,573 KiB | 23,772 KiB | 31,526 KiB | not run |
| per node (both sizes agree) | | ~330 B | ~95 B | ~125 B | |
| time per node (default / large) | | 3.5 µs | 4.5 / 5.0 µs | 9.1 / 9.6 µs | 8.0 µs |

The rewrite's 95 bytes a node are 48 bytes of per-node arrays (stage
offset, hash, blob offset) plus the dedup index while collecting (a hash map
keyed by source location, sized by power-of-two growth) and two 1 MiB
stream buffers. Staging costs about 30% in time against the in-memory
version and is still about twice as fast as upstream per node. Both
implementations scale linearly in the measured range; a mainnet-sized trie
(hundreds of millions of nodes) would need tens of GB in either, so
qualification on a real database remains a next step for both.

### Lookups qualified by walk shape

A MARF lookup's cost depends on what the walk meets: how many back-pointers
it follows into older blocks, whether the records on the path are stored as
patch chains and how deep, and whether the key was read recently. The
rewrite's test-only `View::lookup_profile` reports those properties for a
key; the stored trie is identical for every implementation, so the same key
has the same shape everywhere. `marf-differential alloc` profiles every key
of the database at two tips (distinct keys by construction), buckets them by
`hops` (back-pointers followed, 3+ capped) x `patch` (deepest chain on the
path: none, 1-2, 3-4), and times each bucket under three explicit access
patterns, each 5,000 lookups after one warming pass over the same keys:

* **hot64**: 64 distinct keys cycled. Everything hits a result cache.
* **fit(n)**: up to 1,024 distinct keys cycled. Fits any result cache of
  2,048 entries or more (the reference's), so it still measures hits, with
  less locality than hot64. `n` is the number of distinct keys timed.
* **overflow(n)**: every distinct key of the bucket, in whole passes, only
  for buckets of at least 4,096 keys (twice the reference's result cache):
  an LRU result cache misses on every access. `n` is the bucket's
  population, all of it timed.

`~k nodes` is the average number of records read per walk. Workload as
above (external blobs, compressed); times are best of three, µs per lookup.

* **bulk tip** = block 20, written by 2,000-key blocks: its root is a full
  `Node256` and the second-level `Node256` nodes are patch chains of depth
  3-4 against earlier blocks.
* **patched tip** = block 28, after eight 50-key blocks: root and hot
  children are small patch chains; `hops=0` keys are those written by the
  tip block itself.

| tip | hops | patch | ~nodes | pattern | upstream alloc/op | upstream KB/op | upstream µs | reference alloc/op | reference KB/op | reference µs | rewrite alloc/op | rewrite µs |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| bulk | 0 | 3-4 | 3.6 | hot64 | 42.3 | 92.7 | 1,298 | 4.00 | 4.2 | 11.7 | 0 | 1.07 |
| bulk | 0 | 3-4 | 3.6 | fit(1,024) | 42.3 | 92.7 | 1,243 | 4.13 | 4.3 | 12.7 | 0 | 1.31 |
| bulk | 1 | 3-4 | 3.4 | hot64 | 42.7 | 93.7 | 1,234 | 4.09 | 4.2 | 11.7 | 0 | 1.03 |
| bulk | 1 | 3-4 | 3.4 | fit(1,024) | 43.3 | 93.8 | 1,298 | 4.22 | 4.4 | 12.3 | 0 | 1.17 |
| bulk | 1 | 3-4 | 3.4 | overflow(34,661) | 43.5 | 93.9 | 1,329 | 4.19 | 4.4 | 15.0 | 0 | 1.94 |
| bulk | 2 | 3-4 | 4.0 | hot64 | 56.0 | 96.7 | 1,269 | 6.34 | 4.8 | 14.4 | 0 | 1.00 |
| bulk | 2 | 3-4 | 4.0 | fit(1,024) | 55.8 | 96.8 | 1,333 | 6.32 | 4.9 | 14.7 | 0 | 1.21 |
| patched | 1 | 1-2 | 3.4 | hot64 | 34.4 | 48.7 | 105 | 2.26 | 2.3 | 7.2 | 0 | 1.20 |
| patched | 1 | 1-2 | 3.4 | fit(1,024) | 31.8 | 47.3 | 104 | 0.62 | 0.7 | 5.3 | 0 | 1.26 |
| patched | 1 | 3-4 | 3.4 | hot64 | 44.5 | 51.5 | 151 | 5.06 | 4.2 | 12.2 | 0 | 0.90 |
| patched | 1 | 3-4 | 3.4 | fit(1,024) | 41.7 | 49.9 | 133 | 2.74 | 2.3 | 8.4 | 0 | 0.99 |
| patched | 2 | 1-2 | 3.4 | hot64 | 33.3 | 52.2 | 123 | 1.06 | 1.4 | 5.3 | 0 | 1.57 |
| patched | 2 | 1-2 | 3.4 | fit(1,024) | 34.4 | 52.5 | 111 | 1.44 | 1.8 | 7.0 | 0 | 1.67 |
| patched | 2 | 1-2 | 3.4 | overflow(14,372) | 34.7 | 52.7 | 112 | 1.50 | 1.9 | 7.9 | 0 | 1.98 |
| patched | 2 | 3-4 | 3.4 | hot64 | 47.2 | 55.9 | 157 | 5.00 | 4.5 | 17.1 | 0 | 0.92 |
| patched | 2 | 3-4 | 3.4 | fit(1,024) | 47.5 | 56.0 | 147 | 4.91 | 4.4 | 15.5 | 0 | 0.98 |
| patched | 2 | 3-4 | 3.4 | overflow(15,778) | 47.8 | 56.1 | 155 | 4.89 | 4.4 | 14.6 | 0 | 1.28 |
| patched | 3+ | 1-2 | 4.0 | hot64 | 45.6 | 53.8 | 114 | 3.86 | 2.8 | 8.6 | 0 | 1.42 |
| patched | 3+ | 1-2 | 4.0 | fit(1,024) | 44.9 | 53.6 | 138 | 3.62 | 2.6 | 9.4 | 0 | 2.18 |
| patched | 3+ | 3-4 | 4.0 | hot64 | 58.1 | 56.9 | 155 | 7.11 | 5.0 | 18.6 | 0 | 0.95 |
| patched | 3+ | 3-4 | 4.0 | fit(1,024) | 58.2 | 57.1 | 154 | 7.07 | 5.0 | 19.4 | 0 | 1.04 |

(µs column values above 1,000 are milliseconds: 1,298 µs = 1.30 ms.
Buckets with fewer than 64 keys are not timed; `patch=none` does not occur
at either tip in this workload: with 256-way roots, blocks of 2,000 or 50
inserts always leave the second level, or the root, as patch chains. A
workload whose blocks rewrite every root child would be needed to measure
un-patched walks. Only three buckets exceed 4,096 keys, so overflow is
measured for those three.)

What the buckets show:

* Upstream's cost is set by the records on the path, not by the number of
  hops or by the access pattern: through the bulk tip's full `Node256`
  records it is 1.2-1.3 ms per lookup with ~93 KB allocated (every record
  is decoded into owned 256-slot nodes, patch chains re-resolved each
  time); through the patched tip's small patch records it is 105-157 µs.
  Hops add roughly 10-15 µs each; hot64, fit and overflow are the same
  within noise, as expected without a result cache.
* The reference's caches are visible: on the bulk tip, overflow costs 28%
  more than fit (15.0 vs 12.3 µs) and on the patched tip 10-50% more than
  hot64 in the shallow-patch buckets; its allocations per lookup drop as the
  key set shrinks (0.6-2.3 against 4-7). Hops cost it 1-3 µs each. Its
  lookups never go below 5 µs in any bucket: a hit still walks the trie
  through its node readers.
* The rewrite is 0.9-2.2 µs in every bucket with zero allocations, in all
  three patterns: records are borrowed from the mapping, a walk reads
  3.4-4.0 records, and the patch chains it meets are served from the
  resolved-patch cache after the warming pass. Overflow costs it 30-90%
  over hot64 (1.94 vs 1.03 µs at bulk hops=1), which is page-cache and
  patch-cache locality, not a result cache (it has none). Back-pointer hops
  cost it about 0.1-0.3 µs each.
* The rewrite's figures above are warm-cache figures: the warming pass
  resolves each patch chain once (an owned `Branch`, see "Borrowed
  traversal"). A cold process pays that once per distinct patched node.

Reading the numbers:

* Against upstream on the production configuration (compressed external
  blobs) the rewrite's lookups are two to three orders of magnitude faster:
  upstream re-resolves patch chains on every read and allocates throughout;
  the rewrite's reads are borrowed and its resolved patches are cached.
* Against the reference, which already has mmap reads, a result cache and a
  patch cache, the rewrite's compressed lookups are 4 to 12x faster with
  zero allocations (the gap is widest where the reference's result cache
  misses). On the plain encoding the two are within noise of each other
  (external: rewrite ahead by 3-25%; SQLite-internal: reference ahead by
  5-17%). Commit is faster in all three configurations. Seal is on par,
  not faster: SHA-512/256 bound in all three (70% of the rewrite's seal in
  callgrind), it ranged from 21% faster to 5% slower than the oracles
  across the runs here and was 12% slower in an independent run on another
  machine; treat it as equal within machine variance.
* SQLite-internal storage: all three pay one `blob_open` per node; after
  the read-size fix the rewrite is within 20% of both oracles on every read
  phase and within 10% on proofs.
* Peak heap: inserts and commits of a 2,000-key block are 2.5 MiB and
  0.65 MiB in the rewrite against 2.0-2.6 MiB in the oracles; proofs peak
  higher in the rewrite (0.9 MiB against 0.35-0.5 MiB) because it collects
  the whole proof before encoding it; pruning is covered above.
* Seal allocates more bytes in the rewrite because its two hash buffers
  (17 KiB) are created per seal; proofs allocate in all implementations
  (owned proof nodes are the wire format).

## Consumer integration

The five consumer crates (`stackslib`, `stacks-node`, `contrib/clarity-cli`,
`contrib/stacks-inspect`, `contrib/marf-squash`) depend on the rewrite via
`stacks-marf = { package = "stacks-marf-next" }` (ADR-22). `cargo check
--workspace --all-targets` is clean with the call-site changes listed in
`COMPAT.md` §2. Test suites run against the rewrite (nextest filter: Clarity
VM tests and database, chainstate snapshot, chainstate db and tests,
sortition db, `util_lib::db`, every test mentioning `marf`, `squash` or
`unconfirmed`, Nakamoto node tests, plus the `marf-squash` and
`clarity-cli` crates):

Result of the parallel run: **921 tests selected, 918 passed, 0 failed**, 3
stalled (see below). Command:

```
cargo nextest run -p stackslib -p stacks-node -p marf-squash -p clarity-cli --no-fail-fast \
  -E 'test(/clarity_vm::tests/) | test(/clarity_vm::database/) | test(/snapshot/) | test(/util_lib::db/) | test(/burn::db::sortdb::tests/) | test(/chainstate::stacks::db::/) | test(/chainstate::stacks::tests/) | test(/marf/) | test(/squash/) | test(/unconfirmed/) | test(/nakamoto::tests::node/)'
```

Three tests of that filter stalled in the parallel run and were re-run
serially: `accounting::test_coinbase_pay_to_alt_principal_v210`,
`accounting::test_coinbase_pay_to_contract_v210` and
`block_construction::test_contract_call_across_clarity_versions`. A stack
trace of the stalled processes (zero CPU for over an hour) showed them in
`NetworkState::bind_address`, which retries a `TcpListener::bind` with
doubling backoff for as long as the address is in use; these tests use
hard-coded ports (two block-construction tests share 2032/2033, and the two
coinbase tests call the same helper with one port pair), so running them in
parallel can leave them sleeping indefinitely. Re-run alone with `-j 1`
they still stalled: the address was `0.0.0.0:2024`, which in this container
is held by the sandbox's own service. With the three fixtures moved to
12024/12025 (a separate, clearly labelled commit that can be dropped
elsewhere) all three pass in about one second each: **3 passed, 0 failed**,
bringing the consumer run to **921 of 921**. The stall was environmental,
not a MARF behaviour.

After the staged-pruning change (third review) the pruning consumers were
re-run: the snapshot `clarity` and `index` tests in `stackslib` (which call
`prune_to_path` on real chainstate) and the complete `marf-squash` and
`clarity-cli` packages, **68 of 68 passed**, alongside the 17 crate and
differential tests (both hand-off directions include squash boundaries).

## Known differences and limitations

* Format-6 / stable-ID databases of the optimization reference are refused
  at open; a reference-published direct-hash sidecar is marked invalid on
  the rewrite's first write (`COMPAT.md` §3). Upstream produces neither.
* `TrieHashCalculationMode::Immediate` is accepted but hashing is always
  deferred; the root hash of an unsealed, uncommitted trie reads as zero.
* Reads name their block; there is no "current block" cursor. A read issued
  right after a failed `commit_to` or after `commit_mined` succeeds where
  upstream returns `NotFoundError` (its cursor still names the abandoned
  trie). Consumers `open_block` a tip first; fixtures mirror that.
* No lookup-result cache (ADR-17); no root-node cache (`OPTIMIZATIONS.md`
  A7/A8 for the format-neutral options).
* The blobs map assumes the file is only appended to while open; truncating
  it from outside the process would fault the mapping (the reference's mmap
  path has the same assumption; upstream reads with `pread`).
* Schema-1 export migration is implemented but untested against a real
  schema-1 database.

## Next steps

1. Run the full `stackslib` and `stacks-node` test suites (not only the
   MARF-related filter above) against the rewrite, and a testnet or mainnet
   follower sync as the end-to-end check.
2. Performance qualification on a mainnet-shaped database with separated
   measurements (lookups, inserts, seal, commit, proofs, open; peak memory);
   extend `alloc` with a replay-from-database mode.
3. Format-neutral items from `OPTIMIZATIONS.md` worth measuring first:
   per-building-trie lookup-result cache (A8), covering block-hash index
   (A11), profiler spans (E3).
4. Qualify pruning on a mainnet-sized database (peak heap measured at about
   95 bytes a node; the temp file is about the size of the output blob).
5. Format-changing items stay deferred (`FUTURE.md`).
