# MARF rewrite: architectural decision log

Each entry: decision, rationale, consequences. Newest last. ADR-1, ADR-7 and
ADR-8 describe the first session's set-up and are superseded by ADR-12,
ADR-16 and ADR-19.

## ADR-1: The replacement lives in the `stacks-marf` crate

At the baseline commit `stacks-marf` is an empty shell whose doc comment says
the sources will move there later. We implement the rewrite in that crate so
it depends only on `stacks-common` and `rusqlite`, while the baseline stays
untouched in `stackslib/src/chainstate/stacks/index/` as the oracle. Nothing
in the workspace depends on `stacks-marf` yet, so this is additive.

## ADR-2: One branch representation instead of four structs

`Node4/16/48/256` differ only in capacity and in the Node48 index table, which
is derivable. The rewrite has `Node::Branch { kind, path, children: Vec<Ptr> }`
holding only occupied slots in slot order (insertion order for 4/16/48, by
character for 256). Consequence: hashing/encoding reconstruct empty slots from
`kind`; reading verifies the prefix-occupancy invariant and reports
corruption otherwise. Eliminates the `TrieNode` trait, `TrieNodeType`,
`with_node!`, four `from_bytes`/`insert`/`replace` copies and per-node
`cowptr`/`patches` fields.

## ADR-3: Byte-slice codec at a single boundary

All legacy node encodings (plain, compressed dense/sparse, u64 offsets,
inline annotations, patches) are decoded from `&[u8]` and encoded into
`Vec<u8>` in one module (`codec.rs`). The logical trie never sees control
bits, offsets or patch chains. Future formats become a second codec behind the
same two functions (`decode_node`, `encode_node`) plus a blob reader.

## ADR-4: Deferred hashing only

Hashes of an in-progress trie are computed once, post-order, when the trie is
sealed or committed. `TrieHashCalculationMode::Immediate` is accepted for API
compatibility and treated as `Deferred`; both modes yield identical hashes in
the baseline. This removes the eager rehash-on-insert path, `TrieCursor`
repair bookkeeping and the `All` test mode.

## ADR-5: Reads name their block; no "currently open block" cursor

Every read in the baseline is relative to a mutable `cur_block` that callers
must set and restore. The rewrite passes a `BlockId`/block hash to each read
and keeps a small block-id/hash cache. Consequence: no `open_block` /
`open_block_maybe_id` restore pairs, no `RestoreMarfBlockError`.
`MARF::open_block` remains as a cheap compatibility no-op that validates the
block exists.

## ADR-6: In-memory trie is a flat arena

The trie under construction is `Vec<Slot { node, hash }>` indexed by `u32`;
`Ptr::ptr` of an inline child is an arena index until the trie is serialized,
at which point offsets are assigned. Copy-on-write from ancestors copies the
node into the arena and rewrites its inline children as back-pointers
(`back_block = ancestor block_id`). This is the same logical algorithm as the
baseline's `TrieRAM` without the moved/reinstated data dance.

## ADR-7: Positioned I/O for the blobs file

The `.blobs` file is read with `read_exact_at` (pread) and appended with
`write_all_at`; no shared cursor, no `Read + Seek` plumbing, no RAM/Disk
enum. In-memory databases (`:memory:`) keep blobs inside SQLite exactly like
the baseline does when `external_blobs` is false. Memory mapping is a
contained follow-up inside the same module.

## ADR-8: Differential oracle as a separate crate

`contrib/marf-differential` depends on both `stackslib` (baseline) and
`stacks-marf` (rewrite). Operation sequences are plain data (serde JSON), so
fixtures recorded from the baseline are replayed against the rewrite without
sharing any code with it. Expected values therefore never come from the new
implementation.

## ADR-9: Proof verification is a faithful port, generation is native

Proofs are consumed by third parties, so the verifier (`verify_proof` and its
helpers) is ported line-by-line from the original to accept and reject the
same inputs. Proof *generation* is written against the new node model
(`walk_segment`, `proof_node`, `make_backptr_shunt_proof`) and checked against
recorded baseline proofs byte for byte.

## ADR-10: Pruning stages nodes on disk and streams the blob

