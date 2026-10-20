// Copyright (C) 2026 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

//! Clarity execution benchmark (fourth review, part 5).
//!
//! Real contracts run through `stackslib`'s `ClarityInstance` over a
//! `MarfedKV` on disk, the same consumer code the node uses; the MARF
//! implementation is whichever the surrounding workspace's `stacks-marf`
//! package is (the rewrite in this tree; upstream or the optimized crate in
//! the worktrees `scripts/build-variants.sh` prepares). Header and burn
//! state come from the clarity crate's unit-test databases (epoch 2.5,
//! unlimited block limit); every block is a real `begin_block`, a sequence
//! of `as_transaction` contract calls, `seal` and `commit_block`.
//!
//! Phases per block: `exec` (every transaction of the block), `seal` (the
//! MARF root hash), `commit` (`commit_block`: the trie is serialized and
//! written). Deployment and population of the key space are separate,
//! untimed blocks; the first block of each workload is reported separately
//! from the rest (process-local caches cold for that workload's keys). Each
//! workload verifies every returned value against an in-process model and
//! records the block roots, cost totals and a digest of every returned
//! value for cross-implementation comparison; the whole thing runs `REPEAT`
//! times on fresh databases and times are medians over repetitions with the
//! min-max spread. Harness work inside the timed execution (key generation,
//! argument construction, model updates, assertions) is timed separately
//! and reported as `harness`, so `exec - harness` is the interpreter plus
//! the database. Individual samples and the equivalence record go to
//! `JSON_OUT` when set.
//!
//! What survives between transactions and blocks: the MARF handle (so the
//! rewrite's resolved-patch cache and block-location caches, the optimized
//! crate's resolved-patch cache and mmap; its result cache lives in the
//! open trie and is dropped at commit), the OS page cache, and nothing on
//! the Clarity side (this harness keeps no analysis or contract cache).

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use clarity::vm::clarity::TransactionConnection;
use clarity::vm::costs::ExecutionCost;
use clarity::vm::resource_limiter::ResourceBudget;
use clarity::vm::test_util::{generate_test_burn_state_db, UnitTestBurnStateDB, TEST_HEADER_DB};
use clarity::vm::types::{PrincipalData, QualifiedContractIdentifier, Value};
use clarity::vm::ClarityVersion;
use stacks::chainstate::stacks::boot::BOOT_CODE_COSTS_3;
use stacks::clarity_vm::clarity::{ClarityBlockConnection, ClarityInstance};
use stacks::clarity_vm::database::marf::MarfedKV;
use stacks::util_lib::boot::boot_code_id;
use stacks_common::consts::CHAIN_ID_TESTNET;
use stacks_common::types::chainstate::StacksBlockId;
use stacks_common::types::StacksEpochId;
use stacks_marf::ClarityMarfTrieId as _;

const CONTRACT: &str = r#"
(define-map kv { k: uint } { v: uint })
(define-data-var counter uint u0)
(define-data-var round uint u0)
(define-public (set-round (r uint)) (ok (var-set round r)))
(define-read-only (get-kv (k uint)) (default-to u0 (get v (map-get? kv { k: k }))))
(define-private (set-one (k uint)) (map-set kv { k: k } { v: (+ k (var-get round)) }))
(define-private (add-kv (k uint) (acc uint)) (+ acc (get-kv k)))
(define-public (set-many (ks (list 50 uint))) (begin (map set-one ks) (ok (len ks))))
(define-public (read-many (ks (list 50 uint))) (ok (fold add-kv ks u0)))
(define-public (update-then-read (ks (list 50 uint))) (begin (map set-one ks) (ok (fold add-kv ks u0))))
(define-public (incr) (begin (var-set counter (+ (var-get counter) u1)) (ok (var-get counter))))
(define-public (fail-after-write (k uint)) (begin (map-set kv { k: k } { v: u999999999 }) (err u1)))
"#;

const KEYS: u64 = 20_000;
const CHUNK: usize = 50;

fn block_id(n: u32) -> StacksBlockId {
    let mut b = [0u8; 32];
    b[0] = n as u8;
    b[1..5].copy_from_slice(&n.to_be_bytes());
    b[31] = 7;
    StacksBlockId(b)
}

