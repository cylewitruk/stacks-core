#!/usr/bin/env python3
"""Compile explicit traffic recipes and audit complete stacks-bench blocks.

Standard library only. Exported history is reference data, never a claim that
re-signed historical calls will succeed against a different block context.
"""

import argparse
import copy
import hashlib
import json
import math
import sqlite3
from collections import Counter
from itertools import chain, repeat
from pathlib import Path


def require(condition, message):
    """Reject malformed inputs even when Python assertions are disabled."""
    if not condition:
        raise ValueError(message)


def digest(value):
    """Fingerprint JSON independently of indentation and object-key ordering."""
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def write_new(path, value, compact=False):
    """Preserve prior attempts instead of overwriting an existing artifact."""
    with Path(path).open("x") as stream:
        json.dump(value, stream, indent=None if compact else 2, sort_keys=True,
                  separators=(",", ":") if compact else None)
        stream.write("\n")


def export_corpus(database, start, end):
    """Export ordered user-call payloads and the full-window transaction mix."""
    require(0 < start <= end, "invalid historical range")
    uri = Path(database).resolve().as_uri() + "?mode=ro&immutable=1"
    with sqlite3.connect(uri, uri=True) as db:
        # Immutable reads require a completed database with no pending WAL.
        rows = db.execute("""
            SELECT b.height, lower(hex(b.index_hash)), lower(hex(t.tx_hash)),
                   p.address, c.name, f.name, caller.address, t.contract_call_args_json
            FROM stacks_tx t JOIN stacks_block b ON b.id=t.stacks_block_id
            LEFT JOIN contract c ON c.id=t.contract_id
            LEFT JOIN principal p ON p.id=c.issuer_principal_id
            LEFT JOIN contract_fn f ON f.id=t.contract_fn_id
            LEFT JOIN principal caller ON caller.id=t.caller_principal_id
            WHERE b.height BETWEEN ? AND ? ORDER BY b.height, t.id
        """, (start, end)).fetchall()
    require(rows, "empty reference window")
    calls, mix = [], Counter()
    for height, block, txid, issuer, contract, function, caller, raw in rows:
        if function is None:
            mix["non-contract-call"] += 1
            continue
        require(issuer and contract and raw, "incomplete contract-call reference")
        identifier = issuer + "." + contract
        mix[identifier + "::" + function] += 1
        calls.append({"height": height, "block_id": block, "txid": txid,
                      "caller": caller, "contract": identifier,
                      "function": function, "args": json.loads(raw)})
    return {"schema": 1, "start_height": start, "end_height": end,
            "transactions": len(rows), "mix": dict(sorted(mix.items())),
            "calls": calls, "calls_sha256": digest(calls),
            "limitations": "Calls only; non-call count includes protocol transactions. "
                            "Database transaction ID orders calls within a height. "
                            "Historical application success is not classified by this export. "
                            "Use payloads as shape references, not context-free replay."}


