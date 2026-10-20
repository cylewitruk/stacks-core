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
hand-off tests split every fixture after each `Close` and `Prune`, run the
prefix on one implementation and the suffix on the other over the same
directory, and assert every fixture contributes at least one split.

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

Reads never materialize nodes:

* `codec::Record` borrows `hash || body` from storage; `Record::descend`
  compares the path and selects the child for the next key byte in one pass
  over the slot list (a skip-scan reading two bytes per slot). Plain-encoded
  `Node256`/`Node48` are indexed directly; compressed encodings are scanned.
  No `Cow`; owned `Branch`/`Leaf` are produced only by `to_node`/`to_branch`
  for copy-on-write, proofs and pruning.
* `storage::Blobs` memory-maps the blobs file lazily and re-maps only when a
  read reaches past the mapped length. SQLite-stored tries use one reusable
  buffer per store.
* Patch chains resolve into a bounded per-store cache (`(block_id, offset)`
  -> `Branch`, 4,096 entries), so a hot node that is patched in block after
  block is decoded once. Unconfirmed tries bypass the cache.
* Hashing collects each branch preimage into two reusable buffers and asks
  for every child's contribution once.
* `MarfTransaction` holds `Option<Transaction>`; rollback or an uncommitted
  drop invalidates block-id, location and patch caches.

### Allocation and time per operation

`marf-differential alloc <external|sqlite> <compressed|plain>` installs a
counting global allocator and drives both crates through their public APIs
(no DSL, no observation encoding). Workload: 20 blocks x 2,000 fresh keys,
then 8 blocks rewriting 50 keys each (so lookups at the tip go through patch
chains), 5,000 reads per read phase, 500 proofs. `REPEAT=3`: allocation
counts from the first run (deterministic), times the best of three,
`--release`, this container. "next" is the rewrite in every table.

Two oracles, measured separately with their own binaries:

* **upstream** = `stacks-marf` at `ad5c5778`, `MARFOpenOpts::new(Deferred,
  external).with_compression(c)` (upstream has no mmap or cache options).
  Branch `marf-v3-upstream`.
* **reference** = `stacks-marf` at `29ef093d` with its production options
  (`Config::get_marf_opts`: `with_mmap(true)`, `root_node_cache = false`,
  `resolved_patch_cache_capacity = 64`, default 2,048-entry result cache).
  Built in a worktree of the reference branch with the identical rewrite and
  harness sources copied in. These runs overlapped with a large test build
  on the same machine, so their absolute times carry more noise than the
  upstream runs; ratios within a row are still indicative.

**External blobs, compressed (the node's production storage):**

| phase | ops | upstream alloc/op | upstream µs/op | reference alloc/op | reference µs/op | next alloc/op | next µs/op |
|---|---|---|---|---|---|---|---|
| insert | 40,000 | 38.7 | 12.4 | 8.1 | 7.6 | 0.87 | 1.7 |
| seal (hashing) | 20 | 3,348 | 13,489 | 28.4 | 16,304 | 17.0 | 13,077 |
| commit (serialize + write) | 20 | 2,201 | 10,860 | 1,493 | 9,671 | 531.5 | 3,913 |
| warm_get | 5,000 | 44.3 | 1,224 | 5.0 | 8.4 | 0.02 | 1.6 |
| miss_get | 5,000 | 40.4 | 1,256 | 3.8 | 6.3 | 0.00 | 0.9 |
| patched_get | 5,000 | 41.6 | 143 | 4.8 | 7.2 | 0.00 | 1.5 |
| proof | 500 | 3,030 | 24,616 | 340 | 627 | 103.5 | 383 |

**External blobs, plain:**

| phase | ops | upstream alloc/op | upstream µs/op | reference alloc/op | reference µs/op | next alloc/op | next µs/op |
|---|---|---|---|---|---|---|---|
| insert | 40,000 | 19.0 | 7.9 | 7.7 | 6.1 | 0.70 | 1.5 |
| seal | 20 | 2,729 | 13,459 | 27.6 | 14,024 | 16.9 | 12,191 |
| commit | 20 | 33.4 | 5,531 | 34.9 | 7,757 | 25.5 | 4,323 |
| warm_get | 5,000 | 16.0 | 16.8 | 5.5 | 4.5 | 0.00 | 3.7 |
| miss_get | 5,000 | 13.1 | 12.5 | 4.5 | 3.5 | 0.00 | 3.3 |
| patched_get | 5,000 | 17.9 | 18.9 | 5.5 | 4.4 | 0.00 | 3.6 |
| proof | 500 | 1,798 | 2,456 | 427 | 432 | 126.3 | 442 |

**SQLite-internal blobs, plain:**

| phase | ops | upstream alloc/op | upstream µs/op | reference alloc/op | reference µs/op | next alloc/op | next µs/op |
|---|---|---|---|---|---|---|---|
| insert | 40,000 | 19.0 | 12.8 | 7.7 | 11.9 | 0.70 | 7.0 |
| seal | 20 | 2,729 | 13,457 | 25.9 | 13,527 | 16.9 | 12,354 |
| commit | 20 | 30.4 | 9,557 | 30.6 | 10,262 | 25.5 | 8,599 |
| warm_get | 5,000 | 16.0 | 60.5 | 0 | 67.6 | 0.00 | 63.6 |
| miss_get | 5,000 | 13.1 | 42.8 | 0 | 36.1 | 0.00 | 39.9 |
| patched_get | 5,000 | 17.9 | 31.7 | 0 | 24.5 | 0.00 | 23.8 |
| proof | 500 | 1,798 | 7,128 | 210 | 6,893 | 126.3 | 7,460 |

Reading the numbers:

* Against upstream on the production configuration (compressed external
  blobs) the rewrite's lookups are two to three orders of magnitude faster:
  upstream re-resolves patch chains on every read and allocates throughout;
  the rewrite's reads are borrowed and its resolved patches are cached.
* Against the reference, which already has mmap reads, a result cache and a
  patch cache, the rewrite's compressed lookups are about 4 to 5x faster
  with zero allocations; on the plain encoding the two are within noise of
  each other (the reference's result cache serves repeated keys; the
  rewrite walks the trie each time). Commit is faster in all three
  configurations; seal is SHA-512/256 bound in all three (70% of the
  rewrite's seal in callgrind).
* SQLite-internal storage: all three pay one `blob_open` per node; after the
  read-size fix the rewrite is within a few percent of upstream and faster on
  patched lookups. Proofs are slower on this configuration (more node
  reads per proof than the originals' probe-style reads).
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

CONSUMER_RESULTS

Three tests of that filter stalled in the parallel run and were re-run
serially: `accounting::test_coinbase_pay_to_alt_principal_v210`,
`accounting::test_coinbase_pay_to_contract_v210` and
`block_construction::test_contract_call_across_clarity_versions`. A stack
trace of the stalled processes (zero CPU for over an hour) showed them in
`NetworkState::bind_address`, which retries a `TcpListener::bind` with
doubling backoff for as long as the address is in use; these tests use
hard-coded ports (two block-construction tests share 2032/2033, and the two
coinbase tests call the same helper with one port pair), so running them in
parallel can leave them sleeping indefinitely. This is test-harness
behaviour independent of the MARF. SERIAL_RESULTS

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
* `prune_to_path` keeps collected nodes in memory (ADR-10).
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
4. Harden pruning for very large tries (disk-backed node list).
5. Format-changing items stay deferred (`FUTURE.md`).
