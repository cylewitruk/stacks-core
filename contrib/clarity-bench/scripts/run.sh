#!/usr/bin/env bash
# Run the clarity-bench variants built by build-variants.sh, sequentially,
# each twice in alternating order (rewrite, upstream, optmarf; then the
# reverse), REPEAT in-process repetitions each (fresh database per
# repetition). Logs go to $1 (default contrib/clarity-bench/results/<date>).
set -euo pipefail
cd "$(dirname "$0")/../../.."
OUT=${1:-contrib/clarity-bench/results/$(date -u +%Y%m%d-%H%M%S)}
mkdir -p "$OUT"
BIN=target/bench-bin
export REPEAT=${REPEAT:-5}
BLOCKS=${BLOCKS:-10}
{
  echo "date $(date -u +%FT%TZ)"; echo "tree $(git rev-parse HEAD) ($(git describe --always --dirty))"
  echo "$(rustc -V); kernel $(uname -r); cpu $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | xargs); cores $(nproc)"
  echo "REPEAT=$REPEAT BLOCKS=$BLOCKS"; echo "load before: $(cat /proc/loadavg)"
} | tee "$OUT/ENV.txt"
variants="rewrite upstream optmarf"
[ -x "$BIN/clarity-bench-optstack" ] && variants="$variants optstack"
for pass in 1 2; do
  order=$variants; [ $pass = 2 ] && order=$(echo $variants | tr ' ' '\n' | tac | tr '\n' ' ')
  for v in $order; do
    echo "== pass $pass $v"
    "$BIN/clarity-bench-$v" "$BLOCKS" 2>&1 | grep -v ' INFO \| DEBG \| WARN ' > "$OUT/clarity-$v-pass$pass.log"
  done
done
echo "load after: $(cat /proc/loadavg)" >> "$OUT/ENV.txt"
echo "equivalence lines (must agree):"; grep -h "last_root" "$OUT"/clarity-*-pass1.log | sort | uniq -c | sort -rn | head -20
echo "results in $OUT"
