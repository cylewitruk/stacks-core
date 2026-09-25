# Extent PtrHash prototype

Build only on an offline disposable clone:

```text
build-extent-ptrhash CLONE_DB NEW_INDEX_DIRECTORY
```

The builder holds the SQLite writer transaction, streams the ordered historical index in 256 digest-prefix partitions, and verifies every serialized key-to-location lookup. A partition is limited to two million keys. This bounds buffered input, not the allocator's RSS. It syncs immutable generation files and their directory before atomically renaming the directory and committing the database activation marker and empty delta. Failed builds can leave unreferenced files; they never activate a partial generation. The old index remains as a reference.

Readers retain the compact functions in memory and map the fixed 16-byte location records on demand. A slot fingerprint rejects most nonmembers. The caller must verify the full 40-byte commitment from the extent header before accepting a candidate. New values go into the writer's existing SQLite transaction through `clarity_extent_delta`; rollback/savepoint handling is unchanged.

The manifest binds the value generation, file watermark, partition sizes and artifact checksums. Function checksums and slot lengths are verified on open. The larger slot-file checksums are archived for offline auditing, not rescanned at every open. Native epserde functions are trusted local artifacts pinned to this implementation, not a portable or untrusted import format.

This first prototype does not implement online delta compaction or index-generation replacement. A base built beside its database is registered by sibling directory name and moves with that database; legacy absolute registrations remain supported. Do not remove a published generation while a database references it. The native serialized function is not portable across CPU architectures: rebuild that base on the destination host from the retained historical SQLite extent index before using a transferred chainstate. Corruption recovery and bounded online rebuilding remain production integration work.
