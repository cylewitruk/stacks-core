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
echo "rev $(git rev-parse HEAD) ($(git describe --always --dirty)), $MARF_BENCH_RUSTC"
cargo build --release -p marf-differential
cp target/release/marf-differential "$OUT/marf-differential-upstream"
cargo build --release -p marf-differential --features counters
cp target/release/marf-differential "$OUT/marf-differential-upstream-counters"
cargo build --release -p marf-differential --features alloc-count
cp target/release/marf-differential "$OUT/marf-differential-upstream-alloc"
# leave the plain binary in place
cargo build --release -p marf-differential
ls -la "$OUT"
