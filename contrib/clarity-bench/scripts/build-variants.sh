#!/usr/bin/env bash
# Build clarity-bench against three MARF providers with the same consumer
# code (stackslib's ClarityInstance / MarfedKV), into target/bench-bin/:
#
#   clarity-bench-rewrite    this tree: stackslib (upstream ad5c5778 plus
#                            the rename and one call-site change of this
#                            branch) over stacks-marf-next
#   clarity-bench-upstream   worktree of upstream ad5c5778 as committed,
#                            this crate copied in: stackslib over upstream
#                            stacks-marf
#   clarity-bench-optmarf    worktree of upstream ad5c5778 with its
#                            stacks-marf replaced by the optimized crate from
#                            29ef093d (plus its crates/stacks-mmap and the two
#                            workspace dependency entries it needs), three
#                            `MarfCore` trait imports added where upstream's
#                            stackslib calls methods the optimized crate moved
#                            to that trait, this crate copied in with the
#                            optimized-marf feature (production options:
#                            mmap, root cache off, 64-entry patch cache,
#                            2,048-entry result cache)
#   clarity-bench-optstack   (optional, OPTSTACK=1) worktree of 29ef093d as
#                            committed with this crate copied in: the whole
#                            optimized stack, stackslib changes included;
#                            reported separately, may not compile if that
#                            branch's consumer API differs
#
# Worktrees live under $WORK (default target/clarity-variants). CLEAN=1
# removes each worktree's build directory after its binary is copied (a
# stackslib release build is several GB).
set -euo pipefail
cd "$(dirname "$0")/../../.."
UP_REV=ad5c57786bb51b58d4bdb4992ad534f762d8cf03
REF_REV=29ef093da5dd58c1fa484624d94a9d972356c629
WORK=${WORK:-$PWD/target/clarity-variants}
OUT=$PWD/target/bench-bin
mkdir -p "$OUT" "$WORK"
export MARF_BENCH_RUSTC="$(rustc -V)"
export MARF_BENCH_TREE_REV="$(git rev-parse HEAD)$(git diff --quiet HEAD -- crates contrib stackslib || echo -dirty)"
echo "tree $MARF_BENCH_TREE_REV ($(git describe --always --dirty)); $MARF_BENCH_RUSTC"

add_member() { # <worktree>: register contrib/clarity-bench in the workspace
  python3 - "$1" <<'PY'
import sys
p=sys.argv[1]+'/Cargo.toml'; s=open(p).read()
if '"contrib/clarity-bench"' not in s:
    s=s.replace('members = [\n','members = [\n    "contrib/clarity-bench",\n',1); open(p,'w').write(s)
PY
}
copy_crate() { # <worktree> <marf dep line>
  rm -rf "$1/contrib/clarity-bench"
  mkdir -p "$1/contrib"
  cp -r contrib/clarity-bench "$1/contrib/clarity-bench"
  rm -rf "$1/contrib/clarity-bench/scripts"
  python3 - "$1/contrib/clarity-bench/Cargo.toml" "$2" <<'PY'
import sys,re
p,dep=sys.argv[1],sys.argv[2]; s=open(p).read()
s=re.sub(r'^stacks-marf = .*$', dep, s, flags=re.M); open(p,'w').write(s)
PY
  add_member "$1"
}
worktree() { # <path> <rev>
  if [ ! -d "$1" ]; then git worktree add --detach "$1" "$2"; fi
  git -C "$1" checkout -- . && git -C "$1" clean -fdq
}

# A: this tree
cargo build --release -p clarity-bench
cp target/release/clarity-bench "$OUT/clarity-bench-rewrite"

# B: upstream as committed
worktree "$WORK/upstream" "$UP_REV"
copy_crate "$WORK/upstream" 'stacks-marf = { path = "../../stacks-marf" }'
(cd "$WORK/upstream" && cargo build --release -p clarity-bench)
cp "$WORK/upstream/target/release/clarity-bench" "$OUT/clarity-bench-upstream"
[ "${CLEAN:-0}" = 1 ] && rm -rf "$WORK/upstream/target"

