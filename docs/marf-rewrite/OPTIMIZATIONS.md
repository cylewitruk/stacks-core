# MARF rewrite: optimizations of the reference branch, classified

Pinned compatibility baseline: upstream `stacks-network/stacks-core` `main` at
`ad5c57786bb51b58d4bdb4992ad534f762d8cf03`. Optimization reference:
`cylewitruk/stacks-core` `perf/optimized-stacks-bench-integration` at
`29ef093da5dd58c1fa484624d94a9d972356c629` (tag `optimized-ref`), whose
merge-base with upstream is the pinned commit, so the diff between the two
is exactly the optimization work.

Every optimization the reference adds is listed below and classified as
**(a) IMPLEMENTED** in `crates/stacks-marf-next`, **(b) APPLICABLE** without
changing any persisted format upstream reads or writes, or **(c) DEFERRED**
with the concrete reason (a persisted-format change, a sidecar or migration,
or a consumer-side change). Items under (c) keep their implementation
boundaries in `FUTURE.md`. The inventory was produced by reading the
reference's modules and the rewrite's code; function and type names are the
ones in the sources.

Rule for this milestone: upstream consensus behaviour and bidirectional
database hand-off without conversion are preserved; nothing under (c) is a
prerequisite.


Scope: `git diff upstream/main optimized-ref -- stacks-marf stackslib/src/clarity_vm stackslib/src/chainstate/stacks/db contrib docs stacks-common`
(213 files, +63,439 / -4,827; `stacks-common` is untouched). Every item below names code actually opened at `optimized-ref`
and, for the rewrite, code grepped in `crates/stacks-marf-next/src`.

Already present at `upstream/main` and therefore **not** counted as branch optimizations (verified by `git grep` on upstream):
`posix_fadvise`-based `prefetch_node`/`prefetch_file_range`, `bulk_read_blob_headers_sorted` + `header_read_parallelism` (16..=32 reader threads),
`read_exact_at` (pread) in `file.rs`, `warm_trie_offsets_from_entries`/`cache_trie_offset`, `reserved_root_size` root-first DFS dump,
squash `pre_read_blob_headers`, disk-backed squash `squash/node_store.rs`, the `stacks-profiler` crate itself (upstream `stacks-marf` just did not depend on it),
`ReopenedTrieStorageConnection` + `reopen_connection`, SQLite `mmap_size`/`page_size` pragmas in `marf_sqlite_open`.

Legend: **(a) IMPLEMENTED** in `stacks-marf-next`; **(b) APPLICABLE** without changing upstream persisted formats; **(c) DEFERRED** (format / sidecar / migration / consumer dependency).

---

## A. Read path (stacks-marf)

### A1. Memory-mapped blob reads
- Where: `mapped_file.rs` `FileMapping::{map_with_capacity, map_prefix_with_access, refresh, refresh_prefix}`, `ReservedMapping` (PROT_NONE reservation of `len/8` headroom, min 64 MiB, rounded to a power of two; pages backed in place with `MAP_FIXED` in `extend_prefix`; readers never lock); `file.rs` `TrieFileDisk { mmap_enabled, mmap: Option<FileMapping>, tail: Arc<RwLock<Option<(u64, Arc<Mmap>)>>> }`, `refresh_mapping` (maps complete pages; a separate 64 KiB-overlap tail map covers the partial EOF page), `mmap_slice_at`, `read_item_at_offset` (fast path decodes straight from the slice; slow path `pread` into scratch), `read_trie_item_borrowed` -> `MappedTrieItem::{Node, Patch}`; `new_mmap`, `MARFOpenOpts::with_mmap` (production `Config::get_marf_opts` sets `with_mmap(true)`); `crates/stacks-mmap` `AccessPattern::{Normal, Random}` madvise.
- Why faster: node reads become pointer arithmetic over page-cache memory instead of a `pread` syscall + copy per node; the reservation keeps all previously returned slices valid across appends so no remap/TLB flush per commit.
- Format: no (`marf.sqlite.blobs` unchanged).
- Class: **(a) IMPLEMENTED** – `storage.rs` `Blobs { file: Arc<File>, map: Option<Mmap> }`, `Blobs::ensure_mapped` (lazy, re-maps conventionally when a read passes the end), `Blobs::slice`, `trie_bytes`, `record_in`.
  Sub-item not implemented: reserved-address growth + `madvise` + tail-overlap map. **(b) APPLICABLE** in `storage.rs Blobs::ensure_mapped` (reserve `PROT_NONE` range once, `MAP_FIXED` new pages; optional `memmap2::Advice::Random`). Benefit: small – one `mmap` per commit replaced by one `mmap` of only new pages; avoids re-faulting hot pages of the old map. Worth doing only if profiles show remap cost.

