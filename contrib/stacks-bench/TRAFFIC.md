# Explicit stateful traffic

The manifest mode executes a finite queue of signed transactions against a
disposable copy of a real chainstate. Each manifest batch becomes one synthetic
Clarity block with ordinary reads, writes, sealing and commit. State and account
nonces continue between batches. It does not replay historical transactions out
of context or supply values from a trace.

## Workload definition

A schema-1 manifest contains `name`, `description`, `source_block`, `seed_txid`,
`accounts` (1–64), and `batches`. Each batch has a `phase` and an ordered
`transactions` array. Phases must occur in this order:

- `preparation`: signed calls that acquire assets or create liquidity positions.
- `warmup`: state-evolving work excluded from throughput samples.
- `measured`: complete blocks used for timing.

Each operation specifies a signing `account` index, a workload `class`, an
optional historical `reference_txid`, and an `action`:

```json
{
  "account": 0,
  "class": "native-transfer",
  "reference_txid": null,
  "action": {"kind": "transfer", "recipient": 1, "amount": 1000}
}
```

A call uses `{"kind":"call","contract":"ADDRESS.name","function":"name",
"args":[...]}`. Arguments use the same serialized Clarity `Value` representation
as `stacks_tx.contract_call_args_json`, including full list/tuple type signatures
and 128-bit integers. The harness does not truncate lists, replace storage reads,
or constrain calls to one pool or function. Account indices identify deterministic
benchmark-owned keys; they never authorize historical accounts.

An optional top-level `templates` array stores literal call/transfer actions
once. An operation can use `{"kind":"template","index":0}` as its action.
References cannot be recursive. The compiler uses this compact form to avoid
repeating large argument lists in every JSON transaction. Each reference still
produces an ordinary, separately signed transaction with its own nonce.

Before preparation, the existing funding fixture moves STX and USDCX from the
selected historical seed sender to initially empty benchmark wallets. This is an
explicit, supply-preserving database fixture outside measured work, not a signed
transfer. All later asset acquisition, LP creation and withdrawals execute
contract code. Include sufficient fees, balances and LP shares for the full
finite stream. Every generated transaction must succeed; insufficient balances,
contract errors and partial batches invalidate the attempt.

`source_block` and `seed_txid` bind the workload to the selected historical seed
context. Different traffic amounts and block boundaries will produce different
roots from history. Compare those synthetic roots and receipts between upstream
and optimized builds executing the **identical transaction plan**, rather than comparing
them to a historical block's root.

## Build a gradual ramp

`scripts/traffic.py` uses Python's standard library. A recipe contains the same
identity/description/account fields as a manifest, plus:

- `preparation`: an array of transaction arrays, one per preparation block.
- `templates`: entries containing an integer `weight`, an `operation`, and a
  `transformations` description (including amount, account, bin or deadline edits).

```sh
python3 scripts/traffic.py compile recipe.json manifest.json \
  --start 10 --end 200 --step 10 --repeats 5 --warmup 2
```

The default increment is **10 transactions per block**. `--repeats` repeats each
load level; `--warmup` adds initial blocks at the starting load. A deterministic
weighted scheduler continues through all warmup and measured blocks. It does not
reset accounts or state at each load level. For independent repetitions, run the
same manifest on fresh shadows, preferably in upstream/optimized/optimized/upstream
order. A ramp alone confounds increasing load with state growth; confirm boundary
loads with fixed-dose repetitions on fresh shadows.

The compiler records the recipe fingerprint and transformations in the manifest.
Both compiler and auditor refuse to overwrite existing output files. Preserve
failed attempts under their original names.

## Run

Use the normal `stacks-bench bench run` command with a **disposable shadow**, a
matching source and selected seed block, and these environment settings:

```sh
export STACKS_CAPACITY_FACTOR=1
export STACKS_CAPACITY_END_HEIGHT=<seed-block-height>
export STACKS_CAPACITY_GROWTH=1
export STACKS_GROWTH_SEED=<manifest-seed-txid>
export STACKS_CONTINUOUS_MIX=1
export STACKS_RELAXED_COSTS=1
export STACKS_MIX_MANIFEST=/absolute/path/manifest.json
```

