# Canonical optimized MARF storage

The canonical format combines compact Node256 metadata and direct writing with Clarity stable-ID value storage. Its explicit database version is **6**, with trie header tag `MRF\x06`. It is distinct from the earlier experimental A-only and V5-only formats. Logical commitments, roots and proof semantics remain unchanged.

## Trie layout

Records place the node marker first. Branches retain their hashes and use packed pointer columns. Node256 can omit a full occupancy bitmap and encode common child types or origin-presence bits with exceptions; the writer selects compact metadata only when the complete payload shrinks. It emits bytes directly from logical pointers without serializing an intermediate layout. Node4/16/48 and patches retain their packed representations.

Selected-child reads use checked mmap-backed columns. Raw leaves compact trailing zero regions without changing any of their original 40 value bytes. Small Clarity values can live inline; larger values carry a nonzero four-byte stable ID.

## Clarity values

A stable ID selects a 14-byte, explicitly encoded directory row: partition `u16`, offset `u32`, record length `u32`, descriptor ID `u32`. Mmap-backed partitions are bounded by 2^32 bytes and contain the original 32-byte Clarity commitment plus the packed payload. The eight zero padding bytes of a Clarity commitment are reconstructed; nonzero padding is rejected. This rule never applies to arbitrary raw MARF values.

Variable-length reconstruction descriptors are interned in separate files. Their mmap-backed directory uses `u32` offset and length fields: valid descriptors can exceed 65,535 bytes. Bounded caches accelerate lookup; persistent indexes remain authoritative after cache misses. Deduplication combines an immutable colocated PtrHash base with a transactional delta and full-commitment checks.

Publication preserves data-before-reference ordering. Complete files remain owned by readers across reopening and generation changes. IDs are not silently wrapped or reused. Background compaction and wider IDs are not implemented; exhaustion fails rather than overwriting an existing ID.

New Clarity stores record pending initialization in the same transaction that creates their SQL schema. Opening an interrupted initialization retries into a new, uniquely named private generation. Selecting that generation and clearing the pending marker occur atomically. A canonical Clarity trie without its value-store registration fails closed. Abandoned, unregistered initialization directories may be reclaimed only while the store is offline and after checking the selected registration.

The directory does not contain a per-ID integrity checksum. An in-bounds row retargeted to another complete valid value is not independently detected by ordinary reads. Bounds, generation identity, commitment-aware deduplication and root/proof checks serve different purposes and must not be described as covering that case. Process-failure testing also does not establish power-loss qualification.

## Migration and ownership

Use [stacks-storage-migrate](../contrib/stacks-storage-migrate/README.md) against an offline legacy source. Generic MARFs retain their values and SQL side tables, including `__fork_storage`. Clarity extraction is explicitly selected and writes final stable files directly. No intermediate trie format or legacy extent file is produced.

`stacks-inspect validate-block` uses the same pipeline on its disposable offline chainstate. It discovers MARFs by schema, applies Clarity value handling only to the registered Clarity path, and prepares generic MARFs with the canonical trie codec. File inventories retain the previous generation until verification; interrupted publication rolls forward from its durable inventory. Preparation is outside benchmark node-work timing.

Move completed databases together with every registered sibling file and directory. PtrHash's native function serialization must be checked with the target-built reader; rebuild only when that compatibility check requires it. Preserve the original source and benchmark only writable disposable copies.

Snapshot export copies stable value directories, partitions, descriptor files, and registered PtrHash files together with their SQL metadata. These copies preserve the existing generation and IDs; they do not reclaim unreachable values. Generic MARF snapshots keep their original raw-value semantics.