### A2. Borrowed node views / zero-copy decode
- Where: `lib.rs` `BorrowedNodeBytes<'a>::{mapped_path, mapped_child, mapped_leaf, decode_node}`, `OwnedNodeBytes`, `BytesBacking`, `ReadTrieItem`/`ReadTrieItemKind`, `ReadTrieNode<'a>`/`ReadNodeBacking`; `node.rs` `TrieNodeRef<'a>::{walk, as_leaf, to_owned_node}`, `TrieLeafRef<'a>`, `TrieNodeTransientMeta`; `bits.rs` `decode_nodetype_from_slice_at_head`, `read_trie_item_from_slice_format`, `ptrs_from_slice_into`; `mapped_node.rs` `path_prefix`; `record.rs` `NodeRecord<'a>` (payload borrowed), `RecordContext::read_probe` (reads 33 bytes for type+hash).
- Why faster: a lookup step reads only the path and the one child pointer it needs; no `TrieNodeType` allocation per node.
- Format: no.
- Class: **(a) IMPLEMENTED** – `codec.rs` `Record<'a>::{parse, parse_body, leaf, branch, patch, hash}`, `BranchView::{skip_scan, child_at, fill}`, `LeafView`, `PatchView`; `storage.rs` `StoreNode<'a>::{Record, Patched}`, `Store::read_hash`.

### A3. Single-pass descend
- Where: `node.rs` `TrieCursor::walk_read`/`walk_ref_step`; `lib.rs` `TrieReadSession::walk_to_leaf_or_backptr`; `marf.rs` `MARF::walk` (`WalkAction` loop), `Trie::resolve_backptr`.
- Why faster: path comparison and child selection happen on the borrowed record in one scan; back-pointers are followed without re-opening a block cursor object.
- Format: no.
- Class: **(a) IMPLEMENTED** – `codec.rs` `Record::descend` -> `Descent::{Mismatch, Leaf, Child}`; `marf.rs` `View::lookup`, `View::read(site, ptr)`, `NodeView::descend`.

### A4. Reusable decode / patch-chain scratch
- Where: `scratch.rs` `MarfReadState` (slots for Node4/16/48/256/Leaf/Patch, `node_bytes`, `patch_chain_buf`, `parked`), traits `NodeDecodeScratch`, `NodeParking`, `NodePatching` (`lib.rs`), `PatchChainEntry`; `storage.rs` `read_patched_persisted_node` (takes/restores the chain buffer, `MAX_PATCH_DEPTH`).
- Why faster: steady-state reads allocate nothing; patch diffs and decoded nodes reuse buffers.
- Format: no.
- Class: **(a) IMPLEMENTED** – `storage.rs` `Scratch { bytes: BytesScratch, diffs: Vec<Ptr>, patched, decoded: Option<Branch> }`, `resolve_in`, `Store::branch` (decodes into `scratch.decoded`).

### A5. Inline fixed-size path
- Where: `lib.rs` `NodePath { data: [u8; 32], len: u8 }` (Copy) replacing `Vec<u8>` in `TrieLeaf`, `TrieNode4/16/48/256`; `bits.rs` `path_from_bytes_slice_into`.
- Why faster: cloning or decoding a node no longer heap-allocates its path.
- Format: no.
- Class: **(a) IMPLEMENTED** – `node.rs` `Path { len: u8, data: [u8; 32] }`.

