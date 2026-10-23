#!/usr/bin/env python3
"""Enforce MARF production dependency boundaries for default and all features."""
import json
import pathlib
import subprocess

ROOT = pathlib.Path(__file__).resolve().parents[2]
PACKAGES = ("marf-core", "marf-proofs", "marf-pruning")
STACKS = {"stacks-common", "clarity", "clarity-types", "stacks-profiler", "stacks-marf-next"}


def cargo(*args):
    return subprocess.check_output(["cargo", "--locked", *args], cwd=ROOT, text=True)


for package in PACKAGES:
    forbidden = STACKS | (set(PACKAGES) - {package, "marf-core"})
    for features in ([], ["--all-features"]):
        lines = cargo("tree", "-p", package, "-e", "normal,build", "--prefix", "none", "--format", "{p}", *features)
        names = {line.split()[0] for line in lines.splitlines() if line.strip()}
        assert not names & forbidden, (package, features, sorted(names & forbidden))
        print(package, "all features" if features else "default", "production graph clean")
metadata = json.loads(cargo("metadata", "--format-version", "1", "--no-deps"))
for package in metadata["packages"]:
    if package["name"] not in {"marf-proofs", "marf-pruning"}:
        continue
    for dep in package["dependencies"]:
        if dep["kind"] != "dev":
            assert dep["name"] not in {"rusqlite", "libsqlite3-sys", "memmap2"}, (package["name"], dep)
    for source in pathlib.Path(package["manifest_path"]).parent.joinpath("src").rglob("*.rs"):
        assert not any(word in source.read_text() for word in ("rusqlite::", "libsqlite3_sys::", "sqlite_conn")), source
print("Proof/pruning source and direct dependencies contain no native SQL boundary")
