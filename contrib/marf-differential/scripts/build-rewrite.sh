#!/usr/bin/env bash
# Build the in-tree marf-differential binaries (upstream stacks-marf as the
# oracle, the rewrite as "next") in three flavours:
#   marf-differential-upstream           timing (no instrumentation)
#   marf-differential-upstream-counters  read counters (rewrite; upstream has none)
#   marf-differential-upstream-alloc     counting allocator (allocations, peak heap)
# into target/bench-bin/. Timing runs use the first; the other two are for
# separate counter and allocation runs.
set -euo pipefail
cd "$(dirname "$0")/../../.."
OUT=target/bench-bin
mkdir -p "$OUT"
export MARF_BENCH_RUSTC="$(rustc -V)"
export MARF_BENCH_TREE_REV="$(git rev-parse HEAD)$(git diff --quiet HEAD -- crates contrib || echo -dirty)"
export MARF_BENCH_PROVIDER="upstream stacks-marf ad5c57786bb51b58d4bdb4992ad534f762d8cf03 (in tree)"
echo "rev $MARF_BENCH_TREE_REV ($(git describe --always --dirty)), $MARF_BENCH_RUSTC"
cargo build --release -p marf-differential
cp target/release/marf-differential "$OUT/marf-differential-upstream"
cargo build --release -p marf-differential --features counters
cp target/release/marf-differential "$OUT/marf-differential-upstream-counters"
cargo build --release -p marf-differential --features alloc-count
cp target/release/marf-differential "$OUT/marf-differential-upstream-alloc"
# leave the plain binary in place
cargo build --release -p marf-differential
{
  echo "built $(date -u +%FT%TZ)"
  echo "tree_rev $MARF_BENCH_TREE_REV $(git describe --always --dirty)"
  echo "oracle $MARF_BENCH_PROVIDER"
  echo "toolchain $MARF_BENCH_RUSTC"
  for b in upstream upstream-counters upstream-alloc; do
    echo "binary marf-differential-$b sha256 $(sha256sum "$OUT/marf-differential-$b" | cut -d' ' -f1)"
  done
} > "$OUT/MANIFEST.upstream"
cat "$OUT/MANIFEST.upstream"