### A6. Resolved patch-chain cache
- Where: `cache.rs` `SharedLru<K, V>` (Arc-shared across reopened views, generation-stamped `put_if_current`), `SharedLruGuard` (clears unless `succeed()`); `storage.rs` `TrieStorageTransientData.resolved_patch_cache: SharedLru<(T, u64), ResolvedPatchNode>` keyed by **block hash** + offset (comment: "A row ID may be reused after rollback"), `resolved_patch_node` retention, `MARFOpenOpts.resolved_patch_cache_capacity` (default 16, production 64, `clear_resolved_patch_cache`).
- Why faster: the root and its children are patched in block after block; a hit skips up to `MAX_PATCH_DEPTH` record reads and patch applications.
- Format: no.
- Class: **(a) IMPLEMENTED** – `storage.rs` `Scratch.patched: HashMap<(u32, u64), Cached { node, hash, depth }>`, `PATCH_CACHE_CAPACITY = 4096` (emptied when full, not LRU), cleared by `Store::invalidate` on `MarfTransaction::drop`/`rollback`/`finish`. Difference worth noting: the rewrite keys by `block_id` and relies on `invalidate()` at transaction boundaries instead of block-hash keys; the row-id-reuse hazard the optimized branch guards against is covered only if every rollback path calls `invalidate` (it does in `Drop`, `rollback`, `finish`).

### A7. Root-node cache
- Where: `cache.rs` `ArrayLru<K, V, const N>` (stack array, MRU-first); `storage.rs` `root_node_cache: Option<Box<ArrayLru<u32, (TrieNodeType, Option<TrieHash>, usize), 4>>>`, `root_node_cache_enabled`, used in `read_node_with_state` when `clear_ptr == root_trieptr()` and the block is committed; `MARFOpenOpts.root_node_cache` (default true; **production `get_marf_opts` sets `false`**); `read_bench` counters `root_cache_hits/misses`.
- Why faster: every lookup starts at the root; the decoded root is served without touching storage.
- Format: no.
- Class: **(b) APPLICABLE** – `storage.rs` `Scratch` (a 4-entry `[(u32, Branch, TrieHash); 4]` keyed by block id, consulted in `resolve_in` when `ptr.ptr == ROOT_OFFSET`). Benefit: marginal – the production config disables it, and in the rewrite a patched root already hits `Scratch.patched` while an unpatched root is a single mmap slice read.

### A8. Lookup-result cache (mutable trie only)
- Where: `result_cache.rs` `ResultCache` (indexed doubly-linked LRU over `RapidHashMap<TrieHash, usize>`, `DEFAULT_RESULT_CACHE_CAPACITY = 2048`), `ResultCacheControl { generation, writes }`, `ResultCacheGuard` (invalidates on drop unless `succeed`); `storage.rs` `TrieRAM.result_cache`, `TrieStorageConnection::{cached_result, cache_result, begin_result_cache_write, result_cache_failure_guard}`, `switch_trie` resets it; `marf.rs` `MarfReadCtx::get_leaf_by_path` (hit returns without walking), `MARF::do_insert_leaf` (`begin_result_cache_write` evicts the key), `inner_insert_leaf_batch` (failure guard); consumer: `stackslib/src/clarity_vm/database/marf.rs` calls `get_leaf_by_key` in 8 places.
- Why faster: Clarity re-reads the same keys many times inside one block; a hit avoids a full root-to-leaf walk through the in-RAM trie and its back-pointers.
- Format: no.
- Class: **(b) APPLICABLE** – `marf.rs` `Building { trie, height }` gains `results: HashMap<TrieHash, Option<MARFValue>>` (bounded, cleared in `begin`/`flush`/`drop_*`, key evicted in `insert_raw`/`insert_batch`, whole map cleared on any `Err` from a write or on `MarfTransaction` drop/rollback); `View::lookup` consults it only when `site(tip) == Site::Mem`. Benefit: moderate for Clarity-heavy blocks (repeat reads of contract metadata keys); zero for committed-block reads; must preserve the invariant that no stale value survives a failed write.

