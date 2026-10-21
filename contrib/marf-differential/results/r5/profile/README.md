# Bounded instruction profiles (callgrind) of the isolated ablation cells (round 5; scope caveat)

**Scope caveat (sixth review).** These profiles collected instructions over
the *whole process* (database construction, key profiling, the cold pass of
every cell, the warm passes, verification). The "timed reads" column below
is the inclusive cost of the warm-pass closure, but the per-function figures
(`SharedLru::get`, `read_patched_persisted_node`, ...) are whole-process
inclusive totals, so the percentages this README and the round-5 report
derived by dividing one by the other mixed two scopes and are withdrawn.
What remains valid: the whole-process totals, the warm-loop totals as
*totals* (the same closure in every run, so comparable across runs), and the
ordering of the self costs. Correctly scoped profiles (collection restricted
to the timed function of one phase, one key set per process) are in
`../../r6/profile/`. The late-probe binary profiled here is the round-5
diagnostic, since repaired (see `../../../reference/README.md`).


Method: `valgrind-3.22.0 --tool=callgrind` over one whole `ablate ... std`
process per configuration (`REPEAT=1 PASSES=5`, isolated cells, the eight
`std` key sets, one capacity per process via `CAPS`), on the timing
binaries of the fresh builds at tree revision `5420f1c1`/`b553f53d`
(identical sources for these crates):

| profile | binary | sha256 | CAPS |
|---|---|---|---|
| reference-cap64 / reference-cap4096 | marf-differential-reference (stacks-marf 29ef093d, production probe order) | `b46858c41a70f800f4cdb90ea0bdd815f2372a6ceae4a548cfa8f7827dd950b2` | 64 / 4096 |
| lateprobe-cap64 / lateprobe-cap4096 | marf-differential-reference-lateprobe (diagnostic: probe only after the record is known to be a patch) | `a88e60bccbc0450122ecf1915462cdee1a6bffecd9ba502711df29f6cd85e68c` | 64 / 4096 |
| rewrite-cap4096 / rewrite-cap64 | marf-differential-upstream, `ablate rewrite` | `04aa92a5cc1169eb0998ae80d3e102d414b251ae2705101cae54069333efb14f` | 4096 / 64 |

Instruction counts (Ir) are deterministic and independent of machine load;
the wall-clock figures the harness printed under valgrind
(`*.harness.log`) are not measurements and must not be read as such.
`*.inclusive-top200.txt` and `*.self-top200.txt` are the first 200 lines of
`callgrind_annotate --inclusive=yes|no` (full outputs are 4 MB each and
were not published; the raw `callgrind.out` files were not kept).

"timed reads" below is the inclusive cost of the harness's timed closure
(`measure::<(), ablate::pass_no_counters<..>::{closure#0}>`), i.e. every
`get_value` of every cell, cold and warm passes; the rest of each program
is building the 40k-key database (hashing dominates it) and verification.

| profile | program total Ir | warm-loop closure Ir (all cells of the run) | `SharedLru::get` incl., whole process | `SharedLru::put_if_current` incl., whole process | `read_patched_persisted_node` incl., whole process | rewrite `resolve_in` incl., whole process |
|---|---|---|---|---|---|---|
| reference-cap64 | 16,529,382,966 | 7,680,822,747 | 705,739,848 | 176,027,446 | 1,975,110,388 | - |
| reference-cap4096 | 17,667,251,456 | 8,802,081,394 | 3,894,730,295 | (below 200 lines) | 209,750,522 | - |
| lateprobe-cap64 | 16,305,102,994 | 7,513,160,107 | 211,511,526 | 170,751,952 | 2,090,302,571 | - |
| lateprobe-cap4096 | 15,255,240,126 | 6,667,914,818 | 1,089,586,142 | (below 200 lines) | 392,086,527 | - |
| rewrite-cap4096 | 9,344,605,118 | 2,555,615,965 | - | - | - | 1,015,108,441 |
| rewrite-cap64 | 12,015,681,784 | 4,803,772,331 | - | - | - | 3,687,074,224 |

Self cost inside `SharedLru::get` at 4096 (reference-cap4096): slice
comparison of the `(StacksBlockId, u64)` keys 1,643,804,208 Ir, the
`rposition` iteration 1,131,148,328 Ir, pointer handling 366,262,928 Ir:
the linear scan of `SmallLru` (`entries.iter().rposition(..)`, then
`rotate_left` on a hit) over the retained entries, performed for every
record read, patch or plain. The rewrite's hashed map shows as SipHash
(`DefaultHasher::write`, 149,490,078 Ir self at 4096).
