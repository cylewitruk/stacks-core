"""Regression tests for finite workload planning and fail-closed log auditing."""

import copy
import hashlib
import json
import sqlite3
import tempfile
import unittest
from pathlib import Path

import traffic


def recipe():
    """Provide a small recipe with preparation and two unequal operation weights."""
    return {"schema": 1, "name": "test", "description": "transfer mechanics, not representative",
            "source_block": "00"*32, "seed_txid": "11"*32, "accounts": 2,
            "preparation": [[{"account": 0, "class": "transfer", "action": {
                "kind": "transfer", "recipient": 1, "amount": 1}}]],
            "templates": [{"weight": weight, "transformations": "generated transfers",
                           "operation": {"account": sender, "class": "transfer",
                                         "action": {"kind": "transfer", "recipient": 1-sender, "amount": 1}}}
                          for weight, sender in [(1, 0), (3, 1)]]}


class TrafficTests(unittest.TestCase):
    """Reject bad plans and incomplete results before capacity reporting."""

    def test_ramp_and_weight_continuity(self):
        """Default ramp is ten; warmup and repetitions advance the same stream."""
        source = recipe()
        result = traffic.compile_recipe(source, 10, 30, repeats=2)
        self.assertEqual([len(b["transactions"]) for b in result["batches"]], [1, 10, 10, 10, 10, 20, 20, 30, 30])
        self.assertEqual(result, traffic.compile_recipe(source, 10, 30, repeats=2))
        ops = [o for b in result["batches"][1:] for o in b["transactions"]]
        self.assertEqual(sum(o["account"] == 0 for o in ops), len(ops)//4)
        self.assertEqual(source, recipe())
        inline = copy.deepcopy(result)
        for batch in inline["batches"]:
            for op in batch["transactions"]:
                op["action"] = copy.deepcopy(traffic.literal_action(result, op))
        inline.pop("templates")
        self.assertEqual(traffic.plan_digest(inline), traffic.plan_digest(result))
        inline["batches"][0]["transactions"][0]["action"]["amount"] += 1
        self.assertNotEqual(traffic.plan_digest(inline), traffic.plan_digest(result))

    def test_reject_unreachable_end_and_unknown_action(self):
        """Neither an omitted final dose nor an unsupported operation is silently dropped."""
        with self.assertRaises(ValueError):
            traffic.compile_recipe(recipe(), 10, 25)
        source = recipe()
        source["templates"][0]["operation"]["action"]["kind"] = "pretend-success"
        with self.assertRaises(ValueError):
            traffic.compile_recipe(source, 10, 20)

    def test_complete_audit_and_failure_rejection(self):
        """All phases need successful receipts; timing excludes preparation/warmup."""
        with tempfile.TemporaryDirectory() as root:
            manifest_path, log_path = Path(root)/"manifest.json", Path(root)/"run.log"
            manifest = traffic.compile_recipe(recipe(), 1, 1, warmup=1)
            manifest_path.write_text(json.dumps(manifest))
            txids = [f"{n:064x}" for n in range(3)]
            records = [("TRAFFIC_MANIFEST", {"sha256": hashlib.sha256(manifest_path.read_bytes()).hexdigest(),
                                           "txid_sha256": hashlib.sha256("".join(txids).encode()).hexdigest()})]
            costs = dict.fromkeys(("read_count", "read_length", "write_count", "write_length", "runtime"), 0)
            for i, batch in enumerate(manifest["batches"]):
                records += [("CAPACITY_RECEIPTS", {"index": i, "stop_reason": "candidate-exhausted", "transactions": [
                    {"application_success": True, "contract_call": False, "cost": costs,
                     "txid": txids[i], "receipt_sha256": "aa"*32}]}),
                    ("TRAFFIC_BLOCK", {"index": i, "phase": batch["phase"], "transactions": 1,
                     "tx_start": i, "tx_end": i+1, "mix": {"transfer": 1}, "node_work_us": 2_000_000,
                     "setup_us": 100_000, "execution_us": 900_000, "seal_commit_us": 1_000_000,
                     "cost": costs, "parent_id": str(i), "block_id": str(i+1), "state_root": str(i),
                     "previous_state_root": str(i-1) if i else None, "height": 100+i})]
            records.append(("TRAFFIC_COMPLETE", {"blocks": 3, "transactions": 3, "measured_transactions": 1}))

            def check(rows):
                """Audit a temporary log without creating report files."""
                log_path.write_text("".join(tag+" "+json.dumps(value)+"\n" for tag, value in rows))
                return traffic.audit(manifest_path, log_path)

            result = check(records)
            self.assertEqual(len(result["measured_blocks"]), 1)
            self.assertTrue(traffic.cadence(result, 2.5)[0]["all_blocks_within_five_seconds"])
            self.assertFalse(traffic.cadence(result, 3.5)[0]["all_blocks_within_five_seconds"])
            with self.assertRaises(ValueError):
                check(records[:-1])
            rounded = copy.deepcopy(records)
            rounded[2][1]["node_work_us"] += 2
            self.assertTrue(check(rounded)["passed"])
            rounded[2][1]["node_work_us"] += 1
            with self.assertRaises(ValueError):
                check(rounded)
            broken = copy.deepcopy(records)
            broken[1][1]["transactions"][0]["application_success"] = False
            with self.assertRaises(ValueError):
                check(broken)
            broken = copy.deepcopy(records)
            broken[4][1]["parent_id"] = "wrong"
            with self.assertRaises(ValueError):
                check(broken)
            broken = copy.deepcopy(records)
            broken[5][1]["transactions"][0]["cost"].pop("runtime")
            with self.assertRaises(ValueError):
                check(broken)
            origin = "ab"*32
            observed = copy.deepcopy(records)
            for i, txid in enumerate(txids):
                observed.append(("READ_HISTORY", {"txid": txid, "capacity": True,
                    "source_block": manifest["source_block"], "parent_block": str(i), "parent_height": 99+i,
                    "snapshot": {"overflow": 0, "at_blocks": [], "rows": [
                        {"block": origin, "clarity": True, "operation": 1, "requests": 2, "unique_nodes": 1}]}}))
            headers = Path(root)/"headers.sqlite"
            with sqlite3.connect(headers) as db:
                for table in ("block_headers", "nakamoto_block_headers"):
                    db.execute(f"CREATE TABLE {table} (index_block_hash TEXT, block_height INTEGER)")
                db.execute("INSERT INTO block_headers VALUES (?, 1)", (origin,))
            check(observed)
            profile = traffic.read_profile(manifest_path, log_path, headers)
            self.assertEqual(profile["resolved_origin_hashes"], 1)
            self.assertEqual(profile["groups"]["measured/transfer"]["persisted_read_requests"]["mean"], 2)
            broken = copy.deepcopy(observed)
            broken[-1][1]["parent_height"] += 1
            check(broken)
            with self.assertRaises(ValueError):
                traffic.read_profile(manifest_path, log_path, headers)
            check(observed)
            with sqlite3.connect(headers) as db:
                db.execute("UPDATE block_headers SET block_height=102")
            with self.assertRaises(ValueError):
                traffic.read_profile(manifest_path, log_path, headers)
            with sqlite3.connect(headers) as db:
                db.execute("DELETE FROM block_headers")
            with self.assertRaises(ValueError):
                traffic.read_profile(manifest_path, log_path, headers)


if __name__ == "__main__":
    unittest.main()