### A9. Current-block trie-offset hint and interior-mutable offset cache
- Where: `storage.rs` `TrieStorageTransientData.cur_block_trie_offset`, `current_trie_offset_hint`; `file.rs` `TrieFileDisk.trie_offsets: RefCell<TrieIdOffsets>`, `get_trie_offset(&self)` (was `&mut self` upstream), `validate_header_at` before admitting an offset.
- Why faster: hot-path reads for the open block skip the `HashMap` and the SQL `external_offset` lookup.
- Format: no.
- Class: **(a) IMPLEMENTED** – `storage.rs` `Store.locations: HashMap<u32, Location>`, `location_in` (one `prepare_cached` query per block id, then cached), `Location::External { offset, len }`.

### A10. Block id <-> hash cache, forward-only variant
- Where: `cache.rs` `CachedBlockHash { hash, reverse_cached }`, `BlockHashCache::get_block_hash_forward_only`, `publish_block_hash`; used by `TrieRAM::calculate_branch_hashes` and `get_block_hash_forward_only` in `storage.rs` so sealing does not populate the reverse map.
- Why faster: avoids cloning the 32-byte hash into the reverse map for ids that are only ever read forward.
- Format: no.
- Class: **(a) IMPLEMENTED** (both directions) – `storage.rs` `Store.ids`/`Store.hashes`, `Store::block_id`, `Store::block_hash`; the forward-only refinement is a memory micro-optimization not carried over (negligible).

### A11. Covering SQL index for block-hash lookups
- Where: `trie_sql.rs` `SQL_BLOCK_HASH_LOOKUP_INDEX` = `CREATE INDEX IF NOT EXISTS block_id_hash_marf_data ON marf_data(block_id, block_hash)`, `SQL_GET_BLOCK_HASH` uses `WHERE block_id BETWEEN ?1 AND ?1` so SQLite picks the covering index, `ensure_block_hash_lookup_index`; installed by `StacksChainState` open (`stackslib/src/chainstate/stacks/db/mod.rs`) when no direct-hash table exists.
- Why faster: the hash is served from the index page without a rowid table lookup.
- Format: adds a SQLite index object only (no schema-version bump; old binaries ignore it).
- Class: **(b) APPLICABLE** – `storage.rs open` after `migrate` (`CREATE INDEX IF NOT EXISTS` when writable) and the `block_hash` query. Benefit: small; the rewrite's `Store.hashes` map makes the query rare after warm-up.

### A12. Stack-allocated branch hashing
- Where: `storage.rs` `TrieRAM::calculate_branch_hashes<const N: usize>` (`[TrieHash; N]` child hashes, `write_consensus_bytes_with_child_hashes`), `get_block_hash_forward_only` for back-pointer children; spans `Seal: Node hashes` / `Seal: Ancestor hashes`.
- Why faster: upstream `calculate_node_hashes` re-dispatched per child and wrote consensus bytes through a generic `BlockMap`; the new code hashes with fixed-size scratch and no per-child heap traffic.
- Format: no (hash preimage unchanged).
- Class: **(a) IMPLEMENTED** – `hash.rs` `HashScratch { head, tails }`, `branch_hash` (two buffers, one `update` per section), `Trie::seal`/`hash_subtree` post-order.

### A13. Seal-time ancestor resolution via index
- Where: `trie.rs` `inner_get_trie_ancestor_hashes_bytes` tries `indexed_ancestor_root` before the `__MARF_BLOCK_HEIGHT_TO_HASH` key walk; `marf.rs` `resolve_block_at_height` tries `indexed_ancestor`.
- Depends on the direct-hash sidecar (C7/C8). Class: **(c) DEFERRED** (see C8). Format-neutral alternative noted under C8.

## B. Write / commit path

