#!/usr/bin/env python3
"""Generate the report tables from the published JSON samples and check the
cross-implementation correctness records.

    tables.py ablate   <results-dir>   # one table per workload: medians of the isolated cells
    tables.py construct <results-dir>  # per-phase medians per provider + correctness check
    tables.py clarity  <results-dir>   # per-workload medians per variant + equivalence check
    tables.py check    <results-dir>   # correctness/equivalence checks only (exit 1 on mismatch)

A results directory holds the JSON files written by the harnesses
(`JSON_OUT`), one per process run; repetitions spread over several files
(one per provider x repetition, in alternating order) are merged by their
configuration. Only files whose manifest says `counters=false` and
`alloc_count=false` feed the timing tables; counter files feed the counter
tables. Exit status 1 when any correctness record disagrees.
"""
import glob
import json
import os
import statistics
import sys
from collections import defaultdict


def load(d, prefix):
    """Files are named <experiment>-<tag>-<detail>.json by the run scripts;
    <tag> names the provider build (upstream, reference, reference-lateprobe,
    rewrite, optmarf, ...)."""
    out = []
    for f in sorted(glob.glob(os.path.join(d, prefix + "*.json"))):
        with open(f) as fh:
            j = json.load(fh)
        base = os.path.basename(f)[:-5]
        parts = base.split("-")
        tag = parts[1] if len(parts) > 1 else "?"
        if len(parts) > 2 and parts[2] == "lateprobe":
            tag = "reference-lateprobe"
        j["_file"] = base
        j["_tag"] = tag
        out.append(j)
    return out


def med(xs):
    return statistics.median(xs) if xs else float("nan")


def fmt_us(ns_list, ops):
    if not ns_list:
        return "-"
    per = [n / 1000.0 / ops for n in ns_list]
    return f"{med(per):.2f} ({min(per):.2f}-{max(per):.2f}, n={len(per)})"


def ablate(d):
    runs = load(d, "ablate-")
    timing = [r for r in runs if not r["manifest"]["counters"] and not r["manifest"]["alloc_count"]]
    counters = [r for r in runs if r["manifest"]["counters"]]
    # group timing cells by (workload, plain, mode, page_cold, lateprobe, provider-oracle, set, capacity)
    cells = defaultdict(lambda: {"cold": [], "warm": [], "meta": None, "occ": []})
    for r in timing:
        tag = r["_tag"]
        for c in r["cells"]:
            key = (c["workload"], c["plain"], c["mode"], c["page_cold"], tag, c["set"], str(c["capacity"]))
            e = cells[key]
            e["cold"].append(c["cold_ns"] / 1000.0 / c["n_keys"])
            e["warm"].extend(x / 1000.0 / c["n_keys"] for x in c["warm_ns"])
            e["meta"] = c
            e["occ"].append(c["occupancy_exit"])
    groups = defaultdict(dict)
    for key, e in cells.items():
        groups[key[:4]][key[4:]] = e
    for (wl, plain, mode, pc), cs in sorted(groups.items()):
        print(f"\n### workload `{wl}`, {'plain' if plain else 'compressed'}, mode {mode}, {'page-cold' if pc else 'process-cold'}\n")
        tags = sorted({k[0] for k in cs})
        caps = sorted({k[2] for k in cs}, key=lambda x: (x != "None", int(x) if x not in ("None",) else 10**9))
        sets = []
        for k in cs:
            if k[1] not in sets:
                sets.append(k[1])
        hdr = "| set | n keys | distinct patched nodes | " + " | ".join(f"{t} cap {c if c != 'None' else 'prod'} cold / warm µs (occ)" for t in tags for c in caps) + " |"
        print(hdr)
        print("|" + "---|" * (3 + len(tags) * len(caps)))
        for st in sets:
            m = next(e["meta"] for k, e in cs.items() if k[1] == st)
            row = f"| {st} | {m['n_keys']:,} | {m['distinct_patched_nodes']:,} | "
            vals = []
            for t in tags:
                for c in caps:
                    e = cs.get((t, st, c))
                    if not e:
                        vals.append("-")
                        continue
                    occ = e["occ"][0]
                    vals.append(f"{med(e['cold']):.1f} / {med(e['warm']):.1f} ({'n/a' if occ is None else occ}; n={len(e['cold'])})")
            print(row + " | ".join(vals) + " |")
    if counters:
        print("\n### counters (first repetition of each counter run; warm = all warm passes)\n")
        print("| run | set | capacity | probes | hits | plain misses | admissions | resolutions | applications | materializations | occupancy entry/exit |")
        print("|---|---|---|---|---|---|---|---|---|---|---|")
        for r in counters:
            tag = r["_tag"]
            for c in r["cells"]:
                if c["repetition"] != 0:
                    continue
                w = c["counters_warm"]
                def g(k):
                    v = w.get(k)
                    return "n/a" if v is None else f"{v:,}"
                print(f"| {tag} {c['workload']} {c['mode']} | {c['set']} | {c['capacity'] if c['capacity'] is not None else 'prod'} | {g('patch_cache_probes')} | {g('patch_cache_hits')} | {g('patch_cache_probe_miss_plain')} | {g('patch_cache_admissions')} | {g('patch_resolutions')} | {g('patch_applications')} | {g('owned_materializations')} | {c['occupancy_entry']}/{c['occupancy_exit']} |")


