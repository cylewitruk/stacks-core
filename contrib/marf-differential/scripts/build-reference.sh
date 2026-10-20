#!/usr/bin/env bash
# Build marf-differential against the optimized reference MARF
# (stacks-marf at 29ef093d) as the oracle, with the rewrite as "next".
#
# How: a detached git worktree of the reference commit is created at
# $WORKTREE (default target/reference-worktree); the current tree's
# crates/stacks-marf-next and contrib/marf-differential sources are copied
# in; the two checked-in overlay patches are applied:
#   reference/harness.patch            adapter differences (reference API,
#                                      production options, counters)
#   reference/reference-counters.patch result-cache counters added to the
#                                      reference's opt-in read_bench module
# then the harness is built twice:
#   marf-differential-reference           timing
#   marf-differential-reference-counters  with counters (the reference's
#                                         marf-read-bench-counters feature)
# into target/bench-bin/. Nothing in the reference's production code paths
# is changed apart from the gated counter hooks in the second build.
set -euo pipefail
cd "$(dirname "$0")/../../.."
REF_REV=29ef093da5dd58c1fa484624d94a9d972356c629
WORKTREE=${WORKTREE:-$PWD/target/reference-worktree}
OUT=$PWD/target/bench-bin
mkdir -p "$OUT"
export MARF_BENCH_RUSTC="$(rustc -V)"
echo "tree rev $(git rev-parse HEAD) ($(git describe --always --dirty)); reference $REF_REV; $MARF_BENCH_RUSTC"
if [ ! -d "$WORKTREE" ]; then
  git worktree add --detach "$WORKTREE" "$REF_REV"
fi
git -C "$WORKTREE" checkout -- . && git -C "$WORKTREE" clean -fdq -- contrib/marf-differential crates/stacks-marf-next stacks-marf
rm -rf "$WORKTREE/crates/stacks-marf-next/src" "$WORKTREE/contrib/marf-differential/src"
cp -r crates/stacks-marf-next/src "$WORKTREE/crates/stacks-marf-next/src"
cp crates/stacks-marf-next/Cargo.toml "$WORKTREE/crates/stacks-marf-next/Cargo.toml"
cp -r contrib/marf-differential/src "$WORKTREE/contrib/marf-differential/src"
cp contrib/marf-differential/Cargo.toml "$WORKTREE/contrib/marf-differential/Cargo.toml"
(cd "$WORKTREE" && patch -p1 --no-backup-if-mismatch < "$OLDPWD/contrib/marf-differential/reference/harness.patch")
(cd "$WORKTREE" && git apply "$OLDPWD/contrib/marf-differential/reference/reference-counters.patch")
(cd "$WORKTREE" && cargo build --release -p marf-differential)
cp "$WORKTREE/target/release/marf-differential" "$OUT/marf-differential-reference"
(cd "$WORKTREE" && cargo build --release -p marf-differential --features counters)
cp "$WORKTREE/target/release/marf-differential" "$OUT/marf-differential-reference-counters"
ls -la "$OUT"