def compile_recipe(recipe, start, end, step=10, repeats=1, warmup=2):
    """Expand a weighted operation cycle into state-evolving physical blocks."""
    expected = {"schema", "name", "description", "source_block", "seed_txid",
                "accounts", "preparation", "templates"}
    require(set(recipe) == expected and recipe["schema"] == 1, "invalid recipe schema/fields")
    require(all(type(x) is int for x in (start, end, step, repeats, warmup)), "integer ramp required")
    require(0 < start <= end <= 16384 and step > 0 and (end-start) % step == 0,
            "ramp must reach end exactly, with 1..16384 transactions per block")
    require(1 <= repeats <= 1000 and 0 <= warmup <= 1000, "invalid repeats/warmup")
    count = ((end-start)//step+1)*repeats + warmup + len(recipe["preparation"])
    require(count <= 4096, "too many blocks")
    require(type(recipe["accounts"]) is int and 1 <= recipe["accounts"] <= 64, "invalid account count")
    templates = recipe["templates"]
    require(0 < len(templates) <= 10000, "invalid template count")
    for item in templates:
        require(set(item) == {"weight", "operation", "transformations"}, "invalid template fields")
        require(type(item["weight"]) is int and 0 < item["weight"] <= 1_000_000, "invalid weight")
        require(isinstance(item["transformations"], str) and item["transformations"].strip(),
                "describe template transformations (or explicitly state none)")
    for operations in recipe["preparation"] + [[x["operation"] for x in templates]]:
        require(isinstance(operations, list) and 0 < len(operations) <= 16384, "invalid operation list")
        for op in operations:
            require(set(op) <= {"account", "class", "reference_txid", "action"}
                    and {"account", "class", "action"} <= set(op), "invalid operation fields")
            require(type(op["account"]) is int and 0 <= op["account"] < recipe["accounts"], "invalid sender")
            require(isinstance(op["class"], str) and op["class"].strip(), "missing workload class")
            require(op["action"].get("kind") in ("call", "transfer"), "unsupported action")
    weights = [x["weight"] for x in templates]
    total_weight = sum(weights)
    balances = [0]*len(templates)
    actions, action_indices = [], {}

    def compact_operation(operation):
        """Store each literal action once, retaining per-transaction identity fields."""
        key = digest(operation["action"])
        if key not in action_indices:
            action_indices[key] = len(actions)
            actions.append(copy.deepcopy(operation["action"]))
        return {**{k: v for k, v in operation.items() if k != "action"},
                "action": {"kind": "template", "index": action_indices[key]}}

    preparation = [[compact_operation(op) for op in ops] for ops in recipe["preparation"]]
    template_operations = [compact_operation(item["operation"]) for item in templates]
    require(len(actions) <= 16384, "too many action templates")

    def take(n):
        """Smooth weighted round-robin; state continues across block boundaries."""
        result = []
        for _ in range(n):
            for i, weight in enumerate(weights):
                balances[i] += weight
            selected = max(range(len(weights)), key=lambda i: balances[i])
            balances[selected] -= total_weight
            result.append(copy.deepcopy(template_operations[selected]))
        return result

    doses = list(range(start, end+1, step))
    total = sum(map(len, recipe["preparation"])) + warmup*start + repeats*sum(doses)
    require(total <= 2_000_000, "too many transactions")
    batches = [{"phase": "preparation", "transactions": ops} for ops in preparation]
    batches += [{"phase": "warmup", "transactions": take(start)} for _ in range(warmup)]
    batches += [{"phase": "measured", "transactions": take(dose)}
                for dose in doses for _ in range(repeats)]
    manifest = {key: copy.deepcopy(recipe[key]) for key in
                ("schema", "name", "description", "source_block", "seed_txid", "accounts")}
    manifest["description"] += " Recipe SHA256: " + digest(recipe) + ". Transformations: " + "; ".join(
        f"template {i}: {item['transformations']}" for i, item in enumerate(templates))
    manifest["batches"] = batches
    manifest["templates"] = actions
    return manifest


def literal_action(manifest, operation):
    """Resolve exactly one bounded, nonrecursive manifest action reference."""
    action = operation["action"]
    if action["kind"] == "template":
        index = action["index"]
        require(type(index) is int and 0 <= index < len(manifest.get("templates", [])), "invalid template index")
        action = manifest["templates"][index]
    require(action["kind"] in ("call", "transfer"), "nonliteral action template")
    return action


def plan_digest(manifest):
    """Fingerprint execution inputs equally for inline and shared-action encodings."""
    bank_hashes = [digest(action) for action in manifest.get("templates", [])]
    batches = []
    for batch in manifest["batches"]:
        operations = []
        for op in batch["transactions"]:
            literal = literal_action(manifest, op)
            action_hash = bank_hashes[op["action"]["index"]] if op["action"]["kind"] == "template" else digest(literal)
            operations.append({"account": op["account"], "class": op["class"],
                               "reference_txid": op.get("reference_txid"), "action_sha256": action_hash})
        batches.append({"phase": batch["phase"], "transactions": operations})
    return digest({**{key: manifest[key] for key in ("schema", "source_block", "seed_txid", "accounts")},
                   "batches": batches})


def audit(manifest_path, log_path):
    """Require complete receipts, exact batch shapes and normal root continuity."""
    raw = Path(manifest_path).read_bytes()
    manifest = json.loads(raw)
    tags = {k: [] for k in ("TRAFFIC_MANIFEST", "TRAFFIC_BLOCK", "TRAFFIC_COMPLETE", "CAPACITY_RECEIPTS")}
    observer_transactions = 0
    with Path(log_path).open() as log:
        for line in log:
            tag, separator, payload = line.partition(" ")
            if separator and tag in tags:
                tags[tag].append(json.loads(payload))
            elif tag == "READ_HISTORY":
                observation = json.loads(payload)
                require(observation["snapshot"]["overflow"] == 0, "truncated read observer")
                observer_transactions += 1
    require(len(tags["TRAFFIC_MANIFEST"]) == len(tags["TRAFFIC_COMPLETE"]) == 1,
            "missing/duplicate manifest or completion: run is not a throughput result")
    header = tags["TRAFFIC_MANIFEST"][0]
    require(header["sha256"] == hashlib.sha256(raw).hexdigest(), "manifest changed")
    batches, blocks, receipts = manifest["batches"], tags["TRAFFIC_BLOCK"], tags["CAPACITY_RECEIPTS"]
    require(len(batches) == len(blocks) == len(receipts), "incomplete or split blocks")
    cost_keys = {"read_count", "read_length", "write_count", "write_length", "runtime"}
    offset, measured, previous, semantic, txids = 0, 0, None, [], []
    for index, (batch, block, receipt) in enumerate(zip(batches, blocks, receipts)):
        n = len(batch["transactions"])
        require(block["index"] == receipt["index"] == index, "block ordering mismatch")
        require(block["phase"] == batch["phase"], "phase mismatch")
        require(block["transactions"] == n and block["tx_start"] == offset
                and block["tx_end"] == offset+n, "batch transaction coverage mismatch")
        require(block["mix"] == dict(Counter(x["class"] for x in batch["transactions"])), "mix changed")
        # Each component is truncated to microseconds separately; their Duration
        # sum is truncated once, so the legitimate residual is zero to two.
        residual = block["node_work_us"]-block["setup_us"]-block["execution_us"]-block["seal_commit_us"]
        require(0 <= residual <= 2,
                "whole-block timing partition mismatch")
        require(receipt["stop_reason"] == "candidate-exhausted" and len(receipt["transactions"]) == n,
                "failed/split transaction batch")
        require(set(block["cost"]) == cost_keys, "missing block cost dimensions")
        audited = []
        for item, operation in zip(receipt["transactions"], batch["transactions"]):
            require(item["application_success"] is True, "aborted application transaction")
            require(item["contract_call"] == (literal_action(manifest, operation)["kind"] == "call"), "payload class changed")
            require(set(item["cost"]) == cost_keys, "missing transaction cost dimensions")
            require(len(bytes.fromhex(item["receipt_sha256"])) == 32, "invalid receipt fingerprint")
            txids.append(item["txid"])
            audited.append({key: item[key] for key in ("txid", "receipt_sha256", "cost", "application_success")})
        if previous:
            require(block["parent_id"] == previous["block_id"] and block["previous_state_root"] == previous["state_root"]
                    and block["height"] == previous["height"]+1, "state/height discontinuity")
        else:
            require(block["previous_state_root"] is None, "unexpected initial root link")
        semantic.append({"block": {k: v for k, v in block.items() if not k.endswith("_us")}, "receipts": audited})
        offset += n
        measured += n if batch["phase"] == "measured" else 0
        previous = block
    require(len(set(txids)) == len(txids), "duplicate transaction identity")
    require(hashlib.sha256("".join(txids).encode()).hexdigest() == header["txid_sha256"], "signed stream changed")
    require(tags["TRAFFIC_COMPLETE"][0] == {"blocks": len(batches), "transactions": offset,
            "measured_transactions": measured}, "completion totals mismatch")
    return {"schema": 1, "passed": True, "manifest_sha256": header["sha256"],
            "execution_plan_sha256": plan_digest(manifest),
            "semantic_sha256": digest(semantic), "blocks": len(blocks), "transactions": offset,
            "measured_transactions": measured, "read_observer_transactions": observer_transactions,
            "measured_blocks": [b for b in blocks if b["phase"] == "measured"]}


def cadence(audit_result, multiplier):
    """Summarize measured whole blocks against a five-second scenario budget."""
    require(math.isfinite(multiplier) and multiplier > 0, "invalid scenario multiplier")
    grouped = {}
    for block in audit_result["measured_blocks"]:
        grouped.setdefault(block["transactions"], []).append(block["node_work_us"]*multiplier/1e6)
    result = []
    for n, times in sorted(grouped.items()):
        ordered = sorted(times)
        result.append({"transactions_per_block": n, "offered_transactions_per_second": n/5,
                       "samples": len(times), "serial_work_multiplier": multiplier,
                       "mean_seconds": sum(times)/len(times), "max_seconds": max(times),
                       "p95_seconds": ordered[math.ceil(.95*len(times))-1],
                       "deadline_misses": sum(t > 5 for t in times),
                       "all_blocks_within_five_seconds": max(times) <= 5})
    return result


def read_profile(manifest_path, log_path, header_database):
    """Resolve persisted Clarity read ages and report per-phase/function breadth."""
    audit(manifest_path, log_path)
    manifest = json.loads(Path(manifest_path).read_text())
    observations, txids, heights, blocks = {}, [], {}, []
    with Path(log_path).open() as log:
        for line in log:
            tag, _, payload = line.partition(" ")
            if tag not in ("READ_HISTORY", "CAPACITY_RECEIPTS", "TRAFFIC_BLOCK"):
                continue
            item = json.loads(payload)
            if tag == "READ_HISTORY" and item["capacity"]:
                require(item["txid"] not in observations, "duplicate read profile")
                observations[item["txid"]] = item
            elif tag == "CAPACITY_RECEIPTS":
                txids.extend(t["txid"] for t in item["transactions"])
            elif tag == "TRAFFIC_BLOCK":
                blocks.append(item)
                heights[item["block_id"]] = item["height"]
                heights[item["parent_id"]] = item["height"]-1
    require(set(observations) == set(txids), "missing/extra generated read profiles")
    wanted = {row["block"] for obs in observations.values() for row in obs["snapshot"]["rows"]
              if row["clarity"]}
    wanted.update(h for obs in observations.values() for h, _ in obs["snapshot"]["at_blocks"])
    unresolved = sorted(wanted-set(heights))
    uri = Path(header_database).resolve().as_uri() + "?mode=ro&immutable=1"
    with sqlite3.connect(uri, uri=True) as db:
        for offset in range(0, len(unresolved), 400):
            chunk = unresolved[offset:offset+400]
            marks = ",".join("?" for _ in chunk)
            for table in ("block_headers", "nakamoto_block_headers"):
                for block, height in db.execute(
                        f"SELECT index_block_hash,block_height FROM {table} WHERE index_block_hash IN ({marks})", chunk):
                    require(block not in heights or heights[block] == height, "conflicting origin height")
                    heights[block] = height
    require(wanted <= set(heights), f"unresolved Clarity origin hashes: {sorted(wanted-set(heights))[:10]}")
    groups, rows = {}, []
    ops = [(batch["phase"], op) for batch in manifest["batches"] for op in batch["transactions"]]
    parents = chain.from_iterable(repeat(b, b["transactions"]) for b in blocks)
    for txid, (phase, op), block in zip(txids, ops, parents):
        obs = observations[txid]
        require(obs["parent_block"] == block["parent_id"] and obs["parent_height"] == block["height"]-1
                and obs["source_block"] == manifest["source_block"], "observer context mismatch")
        reads = [r for r in obs["snapshot"]["rows"] if r["clarity"] and r["operation"] == 1]
        require(all(heights[r["block"]] <= obs["parent_height"] for r in reads), "future Clarity read origin")
        require(all(heights[h] <= obs["parent_height"] for h, _ in obs["snapshot"]["at_blocks"]), "future at-block target")
        old = [r for r in reads if obs["parent_height"]-heights[r["block"]] > 100000]
        row = {"txid": txid, "phase": phase, "class": op["class"], "account": op["account"],
               "persisted_read_requests": sum(r["requests"] for r in reads),
               "unique_nodes_per_transaction": sum(r["unique_nodes"] for r in reads),
               "requests_from_origins_older_than_100k_blocks": sum(r["requests"] for r in old),
               "explicit_at_block_calls": sum(n for _, n in obs["snapshot"]["at_blocks"])}
        rows.append(row)
        groups.setdefault(phase+"/"+op["class"], []).append(row)
    summary = {}
    for key, items in groups.items():
        metrics = {}
        for field in ("persisted_read_requests", "unique_nodes_per_transaction",
                      "requests_from_origins_older_than_100k_blocks", "explicit_at_block_calls"):
            values = sorted(x[field] for x in items)
            metrics[field] = {"mean": sum(values)/len(values), "p50": values[(len(values)-1)//2],
                              "p95": values[math.ceil(.95*len(values))-1], "max": max(values)}
        summary[key] = {"transactions": len(items), **metrics}
    return {"schema": 1, "groups": summary, "transactions": rows,
            "resolved_origin_hashes": len(wanted), "unknown_or_future_clarity_origins": 0,
            "scope": "Persisted Clarity node-decode requests, not physical reads/cache misses. "
                     "Unique nodes are per transaction, not a global footprint. Ages describe "
                     "inherited trie nodes; explicit at-block calls are separate. Observer timings excluded."}


def main():
    """Expose reference export, deterministic compilation, and strict auditing."""
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    export = commands.add_parser("export-corpus")
    export.add_argument("database"); export.add_argument("output")
    export.add_argument("--start", type=int, required=True); export.add_argument("--end", type=int, required=True)
    compile_cmd = commands.add_parser("compile")
    compile_cmd.add_argument("recipe"); compile_cmd.add_argument("output")
    compile_cmd.add_argument("--start", type=int, required=True); compile_cmd.add_argument("--end", type=int, required=True)
    compile_cmd.add_argument("--step", type=int, default=10); compile_cmd.add_argument("--repeats", type=int, default=1)
    compile_cmd.add_argument("--warmup", type=int, default=2)
    check = commands.add_parser("audit")
    check.add_argument("manifest"); check.add_argument("log"); check.add_argument("output")
    check.add_argument("--compare", help="Audit JSON from the identical manifest on another build")
    check.add_argument("--performance-qualified", action="store_true", help="Explicitly attest binary has no read observer; emit scenario estimates")
    profile = commands.add_parser("read-profile")
    profile.add_argument("manifest"); profile.add_argument("log")
    profile.add_argument("header_database", help="Read-only completed source chainstate/vm/index.sqlite")
    profile.add_argument("output")
    args = parser.parse_args()
    if args.command == "export-corpus":
        result = export_corpus(args.database, args.start, args.end)
    elif args.command == "compile":
        result = compile_recipe(json.loads(Path(args.recipe).read_text()), args.start, args.end,
                                args.step, args.repeats, args.warmup)
    elif args.command == "read-profile":
        result = read_profile(args.manifest, args.log, args.header_database)
    else:
        result = audit(args.manifest, args.log)
        if args.compare:
            reference = json.loads(Path(args.compare).read_text())
            require(reference["passed"] and result["execution_plan_sha256"] == reference["execution_plan_sha256"]
                    and result["semantic_sha256"] == reference["semantic_sha256"], "paired semantics mismatch")
            result["paired_semantics_match"] = True
        if args.performance_qualified:
            require(result["read_observer_transactions"] == 0, "read-observer timings are not performance evidence")
            result["cadence_scenarios"] = {str(x): cadence(result, x) for x in (2.5, 3.5)}
            result["performance_qualification"] = "Operator attestation; illustrative serial-work budget scenarios, not measured network capacity"
    write_new(args.output, result, compact=args.command == "compile")


if __name__ == "__main__":
    main()