The selected historical prefix runs before funding. This mode inherits the
continuous harness's synthetic block context and relaxed, still-metered cost/byte
allowances. It measures execution capacity, not production consensus validity or
tenure limits. Without `STACKS_MIX_MANIFEST`, the existing generator is unchanged.

`TRAFFIC_MANIFEST` records the input hash and signed transaction-stream hash.
`TRAFFIC_BLOCK` records phase, count, mix, root/parent continuity, five-dimensional
costs and whole-block node work: **setup + transaction execution + seal/commit**.
Receipt/root auditing and explicit SQLite checkpoints are recorded separately.
Signing, workload compilation and funding are outside that denominator. Existing
`CAPACITY_RECEIPTS` records individual receipt fingerprints and all five costs.

## Audit before reporting

```sh
python3 scripts/traffic.py audit manifest.json upstream.log upstream-audit.json
python3 scripts/traffic.py audit manifest.json optimized.log optimized-audit.json \
  --compare upstream-audit.json
```

Auditing requires complete phases, successful individual receipts, exact batch
counts, all cost dimensions, the signed stream fingerprint and continuous roots,
heights and parent IDs. Paired audits compare a canonical execution-plan hash
and every block and transaction's semantics, excluding timings. Inline and
shared-template encodings may differ in file hash but must resolve to identical
actions, accounts, ordering, phases, references and source context. Raw manifest
hashes are retained separately. Also retain successful process exit status,
source metadata and binary/core/driver hashes; a log audit cannot establish those.

For qualified **uninstrumented** timings, add `--performance-qualified` to emit
five-second scenarios for 2.5× and 3.5× serial-work multipliers. Read-observer logs
are rejected for this option. Each scenario shows offered load (`transactions /
5 seconds`), sample count, mean/p95/max whole-block time and deadline-miss count.
The multipliers are illustrative serial-processing budgets; 3.5× allows an
additional execution-equivalent stage. They are not measured miner/signer/append
latencies or measurements of a constrained VM.
Plot the full ramp for each build; report the observed pass/fail brackets and any
nonmonotonic results instead of assuming a single exact capacity threshold.

## Establish representativeness separately

Export a **completed, checkpointed** benchmark database read-only:

```sh
python3 scripts/traffic.py export-corpus /path/to/stacks-bench.db corpus.json \
  --start 7756001 --end 7761000
```

The export preserves historical argument shapes, caller reuse, deployed contracts,
functions, block identities and reference transaction IDs. It is reference data,
not an automatically executable recipe: deadlines, balances, LP ownership and
block context can make re-signed calls fail. Its non-call count includes protocol
transactions and must not be treated as a user-transfer weight.
The export does not classify historical application success; inspect receipts
before using its counts as weights for an all-success workload.

Before describing a workload as representative, compare its function/pool mix,
argument-list breadth, signer reuse and state-growth behavior with the reference.
Use a separate diagnostic run to compare per-function persisted-node requests,
per-transaction unique nodes and inherited-node ages, resolving trie identities
to real header heights. Keep explicit `at-block` targets separate from inherited
old nodes. Node requests and ages do not measure disk reads or cache misses, and
summed per-transaction uniques are not a global cache footprint. Disclose omitted
traffic classes and amount/bin changes. Passing the execution audit alone does
not qualify either representativeness or network TPS.

For a binary with the separately reviewed `READ_HISTORY` observer, resolve its
Clarity origins and summarize each phase/workload class with:

```sh
python3 scripts/traffic.py read-profile manifest.json diagnostic.log \
  /source/chainstate/vm/index.sqlite read-profile.json
```

This requires complete transaction coverage and rejects truncated observations,
unknown Clarity origins and future read targets. It reports old-node requests and
explicit `at-block` calls separately. Use specific class names for different
functions/pools when a per-class comparison is needed. The observer is not
enabled by the manifest mode itself.

## Tests

```sh
cargo nextest run -p stacks-bench --lib -E 'test(traffic_manifest_)'
python3 -m unittest discover -s scripts -p test_traffic.py
```
