# MARF rewrite: analysis of the baseline implementation

> Base note: this analysis was written against upstream `3e835aa0`, where the
> MARF lived in `stackslib/src/chainstate/stacks/index/`. The rewrite now
> targets `29ef093d`, where the same code (plus the optimized storage work)
> lives in `stacks-marf/`. The behavioural contract and hashing sections
> are unchanged by the move; the optimized formats are covered in
> `COMPAT.md`.

Pinned references:

| Reference | Commit | Notes |
|---|---|---|
| Baseline (upstream `stacks-network/stacks-core` main) | `3e835aa0f93271bee3c44538cf32c3d56ae93b7a` | MARF sources live in `stackslib/src/chainstate/stacks/index/`; the `stacks-marf` crate is an empty shell at this commit. |
| Optimized reference | `29ef093da5dd58c1fa484624d94a9d972356c629` | Branch `perf/optimized-stacks-bench-integration`; merge-base with baseline is the baseline commit itself. Tagged locally as `optimized-ref`. |

## Baseline size

Measured over `stackslib/src/chainstate/stacks/index/` at the baseline commit
(non-blank, non-`//` lines; raw line counts in parentheses).

| Module | Code lines | Raw |
|---|---|---|
| storage.rs | 2369 | 3265 |
| node.rs | 1853 | 2481 |
| marf.rs | 1478 | 1978 |
| proofs.rs | 1263 | 1544 |
| trie_sql.rs | 853 | 1052 |
| squash.rs | 803 | 1111 |
| trie.rs | 720 | 1042 |
| bits.rs | 639 | 933 |
| file.rs | 597 | 791 |
| squash/node_store.rs | 370 | 467 |
| mod.rs | 348 | 428 |
| squash/stream.rs | 152 | 241 |
| cache.rs | 41 | 75 |
| blob_layout.rs | 33 | 77 |
| **Total production** | **11,519** | **15,485** |
| Tests (`test/`) | | 14,783 |

14 production modules; 49 public structs/enums/traits, among them 7 traits
(`TrieNode`, `ConsensusSerializable`, `BlockMap`, `NodeHashReader`,
`MarfConnection`, `MarfTrieId`, `ClarityMarfTrieId`), 5 node structs
(`TrieNode4/16/48/256`, `TrieLeaf`) plus the `TrieNodeType` enum, and 6
storage-handle types (`TrieFileStorage`, `TrieStorageConnection`,
`TrieStorageTransaction`, `ReopenedTrieStorageConnection`,
`TrieStorageTransientData`, `TrieRAM`/`UncommittedState`).

## Essential data model

A MARF is a forest of tries, one per block. Each trie is an adaptive radix
trie keyed by the 32-byte `TrieHash` of a key string and storing a 40-byte
`MARFValue` at each leaf. A trie for block *B* contains only the nodes
touched by writes in *B*; untouched children are *back-pointers* into the
trie of an earlier block. Reads at *B* therefore traverse into older blocks
transparently. Forks are implicit: any committed block can be the parent of a
new block; there is no global tip state in the trie files.

Node kinds: `Leaf`, `Node4`, `Node16`, `Node48`, `Node256`. Each node has a
compressed path (0..=32 bytes) and child slots. Essential invariants:

* The root is always a `Node256` (even after a root split).
* Child slots of `Node4/16/48` are filled in insertion order and never
  vacated (the MARF has no deletes), so occupied slots are always a prefix of
  the slot array. `Node256` slots are indexed by path character.
* Slot order is consensus-relevant: node hashes commit to every slot in
  order, including empty ones.
* Each block's trie records block-height mapping keys inside itself:
  `__MARF_BLOCK_HEIGHT_TO_HASH::{h}`, `__MARF_BLOCK_HASH_TO_HEIGHT::{hash}`
  (lower-case hex of the block id) and `__MARF_BLOCK_HEIGHT_SELF`. Values are
  `u32` little-endian or the 32-byte id, zero-padded to 40 bytes. These keys
  are consensus state and are how ancestors at a height are found.

## Consensus-critical hashing (must be reproduced exactly)

All hashes are SHA-512/256.

