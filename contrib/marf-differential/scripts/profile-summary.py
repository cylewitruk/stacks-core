#!/usr/bin/env python3
"""Summarize the scoped callgrind profiles written by scripts/profile.sh.

    profile-summary.py <profile-dir>

For each <name>.inclusive.txt (callgrind_annotate --inclusive=yes over a
collection restricted to one timed-pass function) the PROGRAM TOTALS line is
the inclusive instruction count (Ir) of the timed loop(s) of that phase and
nothing else; the per-lookup figure divides it by the number of lookups the
harness log reports (set size x passes for warm, set size for cold).
Function rows are inclusive costs of the named functions as called from
inside the loop. Inclusive costs of nested functions overlap (a callee's
cost is inside its caller's) and must not be added; the shares are each
function's own fraction of the same denominator. Self rows are exclusive
costs of the top entries of <name>.self.txt.
"""
import glob
import os
import re
import sys

FUNS = [
    ("SharedLru::get", r"cache\.rs:<stacks_marf::cache::SharedLru<.*>>::get "),
    ("SharedLru::put_if_current", r"cache\.rs:<stacks_marf::cache::SharedLru<.*>>::put_if_current "),
    ("read_patched_persisted_node", r"storage\.rs:stacks_marf::storage::read_patched_persisted_node"),
    ("read_node_with_state", r"storage\.rs:<stacks_marf::storage::TrieStorageConnection<.*>>::read_node_with_state"),
    ("TrieCursor::walk_read", r"node\.rs:<stacks_marf::node::TrieCursor<.*>>::walk_read"),
    ("TrieNode256::load_from_parts", r"node\.rs:<stacks_marf::node::TrieNode256 as .*>::load_from_parts"),
    ("ptrs_from_parts_into", r"bits\.rs:stacks_marf::bits::ptrs_from_parts_into"),
    ("TriePtr::from_slice_compressed", r"node\.rs:<stacks_marf::node::TriePtr>::from_slice_compressed"),
    ("ReadTrieNode::decoded_from_bytes", r"lib\.rs:<stacks_marf::ReadTrieNode>::decoded_from_bytes"),
    ("NodeRecord::decode_node", r"record\.rs:<stacks_marf::record::NodeRecord>::decode_node"),
    ("MarfReadState::apply_patches", r"<stacks_marf::scratch::MarfReadState>::apply_patches"),
    ("get_block_identifier (SQLite)", r"trie_sql\.rs:stacks_marf::trie_sql::get_block_identifier"),
    ("get_block_id_caching_impl", r"storage\.rs:stacks_marf::storage::get_block_id_caching_impl"),
    ("rusqlite Rows::advance", r"row\.rs:<rusqlite::row::Rows as .*>::advance"),
    ("sqlite3_step", r"sqlite3\.c:sqlite3_step "),
    ("memcpy", r"__memcpy_avx_unaligned_erms"),
    # rewrite
    ("rewrite resolve_in", r"stacks_marf_next::storage::resolve_in"),
    ("rewrite BranchView::skip_scan", r"codec\.rs:<stacks_marf_next::codec::BranchView>::skip_scan"),
    ("rewrite BranchView::fill_with_end", r"codec\.rs:<stacks_marf_next::codec::BranchView>::fill_with_end "),
    ("rewrite apply_patch", r"stacks_marf_next::codec::apply_patch"),
    ("rewrite read_node / get", r"stacks_marf_next::storage::.*>::read_node"),
    ("SipHash (DefaultHasher::write)", r"sip\.rs:<std::hash::random::DefaultHasher as core::hash::Hasher>::write"),
]


def parse_total(text):
    m = re.search(r"^\s*([\d,]+)\s+\(100\.0%\)\s+PROGRAM TOTALS", text, re.M)
    return int(m.group(1).replace(",", "")) if m else None


