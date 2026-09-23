# Clarity Value Storage Migrator

This offline utility rebuilds a legacy Clarity side-store database in the
Binary V1 format. Its command-line and console presentation live in this crate;
the silent migration engine and authoritative schema remain in `stackslib`.

Build it from the workspace root:

```text
cargo build --release -p clarity-value-storage-migrate
```

Run `clarity-value-storage-migrate --help` for command-line options. See
[`docs/clarity-side-store-migration.md`](../../docs/clarity-side-store-migration.md)
for safety requirements, verification modes, and cutover instructions.