fn marf_opts() -> stacks_marf::MARFOpenOpts {
    use stacks_marf::storage::TrieHashCalculationMode;
    #[allow(unused_mut)]
    let mut o = stacks_marf::MARFOpenOpts::new(TrieHashCalculationMode::Deferred, true).with_compression(true);
    #[cfg(feature = "optimized-marf")]
    {
        o = o.with_mmap(true);
        o.root_node_cache = false;
        o.resolved_patch_cache_capacity = 64;
        o.result_cache_capacity = 2048;
    }
    o
}

#[cfg(feature = "optimized-marf")]
fn describe_marf() -> String {
    let o = marf_opts();
    format!(
        "optimized stacks-marf (29ef093d): external_blobs={} compress={} mmap={} root_node_cache={} resolved_patch_cache={} result_cache={}",
        o.external_blobs, o.compress, o.mmap, o.root_node_cache, o.resolved_patch_cache_capacity, o.result_cache_capacity
    )
}

#[cfg(not(feature = "optimized-marf"))]
fn describe_marf() -> String {
    let o = marf_opts();
    format!(
        "stacks-marf package of this workspace: external_blobs={} compress={} (rewrite: mmap always, patch cache 4096; upstream: no caches)",
        o.external_blobs, o.compress
    )
}

/// xorshift, so every variant sees the same key sequence
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn keys(&mut self, n: usize) -> Vec<u64> {
        (0..n).map(|_| self.next() % KEYS).collect()
    }
}

#[derive(Default)]
struct Model {
    kv: BTreeMap<u64, u64>,
    counter: u64,
    round: u64,
}

impl Model {
    fn get(&self, k: u64) -> u64 {
        *self.kv.get(&k).unwrap_or(&0)
    }
    fn set_many(&mut self, ks: &[u64]) {
        for &k in ks {
            self.kv.insert(k, k + self.round);
        }
    }
    fn sum(&self, ks: &[u64]) -> u64 {
        ks.iter().map(|&k| self.get(k)).sum()
    }
}

struct State {
    model: Model,
    rng: Rng,
    sender: PrincipalData,
    contract: QualifiedContractIdentifier,
    /// Time spent in harness work inside timed blocks (key generation,
    /// argument lists, model updates, assertions).
    harness: Duration,
    /// Fold of every value returned by a contract call in the workload.
    digest: u64,
}

impl State {
    /// Run harness work and account its time.
    fn h<R>(&mut self, f: impl FnOnce(&mut State) -> R) -> R {
        let t = Instant::now();
        let r = f(self);
        self.harness += t.elapsed();
        r
    }
    fn note(&mut self, v: u64) {
        self.digest = (self.digest ^ v).wrapping_mul(0x100_0000_01b3).rotate_left(7);
    }
}

fn ulist(ks: &[u64]) -> Value {
    Value::cons_list_unsanitized(ks.iter().map(|&k| Value::UInt(k as u128)).collect()).unwrap()
}

fn call(st: &State, blk: &mut ClarityBlockConnection<'_, '_>, f: &str, args: &[Value]) -> Value {
    blk.as_transaction(|tx| {
        tx.run_contract_call(
            &st.sender,
            None,
            &st.contract,
            f,
            args,
            |_, _| None,
            &ResourceBudget::unlimited(),
        )
        .map(|(v, _, _)| v)
        .unwrap_or_else(|e| panic!("{f}: {e:?}"))
    })
}

fn ok_uint(v: &Value) -> u64 {
    match v {
        Value::Response(r) if r.committed => match *r.data {
            Value::UInt(u) => u as u64,
            ref other => panic!("expected uint, got {other}"),
        },
        other => panic!("expected (ok uint), got {other}"),
    }
}

fn is_err(v: &Value) -> bool {
    matches!(v, Value::Response(r) if !r.committed)
}

#[derive(Clone, Default, serde::Serialize)]
struct Timing {
    exec: Duration,
    seal: Duration,
    commit: Duration,
    /// Harness work inside `exec`.
    harness: Duration,
    txs: u64,
    blocks: u64,
}

impl Timing {
    fn add(&mut self, o: &Timing) {
        self.exec += o.exec;
        self.seal += o.seal;
        self.commit += o.commit;
        self.harness += o.harness;
        self.txs += o.txs;
        self.blocks += o.blocks;
    }
    fn total(&self) -> Duration {
        self.exec + self.seal + self.commit
    }
}

/// One workload's record for one repetition.
#[derive(Clone)]
struct Record {
    first: Timing,
    rest: Timing,
    cost: ExecutionCost,
    roots: Vec<String>,
    values_digest: u64,
}