Superseded in part. The first version collected the reachable nodes into a
`Vec<Node>` and reused `Trie::seal`/`Trie::serialize`, which is simple but
holds the decoded trie and then the whole serialized blob in memory; the
third review called that out as a capability lost against upstream, whose
squash stages nodes in a temporary file (`squash/node_store.rs`) and streams
the output (`squash/stream.rs`). `Prune::prune_to_path` now does the same:

* Collection writes each node once into a temporary file beside the
  destination, in the plain record encoding behind a 4-byte length
  (`pruning::Staged`). A node's index is assigned when its parent is visited
  (pre-order), so parents are written with child pointers already rewritten
  to inline indices annotated with the child's archival block; there is no
  remap pass, and the dedup index is dropped as soon as collection ends.
* Hashing runs in reverse index order (children before parents) with one
  node decoded at a time and a 32-byte hash per node kept in memory.
* The blob is streamed into the blobs file children-first through a 1 MiB
  buffer, so each pointer's target offset is known when the pointer is
  written; the root goes last into a slot reserved for its widest encoding,
  as upstream does, and the header is written with it. Any slack between the
  root and the first descendant is unused bytes, which readers never see
  (they follow offsets); upstream's squash blobs have the same slack.
* In memory per node: a stage offset, a hash and a blob offset (48 bytes in
  arrays), plus the dedup index while collecting and two 1 MiB buffers.
  Measured peak heap during a prune (`marf-differential alloc`, `prune`
  phase): about 95 bytes a node, against about 330 for the in-memory version
  and about 125 for upstream; see `STATUS.md`. The temp file is about the
  size of the output blob and is removed on drop.

The in-memory `Trie::from_nodes`/`seal`/`serialize` path remains for
ordinary commits, where a trie is one block's worth of nodes.

## ADR-11: The differential harness serializes runs

The baseline mutates the process-wide `TMPDIR` while migrating a fresh
database, which races with `tempfile` in concurrent tests. The harness takes a
global lock around every run rather than changing the baseline.

## ADR-12: The rewrite is `crates/stacks-marf-next`, on the optimized base

Supersedes ADR-1. The intended base is `29ef093d`, where `stacks-marf/`
already holds the migrated original with its optimized storage work. That
crate stays as it is, as the oracle and as the implementation the workspace
still uses; the rewrite is a sibling crate (`stacks-marf-next`, library name
`stacks_marf_next`) with the same public shape (`MARF`, `MarfTransaction`,
`MarfConnection`, `MARFOpenOpts`, `TrieMerkleProof`, `Error`,
`MARFValue`, `MarfTrieId`) so that consumers can switch by import.
Consequence: both crates build in one workspace and the differential harness
depends on both at once.

## ADR-13: Pruning is a trait in `pruning`, and "squash" is called "prune"

The original's `squash_to_path` is an inherent method on `MARF`. The rewrite
exposes it as `pruning::Prune::prune_to_path(src, dst, opts, tip, height,
label)`, implemented for `MARF<T>`, with `PruneInfo`/`PruneStats` in the same
module and `Error::{HistoricalReadInPrunedRange, UnsupportedOnPrunedMarf}`.
The on-disk names (`marf_squash_info`, `squash_root_node_hash`,
`squash_height`, `marf_squashed_blocks`) are unchanged because they are the
format; `compat::trie_sql` keeps the old Rust names for callers that import
them.

## ADR-14: Paths are `impl AsRef<Path>`

Every public entry point that takes a database path (`MARF::from_path`,
`TrieFileStorage::open*`, `Prune::prune_to_path`, `TrieFile::exists`,
`Blobs::exists`) takes `impl AsRef<std::path::Path>`; `get_db_path` returns
`&Path`. The original takes `&str`. Consequence: the differential adapters
convert once at the boundary, and the `:memory:` special case is a `Path`
comparison.

## ADR-15: Borrowed traversal; no `Cow`, no owned node on the read path

