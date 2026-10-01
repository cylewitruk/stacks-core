# Canonical value PtrHash index

Build on an offline disposable clone:

```text
build-stable-ptrhash CLONE_DB NEW_SIBLING_INDEX_DIRECTORY
```

The builder holds the SQLite writer transaction, partitions historical commitments
by their first byte, and verifies every serialized key-to-ID lookup. Each of the
256 shards is limited to two million keys. Generation files are synced before
the database activates the new base. A failed build never selects a partial base.

Readers share the compact functions and map eight-byte ID/fingerprint slots.
A fingerprint only filters candidates: the caller verifies the complete
commitment from the referenced value record. New memberships remain in the
transactional SQLite delta. Values and IDs are never rewritten by this builder.

Registrations use sibling directory names and move with the owning Clarity MARF.
The manifest binds the value generation, directory watermark and artifact hashes.
Native function serialization is architecture-dependent. Validate imported functions
with the destination-built reader before benchmarking. If a native rebuild is needed,
use `contrib/stacks-inspect/scripts/rebuild-stable-ptrhash.py` on an offline disposable
clone with an empty delta and the exact exported committed-membership stream. The
helper does not merge later delta writes. See `docs/canonical-marf-storage.md` and the
migration README for transfer and export requirements.

These are trusted, locally built index artifacts. Online base replacement and
delta compaction are not implemented; retain every generation still referenced
by a database or reader.