* Leaf hash = `H(0x01 || path_len || path || value[40])`.
* Branch node hash = `H(id || ptr_bytes* || path_len || path || child_hash*)`
  where for each slot `ptr_bytes = id(1, with 0x80 back-pointer bit) || chr(1)
  || block_hash_or_zeros(32)`; the 32 bytes are the back-block's block id for
  back-pointers and zeros otherwise; `child_hash` is the child's node hash for
  in-trie children, the back block's **block id** (not the child's hash) for
  back-pointers, and `TrieHash::EMPTY` (hash of the empty string) for empty
  slots.
* Trie root ("MARF root") hash = node hash of the root if the block is at
  height 0, else `H(root_node_hash || anc(h-1) || anc(h-2) || anc(h-4) ...)`
  over the Merkle skip-list of ancestor MARF root hashes at heights
  `h - 2^i`, found through the height-mapping keys above. Non-root `Node256`
  nodes use the plain node hash.
* Squash-annotated inline pointers (non-zero `back_block` without the 0x80
  bit) hash as plain inline pointers.

Proof wire format (`TrieMerkleProof` / `StacksMessageCodec`) is also
externally visible (RPC `proof` fields) and must be reproduced.

## Storage contracts (must be preserved for bidirectional compatibility)

SQLite (`marf.sqlite`), schema version 3:

* `marf_data(block_id INTEGER PRIMARY KEY, block_hash TEXT UNIQUE, data BLOB,
  unconfirmed INTEGER, external_offset INTEGER, external_length INTEGER)`,
  indexes `block_hash_marf_data`, `unconfirmed_marf_data`,
  `index_external_offset`. `block_hash` is lower-case hex text.
* `mined_blocks(block_id, block_hash, data)`, `block_extension_locks(block_hash)`,
  `schema_version`, `migrated_version`, `marf_squash_info`,
  `marf_squashed_blocks` (hashes stored as 32-byte BLOBs there).
* Opened with `journal_mode=WAL`, `synchronous=NORMAL`, `page_size` and
  `mmap_size` pragmas; writes use `BEGIN IMMEDIATE` transactions.
* `block_id` (the rowid) is the back-pointer block identifier stored in every
  persisted trie, so the sequence of row insertions matters: a block's trie
  references ancestors by rowid, and `marf_data` rows may only ever be
  appended (unconfirmed rows are updated in place and deleted; mined rows live
  in their own table).

Trie blob layout (either in `marf_data.data` or appended to `marf.sqlite.blobs`
when `external_blobs` is on; the sortition and chainstate index MARFs choose by
the presence of the `.blobs` file, the Clarity MARF always uses it):

```
0..32   parent block id
32..36  reserved u32 LE (0)
36..    root node; every node = hash(32) || body
```

Node body encodings, selected by the id byte's control bits:

* Plain: `id || slot*N || [Node48: 256 index bytes] || path_len || path`, each
  slot `id || chr || ptr(u32 BE, or u64 BE when id has 0x20) || back_block(u32 BE)`.
* Compressed (`0x40`): slots are either dense (every slot, each compressed:
  no `back_block` unless back-pointer (0x80) or inline-annotation bit 0x10)
  or sparse (`0xff` marker, occupancy bitmap, then only occupied slots).
* Patch (`id == 6`): `base_ptr(compressed) || n-1 || diff ptr*n`; the reader
  follows up to `MAX_PATCH_DEPTH = 4` patches to a base node in an older block
  and re-applies the diffs. Patches are written only when compression is on,
  which is the **default node configuration** (`marf_compress = true`), so
  production databases contain patches and the replacement must read them.
* Leaf: `0x01 || path_len || path || value[40]` (never compressed).

Nothing in the format fixes node order inside a blob; readers follow byte
offsets. The baseline currently writes descendants child-before-parent after
a reserved root slot.

## Required external API (from consumers in `stackslib`, `stacks-node`, `contrib`)

