#!/usr/bin/env bash
# Fifth-review measurement batch: isolated patch-cache comparisons with
# individual samples, alternating the provider order between repetitions
# (each repetition is its own process with REPEAT=1, so the order of
# providers really alternates for the same configuration), plus the counter
# runs, the late-probe diagnostic control, the construction and Clarity runs
# needed for individual samples. Logs and JSON go to $1 (default
# contrib/marf-differential/results/r5). Run on an idle machine.
#
# Binaries: target/bench-bin from scripts/build-rewrite.sh, build-reference.sh
# and ../../clarity-bench/scripts/build-variants.sh. Reps: REPS (default 5).
# BIG=1 adds the 1M-key workload (rewrite and reference only).
set -euo pipefail
cd "$(dirname "$0")/../../.."
OUT=${1:-contrib/marf-differential/results/r5}
mkdir -p "$OUT"
BIN=target/bench-bin
REPS=${REPS:-5}
export PASSES=${PASSES:-5} REPEAT=1
{
  echo "date $(date -u +%FT%TZ)"
  echo "tree $(git rev-parse HEAD) ($(git describe --always --dirty))"
  echo "$(rustc -V); $(cargo -V); kernel $(uname -r); cpu $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | xargs); cores $(nproc); mem $(free -g | awk '/Mem/ {print $2}') GB"
  echo "REPS=$REPS PASSES=$PASSES BIG=${BIG:-0}"
  echo "load before: $(cat /proc/loadavg)"
  echo "manifests:"; cat $BIN/MANIFEST.* 2>/dev/null
} | tee "$OUT/ENV.txt"
# run <tag> <binary> <args...>: tag names the provider build in file names
run() {
  local name=$1 bin=$2; shift 2
  echo "== $name"
  JSON_OUT="$OUT/$name.json" "$bin" "$@" 2>&1 | grep -v '^INFO' > "$OUT/$name.log"
}
# provider order for repetition r (rotates): a b c | b c a | c a b ...
rotate() { local r=$1; shift; local n=$#; local a=("$@"); local out=(); for ((i=0;i<n;i++)); do out+=("${a[$(( (i+r) % n ))]}"); done; echo "${out[@]}"; }

# 1. isolated ablation, std workload: rewrite and reference at 0/64/4096/prod, upstream at prod only
for r in $(seq 0 $((REPS-1))); do
  for p in $(rotate $r rewrite reference upstream); do
    case $p in
      rewrite)   CAPS=0,64,4096,prod run "ablate-rewrite-std-rep$r"   $BIN/marf-differential-upstream  ablate rewrite  std ;;
      reference) CAPS=0,64,4096,prod run "ablate-reference-std-rep$r" $BIN/marf-differential-reference ablate baseline std ;;
      upstream)  CAPS=prod           run "ablate-upstream-std-rep$r"  $BIN/marf-differential-upstream  ablate baseline std ;;
    esac
  done
done
# 2. controls: plain encoding (no patches anywhere), production settings
for r in $(seq 0 $((REPS-1))); do
  for p in $(rotate $r rewrite reference upstream); do
    case $p in
      rewrite)   CAPS=prod run "ablate-rewrite-plain-rep$r"   $BIN/marf-differential-upstream  ablate rewrite  std 5000 plain ;;
      reference) CAPS=prod run "ablate-reference-plain-rep$r" $BIN/marf-differential-reference ablate baseline std 5000 plain ;;
      upstream)  CAPS=prod run "ablate-upstream-plain-rep$r"  $BIN/marf-differential-upstream  ablate baseline std 5000 plain ;;
    esac
  done
done
# 3. page-cold (strict): rewrite and reference, production settings, 3 reps
for r in 0 1 2; do
  for p in $(rotate $r rewrite reference); do
    case $p in
      rewrite)   DROP_CACHES=1 CAPS=prod run "ablate-rewrite-pagecold-rep$r"   $BIN/marf-differential-upstream  ablate rewrite  std ;;
      reference) DROP_CACHES=1 CAPS=prod run "ablate-reference-pagecold-rep$r" $BIN/marf-differential-reference ablate baseline std ;;
    esac
  done
