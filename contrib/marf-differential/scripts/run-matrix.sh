#!/usr/bin/env bash
# Run the committed-read, ablation and block-construction experiments on the
# binaries built by build-rewrite.sh and build-reference.sh, sequentially,
# writing one log per run into $1 (default results/<date>). Run it on an
# idle machine. Timing binaries carry no instrumentation; counter runs use
# the *-counters binaries and allocation runs the *-alloc binary, each in
# its own log. Implementation order alternates between sections.
#
#   REPEAT  timed repetitions per run (default 5; in-process, fresh
#           database each, implementation order alternating where a binary
#           runs both)
#   PASSES  warm passes per key set in `ablate` (default 5)
#   BIG=1   also run the `big` ablation workload (1M keys; minutes per run)
set -euo pipefail
cd "$(dirname "$0")/../../.."
OUT=${1:-contrib/marf-differential/results/$(date -u +%Y%m%d-%H%M%S)}
mkdir -p "$OUT"
BIN=target/bench-bin
export REPEAT=${REPEAT:-5} PASSES=${PASSES:-5}
{
  echo "date $(date -u +%FT%TZ)"
  echo "tree $(git rev-parse HEAD) ($(git describe --always --dirty))"
  echo "upstream oracle: stacks-marf at ad5c57786bb51b58d4bdb4992ad534f762d8cf03 (in tree)"
  echo "reference oracle: stacks-marf at 29ef093da5dd58c1fa484624d94a9d972356c629 (worktree + overlay)"
  echo "$(rustc -V); $(cargo -V)"
  echo "kernel $(uname -r); cpu $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | xargs); cores $(nproc); mem $(free -g | awk '/Mem/ {print $2}') GB"
  echo "REPEAT=$REPEAT PASSES=$PASSES"
  echo "load before: $(cat /proc/loadavg)"
} | tee "$OUT/ENV.txt"
run() { local name=$1; shift; echo "== $name"; "$@" 2>&1 | grep -v ' INFO ' > "$OUT/$name.log"; }
# committed reads, three storage configurations (both implementations per binary, alternating in-process)
for cfg in "external compressed" "external plain" "sqlite plain"; do
  tag=$(echo "$cfg" | tr ' ' '-')
  run "alloc-upstream-$tag"  $BIN/marf-differential-upstream  alloc $cfg
  run "alloc-reference-$tag" $BIN/marf-differential-reference alloc $cfg
done
# allocation and peak-heap figures (separate binary; counting allocator)
run "alloc-counts-upstream-external-compressed" $BIN/marf-differential-upstream-alloc alloc external compressed
# patch-cache ablation, timing: order rewrite, reference, upstream, then reversed
run "ablate-std-rewrite"   $BIN/marf-differential-upstream  ablate rewrite  std
run "ablate-std-reference" $BIN/marf-differential-reference ablate baseline std
run "ablate-std-upstream"  $BIN/marf-differential-upstream  ablate baseline std
run "ablate-std-plain-upstream"  $BIN/marf-differential-upstream  ablate baseline std 5000 plain
run "ablate-std-plain-reference" $BIN/marf-differential-reference ablate baseline std 5000 plain
run "ablate-std-plain-rewrite"   $BIN/marf-differential-upstream  ablate rewrite  std 5000 plain
# page-cold variant (drops the OS page cache before every fresh open; root)
DROP_CACHES=1 run "ablate-std-pagecold-rewrite"   $BIN/marf-differential-upstream  ablate rewrite  std
DROP_CACHES=1 run "ablate-std-pagecold-reference" $BIN/marf-differential-reference ablate baseline std
# counters (separate binaries)
run "ablate-std-counters-rewrite"   $BIN/marf-differential-upstream-counters  ablate rewrite  std
run "ablate-std-counters-reference" $BIN/marf-differential-reference-counters ablate baseline std
if [ "${BIG:-0}" = 1 ]; then
  run "ablate-big-reference" $BIN/marf-differential-reference ablate baseline big
  run "ablate-big-rewrite"   $BIN/marf-differential-upstream  ablate rewrite  big
  run "ablate-big-counters-rewrite"   $BIN/marf-differential-upstream-counters  ablate rewrite  big
  run "ablate-big-counters-reference" $BIN/marf-differential-reference-counters ablate baseline big
fi
# reads during block construction: reference with its result cache on and off
run "construct-rewrite"           $BIN/marf-differential-upstream  construct rewrite
run "construct-reference-rc2048"  $BIN/marf-differential-reference construct baseline result-cache 2048
run "construct-reference-rc0"     $BIN/marf-differential-reference construct baseline result-cache off
run "construct-upstream"          $BIN/marf-differential-upstream  construct baseline
run "construct-counters-rewrite"          $BIN/marf-differential-upstream-counters  construct rewrite
run "construct-counters-reference-rc2048" $BIN/marf-differential-reference-counters construct baseline result-cache 2048
run "construct-counters-reference-rc0"    $BIN/marf-differential-reference-counters construct baseline result-cache off
echo "load after: $(cat /proc/loadavg)" >> "$OUT/ENV.txt"
echo "results in $OUT"
