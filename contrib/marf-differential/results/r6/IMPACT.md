# Round 6: impact matrix (written before any rerun)

Reviewed tip: `c1fe612922f6b9ee72650187ab5ea8850f515c51`. Each row names a
harness or control change, the claims it affects, the rows rerun because of
it and the evidence reused unchanged.

| change | what it alters | claims affected | rows rerun | evidence reused as is |
|---|---|---|---|---|
| per-pass verification in `ablate` (`src/ablate.rs`): every warm pass is checked against the model outside its timed region; regression test that an incorrect intermediate pass is detected | validation only; the timed loop body is unchanged | "every returned read is verified" for the ablation rows | the interleaved `std` rows below (they carry the renewed claim) | round-5 ablation rows (`results/r5/`) stay as measured, labelled "final pass verified only" |
| `SETS` filter and `#[inline(never)]` timed-pass wrappers in `ablate` (profiling aids) | none for timing; the wrappers are the functions the profiler toggles on | none | none (used by the profiles) | - |
| repaired late-probe diagnostic (reference overlay, `marf-diag-late-probe`): the SQLite fallback consults the cache as production does; the late probe applies to mapped records only; counters count the extra parse | the diagnostic's semantics and counters | every statement derived from the late-probe timings and counters (PERF.md section 6.1, question 2/3) | late-probe timing and counters, interleaved with the normal reference and the rewrite within each repetition: `std`, capacities 64 and 4,096, 5 repetitions | nothing from the old late-probe runs is cited except as withdrawn |
| scoped profiling (`scripts/profile.sh`, `--toggle-collect` on the timed-pass wrappers, warm and cold separately, one set per run, summary by `scripts/profile-summary.py`) | the instruction-attribution method | PERF.md section 6.2 and the "44% of read instructions" statements | profiles of the reference (64, 4,096), the repaired diagnostic (64, 4,096) and the rewrite (64, 4,096) on patch-free, 64-node and 995-node sets, warm; cold for a subset | the round-5 whole-process warm-loop totals, relabelled as such |
| interpretation corrections (PERF.md, STATUS.md, requirement matrix) | prose only | capacity's contribution, cache equivalence, construction ratios per phase, "no regression", interpreter vs MarfedKV | none | round-5 construction and Clarity samples (`results/r5/construct-*.json`, `clarity-*.json`) |
| Clarity builder provenance (`build-variants.sh` verifies worktree revisions and records them; one build from a fresh clone into a new empty `WORK`) | build evidence | requirement 2c | none (build only) | round-5 Clarity timings |

Not rerun this round: the 1M-key and page-cold matrices, the plain-encoding
control, the upstream rows, construction, Clarity, the hand-off and consumer
suites. Timing binaries are uninstrumented release builds; counters and
profiles come from separate builds; no build or test runs during timing.