### B1. Positioned blob append
- Where: `file.rs` `pwrite_all`, `append_trie_blob` (`pwrite` -> `sync_data` -> `refresh_mapping`), `TrieFile::sync_data` spans.
- Why faster: no shared cursor state; the mapping is extended immediately after the sync so readers see the new trie without reopening.
- Format: no.
- Class: **(a) IMPLEMENTED** – `storage.rs` `Blobs::write_at` -> `write_all_at` (`FileExt::write_all_at`) + `sync_data`; `Store::write_confirmed` computes the offset from `external_blobs_length`.

### B2. Transaction-scoped cache guards
- Where: `storage.rs` `TrieFileStorage::transaction` creates `DirectHashGuard`, `ResultCacheGuard`, `SharedLruGuard`; `commit_tx` calls `succeed()` on each after `db.commit()`; drop without success clears/invalidates.
- Format: no.
- Class: **(a) IMPLEMENTED** – `marf.rs` `impl Drop for MarfTransaction` and `rollback`/`finish` call `Store::invalidate` (clears `ids`, `hashes`, `locations`, `patched`).

### B3. Direct packed-node emission and packed root reservation
- Where: `canonical_branch.rs` `Node256Plan::{build, write}`, `write_payload`; `storage.rs` `packed_root_reservation`, `dump_consume`/`dump_compressed_consume` branches on `NodeRecordFormat::Optimized`.
- Why faster: bytes are emitted from logical pointers without an intermediate layout buffer.
- Format: yes (format 6 only). Class: **(c) DEFERRED** with C1; boundary `trie.rs serialize_at` (layout fixpoint already separated from emission).

## C. Persisted-format changes (all DEFERRED)

