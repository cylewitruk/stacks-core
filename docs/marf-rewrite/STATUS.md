# MARF rewrite: status

Branch: `claude/awesome-brown-xj309n`.
Base: `29ef093da5dd58c1fa484624d94a9d972356c629` (tag `optimized-ref`): the
original MARF ("v1") lives in `stacks-marf/` with its optimized storage
work; it is the oracle and is untouched.
Rewrite: `crates/stacks-marf-next/` (crate `stacks-marf-next`).

Documents: `ANALYSIS.md` (behavioural contract and accidental complexity of
the original), `COMPAT.md` (what is read/written, and how the optimized
reference's formats are handled), `DECISIONS.md` (architecture log),
`FUTURE.md` (format-breaking optimizations and where they plug in).

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

| | Original at the base (`stacks-marf/src`) | Rewrite (`crates/stacks-marf-next/src`) |
|---|---|---|
| Production lines | 18,490 | 5,627 |
| Modules | 26 | 10 |
| Node representations | `TrieNode4/16/48/256`, `TrieLeaf`, `TrieNodeType`, packed/canonical/mapped variants | `Node::{Leaf, Branch}` plus borrowed `Record`/`BranchView`/`LeafView`/`PatchView` |
| Storage handle types | `TrieFileStorage`, `TrieStorageConnection`, `TrieStorageTransaction`, `ReopenedTrieStorageConnection`, `TrieRAM`, `TrieFile`, … | `MARF`, `MarfTransaction`, `View`, `ReopenedMarf`, `TrieFileStorage` (shim) |

The differential harness is another 2,618 lines (`contrib/marf-differential/src`).

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

Fixtures (`contrib/marf-differential/fixtures/*.json`, recorded from the
original with `marf-differential record-all`; provenance: crate
`stacks-marf` at `29ef093d`), 22 in all. Suffixes name the storage variant
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
(no DSL, no observation encoding). 20 blocks x 2,000 fresh keys, then 8
blocks rewriting 50 keys each (patch chains), 5,000 reads per read phase,
500 proofs. `REPEAT=3`: allocation counts from the first run, times the
best of three, `--release`, this container. "orig" is `stacks-marf` at the
base, "next" the rewrite.

External blobs, compressed (the production configuration):

| phase | ops | orig alloc/op | orig B/op | orig µs/op | next alloc/op | next B/op | next µs/op |
|---|---|---|---|---|---|---|---|
| insert | 40,000 | 8.08 | 9,201 | 6.79 | 0.87 | 1,316 | 1.95 |
| seal (hashing) | 20 | 26.8 | 5,915 | 14,479 | 17.0 | 18,478 | 14,685 |
| commit (serialize + write) | 20 | 1,311.6 | 2,345,337 | 6,259 | 531.5 | 617,039 | 4,353 |
| warm_get | 5,000 | 4.34 | 4,402 | 13.27 | 0.02 | 1 | 1.53 |
| miss_get | 5,000 | 4.08 | 4,316 | 10.42 | 0.00 | 0 | 0.99 |
| patched_get | 5,000 | 3.87 | 3,602 | 13.68 | 0.00 | 0 | 1.46 |
| proof | 500 | 316.1 | 717,196 | 1,381 | 103.5 | 433,860 | 383 |

SQLite-internal blobs, plain:

| phase | ops | orig alloc/op | orig B/op | orig µs/op | next alloc/op | next B/op | next µs/op |
|---|---|---|---|---|---|---|---|
| insert | 40,000 | 7.71 | 8,865 | 10.14 | 0.70 | 998 | 6.09 |
| seal | 20 | 25.9 | 5,722 | 11,523 | 16.9 | 18,471 | 12,716 |
| commit | 20 | 30.6 | 2,728,051 | 8,974 | 25.5 | 1,010,918 | 7,979 |
| warm_get | 5,000 | 0 | 0 | 55.9 | 0 | 0 | 64.8 |
| miss_get | 5,000 | 0 | 0 | 32.9 | 0 | 0 | 43.7 |
| patched_get | 5,000 | 0 | 0 | 22.4 | 0 | 0 | 32.0 |
| proof | 500 | 214.0 | 610,270 | 6,994 | 126.3 | 454,372 | 6,825 |

Reading the numbers: the original's warm lookups are served by its
per-trie lookup-result cache (2,048 entries) and still allocate; the
rewrite's lookups walk the trie each time and allocate nothing (the 0.02 is
patch-cache fills). Seal allocates more bytes in the rewrite because the two
hash buffers (17 KiB) are created per seal; seal time is dominated by
SHA-512/256 in both (70% of the rewrite's seal in callgrind). Over
SQLite-internal storage each node is a `blob_open` + read in both
implementations and the rewrite is 15-40% slower per lookup; a whole-trie
cache was tried and thrashes (lookups cross up to 20 tries), so this stays
as is for the legacy internal-blob configuration. Proofs allocate in both
(owned proof nodes are the wire format).

## Known differences and limitations

* Format 6 / stable-ID databases are refused (`COMPAT.md` §2.1-2.2); a
  published direct-hash sidecar is marked invalid on the rewrite's first
  write (§2.3).
* `TrieHashCalculationMode::Immediate` is accepted but hashing is always
  deferred; the root hash of an unsealed, uncommitted trie reads as zero.
* Reads name their block; there is no "current block" cursor. A read issued
  right after a failed `commit_to` or after `commit_mined` succeeds where the
  original returns `NotFoundError` (its cursor still names the abandoned
  trie). Consumers `open_block` a tip first; fixtures mirror that.
* No lookup-result cache (ADR-17).
* `prune_to_path` keeps collected nodes in memory (ADR-10).
* The blobs map assumes the file is only appended to while open; truncating
  it from outside the process would fault the mapping (the original's mmap
  path has the same assumption).
* Schema-1 export migration is implemented but untested against a real
  schema-1 database.
* Not integrated into Stacks consumers yet (see below).

## Next steps

1. Integration behind a feature or a workspace switch: point
   `stackslib`, `stacks-node` and `contrib` at `stacks-marf-next` while the
   original stays as the oracle; resolve the three decisions in
   `COMPAT.md` §3 (legacy-only Clarity MARF creation first); run the
   chainstate, Clarity, sortition and node test suites.
2. Performance qualification on a representative (mainnet-shaped) database
   with separated measurements: lookups, inserts, seal, commit, proofs,
   open; peak memory. Extend `alloc` with a replay-from-database mode.
3. Format 6 reader/writer and value resolver (`FUTURE.md` §1-2) once the
   legacy integration is green.
4. Harden pruning for very large tries (disk-backed node list).