def fun_cost(text, pat):
    """Inclusive cost of the first top-level entry matching pat (the entry
    with the binary in brackets; file-split sub-entries are skipped)."""
    best = None
    for line in text.splitlines():
        m = re.match(r"^\s*([\d,]+)\s+\(\s*[\d.]+%\)\s+(.*)$", line)
        if not m:
            continue
        rest = m.group(2)
        if "[" not in rest:
            continue  # a file-split line, not the function's total
        if re.search(pat, rest):
            v = int(m.group(1).replace(",", ""))
            if best is None or v > best:
                best = v
    return best


def lookups(harness_log, phase):
    """set size (one SETS-filtered set) and passes from the harness log."""
    text = open(harness_log).read()
    n = None
    for line in text.splitlines():
        m = re.match(r"^\s+(\S.*?)\s+n=(\d+)\s+distinct patched nodes", line)
        if m:
            n = int(m.group(2))
    passes = int(os.environ.get("PASSES", "5"))
    m = re.search(r"REPEAT=1 PASSES=(\d+)", open(os.path.join(os.path.dirname(harness_log), "MANIFEST")).read())
    if m:
        passes = int(m.group(1))
    return n, (passes if phase == "warm" else 1)


def self_top(name_dir, name, k=8):
    rows = []
    for line in open(os.path.join(name_dir, name + ".self.txt")).read().splitlines():
        m = re.match(r"^\s*([\d,]+)\s+\(\s*([\d.]+)%\)\s+(\S.*)$", line)
        if not m or "PROGRAM TOTALS" in line:
            continue
        where = re.sub(r"\[.*$", "", m.group(3)).strip()
        where = re.sub(r"^.*/(target/reference-worktree/|crates/|stacks-marf/|stacks-marf-next/)", r"\1", where)
        where = re.sub(r"/root/\.cargo/registry/src/[^/]+/", "", where)
        where = re.sub(r"/rustc/[0-9a-f]+/library/", "", where)
        where = where.replace("<stacks_common::types::chainstate::StacksBlockId>", "")
        rows.append((int(m.group(1).replace(",", "")), float(m.group(2)), where[:110]))
        if len(rows) >= k:
            break
    return rows


def main():
    d = sys.argv[1]
    names = sorted(os.path.basename(f)[: -len(".inclusive.txt")] for f in glob.glob(os.path.join(d, "*.inclusive.txt")))
    rows = {}
    for name in names:
        text = open(os.path.join(d, name + ".inclusive.txt")).read()
        total = parse_total(text)
        phase = "cold" if name.endswith("-cold") else "warm"
        n, passes = lookups(os.path.join(d, name + ".harness.log"), phase)
        rows[name] = (total, n, passes, {label: fun_cost(text, pat) for label, pat in FUNS})
    print("## Timed-loop totals (collection restricted to the phase's timed-pass function)\n")
    print("| profile | phase | lookups (n x passes) | Ir, timed loop | Ir per lookup |")
    print("|---|---|---|---|---|")
    for name in names:
        total, n, passes, _ = rows[name]
        per = total / (n * passes) if total and n else float("nan")
        print(f"| {name} | {'cold' if name.endswith('-cold') else 'warm'} | {n:,} x {passes} | {total:,} | {per:,.0f} |")
    print("\n## Inclusive cost of selected functions inside the timed loop (share of the same total; nested costs overlap and are not additive)\n")
    labels = [l for l, _ in FUNS if any(rows[nm][3][l] for nm in names)]
    print("| profile | " + " | ".join(labels) + " |")
    print("|---|" + "---|" * len(labels))
    for name in names:
        total, _, _, fc = rows[name]
        cells = []
        for l in labels:
            v = fc[l]
            cells.append("-" if not v else f"{v:,} ({100.0 * v / total:.1f}%)")
        print(f"| {name} | " + " | ".join(cells) + " |")
    print("\n## Top self (exclusive) costs inside the timed loop\n")
    for name in names:
        print(f"**{name}**\n")
        print("| Ir (self) | share | function (file) |")
        print("|---|---|---|")
        for ir, pct, where in self_top(d, name):
            print(f"| {ir:,} | {pct:.1f}% | `{where}` |")
        print()


if __name__ == "__main__":
    main()
