#!/usr/bin/env python3
"""Rebuild V5 PtrHash natively from the retained exact membership stream."""

import argparse
import hashlib
import json
import os
import platform
import sqlite3
import struct
import subprocess
import sys
from pathlib import Path


def sha256(path: Path) -> str:
    """Hash a potentially large input without retaining it in memory."""
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(8 * 1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def write_audit(path: Path, audit: dict) -> None:
    """Persist a reviewable checkpoint without truncating the previous record."""
    temporary = path.with_name(path.name + ".tmp")
    temporary.write_text(json.dumps(audit, indent=2, sort_keys=True) + "\n")
    os.replace(temporary, path)


def inspect_inputs(template: Path, clone: Path, pairs: Path, expected_sha: str) -> dict:
    """Validate the clone, registration, exact pair stream and original base."""
    template_db = (template / "chainstate/vm/clarity/marf.sqlite").resolve(strict=True)
    clone_db = (clone / "chainstate/vm/clarity/marf.sqlite").resolve(strict=True)
    if template_db == clone_db or os.path.samefile(template_db, clone_db):
        raise ValueError("database must be a distinct disposable clone")
    template_stat = template_db.stat()
    with sqlite3.connect(f"file:{clone_db}?mode=ro", uri=True) as db:
        registration = db.execute(
            "SELECT path,manifest_sha256 FROM clarity_stable_ptrhash_base WHERE singleton=1"
        ).fetchone()
        if registration is None:
            raise ValueError("missing imported stable PtrHash registration")
        name, digest = registration
        if Path(name).name != name or name in ("", ".", ".."):
            raise ValueError("imported base must have a sibling name")
        version, path, store_id = db.execute(
            "SELECT version,path,store_id FROM clarity_stable_format WHERE singleton=1"
        ).fetchone()
        if version != 1 or Path(path).name != path or len(store_id) != 16:
            raise ValueError("invalid stable-value registration")
        delta_count = db.execute("SELECT COUNT(*) FROM clarity_stable_value_delta").fetchone()[0]
        if delta_count:
            raise ValueError("nonempty delta needs a merge-aware rebuild")
        high = db.execute(
            "SELECT value_id FROM clarity_stable_value_high WHERE singleton=1"
        ).fetchone()[0]
    old_base = clone_db.parent / name
    manifest_path = old_base / "manifest.json"
    manifest_bytes = manifest_path.read_bytes()
    if hashlib.sha256(manifest_bytes).hexdigest() != digest:
        raise ValueError("imported PtrHash manifest checksum mismatch")
    manifest = json.loads(manifest_bytes)
    counts = [shard["count"] for shard in manifest["shards"]]
    if (manifest["version"] != 1 or len(counts) != 256
            or any(not isinstance(count, int) or count < 0 for count in counts)
            or sum(counts) != manifest["count"]
            or manifest["row_count"] != high
            or bytes(manifest["store_id"]) != bytes(store_id)):
        raise ValueError("imported PtrHash manifest disagrees with stable-value database")
    directory = clone_db.parent / path / "value-directory.dat"
    with directory.open("rb") as stream:
        header = stream.read(32)
    if (len(header) != 32 or header[:8] != b"SVIDDIR1"
            or int.from_bytes(header[8:12], "little") != 1
            or int.from_bytes(header[12:16], "little") != 0
            or header[16:] != bytes(store_id)
            or directory.stat().st_size != 32 + 14 * high):
        raise ValueError("stable-value directory identity or row count mismatch")
    if pairs.stat().st_size != manifest["count"] * 44:
        raise ValueError("sorted membership stream length mismatch")
    pair_digest = sha256(pairs)
    if pair_digest != expected_sha:
        raise ValueError("sorted membership stream checksum mismatch")
    return {
        "template_database": str(template_db),
        "template_database_size": template_stat.st_size,
        "template_database_mtime_ns": template_stat.st_mtime_ns,
        "clone_database": str(clone_db),
        "imported_base": str(old_base),
        "imported_manifest_sha256": digest,
        "sorted_pairs": str(pairs),
        "sorted_pairs_sha256": pair_digest,
        "memberships": manifest["count"],
        "value_rows": high,
        "store_id_hex": bytes(store_id).hex(),
        "source_manifest": str(manifest_path),
    }


def seal_native_template(template: Path, clone: Path, base_name: str) -> dict:
    """Bind a rebuilt transfer clone to fresh checksums for normal target preflight."""
    source_manifest_path = template / "PORTABILITY.json"
    if not source_manifest_path.is_file():
        return {"sealed": False}
    source_manifest = json.loads(source_manifest_path.read_text())
    if source_manifest["status"] != "locally-validated-destination-preflight-required":
        raise ValueError("source transfer manifest has not passed local validation")
    lines = []
    total_bytes = 0
    for path in sorted(clone.rglob("*")):
        if path.is_symlink():
            raise ValueError(f"native template contains a symlink: {path}")
        if not path.is_file():
            continue
        relative = path.relative_to(clone).as_posix()
        if relative in ("SHA256SUMS", "PORTABILITY.json"):
            continue
        lines.append(f"{sha256(path)}  {relative}\n")
        total_bytes += path.stat().st_size
    sums = clone / "SHA256SUMS"
    temporary = clone / "SHA256SUMS.tmp"
    temporary.write_text("".join(lines))
    os.replace(temporary, sums)
    source_manifest.update(
        status="target-native-rebuilt-preflight-required",
        origin_portability_sha256=sha256(source_manifest_path),
        ptrhash_base_name=base_name,
        ptrhash_build_architecture={
            "machine": platform.machine(),
            "pointer_bits": struct.calcsize("P") * 8,
            "byte_order": sys.byteorder,
        },
        portable_file_count=len(lines),
        portable_logical_bytes=total_bytes,
        checksum_manifest_sha256=sha256(sums),
        next_gate="target-built V5 inspect and canonical replay parity before benchmarking",
    )
    portability = clone / "PORTABILITY.json"
    temporary = clone / "PORTABILITY.json.tmp"
    temporary.write_text(json.dumps(source_manifest, indent=2) + "\n")
    os.replace(temporary, portability)
    return {"sealed": True, "files": len(lines), "bytes": total_bytes,
            "checksum_manifest_sha256": source_manifest["checksum_manifest_sha256"]}


def rebuild(args: argparse.Namespace) -> None:
    """Detach only a verified clone's old base, then build and audit a new one."""
    template = args.template.resolve(strict=True)
    clone = args.clone.resolve(strict=True)
    pairs = args.pairs.resolve(strict=True)
    builder = args.builder.resolve(strict=True)
    audit_path = args.audit.resolve()
    if not builder.is_file() or not os.access(builder, os.X_OK):
        raise ValueError("builder must be an executable file")
    if (audit_path.is_relative_to(template) or audit_path.is_relative_to(clone)
            or pairs.is_relative_to(template) or pairs.is_relative_to(clone)):
        raise ValueError("audit and membership stream must remain outside both chainstate roots")
    if audit_path.exists() or audit_path.with_name(audit_path.name + ".tmp").exists():
        raise ValueError("audit path already exists")
    inputs = inspect_inputs(template, clone, pairs, args.pairs_sha256)
    clone_db = Path(inputs["clone_database"])
    output = clone_db.parent / "marf.sqlite.stable-ptrhash-native-v1"
    if output.exists() or output.with_suffix(".building").exists():
        raise ValueError("native output path already exists")
    audit_path.parent.mkdir(parents=True, exist_ok=True)
    audit = dict(inputs, status="validated", native_output=str(output))
    write_audit(audit_path, audit)
    try:
        with sqlite3.connect(f"file:{clone_db}?mode=rw", uri=True, timeout=0) as db:
            db.execute("PRAGMA busy_timeout=0")
            db.execute("BEGIN EXCLUSIVE")
            current = db.execute(
                "SELECT path,manifest_sha256 FROM clarity_stable_ptrhash_base WHERE singleton=1"
            ).fetchone()
            if current != (Path(inputs["imported_base"]).name,
                           inputs["imported_manifest_sha256"]):
                raise ValueError("registration changed since input validation")
            if db.execute("SELECT COUNT(*) FROM clarity_stable_value_delta").fetchone()[0]:
                raise ValueError("delta changed since input validation")
            db.execute("DROP TABLE clarity_stable_ptrhash_base")
            db.execute("DROP TABLE clarity_stable_value_delta")
            db.execute("DROP TABLE clarity_stable_value_high")
        audit["status"] = "detached-on-disposable-clone"
        write_audit(audit_path, audit)
        subprocess.run([
            str(builder), str(clone_db), str(output), str(pairs),
            inputs["source_manifest"],
        ], check=True)
        with sqlite3.connect(f"file:{clone_db}?mode=ro", uri=True) as db:
            name, digest = db.execute(
                "SELECT path,manifest_sha256 FROM clarity_stable_ptrhash_base WHERE singleton=1"
            ).fetchone()
            high = db.execute(
                "SELECT value_id FROM clarity_stable_value_high WHERE singleton=1"
            ).fetchone()[0]
            delta_count = db.execute("SELECT COUNT(*) FROM clarity_stable_value_delta").fetchone()[0]
        native_manifest = (output / "manifest.json").read_bytes()
        parsed = json.loads(native_manifest)
        if (name != output.name or digest != hashlib.sha256(native_manifest).hexdigest()
                or parsed["count"] != inputs["memberships"]
                or parsed["row_count"] != high or high != inputs["value_rows"]
                or bytes(parsed["store_id"]).hex() != inputs["store_id_hex"]
                or delta_count):
            raise ValueError("native base publication audit failed")
        source = Path(inputs["template_database"]).stat()
        if (source.st_size, source.st_mtime_ns) != (
            inputs["template_database_size"], inputs["template_database_mtime_ns"]
        ):
            raise ValueError("immutable template database changed")
        seal = seal_native_template(template, clone, output.name)
        audit.update(status="native-base-published", native_manifest_sha256=digest,
                     native_template=seal)
        write_audit(audit_path, audit)
    except Exception as error:
        audit.update(status="failed-clone-must-be-discarded", error=str(error))
        write_audit(audit_path, audit)
        raise


def main() -> None:
    """Read guarded rebuild arguments for one offline disposable destination clone."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--template", type=Path, required=True)
    parser.add_argument("--clone", type=Path, required=True)
    parser.add_argument("--pairs", type=Path, required=True)
    parser.add_argument("--pairs-sha256", required=True)
    parser.add_argument("--builder", type=Path, required=True)
    parser.add_argument("--audit", type=Path, required=True)
    args = parser.parse_args()
    if len(args.pairs_sha256) != 64 or any(c not in "0123456789abcdef" for c in args.pairs_sha256):
        parser.error("--pairs-sha256 must be a lowercase SHA-256 digest")
    rebuild(args)


if __name__ == "__main__":
    main()
