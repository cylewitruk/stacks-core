# MARF rewrite: format-breaking optimizations and where they plug in

The rewrite preserves upstream's persisted format (`COMPAT.md` §1). The
optimization reference (`29ef093d`, see `OPTIMIZATIONS.md` for the full
classified inventory) adds a new record format, a Clarity value store, a
direct block-hash index and ancestry records, all produced by offline
tooling or, for new Clarity MARFs, at creation. This note records what each
one is, what the rewrite already has in place for it, and the boundary it
would cross. None of the format-changing items is implemented, and none is
a prerequisite for this milestone; items marked *done* are the
format-preserving ones that landed.

## Boundaries in the rewrite

| Concern | Module | Interface to the rest of the crate |
|---|---|---|
| Logical node model | `node.rs` | `Node`, `Branch`, `Leaf`, `Ptr`, `Path` |
| Consensus hashing | `hash.rs` | `leaf_hash`, `branch_hash`, `marf_root_hash` over `node` types and a `ChildHash` callback |
| Persisted record encoding | `codec.rs` | `Record::parse` -> `{leaf, branch, patch, descend, decode}`; `encode_node`, `encode_patch`, `node_len` (and `_mapped` variants for layout) |
| Blob layout | `trie.rs` (`serialize_at`, `load`) | header constants, pre-order placement, offset width fixpoint |
| Physical storage | `storage.rs` | `Store::{node, branch, read_node, read_hash, read_trie_blob, write_*}` over SQLite + `Blobs` (mmap) |
| Forest semantics | `marf.rs`, `proofs.rs`, `pruning.rs` | `NodeView::descend` and the `Store` interface only |

`marf.rs`, `trie.rs` (insertion, hashing) and `proofs.rs` never see control
bits, offset widths, patch chains or file layout. A second persisted format
is a second `Record` parser and a second encoder behind the same functions,
selected per database at open time, plus a `Store` that knows where records
live.

## 1. Record format 6 (type-first, packed branches)

Reference (at `optimized-ref`): `record.rs` (`NodeRecordFormat::{Legacy, Optimized}`),
`packed_branch.rs`, `canonical_branch.rs`, `blob_layout.rs` (header tag
`MRF\x06`), `raw_leaf.rs`, `inline_value.rs`; selected by
`marf_record_format.version = 6` with `schema_version = 4`.

What changes: node marker first; branch pointer columns packed by field;
`Node256` occupancy bitmaps replaced by common-type / origin-presence
shortcuts when smaller; raw leaves drop trailing zero bytes; Clarity leaves
are inline values or 4-byte stable IDs. Branch hashes stay stored.

Where it fits: `codec::Record` becomes format-aware (`parse` takes the
database's `RecordFormat`); `BranchView::skip_scan`/`child_at` get a second
slot-list walker; `LeafView` gains the inline/raw/stable-ID shapes;
`encode_*` and `node_len_mapped` get the packed writer. `trie::serialize_at`
already separates layout (offset fixpoint) from emission, so only the
per-record length and bytes change. `storage::open` reads the marker and
`migrate` stops refusing schema 4. Hashing is untouched: the preimage is
defined over logical nodes.

Boundary to respect: a format-6 trie's root slot is reserved at a fixed
size by the reference's writer (`packed_root_reservation`); the rewrite's
layout step must reproduce it for the reference to read rewrite-written
tries. Upstream itself never produces format 6, so this only matters if the
rewrite is asked to interoperate with reference-written databases.

## 2. Clarity stable-ID value store

Reference: `inline_value.rs`, `raw_leaf.rs`, `value_relocation.rs`,
`docs/canonical-marf-storage.md`; owned by `stackslib`'s `MarfedKV`
(`StableValueStore`, `clarity_stable_format`, directory
`marf.sqlite.stable-<hex>`).

What changes: a leaf stores a small inline value or a stable ID; the 40-byte
commitment is reconstructed through a resolver installed by the consumer.

Where it fits: `Leaf.value` stays the 40-byte `MARFValue` for hashing and
proofs; `codec::LeafView::value()` consults a `ValueResolver` held by `Store`
when the record is a stable-ID leaf, which is the only place a leaf's bytes
are turned into a value. Writing stable-ID leaves needs the consumer to hand
the ID in (the reference does this through `MarfedKV`), so the insertion API
gains a value kind. Snapshot export and migration ownership
(`canonical-marf-storage.md` §"Migration and ownership") stay with the
tooling; the rewrite must read "data before reference" publication order and
fail closed when a registered store is missing, as the reference does.

## 3. Memory-mapped trie blobs (*done, format-preserving*)

`storage::Blobs` maps the blobs file lazily and extends the map on demand;
records are borrowed from it. Remaining difference from the reference's
`mapped_file.rs`: no reserved address range with incremental page mapping
and no shared mapping across reopened handles (each handle maps on first
use). Both are internal to `Blobs`.

## 4. Direct block-hash index and ancestry

Reference: `direct_hash_index.rs` (`<db>.hashes-<token>` sidecar,
`marf_direct_hash_index` table), `ancestry.rs` (height, parent id, skip id
per block, 16 bytes).

What changes: `block_id <-> block_hash` and ancestor-at-height resolution
become array reads instead of SQL queries and trie lookups.

Where it fits: `Store::{block_id, block_hash}` and `View::{ancestor_hashes,
get_block_at_height, get_block_height}` are the consumers. Today the rewrite
retires a published sidecar on first write (`COMPAT.md` §2.3). Maintaining
it means appending a slot per committed block inside the same transaction
and honouring the token/`max_id` publication protocol; it is additive and
does not alter consensus state. A covering SQL index on
`marf_data(block_hash, block_id)` is the cheap alternative the reference
also uses when no sidecar exists.

## 5. Caches (*partly done, no format impact*)

Done: bounded resolved-patch cache (ADR-17). Not done, deliberately: the
reference's lookup-result cache and root-node cache. Both are additive and
would live in `Store`/`View`; measure first (`marf-differential alloc`),
since borrowed lookups are already below the reference's cache-hit cost.

## 6. Atomic publication and snapshot ownership

Reference: `stacks-storage-migrate` (private `.NAME.migrating-PID`
directory, single rename), snapshot export of value directories and sidecar
files together with SQL metadata.

Where it fits: outside the crate. The rewrite's responsibility is to open
what the tooling publishes (§1, §2) and to fail closed on partial state;
`storage::open` is the single place that checks markers.

## Not planned

Pruning already stages nodes on disk and streams the blob (ADR-10, matching
upstream's `squash/node_store.rs` and `squash/stream.rs`). What the
reference adds on top, parallel header pre-reads (`pre_read_blob_headers`)
during collection, is local to `pruning.rs` if collection time on a
mainnet-sized trie turns out to matter. A squash crate with its own source
and destination traits, shared by both implementations and used by
`contrib/marf-squash`, is the natural next refactor once the upstream crate
may be touched.
