#!/usr/bin/env bash
# Build marf-differential against the optimized reference MARF
# (stacks-marf at 29ef093d) as the oracle, with the rewrite as "next", from
# a script-owned worktree. Nothing outside this script is needed.
#
#   1. a detached git worktree of the reference commit at $WORKTREE
#      (default target/reference-worktree), created if missing, reset to the
#      committed state otherwise, and verified to be at the reference revision;
#   2. the current tree's crates/stacks-marf-next and contrib/marf-differential
#      copied in (directories created) and registered as workspace members;
#   3. the checked-in overlays applied: reference/harness.patch (adapter
#      differences, production options, counters) and reference/reference.patch
#      (feature-gated counters and the late-probe diagnostic in the reference
#      crate); see reference/README.md;
#   4. four binaries built into target/bench-bin/:
#        marf-differential-reference            timing, no instrumentation
#        marf-differential-reference-counters   read counters (the reference's
#                                               marf-read-bench-counters feature)
#        marf-differential-reference-lateprobe  diagnostic control: the reference
#                                               probes its patch cache only after a
#                                               borrowed read says the record is a
#                                               patch (reference/README.md); timing
#        marf-differential-reference-lateprobe-counters  the same with counters
#   5. target/bench-bin/MANIFEST.reference: tree revision, worktree revision,
#      overlay patch hashes, toolchain, and the sha256 of each binary.
#
# With RUN_TESTS=1 (default) the harness's counter sanity tests run in the
# worktree after the counters build.
set -euo pipefail
cd "$(dirname "$0")/../../.."
REF_REV=29ef093da5dd58c1fa484624d94a9d972356c629
WORKTREE=${WORKTREE:-$PWD/target/reference-worktree}
OUT=$PWD/target/bench-bin
TREE=$PWD
mkdir -p "$OUT"
export MARF_BENCH_RUSTC="$(rustc -V)"
export MARF_BENCH_PROVIDER="reference stacks-marf $REF_REV"
export MARF_BENCH_TREE_REV="$(git rev-parse HEAD)$(git diff --quiet HEAD -- crates contrib || echo -dirty)"
echo "tree rev $(git rev-parse HEAD) ($(git describe --always --dirty)); reference $REF_REV; $MARF_BENCH_RUSTC"

# 1. worktree at the reference revision
if [ ! -d "$WORKTREE/.git" ] && [ ! -f "$WORKTREE/.git" ]; then
  mkdir -p "$(dirname "$WORKTREE")"
  git worktree add --detach "$WORKTREE" "$REF_REV"
fi
git -C "$WORKTREE" checkout --detach "$REF_REV" -- 2>/dev/null || git -C "$WORKTREE" checkout --detach "$REF_REV"
git -C "$WORKTREE" reset --hard -q "$REF_REV"
git -C "$WORKTREE" clean -fdq -e target
actual=$(git -C "$WORKTREE" rev-parse HEAD)
[ "$actual" = "$REF_REV" ] || { echo "worktree is at $actual, expected $REF_REV" >&2; exit 1; }

# 2. copy the harness and the rewrite in, register them
mkdir -p "$WORKTREE/crates/stacks-marf-next" "$WORKTREE/contrib/marf-differential"
cp -r "$TREE/crates/stacks-marf-next/src" "$WORKTREE/crates/stacks-marf-next/src"
cp "$TREE/crates/stacks-marf-next/Cargo.toml" "$WORKTREE/crates/stacks-marf-next/Cargo.toml"
cp -r "$TREE/contrib/marf-differential/src" "$WORKTREE/contrib/marf-differential/src"
cp -r "$TREE/contrib/marf-differential/tests" "$WORKTREE/contrib/marf-differential/tests"
cp -r "$TREE/contrib/marf-differential/fixtures" "$WORKTREE/contrib/marf-differential/fixtures"
cp "$TREE/contrib/marf-differential/Cargo.toml" "$WORKTREE/contrib/marf-differential/Cargo.toml"
python3 - "$WORKTREE/Cargo.toml" <<'PY'
import sys
p=sys.argv[1]; s=open(p).read()
for m in ['crates/stacks-marf-next','contrib/marf-differential']:
    if f'"{m}"' not in s:
        s=s.replace('members = [\n', f'members = [\n    "{m}",\n', 1)
open(p,'w').write(s)
PY

# 3. overlays
(cd "$WORKTREE" && patch -p1 --no-backup-if-mismatch < "$TREE/contrib/marf-differential/reference/harness.patch")
(cd "$WORKTREE" && git apply "$TREE/contrib/marf-differential/reference/reference.patch")

# 4. binaries
(cd "$WORKTREE" && cargo build --release -p marf-differential)
cp "$WORKTREE/target/release/marf-differential" "$OUT/marf-differential-reference"
(cd "$WORKTREE" && cargo build --release -p marf-differential --features counters)
cp "$WORKTREE/target/release/marf-differential" "$OUT/marf-differential-reference-counters"
if [ "${RUN_TESTS:-1}" = 1 ]; then
  (cd "$WORKTREE" && cargo test --release -p marf-differential --features counters --test counters 2>&1 | tail -3)
fi
(cd "$WORKTREE" && cargo build --release -p marf-differential --features reference-lateprobe)
cp "$WORKTREE/target/release/marf-differential" "$OUT/marf-differential-reference-lateprobe"
(cd "$WORKTREE" && cargo build --release -p marf-differential --features reference-lateprobe,counters)
cp "$WORKTREE/target/release/marf-differential" "$OUT/marf-differential-reference-lateprobe-counters"

# 5. manifest
{
  echo "built $(date -u +%FT%TZ)"
  echo "tree_rev $(git rev-parse HEAD) $(git describe --always --dirty)"
  echo "reference_rev $actual"
  echo "toolchain $MARF_BENCH_RUSTC"
  for p in harness reference; do
    echo "overlay $p.patch sha256 $(sha256sum "$TREE/contrib/marf-differential/reference/$p.patch" | cut -d' ' -f1)"
  done
  for b in reference reference-counters reference-lateprobe reference-lateprobe-counters; do
    echo "binary marf-differential-$b sha256 $(sha256sum "$OUT/marf-differential-$b" | cut -d' ' -f1)"
  done
} > "$OUT/MANIFEST.reference"
cat "$OUT/MANIFEST.reference"