Hot: `MARF::{from_path, from_path_unconfirmed, from_storage, begin_tx,
open_block, sqlite_conn, storage_tx, get_root_hash_at, get_with_proof[_from_hash],
reopen_readonly, reopen_connection, get_db_path, into_sqlite_conn,
get_open_chain_tip[_height], get_block_height_of, get_bhh_at_height, insert,
insert_batch, commit*, drop_*, squash_to_path, for_each_leaf,
make_unconfirmed_chain_tip}`, `MarfTransaction::{begin, begin_unconfirmed,
insert_batch, seal, commit, commit_to, commit_mined, drop_current,
drop_unconfirmed, sqlite_tx, get_open_chain_tip[_height], get_block_height_of}`,
the `MarfConnection` trait (`get`, `get_from_hash`, `get_with_proof*`,
`get_block_at_height`, `get_block_height`, `get_root_hash_at`,
`check_ancestor_block_hash`, `sqlite_conn`, `with_conn`), `MARFOpenOpts`,
`TrieHashCalculationMode`, `TrieFileStorage::{open, open_readonly,
open_unconfirmed}`, `TrieFile::exists`, `Error`, `MARFValue`, `TrieLeaf`,
`TrieMerkleProof` (+ codec, `verify`, `to_hex`), `MarfTrieId`,
`ClarityMarfTrieId`, `test_override_marf_compression`, and a handful of
`trie_sql` helpers used by the snapshot tooling (`get_latest_confirmed_block_hash`,
`bulk_read_squashed_blocks`, `read_squash_info`).

Nothing outside the module uses `TrieCursor`, `TrieNode*`, `TrieRAM`,
`BlockMap`, `NodeHashReader`, `ConsensusSerializable`, `TrieStorageTransaction`
or the `bits` helpers (except the squash tool's internal reuse).

## Accidental complexity identified

1. **Five node structs + an enum + a `TrieNode` trait + a `with_node!`
   macro.** The structs differ only in slot count and (for Node48) an index
   table that is derivable from the slots. One `Branch { kind, path, children }`
   suffices; the Node48 index is a serialization detail.
2. **Stream-oriented decoding** (`Read + Seek` everywhere, `ptrs_from_bytes`
   speculatively over-reads then seeks back). Nodes are small and bounded;
   decoding from a byte slice is simpler and faster.
3. **Three overlapping pointer semantics in `TriePtr::ptr`** (RAM index, disk
   offset, patch-diff index) handled by convention. The replacement keeps RAM
   indices and disk offsets in distinct types.
4. **Storage handle zoo** (`TrieFileStorage`, `TrieStorageConnection`,
   `TrieStorageTransaction`, `ReopenedTrieStorageConnection`,
   `TrieStorageTransientData`, `UncommittedState`, `TrieRAM::with_reinstated_data`
   move dance). The transient "currently open block" cursor that every read
   consults (`open_block` / restore pairs in nearly every function) is the
   single largest source of complexity and bugs. Reads can simply take the
   block they target as an argument.
5. **Two hashing modes** (`Immediate`, `Deferred`, plus `All` for tests) with
   `TrieCursor` repair/retarget bookkeeping to rehash paths eagerly. Both
   produce identical hashes; deferred hashing (the production default) only
   needs a post-order pass at seal time.
6. **`TrieCursor` as a mutable state machine** with error-driven control flow
   (`CursorError::BackptrEncountered` + `repair_*` methods) shared between
   lookup, insert and proof generation.
7. **`BlockMap` / `NodeHashReader` / `ConsensusSerializable<M>` generic
   plumbing** whose only purpose is to resolve a back-block id to its hash
   while hashing. A closure or a small cache reference does the same.
8. **Patch bookkeeping stored inside every node** (`cowptr`, `patches`
   fields on all four branch structs) although it is only needed during
   compressed flushes.
9. **`TrieFile` enum with RAM/Disk variants and `Read/Write/Seek` boilerplate**;
   positioned reads over `&File` remove the cursor state entirely.
10. **Squash side tables and read guards threaded through every lookup.**
    Essential behaviour, but it can live at the storage boundary (one
    `height_of`/`root_of` resolution) instead of in the trie walk.

What is essential and retained: the four branch arities, slot semantics,
back-pointers and COW-forwarding, the skip-list root hash, height mapping
keys inside the trie, the SQLite/blob schema, compressed/patch decoding,
unconfirmed and mined tries, extension locks, proofs and squashing.