impl Default for Record {
    fn default() -> Record {
        Record {
            first: Timing::default(),
            rest: Timing::default(),
            cost: ExecutionCost::ZERO,
            roots: vec![],
            values_digest: 0,
        }
    }
}

struct Bench {
    clarity: ClarityInstance,
    burn_db: UnitTestBurnStateDB,
    tip: StacksBlockId,
    next: u32,
    st: State,
}

impl Bench {
    fn new(dir: &std::path::Path) -> Bench {
        let kv = MarfedKV::open(dir.to_str().unwrap(), None, Some(marf_opts())).expect("open MarfedKV");
        let mut clarity = ClarityInstance::new(false, CHAIN_ID_TESTNET, kv);
        let burn_db = generate_test_burn_state_db(StacksEpochId::Epoch25);
        let issuer = PrincipalData::parse_standard_principal("SZ2J6ZY48GV1EZ5V2V5RB9MP66SW86PYKKQ9H6DPR").unwrap();
        let st = State {
            model: Model::default(),
            rng: Rng(0x9e3779b97f4a7c15),
            sender: issuer.into(),
            contract: QualifiedContractIdentifier::parse("SZ2J6ZY48GV1EZ5V2V5RB9MP66SW86PYKKQ9H6DPR.bench").unwrap(),
            harness: Duration::ZERO,
            digest: 0,
        };
        // genesis: the default cost contract (epoch 2.0) plus costs-3 for
        // the epoch 2.5 blocks that follow
        let genesis = block_id(1);
        {
            let mut gb = clarity.begin_test_genesis_block(&StacksBlockId::sentinel(), &genesis, &TEST_HEADER_DB, &burn_db);
            gb.as_transaction(|tx| {
                let id = boot_code_id("costs-3", false);
                let (ast, _) = tx
                    .analyze_smart_contract(&id, ClarityVersion::Clarity1, BOOT_CODE_COSTS_3, &ResourceBudget::unlimited())
                    .expect("analyze costs-3");
                tx.initialize_smart_contract(&id, ClarityVersion::Clarity1, &ast, BOOT_CODE_COSTS_3, None, |_, _| None, &ResourceBudget::unlimited())
                    .expect("deploy costs-3");
            });
            gb.commit_to_block(&genesis);
        }
        let mut b = Bench {
            clarity,
            burn_db,
            tip: genesis,
            next: 2,
            st,
        };
        // deploy the benchmark contract (untimed)
        b.block(|st, blk| {
            blk.as_transaction(|tx| {
                let (ast, _) = tx
                    .analyze_smart_contract(&st.contract, ClarityVersion::Clarity2, CONTRACT, &ResourceBudget::unlimited())
                    .expect("analyze");
                tx.initialize_smart_contract(&st.contract, ClarityVersion::Clarity2, &ast, CONTRACT, None, |_, _| None, &ResourceBudget::unlimited())
                    .expect("deploy");
            });
            0
        });
        b
    }

    /// Run one block: `body` issues the transactions and returns how many;
    /// exec, seal and commit are timed separately. Returns the timing, the
    /// block's cost and its root.
    fn block<F: FnOnce(&mut State, &mut ClarityBlockConnection<'_, '_>) -> u64>(&mut self, body: F) -> (Timing, ExecutionCost, String) {
        let next = block_id(self.next);
        self.next += 1;
        let tip = self.tip.clone();
        let mut blk = self.clarity.begin_block(&tip, &next, &TEST_HEADER_DB, &self.burn_db);
        let h0 = self.st.harness;
        let t0 = Instant::now();
        let txs = body(&mut self.st, &mut blk);
        let exec = t0.elapsed();
        let harness = self.st.harness - h0;
        let t1 = Instant::now();
        let root = blk.seal();
        let seal = t1.elapsed();
        let t2 = Instant::now();
        let tracker = blk.commit_to_block(&next);
        let commit = t2.elapsed();
        self.tip = next;
        (
            Timing {
                exec,
                seal,
                commit,
                harness,
                txs,
                blocks: 1,
            },
            tracker.get_total(),
            format!("{root}"),
        )
    }

    /// A block whose writes are abandoned (`rollback_block`).
    fn rolled_back_block<F: FnOnce(&mut State, &mut ClarityBlockConnection<'_, '_>)>(&mut self, body: F) -> Duration {
        let next = block_id(self.next);
        self.next += 1;
        let tip = self.tip.clone();
        let t0 = Instant::now();
        let mut blk = self.clarity.begin_block(&tip, &next, &TEST_HEADER_DB, &self.burn_db);
        body(&mut self.st, &mut blk);
        blk.rollback_block();
        t0.elapsed()
    }

