#!/usr/bin/env bash
# Sixth-review bounded rerun: interleaved timing and counter comparisons of
# the normal reference, the repaired late-probe diagnostic and the rewrite
# on the `std` workload at capacities 64 and 4,096 (patch-free, fitting
# patched and wider patched sets are all in `std`). Each repetition is one
# process per provider, the three providers rotated within the repetition,
# so the diagnostic is never systematically measured after the reference.
# Every pass of every cell is verified (ablate's per-pass check).
#
#   scripts/run-r6.sh [out-dir]   (default contrib/marf-differential/results/r6)
#   REPS (default 5), PASSES (default 5). Binaries: target/bench-bin.
set -euo pipefail
cd "$(dirname "$0")/../../.."
OUT=${1:-contrib/marf-differential/results/r6}
mkdir -p "$OUT"
BIN=target/bench-bin
REPS=${REPS:-5}
export PASSES=${PASSES:-5} REPEAT=1 CAPS=64,4096
{
  echo "date $(date -u +%FT%TZ)"
  echo "tree $(git rev-parse HEAD) ($(git describe --always --dirty))"
  echo "$(rustc -V); kernel $(uname -r); cpu $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | xargs); cores $(nproc); mem $(free -g | awk '/Mem/ {print $2}') GB"
  echo "REPS=$REPS PASSES=$PASSES CAPS=$CAPS"
  echo "load before: $(cat /proc/loadavg)"
  echo "manifests:"; cat $BIN/MANIFEST.upstream $BIN/MANIFEST.reference
} | tee "$OUT/ENV.txt"
run() { local name=$1 bin=$2; shift 2; echo "== $name"; JSON_OUT="$OUT/$name.json" "$bin" "$@" 2>&1 | grep -v '^INFO' > "$OUT/$name.log"; }
rotate() { local r=$1; shift; local n=$#; local a=("$@"); local out=(); for ((i=0;i<n;i++)); do out+=("${a[$(( (i+r) % n ))]}"); done; echo "${out[@]}"; }
for r in $(seq 0 $((REPS-1))); do
  for p in $(rotate $r reference lateprobe rewrite); do
    case $p in
      reference) run "ablate-reference-std-rep$r"           $BIN/marf-differential-reference           ablate baseline std ;;
      lateprobe) run "ablate-reference-lateprobe-std-rep$r" $BIN/marf-differential-reference-lateprobe ablate baseline std ;;
      rewrite)   run "ablate-rewrite-std-rep$r"             $BIN/marf-differential-upstream            ablate rewrite  std ;;
    esac
  done
done
# counters: separate binaries, one process each
run "ablate-reference-std-counters"           $BIN/marf-differential-reference-counters           ablate baseline std
run "ablate-reference-lateprobe-std-counters" $BIN/marf-differential-reference-lateprobe-counters ablate baseline std
run "ablate-rewrite-std-counters"             $BIN/marf-differential-upstream-counters            ablate rewrite  std
echo "load after: $(cat /proc/loadavg)" >> "$OUT/ENV.txt"
echo "results in $OUT"
