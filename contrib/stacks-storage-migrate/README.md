# Canonical MARF migration

This package converts an offline upstream legacy MARF directly into the canonical optimized format: packed branches with the compact Node256 writer, compact raw leaves, and optional Clarity stable-ID values. It creates one final trie file without intermediate format databases.

```bash
cargo build --release -p stacks-storage-migrate
./target/release/stacks-storage-migrate marf \
  --source /offline/legacy/marf.sqlite \
  --destination /new/canonical-directory \
  --value-store clarity

./target/release/stacks-storage-migrate chainstate \
  --source /offline/mainnet \
  --destination /new/canonical-mainnet
```

Omit `--value-store clarity` for a generic MARF. Generic values and side tables, including `__fork_storage`, retain their semantics. Clarity mode verifies source-value commitments, writes final partition/directory/descriptor files directly, and builds a colocated stable-ID PtrHash index. It accepts upstream text values and legacy tries with validated BinaryV1 SQL values. Small values become inline leaves; no intermediate extent file is produced.

Chainstate mode takes the directory containing `chainstate/`, requires its Clarity and VM-index MARFs, and discovers other MARFs by their SQLite schema. Only `chainstate/vm/clarity/marf.sqlite` receives Clarity value extraction. It copies other state using copy-on-write where supported, preserves unrelated database siblings, verifies already canonical MARFs, and publishes the whole destination with one directory rename. On filesystems without reflinks, budget a full copy of that other state. Symlinks and special files are rejected. An interrupted private output stays unpublished and must be inspected or discarded before retrying.

The source must be offline and checkpointed. The converter rejects nonempty journals and experimental formats. It never checkpoints or modifies the source. Keep the source offline until conversion finishes: mmap input assumes immutable files.

The destination must not exist. Work takes place in a sibling `.NAME.migrating-PID` directory. Failure leaves that private directory for inspection; it is never resumed automatically. An explicitly selected completed Clarity extraction can be revalidated and reused as described below. Only a completed, synchronized directory is renamed to the requested destination. The source remains available after success.

The default encoded-input limit is 512 MiB per trie. `--max-trie-bytes` can raise this for a verified unusually large trie. Decoded nodes and relocation maps require additional memory. Ancestor offsets live in mapped scratch with a bounded mutable tail; the current implementation retains per-trie descriptor metadata in memory.

Clarity encoding and source-leaf preparation use up to eight available CPU workers. Input batches contain at most 4,096 rows or 16 MiB plus one permitted record; workers preserve source order, and destination writes remain serial. SQL caches, descriptor hints and the ancestor-map mutable tail are bounded. The converter verifies each value's reconstructed commitment before admitting it to the destination.

Private value lookups are loaded sequentially before their covering indexes are bulk-built and checked for duplicate commitments. The final PtrHash base is built from a sorted membership stream; migration does not insert each historical membership into the runtime dedup index. The covering indexes are merged once into a prefix-indexed mmap lookup with full-key comparisons and compact inline payload storage. Trie workers resolve leaves through these mappings; they do not query SQLite per leaf. Pointer relocation and output publication remain dependency ordered. The ordered queue admits at most32 jobs and charges eight times each encoded input size; its byte budget is the larger of512MiB and eight times the configured maximum trie size. Decoder scratch and resident file pages are additional. Lookup tables, indexes and scratch streams are removed before publication.

For a stopped private Clarity conversion whose extraction finished, explicit reuse preserves source and retained files:

```bash
./target/release/stacks-storage-migrate marf \
  --source /offline/legacy/marf.sqlite \
  --destination /new/restarted-directory --value-store clarity \
  --reuse-clarity-extraction /offline/retained-private-directory
```

The retained directory must be offline, contain the unpublished canonical marker, and pass SQL integrity checks. The converter clones its files, rebuilds logical tables and metadata from the original source, verifies exact source-key coverage and all retained external value reconstructions/mappings, and validates inline commitments. Relocation metadata is recomputed. Complete retained trie bytes are reused only if byte-for-byte equal to regenerated output; a partial buffered tail is regenerated, and a differing complete prefix fails. This is verified reuse on a new private copy, not crash recovery for the interrupted process. Both source and retained input must remain unchanged throughout. Budget additional copy-on-write pages and lookup scratch, and retain the failed attempt's evidence until replacement verification finishes.

Provide space for the final files, ancestor relocation scratch, index construction and SQLite maintenance. These peaks overlap within each MARF; MARFs convert sequentially, releasing their scratch before the next one. The free-space guard checks periodically and keeps room for a maximum-size trie rewrite and, for Clarity, a new 4-GiB partition. This guard is not a total-space estimate or a reservation against other processes. Running out of space fails the private build without publishing it or modifying the source.

Transfer the entire completed directory, preserving sibling registrations. PtrHash functions use native serialization: validate with the destination-built reader before benchmarking, and use the included native rebuild tooling if required. Migration time is preparation overhead and must be excluded from node-work benchmark timings.

Independent offline qualification tools are available as examples:

```bash
cargo build --release -p stacks-storage-migrate --examples
./target/release/examples/verify_pair /legacy/marf.sqlite /canonical/marf.sqlite
./target/release/examples/verify_values /canonical/clarity/marf.sqlite
```

`verify_pair` compares local leaf commitments, ordinary proofs and direct ancestry at up to 64 tries spread across the source ID range. `verify_values` reconstructs every external stable value, checks its commitment and exact PtrHash membership, and uses at most four workers. The latter expects a fresh completed conversion with no orphan IDs or subsequent writes; it is not an online maintenance command. Keep both inputs offline throughout these checks. Neither tool replaces full coverage/root auditing or matched replay.

Before transferring a newly converted snapshot, retain a native-rebuild membership stream outside the chainstate:

```bash
./target/release/examples/export_stable_pairs \
  /canonical-mainnet/chainstate/vm/clarity/marf.sqlite \
  /new/native-rebuild-memberships
```

This read-only exporter requires a fresh PtrHash base covering every stable ID and an empty delta. It verifies each ID against the base, sorts full commitment/ID pairs in bounded prefix buckets, and publishes `committed-pairs.bin` with a checksummed `MANIFEST.json`. Sorting is capped at 4,194,304 records per prefix (176 MiB); temporary and final streams can require up to 88 bytes per membership. Keep the input offline. An interrupted export remains private and can be discarded; the published chainstate is unchanged. Transfer this separate stream only if a target-native PtrHash rebuild is needed, using its recorded SHA-256 with the `stacks-inspect` rebuild helper. It is not part of normal runtime storage.

The 14-byte stable value directory retains the qualified V5 integrity policy: bounds, file/generation identity, commitment-aware deduplication and publication checks are enforced. It does not independently detect an in-bounds directory row retargeted to another complete valid value. Process-failure tests do not establish power-loss guarantees. Background compaction and VM-index value extraction are outside this migration.
