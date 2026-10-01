# Clarity storage migration

Use the [canonical MARF migrator](../contrib/stacks-storage-migrate/README.md) with `--value-store clarity` to convert an offline legacy Clarity MARF directly into the current partitioned stable-ID store. The standalone intermediate binary-value and extent converters have been removed.

See [canonical storage](canonical-marf-storage.md) for trie/value layouts, publication, portability and integrity limits. Generic MARFs retain their existing side-store semantics.