# C: upstream consumer code over the optimized MARF crate
worktree "$WORK/optmarf" "$UP_REV"
if [ ! -d "$WORK/refsrc" ]; then git worktree add --detach "$WORK/refsrc" "$REF_REV"; fi
rm -rf "$WORK/optmarf/stacks-marf" "$WORK/optmarf/crates/stacks-mmap"
cp -r "$WORK/refsrc/stacks-marf" "$WORK/optmarf/stacks-marf"
mkdir -p "$WORK/optmarf/crates" && cp -r "$WORK/refsrc/crates/stacks-mmap" "$WORK/optmarf/crates/stacks-mmap"
python3 - "$WORK/optmarf" "$WORK/refsrc" <<'PY'
import sys,re
w,r=sys.argv[1],sys.argv[2]
p=w+'/Cargo.toml'; s=open(p).read()
if '"crates/stacks-mmap"' not in s:
    s=s.replace('members = [\n','members = [\n    "crates/stacks-mmap",\n',1)
# workspace dependency entries the optimized crates use that upstream lacks
ref=open(r+'/Cargo.toml').read()
def entry(name):
    m=re.search(r'^'+re.escape(name)+r'\s*=.*$', ref, re.M); return m.group(0)
need=set()
for crate in ['stacks-marf','crates/stacks-mmap']:
    t=open(f'{w}/{crate}/Cargo.toml').read()
    for mm in re.finditer(r'^([A-Za-z0-9_-]+)\s*(?:=\s*\{[^}]*workspace\s*=\s*true|\.workspace\s*=\s*true)', t, re.M):
        need.add(mm.group(1))
block=re.search(r'\[workspace\.dependencies\](.*?)(\n\[|\Z)', s, re.S).group(1)
have={m.group(1) for m in re.finditer(r'^([A-Za-z0-9_-]+)\s*=', block, re.M)}
add='\n'.join(entry(d) for d in sorted(need) if d not in have and re.search(r'^'+re.escape(d)+r'\s*=', ref, re.M))
if add:
    s=s.replace('[workspace.dependencies]\n','[workspace.dependencies]\n'+add+'\n',1)
open(p,'w').write(s)
# the three call sites that need the trait the optimized crate introduced
for f in ['stackslib/src/util_lib/db.rs','stackslib/src/clarity_vm/database/ephemeral.rs']:
    p=f'{w}/{f}'; t=open(p).read()
    t=t.replace('use stacks_marf::marf::{MarfConnection, MarfTransaction, MARF};','use stacks_marf::marf::{MarfConnection, MarfCore, MarfTransaction, MARF};'); open(p,'w').write(t)
p=f'{w}/stackslib/src/clarity_vm/database/marf.rs'; t=open(p).read()
if 'MarfCore' not in t:
    t=t.replace('use stacks_marf::marf::{\n','use stacks_marf::marf::{\n    MarfCore,\n',1); open(p,'w').write(t)
print("optmarf worktree prepared")
PY
copy_crate "$WORK/optmarf" 'stacks-marf = { path = "../../stacks-marf" }'
(cd "$WORK/optmarf" && cargo build --release -p clarity-bench --features optimized-marf)
cp "$WORK/optmarf/target/release/clarity-bench" "$OUT/clarity-bench-optmarf"
(cd "$WORK/optmarf" && git diff --stat -- stackslib Cargo.toml | tail -5)
[ "${CLEAN:-0}" = 1 ] && rm -rf "$WORK/optmarf/target"

# D (optional): the whole optimized stack
if [ "${OPTSTACK:-0}" = 1 ]; then
  worktree "$WORK/optstack" "$REF_REV"
  copy_crate "$WORK/optstack" 'stacks-marf = { path = "../../stacks-marf" }'
  if (cd "$WORK/optstack" && cargo build --release -p clarity-bench --features optimized-marf); then
    cp "$WORK/optstack/target/release/clarity-bench" "$OUT/clarity-bench-optstack"
    [ "${CLEAN:-0}" = 1 ] && rm -rf "$WORK/optstack/target"
  else
    echo "optstack variant did not build (its stackslib API differs); see above" >&2
  fi
fi
{
  echo "built $(date -u +%FT%TZ)"
  echo "tree_rev $MARF_BENCH_TREE_REV $(git describe --always --dirty)"
  echo "upstream_rev $UP_REV"
  echo "reference_rev $REF_REV"
  echo "toolchain $MARF_BENCH_RUSTC"
  for v in rewrite upstream optmarf optstack; do
    [ -x "$OUT/clarity-bench-$v" ] && echo "binary clarity-bench-$v sha256 $(sha256sum "$OUT/clarity-bench-$v" | cut -d' ' -f1)"
  done
  echo "optmarf worktree changes vs $UP_REV:"; (cd "$WORK/optmarf" && git diff --stat -- stackslib Cargo.toml | tail -5)
} > "$OUT/MANIFEST.clarity"
cat "$OUT/MANIFEST.clarity"
ls -la "$OUT"