Reads are expressed over views that borrow their bytes: `codec::Record`
(a `hash || body` slice), `BranchView`, `LeafView`, `PatchView`, and
`marf::NodeView` (an arena node or a `storage::StoreNode`). Traversal
operations exist on the views: `descend(key, depth)` compares the node's
path and selects the child for the next key byte in a single pass over the
slot list; `path`, `child`, `leaf_value`, `for_each_child` for the callers
that need one of them. Every encoding, plain `Node256`/`Node48` included,
is skip-scanned: one pass reading two bytes per slot (the per-pointer wide
flag decides each pointer's width, so slot positions are not computable
without it); the `Node48` index array is validated on decode but not used
for lookup. Owned `Branch`/`Leaf`/`Node` values are produced only where the
algorithm needs to own them: copy-on-write into the arena, proof nodes (the
wire format), pruning, and the resolution of a patch chain on a patch-cache
miss (the resolved `Branch` is cached and borrowed afterwards). A
`Cow`-style "maybe owned" type was rejected: the owned cases are few and
explicit, and a blanket `Cow` would hide allocations in the read path. The hot decoding loops return
a two-variant error (`Bad`) converted to `Error` at the public boundary,
after profiling showed `Result<_, Error>` drop glue at 16% of lookup time.

## ADR-16: Memory-mapped blobs, extended lazily

Supersedes ADR-7. `storage::Blobs` keeps an `Option<Mmap>` over the blobs
file and re-maps only when a read reaches past the mapped length. The file
is append-only and bytes once written are never rewritten, so an older map
stays valid; a clone starts unmapped and maps on first use. Writes stay
positioned (`write_all_at` + `sync_data`). SQLite-stored tries (internal
blobs, every unconfirmed trie) are read with incremental `blob_open` reads
into one reusable buffer; a whole-trie cache was measured and thrashes
because lookups cross many tries.

## ADR-17: A resolved-patch cache, not a lookup-result cache

The original at the base caches complete lookup results per mutable trie
(2,048 entries) with generation counters and guards for invalidation, and
caches resolved patched nodes in an LRU. Profiling the rewrite's borrowed
lookups showed patch-chain resolution (decode base, re-apply up to four
patches) as the whole cost. The rewrite caches only that: `(block_id,
offset) -> Branch` per store, bounded (4,096 entries, emptied when full),
confirmed tries only. Resolved patch chains are a pure function of immutable
bytes, so the cache needs no generation logic. A result cache was rejected:
the borrowed walk is already faster than the original's cache hit, and
result caches need invalidation on every write path.

## ADR-18: A transaction's drop invalidates the store's caches

`MarfTransaction` owns `Option<Transaction>`. Commit paths take the
transaction; `rollback`, a failed commit, `into_sqlite_tx` and dropping an
uncommitted transaction call `Store::invalidate`, which clears the block-id,
block-hash, location and patch caches. Rows written inside a rolled-back
transaction may be re-created with the same ids and different content, so
nothing cached across the rollback can be trusted.

## ADR-19: Differential adapters call the real APIs

Supersedes ADR-8's description. The harness's `MarfApi` trait has one hook
per operation, and every observation (`Ok`, `Err(kind)`, `Hash`, `Value`,
`Proof`, `Tip`, `Bool`) comes from calling the original's or the rewrite's
public API with the same arguments; no adapter synthesizes an outcome. Error
kinds are mapped by variant name, with the pruning rename applied to the
original's names. The oracle is the original at `29ef093d`; fixtures record
its observations and are never regenerated from the rewrite.

## ADR-20: Optimized-format artifacts are refused or retired, never guessed

Format-6 databases (`schema_version = 4`) are refused at open with an error
naming the schema. A published direct-hash sidecar is marked invalid before
the rewrite's first write to its database, so the original falls back to
the SQL tables rather than trusting a sidecar the rewrite did not extend.
Both are deliberate: reading format 6 is a planned feature (`FUTURE.md`
§1-2), and silently serving a stale accelerator is the one failure that
could look like a consensus bug.

## ADR-21: Upstream main is the compatibility baseline; the optimized branch is a reference

The rewrite is based on upstream `stacks-network/stacks-core` `main` at
`ad5c5778` (the merge-base of the optimized fork). Upstream's `stacks-marf`
is the oracle for behaviour and storage and is left untouched; fixtures are
recorded from it. The optimized fork (`29ef093d`) is used for optimization
ideas (`OPTIMIZATIONS.md`) and benchmarked against separately. Consequence:
the compatibility contract (`COMPAT.md`) covers exactly what upstream
persists; format 6, the stable-ID store and the direct-hash sidecar are
reference-only and stay out of scope for this milestone. The fixtures
recorded from upstream are byte-identical to the ones recorded earlier from
the optimized reference, which was recorded as a cross-check.

