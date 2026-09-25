#!/usr/bin/env python3
"""Replace an imported PtrHash base on an offline disposable x86_64 snapshot."""

import argparse
import hashlib
import json
import sqlite3
import subprocess
from pathlib import Path


def rebuild(db_path: Path, builder: Path, expected_keys: int, audit_path: Path) -> None:
    """Verify the historical index, detach a stale base, then build a native one."""
    db_path = db_path.resolve(strict=True)
    builder = builder.resolve(strict=True)
    output = Path(f"{db_path}.ptrhash-base")
    if output.exists() or output.with_suffix(".building").exists():
        raise RuntimeError(f"new generation path already exists: {output}")
    if audit_path.exists():
        raise RuntimeError(f"audit path already exists: {audit_path}")
    value_path = Path(f"{db_path}.values")
    values_before = value_path.stat()
    with value_path.open("rb") as values:
        header = values.read(48)
    if len(header) != 48 or header[:8] != b"CLREXT01":
        raise RuntimeError("missing or invalid Clarity value file")

    # The caller keeps all nodes, validation and benchmark processes stopped.
    with sqlite3.connect(f"file:{db_path}?mode=rw", uri=True, timeout=0) as db:
        db.execute("PRAGMA busy_timeout=0")
        busy, _, _ = db.execute("PRAGMA wal_checkpoint(TRUNCATE)").fetchone()
        if busy:
            raise RuntimeError("source WAL is busy")
        db.execute("BEGIN EXCLUSIVE")
        try:
            format_version, store_id = db.execute(
                "SELECT version,store_id FROM clarity_extent_format WHERE singleton=1"
            ).fetchone()
            if format_version != 1 or bytes(store_id) != header[8:24]:
                raise RuntimeError("Clarity value generation mismatch")
            if db.execute("SELECT EXISTS(SELECT 1 FROM clarity_extent_delta)").fetchone()[0]:
                raise RuntimeError("nonempty delta requires a merge-capable rebuild; source unchanged")
            key_count = db.execute("SELECT count(*) FROM clarity_extent_index").fetchone()[0]
            if key_count != expected_keys:
                raise RuntimeError(f"historical key count {key_count} != expected {expected_keys}")
            marker = db.execute(
                "SELECT path,manifest_sha256 FROM clarity_ptrhash_base WHERE singleton=1"
            ).fetchone()
            if marker is None:
                raise RuntimeError("expected imported PtrHash registration is missing")
            if audit_path.parent:
                audit_path.parent.mkdir(parents=True, exist_ok=True)
            audit_path.write_text(
                json.dumps(
                    {
                        "database": str(db_path),
                        "imported_registration": {"path": marker[0], "manifest_sha256": marker[1]},
                        "historical_keys": key_count,
                        "delta_rows": 0,
                        "value_file_bytes": values_before.st_size,
                        "value_file_mtime_ns": values_before.st_mtime_ns,
                        "store_id_hex": store_id.hex(),
                        "intended_native_generation": str(output),
                    },
                    indent=2,
                )
                + "\n"
            )
            # Existing index still contains every historical key; no value is removed.
            db.execute("DROP TABLE clarity_ptrhash_base")
            db.execute("DROP TABLE clarity_extent_delta")
            db.commit()
        except Exception:
            db.rollback()
            raise

    subprocess.run([str(builder), str(db_path), str(output)], check=True)
    with sqlite3.connect(f"file:{db_path}?mode=ro", uri=True) as db:
        path, recorded_hash = db.execute(
            "SELECT path,manifest_sha256 FROM clarity_ptrhash_base WHERE singleton=1"
        ).fetchone()
        delta_rows = db.execute("SELECT count(*) FROM clarity_extent_delta").fetchone()[0]
    manifest_bytes = (output / "manifest.json").read_bytes()
    manifest = json.loads(manifest_bytes)
    if path != output.name or delta_rows or manifest["count"] != expected_keys:
        raise RuntimeError("native PtrHash registration is incomplete or nonportable")
    if hashlib.sha256(manifest_bytes).hexdigest() != recorded_hash:
        raise RuntimeError("native PtrHash manifest digest mismatch")
    values_after = value_path.stat()
    if (values_before.st_size, values_before.st_mtime_ns) != (
        values_after.st_size,
        values_after.st_mtime_ns,
    ):
        raise RuntimeError("Clarity values changed during PtrHash rebuild")
    print(f"Native PtrHash ready: {manifest['count']:,} keys; relative registration {path}")


def main() -> None:
    """Read required offline input paths and run the guarded rebuild."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--db", type=Path, required=True)
    parser.add_argument("--builder", type=Path, required=True)
    parser.add_argument("--expected-keys", type=int, required=True)
    parser.add_argument("--audit", type=Path, required=True)
    args = parser.parse_args()
    if args.expected_keys <= 0:
        parser.error("--expected-keys must be positive")
    rebuild(args.db, args.builder, args.expected_keys, args.audit)


if __name__ == "__main__":
    main()