done
# 4. the late-probe diagnostic control (reference only), same cells as 1
for r in $(seq 0 $((REPS-1))); do
  CAPS=0,64,4096,prod run "ablate-reference-lateprobe-std-rep$r" $BIN/marf-differential-reference-lateprobe ablate baseline std
done
# 5. counters (separate binaries), one repetition, isolated and shared-history
CAPS=0,64,4096,prod run "ablate-rewrite-std-counters"   $BIN/marf-differential-upstream-counters  ablate rewrite  std
CAPS=0,64,4096,prod run "ablate-reference-std-counters" $BIN/marf-differential-reference-counters ablate baseline std
CAPS=0,64,4096,prod run "ablate-reference-lateprobe-std-counters" $BIN/marf-differential-reference-lateprobe-counters ablate baseline std 2>/dev/null || echo "(no lateprobe counters binary)"
ABLATE_MODE=shared CAPS=0,64,4096,prod run "ablate-rewrite-std-shared-counters"   $BIN/marf-differential-upstream-counters  ablate rewrite  std
ABLATE_MODE=shared CAPS=0,64,4096,prod run "ablate-reference-std-shared-counters" $BIN/marf-differential-reference-counters ablate baseline std
# 6. the big workload (1M keys), rewrite and reference, 3 reps, 0/64/4096
if [ "${BIG:-0}" = 1 ]; then
  for r in 0 1 2; do
    for p in $(rotate $r rewrite reference); do
      case $p in
        rewrite)   CAPS=0,64,4096 run "ablate-rewrite-big-rep$r"   $BIN/marf-differential-upstream  ablate rewrite  big ;;
        reference) CAPS=0,64,4096 run "ablate-reference-big-rep$r" $BIN/marf-differential-reference ablate baseline big ;;
      esac
    done
  done
  CAPS=0,64,4096 run "ablate-rewrite-big-counters"   $BIN/marf-differential-upstream-counters  ablate rewrite  big
  CAPS=0,64,4096 run "ablate-reference-big-counters" $BIN/marf-differential-reference-counters ablate baseline big
fi
# 7. construction: four configurations, alternating, individual samples
for r in $(seq 0 $((REPS-1))); do
  for p in $(rotate $r rewrite reference-rc2048 reference-rc0 upstream); do
    case $p in
      rewrite)         run "construct-rewrite-rep$r"        $BIN/marf-differential-upstream  construct rewrite ;;
      reference-rc2048) run "construct-reference-rc2048-rep$r" $BIN/marf-differential-reference construct baseline result-cache 2048 ;;
      reference-rc0)   run "construct-reference-rc0-rep$r"  $BIN/marf-differential-reference construct baseline result-cache off ;;
      upstream)        run "construct-upstream-rep$r"       $BIN/marf-differential-upstream  construct baseline ;;
    esac
  done
done
run "construct-rewrite-counters"          $BIN/marf-differential-upstream-counters  construct rewrite
run "construct-reference-rc2048-counters" $BIN/marf-differential-reference-counters construct baseline result-cache 2048
run "construct-reference-rc0-counters"    $BIN/marf-differential-reference-counters construct baseline result-cache off
# 8. Clarity: three variants, alternating, individual samples
for r in $(seq 0 $((REPS-1))); do
  for v in $(rotate $r rewrite upstream optmarf); do
    echo "== clarity-$v-rep$r"
    JSON_OUT="$OUT/clarity-$v-rep$r.json" $BIN/clarity-bench-$v 10 2>&1 | grep -v ' INFO \| DEBG \| WARN ' > "$OUT/clarity-$v-rep$r.log"
  done
done
echo "load after: $(cat /proc/loadavg)" >> "$OUT/ENV.txt"
python3 contrib/marf-differential/scripts/tables.py check "$OUT" | tee "$OUT/CHECK.txt"
echo "results in $OUT"