## ADR-22: Integration by dependency rename, upstream crate kept in the workspace

Consumers (`stackslib`, `stacks-node`, `contrib/clarity-cli`,
`contrib/stacks-inspect`, `contrib/marf-squash`) are switched with
`stacks-marf = { package = "stacks-marf-next", path = ... }`, so every
`stacks_marf::` import resolves to the rewrite without touching import
paths; `profile-sqlite` exists on the rewrite for feature parity. The
upstream crate stays a workspace member because `contrib/marf-differential`
depends on both. Call-site changes were kept to the API decisions already
made (`get_db_path` returns `&Path`, `Prune::prune_to_path`, `PruneInfo`
field names) plus test helpers re-exposed under their upstream names
(`trie_sql::{read_squash_info, test_insert_squashed_block,
test_append_squashed_block}`, `fault_injection_marf_compression`,
`View::read_block_root_hash`, `MARF::get_root_hash_at`).


## ADR-23: Capability boundaries for proof and archive extensions

The crate keeps `codec`, `trie`, `storage`, `marf`, `proofs`, and `pruning`
as sibling domains. Their `mod.rs` files define shared types and contracts;
child modules own encoding, traversal, persistence, transaction, proof,
and staging operations. Responsibility and private caller scope determine
file boundaries; roughly 1,000 lines is a soft review threshold, not a cap.
Small related implementations stay together rather than adding thin files.

`access::AuthenticatedRead` and `NodeRead` provide authenticated traversal
with borrowed nodes and explicit materialization. `proofs::Prove` is a
blanket extension over that contract. Existing proof constructors and MARF
convenience methods remain available.

`archive::{Archive, ArchiveSource, ArchiveDestination, ArchiveTransaction,
ArchiveBlob}` separate historical reads and destination persistence from
pruning. `pruning::Prune` owns traversal, temporary staging, hashing and
streaming; the backend adapter owns opening files, SQLite transactions,
sync and finalization. Archive boundary metadata belongs to the core
contract, so storage does not depend on the pruning algorithm.

MARF coordination state, storage fields, and trie identity are private.
`StorageContext` exposes immutable policy and metadata; `TrieIdentity`
exposes block identity. Capability implementations stay with the owning
domain. Logical node values and operation results remain ordinary data.
Borrowed record, branch and patch decoding lives in `codec::record` with
its private scanning helpers; encoding, pointer primitives and patch application
remain separate. `storage::read` owns both physical record reads and patch-chain
resolution. `marf::operations` combines borrowed traversal, ancestry and
authenticated reads; its nested write module owns pending-trie persistence
and transaction completion. Public MARF API wrappers, connection adapters and
archive adapters are siblings of writes. Only the shared trie-abandonment
helper lives in their common parent; flush machinery stays with writes.
Shared state stays in `mod.rs`; operation-specific helpers stay with their
callers and use inherent methods rather than organizational traits. Nested
modules preserve private access without expanding the public API.
`MarfConnection` stays available at `marf::MarfConnection` through a reexport.
Archive transactions use `commit_archive`, distinct from MARF trie commits;
authenticated ancestry delegates to a separately named private helper.
There are no restricted visibility modifiers in the crate.

`tests/extraction.rs` compiles the actual proof and pruning modules, plus
the portable positioned-I/O helper, in a separate test crate. It exercises
proof generation on building/committed tries and pruning through public
core capabilities. Its algorithm-facing facades expose only `MARFOpenOpts`
and trie format constants; backend setup imports the concrete crate directly.
Included domain unit tests deliberately run in both crates, and pruning tests
obtain blob paths through `Archive`. Backend access through those facades or
an inherent implementation on a core type fails compilation there. This is an extraction guard,
not a new production crate split; a future split must also decide where
the existing MARF proof convenience API lives.

The codecs, consensus algorithms, streaming buffers and sampling rates
are unchanged. Profiler context labels follow the new module paths.