def construct(d, check_only=False):
    runs = load(d, "construct-")
    timing = [r for r in runs if not r["manifest"]["counters"] and not r["manifest"]["alloc_count"]]
    ok = True
    # correctness: every run's per-phase digests and roots must agree
    records = defaultdict(set)
    for r in runs:
        tag = f"{r['provider']}/{r['manifest']['oracle']}/rc={r['config']['result_cache']}"
        for p in r["phases"]:
            if p["digest"]:
                records[("digest", p["phase"])].add((p["digest"], tag))
        records[("root", "child")].add((r["root_child"], tag))
        records[("root", "rebuilt")].add((r["root_rebuilt"], tag))
    for key, vals in sorted(records.items()):
        distinct = {v for v, _ in vals}
        if len(distinct) != 1:
            ok = False
            print(f"MISMATCH {key}: {sorted(vals)}")
    print(f"construct correctness records: {len(records)} keys, {'all agree' if ok else 'MISMATCH'} across {len(runs)} runs")
    if check_only:
        return ok
    by = defaultdict(lambda: defaultdict(list))
    labels = {}
    for r in timing:
        tag = r["_tag"]
        if r["config"]["result_cache"] is not None:
            tag += f" rc{r['config']['result_cache']}"
        for p in r["phases"]:
            by[p["phase"]][tag].extend(p["ns"])
            labels[p["phase"]] = (p["kind"], p["ops"])
    tags = sorted({t for ph in by.values() for t in ph})
    print("\n| phase | kind | ops | " + " | ".join(f"{t} µs/op median (min-max, n)" for t in tags) + " |")
    print("|" + "---|" * (3 + len(tags)))
    for ph, per in by.items():
        kind, ops = labels[ph]
        cells = []
        for t in tags:
            ns = per.get(t, [])
            if not ns:
                cells.append("-")
            elif kind == "total" or ph in ("seal", "commit", "begin"):
                ms = [x / 1e6 for x in ns]
                cells.append(f"{med(ms):.1f} ms ({min(ms):.1f}-{max(ms):.1f}, n={len(ms)})")
            else:
                cells.append(fmt_us(ns, ops))
        print(f"| {ph} | {kind} | {ops:,} | " + " | ".join(cells) + " |")
    return ok


def clarity(d, check_only=False):
    runs = load(d, "clarity-")
    ok = True
    records = defaultdict(set)
    for r in runs:
        tag = f"{r['variant'][:40]}/{r['_file']}"
        for name, a in r["workloads"].items():
            c = a["cost"]
            records[("cost", name)].add((json.dumps(c, sort_keys=True), tag))
            records[("roots", name)].add((json.dumps(a["roots"]), tag))
            records[("values", name)].add((a["values_digest"], tag))
    for key, vals in sorted(records.items()):
        distinct = {v for v, _ in vals}
        if len(distinct) != 1:
            ok = False
            print(f"MISMATCH {key}: {len(distinct)} distinct values across {sorted(t for _, t in vals)}")
    print(f"clarity equivalence records: {len(records)} keys, {'all agree' if ok else 'MISMATCH'} across {len(runs)} runs")
    if check_only:
        return ok
    by = defaultdict(lambda: defaultdict(lambda: defaultdict(list)))
    for r in runs:
        tag = r["_tag"]
        for name, a in r["workloads"].items():
            for k in ("rest_exec", "rest_harness", "rest_seal", "rest_commit", "first_exec"):
                by[name][tag][k].extend(x["secs"] * 1e3 + x["nanos"] / 1e6 for x in a[k])
    for name, per in by.items():
        print(f"\n**{name}**\n")
        print("| variant | exec ms (median, min-max, n) | harness ms | seal ms | commit ms | first block exec ms |")
        print("|---|---|---|---|---|---|")
        for tag, ks in sorted(per.items()):
            def f(k):
                xs = ks[k]
                return f"{med(xs):.1f} ({min(xs):.1f}-{max(xs):.1f}, n={len(xs)})" if xs else "-"
            print(f"| {tag} | {f('rest_exec')} | {f('rest_harness')} | {f('rest_seal')} | {f('rest_commit')} | {f('first_exec')} |")
    return ok


def main():
    if len(sys.argv) < 3:
        print(__doc__)
        sys.exit(2)
    cmd, d = sys.argv[1], sys.argv[2]
    ok = True
    if cmd == "ablate":
        ablate(d)
    elif cmd == "construct":
        ok = construct(d)
    elif cmd == "clarity":
        ok = clarity(d)
    elif cmd == "check":
        ok = construct(d, check_only=True) and clarity(d, check_only=True)
    else:
        print(__doc__)
        sys.exit(2)
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