| # | Optimization | Reference | What / why | Format impact | Rewrite boundary |
|---|---|---|---|---|---|
| C1 | Record format 6, type-first records | `record.rs` `NodeRecordFormat::{Legacy, Optimized}`, `RecordContext`, `NodeRecord`, `HashAfterMarker`; `blob_layout.rs` `TYPE_FIRST_TAG = MRF\x06`, `validate_reader_header`; `trie_sql.rs` `SQL_MARF_TYPE_FIRST_SCHEMA_VERSION = 4`, `NodeRecordFormat::{from_database, publish}` | Marker before hash lets a reader know the record shape from byte 0; leaves may omit the stored hash | Trie blob bytes + `marf_record_format` row + schema 4 | `codec.rs Record::parse` (second parser behind the same API), `storage.rs open/migrate` |
| C2 | Packed branch pointer columns | `packed_branch.rs` `PackedBranch::{parse, child, pointer_at}`, `BranchView::parse(format, ...)`, 2/3/4/8-byte targets, 1–4-byte origins, 6-bit Node48 selectors | Selected-child read touches one column slot instead of scanning pointers | Yes (format 6) | `codec.rs BranchView::{skip_scan, child_at}` |
| C3 | Compact Node256 metadata | `canonical_branch.rs` `CompactBranch`, flags `FULL_OCCUPANCY`/`SPARSE_KINDS`/`SPARSE_ORIGINS`, `expand_packed` | Drops the 32-byte bitmap / per-child kind bytes when a shortcut encoding is smaller | Yes (format 6) | same as C2 |
| C4 | Compact raw leaves | `raw_leaf.rs` `WIDTHS = [0, 4, 32, 40]`, header `path_len | mode<<6` | Trailing zero bytes of the 40-byte value are not stored | Yes (format 6) | `codec.rs LeafView` |
| C5 | Inline Clarity values | `inline_value.rs` `InlineValue`, `InlineValueBytes` (mapping-backed, `INLINE_BYTES = 30`), `TrieNodeID::InlineLeaf = 9`, `TrieLeaf.inline` | Small values are read from the trie mapping with no SQLite side-store query | Yes (format 6) + consumer | `codec.rs LeafView`; `View::lookup` returns `MARFValue` only |
| C6 | Stable-ID leaves / hashless leaves / value resolver | `TrieNodeID::StableIdLeaf = 10`, `TrieNodeID::RawLeaf = 8`, `ValueLeaf = 7`; `TrieLeaf { data: Option<MARFValue>, value_id, inline }`; `ValueResolver::{commitment_by_id, inline_commitment}`, `RecordContext::{resolve_leaf, hash}`; consumer `stackslib/src/clarity_vm/database/{stable_value_store.rs StableValueStore, value_extents.rs LockedValueBackend (impl ValueResolver), binary_value_store.rs}`, `MarfedKV::enable_stable_value_ids`, `docs/canonical-marf-storage.md` (14-byte directory rows, mmap partitions, PtrHash dedup) | Leaf stores a 4-byte id; value bytes live in mmap-backed partitions; leaf hash computed on demand | Yes: trie format + new sidecar directory `marf.sqlite.stable-*` + Clarity SQL schema | `codec.rs LeafView`, `lib.rs MARFValue`; consumer `MarfedKV` |
| C7 | Direct block-hash sidecar | `direct_hash_index.rs` `DirectHashIndex::{open, begin, append_with_parent, block_hash, root_hash}`, `DirectHashGuard::prepare_commit`, file `<db>.hashes-<token>` (`MARFDH03`, 4 KiB header, 80-byte slots, checkpoints every 256), tables `marf_direct_hash_index`/`marf_direct_hash_live` + triggers; `build()` offline | Block-id -> (hash, root) without any SQL; used by `BlockMap`, `read_node_hash` for committed roots | Sidecar file + SQL tables/triggers | `storage.rs Store::{block_hash, read_root_hash}`; the rewrite already **retires** a found sidecar (`retire_direct_hash_sidecar` sets `valid = 0` on first write) |
| C8 | Ancestry records | `ancestry.rs` `Ancestry { height, parent, skip }` (16-byte slot extension, `skip = ancestor at height & (height-1)`, `MAX_STEPS = 1024`), `ancestor()`, `TrieRAM.ancestry_safe`, `exclude_ancestry`; `indexed_ancestor`, `indexed_ancestor_root` | `get_block_at_height` and seal-time ancestor roots become O(log h) sidecar reads instead of trie walks for `__MARF_BLOCK_HEIGHT_TO_HASH::h` | Sidecar (part of C7) | `marf.rs View::{get_block_at_height, ancestor_hashes}`. Format-neutral variant **(b)**: an in-memory per-store `HashMap<u32, Ancestry>` filled when `set_block_heights`/`begin` runs and from `BlobHeader.parent_hash` on first miss; honest benefit: cuts ~log2(h) key walks per seal, but a cold process must still walk once per block, so modest |
| C9 | Offline relocation / migration | `value_relocation.rs` `BlobRelocation::{plan_packed_format, rewrite_format}`, `RelocationPlan`; `contrib/stacks-storage-migrate` (`marf`/`chainstate` subcommands, `--value-store clarity`, `--reuse-clarity-extraction`, examples `verify_pair`/`verify_values`/`export_stable_pairs`); `contrib/stacks-inspect/src/storage_prepare.rs` `prepare_for_validation` | Produces format-6 databases from legacy ones | Tooling for C1–C8 | n/a |
| C10 | Squash adaptations | `squash.rs` `squash_to_path_with_resolver`, format publication to the destination, `MarfReadState` DFS; `squash/node_store.rs` leaf tag bits for `inline`/`value_id`; `contrib/marf-squash/src/ops.rs` passes `open_clarity_value_resolver` | Compatibility with C1/C6; **no parallel or disk-backed squash is added** (upstream already had `node_store`, `pre_read_blob_headers`, `prefetch_node`) | Follows C1/C6 | `pruning.rs` |

## D. Direct consumers (stackslib), outside the MARF crate

