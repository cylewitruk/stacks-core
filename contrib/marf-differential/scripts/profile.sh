#!/usr/bin/env bash
# Scoped instruction profiles of the ablation read loops (callgrind).
#
# One process per (provider build, capacity, key set, phase). Collection is
# switched on only inside the harness's timed-pass function for the phase
# (`--collect-atstart=no --toggle-collect=marf_differential::ablate::timed_<phase>_pass*`),
# so PROGRAM TOTALS in the annotation is exactly the inclusive cost of the
# timed loop(s) of that phase: database construction, key profiling,
# verification and the other phase are excluded. `SETS` restricts the
# process to one key set, `CAPS` to one capacity; REPEAT=1; PASSES warm
# passes (default 5). Instruction counts are deterministic and independent
# of machine load; the wall-clock figures the harness prints under valgrind
# are not measurements.
#
#   scripts/profile.sh [out-dir]      (default contrib/marf-differential/results/r6/profile)
#   PROFILES="name:bin:impl:cap:set:phase ..." overrides the default list.
#
# Outputs per profile <name>: <name>.callgrind.out.xz (raw), <name>.inclusive.txt
# and <name>.self.txt (callgrind_annotate, complete), <name>.harness.log,
# <name>.valgrind.log; MANIFEST (binaries' sha256, valgrind version, exact
# commands). Summary tables: scripts/profile-summary.py <out-dir>.
set -euo pipefail
cd "$(dirname "$0")/../../.."
OUT=${1:-contrib/marf-differential/results/r6/profile}
BIN=target/bench-bin
mkdir -p "$OUT"
export REPEAT=1 PASSES=${PASSES:-5}
SETS_DEFAULT=("patchfree" "patched(nodes<=64)" "patched(nodes<=1024)")
if [ -z "${PROFILES:-}" ]; then
  PROFILES=""
  for set in "${SETS_DEFAULT[@]}"; do
    tag=$(echo "$set" | tr -cd 'a-z0-9')   # patchfree, patchednodes64, patchednodes1024
    for cap in 64 4096; do
      PROFILES+="reference-$cap-$tag-warm:marf-differential-reference:baseline:$cap:$set:warm "
      PROFILES+="lateprobe-$cap-$tag-warm:marf-differential-reference-lateprobe:baseline:$cap:$set:warm "
      PROFILES+="rewrite-$cap-$tag-warm:marf-differential-upstream:rewrite:$cap:$set:warm "
    done
  done
  # cold passes for a subset
  for cap in 64 4096; do
    PROFILES+="reference-$cap-patchednodes64-cold:marf-differential-reference:baseline:$cap:patched(nodes<=64):cold "
    PROFILES+="rewrite-$cap-patchednodes64-cold:marf-differential-upstream:rewrite:$cap:patched(nodes<=64):cold "
  done
  PROFILES+="reference-64-patchednodes1024-cold:marf-differential-reference:baseline:64:patched(nodes<=1024):cold "
  PROFILES+="rewrite-4096-patchednodes1024-cold:marf-differential-upstream:rewrite:4096:patched(nodes<=1024):cold "
fi
{
  echo "date $(date -u +%FT%TZ)"
  echo "tree $(git rev-parse HEAD) ($(git describe --always --dirty))"
  echo "$(valgrind --version); $(rustc -V)"
  echo "REPEAT=1 PASSES=$PASSES"
  for b in marf-differential-reference marf-differential-reference-lateprobe marf-differential-upstream; do
    echo "binary $b sha256 $(sha256sum $BIN/$b | cut -d' ' -f1)"
  done
  cat $BIN/MANIFEST.upstream $BIN/MANIFEST.reference 2>/dev/null | sed 's/^/manifest: /'
} > "$OUT/MANIFEST"
for spec in $PROFILES; do
  IFS=: read -r name bin impl cap set phase <<<"$spec"
  cmd="SETS='$set' CAPS=$cap REPEAT=1 PASSES=$PASSES valgrind --tool=callgrind --collect-atstart=no --toggle-collect='marf_differential::ablate::timed_${phase}_pass*' --compress-strings=no --compress-pos=no --callgrind-out-file=$OUT/$name.callgrind.out $BIN/$bin ablate $impl std"
  echo "== $name $(date -u +%T)"
  echo "profile $name: $cmd" >> "$OUT/MANIFEST"
  SETS="$set" CAPS=$cap valgrind --tool=callgrind --collect-atstart=no \
    --toggle-collect="marf_differential::ablate::timed_${phase}_pass*" \
    --compress-strings=no --compress-pos=no \
    --callgrind-out-file="$OUT/$name.callgrind.out" \
    "$BIN/$bin" ablate "$impl" std > "$OUT/$name.harness.log" 2> "$OUT/$name.valgrind.log"
  grep -v '^INFO' "$OUT/$name.harness.log" > "$OUT/$name.harness.log.tmp" && mv "$OUT/$name.harness.log.tmp" "$OUT/$name.harness.log"
  callgrind_annotate --inclusive=yes "$OUT/$name.callgrind.out" > "$OUT/$name.inclusive.txt"
  callgrind_annotate --inclusive=no "$OUT/$name.callgrind.out" > "$OUT/$name.self.txt"
  xz -f -T0 "$OUT/$name.callgrind.out"
done
echo "profiles in $OUT"
