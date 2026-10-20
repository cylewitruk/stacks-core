# MARF rewrite: compatibility contract

Compatibility baseline: upstream `stacks-network/stacks-core` `main` at
`ad5c57786bb51b58d4bdb4992ad534f762d8cf03`. The crate `stacks-marf/` at that
commit is the authoritative behavioural and storage oracle; it is unchanged
in this tree. The rewrite is `crates/stacks-marf-next/`.

Optimization reference: `29ef093da5dd58c1fa484624d94a9d972356c629` (tag
`optimized-ref`). It is consulted for performance ideas (`OPTIMIZATIONS.md`)
and is **not** a compatibility requirement; its storage additions are
described in §3 only so that their boundaries are explicit.

## 1. Contract: the upstream format, bidirectionally, without conversion

A database written by upstream `stacks-marf` is read and extended by the
rewrite, and vice versa, with no migration step, for every artifact upstream
persists:

| Artifact | Contract |
|---|---|
| `marf.sqlite` schema | Versions 1 to 3 are opened and migrated to 3 exactly as upstream does (`schema_version`, `migrated_version`, `marf_data`, `mined_blocks`, `block_extension_locks`, `marf_squash_info`, `marf_squashed_blocks`). Table and column names are unchanged; the `pruning` API maps onto the `squash_*` columns. |
| `marf.sqlite.blobs` | Append-only; tries located by `marf_data.external_offset/external_length`; the schema-2 internal-to-external export is implemented. |
| Trie blob | 32-byte parent hash, 4 zero reserved bytes, root record at offset 36, remaining records in pre-order. |
| Records | `hash(32) || body`; every upstream encoding: plain, compressed dense, compressed sparse, wide (u64) offsets, inline back-block annotations, `Node48` index arrays, patch records (id 6) with `MAX_PATCH_DEPTH = 4`. |
| Patch semantics | Patches are generated for copied-forward nodes against the node as resolved at its origin and resolved with upstream's `apply_node*` rules. |
| Consensus | Node hashes, MARF root hashes (ancestor skip-list), block-height mapping keys, proof wire format and proof verification are byte-identical. |
| Squash / prune | `Prune::prune_to_path` writes the same side tables and shared-blob layout as `MARF::squash_to_path`; squashed databases are read by either implementation. |

How this is checked: 22 operation sequences are recorded from upstream's
crate (`contrib/marf-differential/fixtures`, see `PROVENANCE.md`) and
replayed against the rewrite comparing every observation (values, root
hashes, heights, blocks at height, fork checks, error kinds, serialized
proofs and their verification). Each fixture is also split at every
`Close`/`Prune` and run with the prefix on one implementation and the suffix
on the other over the same directory, in both directions. Command and
results: `STATUS.md`.

## 2. API surface for consumers

`stacks-marf-next` exposes the names upstream consumers import
(`MARF`, `MarfTransaction`, `MarfConnection`, `MARFOpenOpts`,
`TrieFileStorage`, `TrieHashCalculationMode`, `TrieMerkleProof`,
`MARFValue`, `MarfTrieId`, `ClarityMarfTrieId`, `Error`, `trie_sql::*`,
`file::TrieFile`, `test_override_marf_compression`,
`fault_injection_marf_compression`). Differences a consumer sees when the
dependency is pointed at the rewrite (`stacks-marf = { package =
"stacks-marf-next", path = ... }`):

* Paths are `impl AsRef<Path>`; `MARF::get_db_path` returns `&Path` rather
  than `&str` (one call site in `stackslib` converts).
* Squashing is `pruning::Prune::prune_to_path` returning `PruneStats`;
  `PruneInfo` fields are `prune_root_node_hash` / `prune_height`
  (`trie_sql::read_squash_info` and `SqlSquashInfo` remain as aliases).
  Error variants are `HistoricalReadInPrunedRange` and
  `UnsupportedOnPrunedMarf`.
* `Error` has no `BackptrNotFoundError`, `BadSeekValue`,
  `BlockHashMapCorruptionError`, `NotDirectoryError`, `PartialWriteError`,
  `CursorError`, `RequestedIdentifierForExtensionTrie` variants; the
  situations that raised them are reported as `CorruptionError` or
  `NotFoundError`. No consumer matches on the removed variants.
* `TrieHashCalculationMode::Immediate` is accepted and treated as
  `Deferred` (identical hashes); the root hash of an unsealed trie reads
  as zero.
* Reads name their block; `open_block` only sets the tip that
  `check_ancestor_block_hash` uses.

The integration commit on this branch switches `stackslib`, `stacks-node`,
`contrib/clarity-cli`, `contrib/stacks-inspect` and `contrib/marf-squash`
to the rewrite with those adaptations; the upstream crate stays in the
workspace, used by `contrib/marf-differential` as the oracle.

## 3. The optimization reference's storage additions (not in scope)

These exist only on the reference branch; upstream neither writes nor reads
them, so a rewrite that is compatible with upstream is complete without
them. They are listed to fix the boundary.

* **Record format 6** (`marf_record_format.version = 6`, `schema_version =
  4`, header tag `MRF\x06`): packed branches, compact leaves, inline or
  stable-ID Clarity values. The rewrite refuses such a database at open with
  a `CorruptionError` naming the schema. Boundary: `codec::Record::parse`,
  `storage::open`/`migrate` (`FUTURE.md` §1).
* **Clarity stable-ID value store** (`marf.sqlite.stable-<hex>` directory,
  `clarity_stable_format`, resolver installed by `MarfedKV`): lives inside
  format-6 databases; same refusal. Boundary: `codec::LeafView::value`
  (`FUTURE.md` §2).
* **Direct block-hash sidecar** (`<db>.hashes-<token>`,
  `marf_direct_hash_index`): never read by the rewrite; marked `valid = 0`
  before the rewrite's first write to a database that carries one, so the
  reference falls back to the SQL tables rather than trusting a stale file.
  Boundary: `Store::{block_id, block_hash}` (`FUTURE.md` §4).
* **Open options** `mmap`, `root_node_cache`, `resolved_patch_cache_capacity`,
  `result_cache_capacity`: no persisted footprint; the rewrite always maps
  the blobs file and always keeps a bounded resolved-patch cache.

Nothing in this milestone introduces format 6, stable-ID storage or a
changed Clarity initialization path.