| # | Change | Reference | Format impact | Class |
|---|---|---|---|---|
| D1 | `DecodedRewardSetCache` (4-entry LRU of `Arc<RewardSet>`), `get_reward_set_cached` | `stackslib/src/chainstate/stacks/db/mod.rs` | no | (c) consumer-side; independent of the MARF crate |
| D2 | `checkpoint_sqlite_dbs` (`PRAGMA wal_checkpoint(RESTART)` on Clarity and index MARFs) | same file | no | (c) consumer/operational |
| D3 | Typed/binary value reads (`stores_typed_values`, `get_typed_value`, `get_stored_value`, `put_all_data_entries`; `binary_value_store.rs` BinaryV1 + `ShapeCache`; `ephemeral.rs` same trait methods) | `clarity_vm/database/{marf.rs, ephemeral.rs, binary_value_store.rs}` | Clarity side-store schema (BinaryV1) | (c) consumer + side-store format |
| D4 | `handle_contract_call_special_cases_ref` (borrowed `ValueRef` args, PoX-only clone) | `clarity_vm/special.rs`, `database/mod.rs` | no | (c) consumer |
| D5 | Snapshot/fork-storage support for stable values (`open_clarity_value_resolver`, `copy_stable_generations`, `ReferencedRowLayout`) | `chainstate/stacks/db/snapshot/{clarity.rs, common.rs, fork_storage.rs}` | follows C6 | (c) |
| D6 | Production MARF options: `with_mmap(true)`, `root_node_cache = false`, `resolved_patch_cache_capacity = 64` | `stackslib/src/config/mod.rs` `get_marf_opts` | no | informs A1/A6/A7 defaults |

## E. Measurement / benchmark hooks

| Hook | Reference | Notes | Class |
|---|---|---|---|
| `read_bench` counters | `stacks-marf/src/read_bench.rs` (`Counters`: logical/ram node reads, root cache hits/misses, `mmap_node_returns`, `resolved_item_reads`, `patch_records`, patch-cache hits/misses, `cursor_steps`, `backpointer_follows`; thread-local `update`/`take`), feature `marf-read-bench-counters` | zero cost when the feature is off | (b) APPLICABLE: same `#[cfg(feature)]` thread-local in `storage.rs resolve_in` / `marf.rs View::lookup` |
| SQLite I/O probe | `db_io_probe.rs` `QueryProbe::begin` (swaps `sqlite3_io_methods` for one query, counts reads/bytes/fetches/syncs via `stacks_profiler::diagnostics`), `pager_bytes`; features `sqlite-io-diagnostics`, `commit-residency-diagnostics`; used in `trie_sql::get_block_hash` and `stackslib/src/util_lib/db.rs` | unsafe FFI, diagnostic only | (b) APPLICABLE but low value; needs `stacks-profiler` dependency |
| Profiler spans | `storage.rs`: `MARF: Read persisted node` (1/256), `MARF: Read persisted hash` (1/64), `Seal: Node hashes`, `Seal: Ancestor hashes`, `Flush: Serialize trie`, `Flush: Store trie`, `Commit: Prepare hash publication`, `Commit: SQLite transaction`, `Commit: Publish caches`; `file.rs`: `Commit: Blob write/sync/mapping`; `direct_hash_index.rs`: `Direct hash: *`; `trie.rs`: `Seal: Resolve ancestor identity`, `Seal: Retrieve ancestor root` (diagnostic); `trie_sql.rs`: `MARF: Block hash SQL`; stackslib `clarity.rs`: `Tx: Writeback`; documented in `contrib/stacks-bench/PROFILING.md` | `stacks-marf` gains a `stacks-profiler` dependency (upstream crate has none) | (b) APPLICABLE: `marf.rs View::{seal, flush}`, `storage.rs Blobs::write_at`, `resolve_in`; keep the same span names so `stacks-bench` filters (`--span 'Seal:*' --span 'Flush:*' --span 'Commit:*'`) work |
| mmap advice stats | `crates/stacks-mmap` `AdviceStats`, `advice_stats()` | tied to A1 sub-item | (b) with A1 |
| `stacks-bench` | `contrib/stacks-bench` (replay benchmark, baseline calibration, per-block/tx stats, MCP server, `context.rs` toggles `root_node_cache`/`resolved_patch_cache_capacity`/`result_cache_capacity`/`with_mmap`) | out-of-crate tooling | (b) usable against the rewrite once stackslib is wired to it; its option toggles map to A6–A8 |
| Test/bench adaptations | `test/marf_perfs.rs` (uses `MarfReadCtx`/`MarfReadState`), new `test/{cache.rs, scratch.rs, record_format.rs}`; `contrib/tools/run-fuzz-campaign.sh` (cargo-fuzz campaign runner, not perf) | – | n/a |

