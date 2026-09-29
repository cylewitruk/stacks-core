# Compact Node256 metadata (V4.1)

V4.1 retains V4 pointer columns and leaf encodings. Node256 records can omit a full occupancy bitmap, encode a common child kind plus exceptions, and encode common origin-presence bits plus exceptions. Each choice is used only when the complete payload shrinks. Node4, Node16, Node48 and patch records keep their existing encoding.

The runtime writes directly from logical pointers, with no intermediate V4 serialization and no codec environment switch. Selected-child reads access checked mmap-backed columns. Existing V4 databases remain readable and continue writing V4 until explicitly migrated. V4.1 databases use A automatically in every consumer, including stacks-node, stacks-inspect and stacks-bench.

## Migration

Run against an offline disposable copy, with separate scratch and destination directories:

```sh
cargo build --release -p stacks-trie-format-v41
./target/release/stacks-trie-format-v41 all SOURCE_SQLITE SOURCE_BLOBS SCRATCH DESTINATION
```

The converter requires a completed V4 source and preserves logical roots, leaves, and ancestor relationships. It publishes format version 41 (`MRF\x29`) only after the rewrite. The source is unchanged. The same tool handles a V4 Clarity or VM-index MARF; it does not extract values or perform unrelated legacy migrations. For an index MARF, place the resulting `marf.sqlite` and associated sidecars at the index database's expected paths only while all consumers are stopped.

`stacks-inspect` preparation adds this stage after Clarity V4 conversion and includes its directory cutover in recovery. Older binaries cannot read V4.1. B/Bbit/C experimental payloads are rejected; only A is promoted.
