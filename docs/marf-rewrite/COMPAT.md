# MARF rewrite: compatibility contract

Base: `29ef093da5dd58c1fa484624d94a9d972356c629` (tag `optimized-ref`), where
`stacks-marf/` holds the original MARF with the optimized storage work
("MARF v1"). The rewrite is `crates/stacks-marf-next/`. This document states
exactly which persisted artifacts the rewrite reads and writes today, how it
behaves when it meets each of the optimized reference's artifacts, and what
the integration into Stacks must decide because of that.

## 1. What the rewrite implements: the legacy format, bidirectionally

A database written by either implementation in the **legacy** format is
read and extended by the other with no migration. "Legacy" means everything
`stacks-marf` at the base produces when its `NodeRecordFormat` is `Legacy`,
which is every MARF a node at the base operates on except a freshly created
Clarity MARF (see §2.1).

| Artifact | Contract |
|---|---|
| `marf.sqlite` schema | Versions 1 to 3 are opened and migrated to 3 exactly as the reference does (`schema_version`, `migrated_version`, `marf_data`, `mined_blocks`, `block_extension_locks`, `marf_squash_info`, `marf_squashed_blocks`). Column and table names are unchanged, including `squash_*` names behind the `pruning` API. |
| `marf.sqlite.blobs` | Append-only; tries are located by `marf_data.external_offset/external_length`; the schema-2 internal-to-external export is implemented. |
| Trie blob | 32-byte parent hash, 4 zero reserved bytes, root record at offset 36, remaining records in pre-order. |
| Records | `hash(32) || body`; all legacy encodings: plain, compressed dense, compressed sparse, wide (u64) offsets, inline back-block annotations, `Node48` index arrays, patch records (id 6) with `MAX_PATCH_DEPTH = 4`. |
| Patch semantics | Patches are generated for copied-forward nodes against the node as resolved at its origin (base pointer = the record or the outermost patch there), and resolved with the reference's `apply_node*` rules. |
| Consensus | Node hashes, MARF root hashes (ancestor skip-list), block-height mapping keys, proof wire format and proof verification are byte-identical. Verified by 22 recorded fixtures and both hand-off directions (`contrib/marf-differential`). |
| Pruning ("squash") | `Prune::prune_to_path` writes the same side tables and shared blob layout; a pruned database is read by the reference and vice versa (fixtures `prune_*`). |

The guarantee is checked, not assumed: every fixture is recorded from the
reference and replayed against the rewrite, and every fixture is split at
each `Close`/`Prune` with the prefix run by one implementation and the suffix
by the other, over the same files.

## 2. The optimized reference's artifacts and what the rewrite does with them

The base introduces four persisted things beyond the legacy format. None
changes consensus (roots, proofs); all are storage-level.

### 2.1 Record format 6 ("type-first", `MRF\x06`)

* Marker: `marf_record_format(singleton=1, version=6)` and `schema_version = 4`
  (`SQL_MARF_TYPE_FIRST_SCHEMA_VERSION`); each trie's reserved header bytes
  carry `MRF\x06`. Records put the node marker first, pack pointer columns,
  omit `Node256` occupancy bitmaps where cheaper, compact raw leaves, and
  store Clarity values either inline or as a 4-byte stable ID (§2.2).
* Producers at the base: `stacks-storage-migrate` (offline conversion of a
  whole MARF or chainstate), and **`MarfedKV::setup_db` for a brand-new
  Clarity MARF** (`enable_stable_value_ids` runs when the store is empty).
  The chainstate index MARF and the sortition MARF stay legacy.
* Rewrite behaviour: **refused at open** with a `CorruptionError` naming the
  schema and this document. Nothing is read or written, so a format-6
  database cannot be damaged by the rewrite. Reading format 6 is required
  work before the rewrite can replace `stacks-marf` under a node that keeps
  that initialization path; see §3 and `FUTURE.md` §1.

### 2.2 Clarity stable-ID value store

* Artifacts: directory `marf.sqlite.stable-<16-byte-hex>` next to the
  database (partitions, directory, descriptors, PtrHash index), SQL table
  `clarity_stable_format`, and the `canonical_clarity_initialization` pending
  marker. Leaves in such a trie carry stable IDs; the 40-byte commitment is
  reconstructed by a `ValueResolver` that `stackslib` installs on the MARF.
* Rewrite behaviour: the value store only exists inside format-6 databases,
  so it is covered by the refusal in §2.1. The rewrite's leaf is the raw
  40-byte value; the `MARFValue` hashing and proof code already treat it as
  opaque, which is where a resolver would plug in (`FUTURE.md` §2).

### 2.3 Direct block-hash index (`<db>.hashes-<token>`)

* Artifacts: sidecar file named by a 16-byte token, described by
  `marf_direct_hash_index(singleton, token, max_id, valid)`; each slot holds
  a block hash and a 16-byte ancestry extension (height, parent id, skip id).
  Published by `stacks-storage-migrate`; the node extends it as it commits
  when the table exists, and falls back to the SQL tables for any id the
  sidecar does not cover or when `valid = 0`.
* Rewrite behaviour: the sidecar is **never read** (block ids and ancestry
  come from `marf_data` and the trie's height keys, as in the legacy path)
  and **never extended**. Before its first write to a database that has the
  table, the rewrite sets `valid = 0`, so a reference binary that later opens
  the database ignores the now-stale file instead of trusting it. Reading
  such a database without writing leaves the publication untouched. The
  sidecar is an accelerator with the SQL tables as the source of truth, so
  this costs performance in the reference, never correctness.

### 2.4 Caches and memory mapping (no persisted footprint)

The reference's `MARFOpenOpts` gained `mmap`, `root_node_cache`,
`resolved_patch_cache_capacity` and `result_cache_capacity`. The rewrite's
`MARFOpenOpts` keeps the four original fields; the rewrite always maps the
blobs file, always keeps a bounded resolved-patch cache (4,096 locations),
and deliberately has no lookup-result cache (see `DECISIONS.md` ADR-17). A
consumer that sets the reference's extra fields needs a shim at integration
time; none of them affects what is on disk.

## 3. What integration must decide

1. **Clarity MARF creation path.** At the base a fresh Clarity MARF is
   initialized in format 6 with a stable-ID store. Replacing `stacks-marf`
   with the rewrite as-is would make `MarfedKV::setup_db` fail on a fresh
   chainstate. Either (a) the integration keeps new Clarity MARFs legacy
   (skip `enable_stable_value_ids`; existing legacy chainstates are
   unaffected), or (b) format 6 and the value resolver are implemented in
   the rewrite first. Legacy-only integration is the smaller step and keeps
   the reference as a drop-in oracle for every consumer test.
2. **Migrated databases.** A chainstate converted by `stacks-storage-migrate`
   is format 6 throughout and is out of scope until (b).
3. **Open options.** `with_mmap`, `root_node_cache`,
   `resolved_patch_cache_capacity`, `result_cache_capacity` have no
   equivalents; the config layer (`Config::get_marf_opts`) needs the
   corresponding fields dropped or accepted and ignored.
4. **Direct hash sidecar.** Databases with a published sidecar lose it on the
   rewrite's first write (§2.3). Acceptable for the integration branch; a
   maintained sidecar is `FUTURE.md` §4.

## 4. Out of scope on purpose

* The retired experimental formats (`marf_record_format` versions 1 to 5 and
  41) are refused by the reference too.
* `stacks-storage-migrate` and `stacks-inspect validate-block` pipelines
  are tooling over the reference's codec; they are not ported.
