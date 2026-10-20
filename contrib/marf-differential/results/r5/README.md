# Round-5 results (corrected runs)

Produced by `scripts/run-r5.sh` (`BIG=1 REPS=5`) on the binaries of
`scripts/build-rewrite.sh`, `scripts/build-reference.sh` and
`../../clarity-bench/scripts/build-variants.sh`; `ENV.txt` records revision,
toolchain, machine, load before and after, and the three binary manifests
(sha256 of every binary and of the reference overlay patches). Every run
writes a human-readable `.log` and a machine-readable `.json` with the
manifest (revisions, oracle, binary sha256, features, arguments), the
configuration, the seeds and every individual sample; file names are
`<experiment>-<provider build>-<detail>-rep<N>.json`, one process per
repetition so the provider order rotates per repetition (`rep0`: rewrite,
reference, upstream; `rep1`: reference, upstream, rewrite; ...). Provider
builds: `rewrite` and `upstream` come from `marf-differential-upstream`
(the in-tree harness, upstream oracle), `reference` from
`marf-differential-reference` (overlay build), `reference-lateprobe` from the
diagnostic build; `-counters` files from the counter binaries never feed a
timing table. `CHECK.txt` is the output of `scripts/tables.py check` over
this directory (cross-provider digests, roots and costs). Tables in
`docs/marf-rewrite/PERF.md` are generated from these files by
`scripts/tables.py ablate|construct|clarity`; `TABLES-*.md` are that
script's complete outputs over this directory (including every counter
table), committed for reading without running it. The Clarity runs
(`clarity-*.json/.log`) are in this directory too, written by the same
batch script.

Subdirectories:

* `evidence/`: build logs (cargo's per-crate `Compiling` lines removed,
  nothing else): the reproduced failure of the round-4 `build-reference.sh`
  from a fresh worktree at `e96c041a` (the review's finding: the copy
  targeted a directory the reference revision does not have), the fresh
  script-owned worktree build at `5420f1c1`, the repeat invocation and the
  rewrite build at `d6fd444c`.
* `profile/`: bounded callgrind profiles of the isolated ablation cells
  (its README has the method and the extracted instruction counts).
* `validation/`: nextest logs at `b553f53d` (`REV.txt` has the exit codes).

Revision note: commits between `b553f53d` and `d6fd444c` add results files
only; the manifests print `git rev-parse HEAD` at build start and `git
describe` at the end, and one such commit landed between them during the
rewrite build, which is why `MANIFEST.upstream` names both.

The round-4 originals stay in `../r4/` unchanged (its README names their
defects); see `../r3/README.md` for why neither earlier round has
individual samples.