    fn workload<F: FnMut(&mut State, &mut ClarityBlockConnection<'_, '_>, usize) -> u64>(&mut self, blocks: usize, mut body: F) -> Record {
        let mut rec = Record::default();
        self.st.digest = 0;
        for i in 0..blocks {
            let (t, cost, root) = self.block(|st, blk| body(st, blk, i));
            if i == 0 {
                rec.first.add(&t);
            } else {
                rec.rest.add(&t);
            }
            rec.cost.add(&cost).expect("cost overflow");
            rec.roots.push(root);
        }
        rec.values_digest = self.st.digest;
        rec
    }
}

fn populate(b: &mut Bench, blocks: usize) {
    // every key once, in order, 50 per transaction, 10 transactions per block
    let per_block = (KEYS as usize).div_ceil(blocks);
    for i in 0..blocks {
        let start = (i * per_block) as u64;
        let end = ((i + 1) * per_block).min(KEYS as usize) as u64;
        b.block(|st, blk| {
            let mut txs = 0;
            let mut k = start;
            while k < end {
                let ks: Vec<u64> = (k..(k + CHUNK as u64).min(end)).collect();
                let v = call(st, blk, "set-many", &[ulist(&ks)]);
                assert_eq!(ok_uint(&v), ks.len() as u64);
                st.model.set_many(&ks);
                k += CHUNK as u64;
                txs += 1;
            }
            txs
        });
    }
}

fn read_heavy(b: &mut Bench, blocks: usize) -> Record {
    let hot: Vec<u64> = (0..CHUNK as u64).collect();
    b.workload(blocks, |st, blk, _| {
        for _ in 0..20 {
            let (ks, args) = st.h(|st| {
                let ks = st.rng.keys(CHUNK);
                let args = [ulist(&ks)];
                (ks, args)
            });
            let v = call(st, blk, "read-many", &args);
            st.h(|st| {
                let got = ok_uint(&v);
                assert_eq!(got, st.model.sum(&ks));
                st.note(got);
            });
        }
        let args = st.h(|_| [ulist(&hot)]);
        for _ in 0..10 {
            let v = call(st, blk, "read-many", &args);
            st.h(|st| {
                let got = ok_uint(&v);
                assert_eq!(got, st.model.sum(&hot));
                st.note(got);
            });
        }
        30
    })
}

fn write_heavy(b: &mut Bench, blocks: usize) -> Record {
    b.workload(blocks, |st, blk, i| {
        let round = 1000 + i as u64;
        let v = call(st, blk, "set-round", &[Value::UInt(round as u128)]);
        st.h(|st| {
            assert!(matches!(v, Value::Response(ref r) if r.committed));
            st.model.round = round;
        });
        for _ in 0..20 {
            let (ks, args) = st.h(|st| {
                let ks = st.rng.keys(CHUNK);
                let args = [ulist(&ks)];
                (ks, args)
            });
            let v = call(st, blk, "update-then-read", &args);
            st.h(|st| {
                st.model.set_many(&ks);
                let got = ok_uint(&v);
                assert_eq!(got, st.model.sum(&ks));
                st.note(got);
            });
        }
        21
    })
}

fn mixed(b: &mut Bench, blocks: usize) -> Record {
    let hot: Vec<u64> = (100..100 + CHUNK as u64).collect();
    b.workload(blocks, |st, blk, _| {
        let hot_args = st.h(|_| [ulist(&hot)]);
        for j in 0..30 {
            match j % 3 {
                0 => {
                    let v = call(st, blk, "incr", &[]);
                    st.h(|st| {
                        st.model.counter += 1;
                        let got = ok_uint(&v);
                        assert_eq!(got, st.model.counter);
                        st.note(got);
                    });
                }
                1 => {
                    let v = call(st, blk, "set-many", &hot_args);
                    st.h(|st| {
                        let got = ok_uint(&v);
                        assert_eq!(got, hot.len() as u64);
                        st.model.set_many(&hot);
                        st.note(got);
                    });
                }
                _ => {
                    let (ks, args) = st.h(|st| {
                        let ks = st.rng.keys(CHUNK);
                        let args = [ulist(&ks)];
                        (ks, args)
                    });
                    let v = call(st, blk, "read-many", &args);
                    st.h(|st| {
                        let got = ok_uint(&v);
                        assert_eq!(got, st.model.sum(&ks));
                        st.note(got);
                    });
                }
            }
        }
        30
    })
}

