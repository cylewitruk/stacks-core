# Round-5 results (corrected runs)

Produced by `scripts/run-r5.sh` on the binaries of `scripts/build-rewrite.sh`,
`scripts/build-reference.sh` and `../clarity-bench/scripts/build-variants.sh`;
`ENV.txt` records revision, toolchain, machine, load and the binary
manifests. Every run writes a human-readable `.log` and a machine-readable
`.json` with the manifest (revisions, oracle, binary sha256, features,
arguments), the configuration and every individual sample; file names are
`<experiment>-<provider build>-<detail>-rep<N>.json`, one process per
repetition so the provider order alternates per repetition. Tables in
`docs/marf-rewrite/PERF.md` are generated from these files by
`scripts/tables.py`, which also fails on any cross-provider mismatch of the
correctness records (digests of every read, roots, costs).

`evidence/`: the reproduced failure of the round-4 `build-reference.sh` from
a fresh worktree at `e96c041a` (the review's finding; the copy targeted a
directory the reference revision does not have), kept as it happened.

The round-4 originals stay in `../r4/` unchanged; see `../r3/README.md` for
why neither round had individual samples.