## Summary table

| # | Optimization | Format impact | Class |
|---|---|---|---|
| A1 | mmap blob reads (lazy map) | no | (a) IMPLEMENTED |
| A1' | reserved-address growth + madvise + tail map | no | (b) APPLICABLE, small |
| A2 | borrowed node/leaf/branch views | no | (a) IMPLEMENTED |
| A3 | single-pass descend | no | (a) IMPLEMENTED |
| A4 | reusable decode/patch scratch | no | (a) IMPLEMENTED |
| A5 | inline `NodePath` | no | (a) IMPLEMENTED |
| A6 | resolved patch-chain cache | no | (a) IMPLEMENTED (HashMap, block-id key) |
| A7 | root-node cache | no | (b) APPLICABLE, marginal (prod disables) |
| A8 | lookup-result cache (mutable trie) | no | (b) APPLICABLE, moderate |
| A9 | trie-offset hint / location cache | no | (a) IMPLEMENTED |
| A10 | block id<->hash cache (forward-only) | no | (a) IMPLEMENTED (both directions) |
| A11 | covering index `block_id_hash_marf_data` | SQLite index only | (b) APPLICABLE, small |
| A12 | stack-scratch branch hashing | no | (a) IMPLEMENTED |
| A13 | indexed ancestor roots at seal | sidecar | (c) DEFERRED (C8) |
| B1 | positioned append + sync + remap | no | (a) IMPLEMENTED |
| B2 | transaction cache guards | no | (a) IMPLEMENTED (`Store::invalidate`) |
| B3 | direct packed emission / packed root reservation | format 6 | (c) DEFERRED |
| C1 | record format 6 (type-first) | trie blob + schema 4 | (c) DEFERRED |
| C2 | packed branch columns | format 6 | (c) DEFERRED |
| C3 | compact Node256 metadata | format 6 | (c) DEFERRED |
| C4 | compact raw leaves | format 6 | (c) DEFERRED |
| C5 | inline Clarity values | format 6 + consumer | (c) DEFERRED |
| C6 | stable-ID / hashless leaves + value store | format 6 + sidecar dir + Clarity schema | (c) DEFERRED |
| C7 | direct block-hash sidecar | sidecar + SQL tables | (c) DEFERRED (rewrite retires it) |
| C8 | ancestry records | sidecar | (c) DEFERRED; in-memory variant (b) modest |
| C9 | relocation / migration tooling | tooling | (c) DEFERRED |
| C10 | squash adaptations | follows C1/C6 | (c) DEFERRED |
| D1 | decoded reward-set cache | no | (c) consumer |
| D2 | WAL checkpoint helper | no | (c) consumer |
| D3 | typed/binary value reads | Clarity side-store | (c) consumer |
| D4 | borrowed special-case args | no | (c) consumer |
| D5 | snapshot stable-value support | follows C6 | (c) consumer |
| E1 | `read_bench` counters | no | (b) APPLICABLE |
| E2 | `db_io_probe` | no | (b) APPLICABLE, low value |
| E3 | profiler spans | no (new dep) | (b) APPLICABLE |
| E4 | `stacks-bench` tooling | no | (b) reusable |

Counts: (a) 11 · (b) 8 (A1', A7, A8, A11, E1–E4; plus the (b) variant of C8) · (c) 17 (A13, B3, C1–C10, D1–D5).