fn abort(b: &mut Bench, blocks: usize) -> Record {
    let rec = b.workload(blocks, |st, blk, _| {
        for _ in 0..20 {
            let (k, args, rargs) = st.h(|st| {
                let k = st.rng.next() % KEYS;
                (k, [Value::UInt(k as u128)], [ulist(&[k])])
            });
            let v = call(st, blk, "fail-after-write", &args);
            st.h(|_| assert!(is_err(&v), "expected (err u1)"));
            // the aborted call's write must not be visible
            let r = call(st, blk, "read-many", &rargs);
            st.h(|st| {
                let got = ok_uint(&r);
                assert_eq!(got, st.model.get(k), "aborted write leaked for key {k}");
                st.note(got);
            });
        }
        40
    });
    // a whole block rolled back: its writes vanish, the next block sees the
    // previous state
    let hot: Vec<u64> = (200..200 + CHUNK as u64).collect();
    let before: Vec<u64> = hot.iter().map(|&k| b.st.model.get(k)).collect();
    let _ = b.rolled_back_block(|st, blk| {
        st.model.round = 777;
        let v = call(st, blk, "set-round", &[Value::UInt(777)]);
        assert!(matches!(v, Value::Response(ref r) if r.committed));
        let v = call(st, blk, "set-many", &[ulist(&hot)]);
        assert_eq!(ok_uint(&v), hot.len() as u64);
        // visible inside the block
        let r = call(st, blk, "read-many", &[ulist(&hot)]);
        assert_eq!(ok_uint(&r), hot.iter().map(|&k| k + 777).sum::<u64>());
    });
    // the model does not take the round or the writes
    b.st.model.round = 0;
    b.block(|st, blk| {
        let r = call(st, blk, "read-many", &[ulist(&hot)]);
        assert_eq!(ok_uint(&r), before.iter().sum::<u64>(), "rolled-back block's writes visible");
        1
    });
    rec
}

#[derive(Default, serde::Serialize)]
struct Agg {
    first_exec: Vec<Duration>,
    rest_exec: Vec<Duration>,
    rest_seal: Vec<Duration>,
    rest_commit: Vec<Duration>,
    rest_total: Vec<Duration>,
    rest_harness: Vec<Duration>,
    txs: u64,
    blocks: u64,
    cost: Option<ExecutionCost>,
    roots: Option<Vec<String>>,
    values_digest: Option<String>,
}

#[derive(serde::Serialize)]
struct Output<'a> {
    harness: &'static str,
    experiment: &'static str,
    variant: String,
    tree_rev: String,
    binary_sha256: String,
    rustc: String,
    profile: &'static str,
    repeat: usize,
    blocks: usize,
    keys: u64,
    started: String,
    populate_ns: Vec<u128>,
    workloads: &'a BTreeMap<&'static str, Agg>,
}

fn median(v: &[Duration]) -> Duration {
    let mut s = v.to_vec();
    s.sort();
    if s.is_empty() {
        Duration::ZERO
    } else if s.len() % 2 == 1 {
        s[s.len() / 2]
    } else {
        (s[s.len() / 2 - 1] + s[s.len() / 2]) / 2
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let repeat: usize = std::env::var("REPEAT").ok().and_then(|s| s.parse().ok()).unwrap_or(5).max(1);
    let blocks: usize = args.first().and_then(|s| s.parse().ok()).unwrap_or(10);
    let cpu = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("model name")).map(|l| l.split(':').nth(1).unwrap_or("").trim().to_string()))
        .unwrap_or_default();
    println!(
        "clarity-bench: {} | {} | profile {} | repeat {} | {} keys, {} timed blocks per workload",
        describe_marf(),
        cpu,
        if cfg!(debug_assertions) { "debug" } else { "release" },
        repeat,
        KEYS,
        blocks
    );
    let names = ["read-heavy", "write-heavy", "mixed", "inherited-after-history", "abort"];
    let mut aggs: BTreeMap<&str, Agg> = names.iter().map(|n| (*n, Agg::default())).collect();
    let mut populate_time = vec![];
    for _ in 0..repeat {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut b = Bench::new(dir.path());
        let t = Instant::now();
        populate(&mut b, 40);
        populate_time.push(t.elapsed());
        let recs: Vec<(&str, Record)> = vec![
            ("read-heavy", read_heavy(&mut b, blocks)),
            ("write-heavy", write_heavy(&mut b, blocks)),
            ("mixed", mixed(&mut b, blocks)),
            // reads of state written 40 to 70 blocks earlier
            ("inherited-after-history", read_heavy(&mut b, blocks.max(2) / 2)),
            ("abort", abort(&mut b, blocks.max(2) / 2)),
        ];
        for (name, r) in recs {
            let a = aggs.get_mut(name).unwrap();
            a.first_exec.push(r.first.exec);
            a.rest_exec.push(r.rest.exec);
            a.rest_seal.push(r.rest.seal);
            a.rest_commit.push(r.rest.commit);
            a.rest_total.push(r.rest.total());
            a.rest_harness.push(r.rest.harness);
            a.txs = r.rest.txs;
            a.blocks = r.rest.blocks;
            let dg = format!("{:016x}", r.values_digest);
            if let Some(prev) = &a.values_digest {
                assert_eq!(*prev, dg, "{name}: returned values differ between repetitions");
            }
            a.values_digest = Some(dg);
            if let Some(c) = &a.cost {
                assert_eq!(*c, r.cost, "{name}: costs differ between repetitions");
            }
            a.cost = Some(r.cost);
            if let Some(prev) = &a.roots {
                assert_eq!(*prev, r.roots, "{name}: roots differ between repetitions");
            }
            a.roots = Some(r.roots);
        }
    }
    println!("populate (40 blocks, {KEYS} keys, untimed setup): median {:.1} ms", ms(median(&populate_time)));
    println!();
    println!(
        "{:<26} {:>6} {:>7} | {:>14} {:>10} {:>14} {:>14} {:>14} | {:>20}",
        "workload", "blocks", "txs", "exec ms", "harness ms", "seal ms", "commit ms", "total ms", "first block exec ms"
    );
    for name in names {
        let a = &aggs[name];
        let spread = |v: &[Duration]| {
            let mut s = v.to_vec();
            s.sort();
            format!("{:.1}-{:.1}", ms(s[0]), ms(*s.last().unwrap()))
        };
        println!(
            "{:<26} {:>6} {:>7} | {:>14} {:>10} {:>14} {:>14} {:>14} | {:>20}",
            name,
            a.blocks,
            a.txs,
            format!("{:.1} ({})", ms(median(&a.rest_exec)), spread(&a.rest_exec)),
            format!("{:.1}", ms(median(&a.rest_harness))),
            format!("{:.1}", ms(median(&a.rest_seal))),
            format!("{:.1}", ms(median(&a.rest_commit))),
            format!("{:.1}", ms(median(&a.rest_total))),
            format!("{:.1} ({})", ms(median(&a.first_exec)), spread(&a.first_exec)),
        );
    }
    println!();
    println!("equivalence (must match across implementations):");
    for name in names {
        let a = &aggs[name];
        let c = a.cost.as_ref().unwrap();
        let roots = a.roots.as_ref().unwrap();
        println!(
            "  {:<26} cost runtime={} read_count={} read_length={} write_count={} write_length={} blocks={} values_digest={} last_root={}",
            name, c.runtime, c.read_count, c.read_length, c.write_count, c.write_length, roots.len(), a.values_digest.as_deref().unwrap_or(""), roots.last().unwrap()
        );
    }
    if let Ok(path) = std::env::var("JSON_OUT") {
        let exe = std::env::current_exe()
            .ok()
            .and_then(|p| std::fs::read(p).ok())
            .map(|b| stacks_common::util::hash::Sha256Sum::from_data(&b).to_hex())
            .unwrap_or_else(|| "unknown".into());
        let out = Output {
            harness: "clarity-bench",
            experiment: "clarity",
            variant: describe_marf(),
            tree_rev: option_env!("MARF_BENCH_TREE_REV").unwrap_or("unknown").to_string(),
            binary_sha256: exe,
            rustc: option_env!("MARF_BENCH_RUSTC").unwrap_or("unknown").to_string(),
            profile: if cfg!(debug_assertions) { "debug" } else { "release" },
            repeat,
            blocks,
            keys: KEYS,
            started: String::new(),
            populate_ns: populate_time.iter().map(|d| d.as_nanos()).collect(),
            workloads: &aggs,
        };
        std::fs::write(&path, serde_json::to_string_pretty(&out).unwrap()).unwrap_or_else(|e| panic!("write {path}: {e}"));
        eprintln!("[json] wrote {path}");
    }
}
