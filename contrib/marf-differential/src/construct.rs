//! Reads during block construction (fourth review, part 4).
//!
//! A base chain is built, then a child block is opened and reads are
//! interleaved with writes against the open block: inherited values (a hot
//! set and a wide set), freshly inserted values, updated values, misses
//! followed by insertion of the missing keys, successive updates of one
//! key, ancestor-tip reads while the child is open, then seal and commit;
//! finally a rollback (`drop_current`) path and a failed-begin path. Every
//! read result is collected in a preallocated buffer inside the timed
//! region and checked against a model after it: exact values for present
//! keys, absence for misses. Read time, write time and the whole
//! construction time are reported separately, as medians over `REPEAT`
//! repetitions (fresh database each time). The implementation's read
//! counters, when built in, are reported per phase; for the reference this
//! includes its lookup-result cache, which only serves the open block.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::{Duration, Instant};

use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha20Rng;
use stacks_common::types::chainstate::StacksBlockId;
use stacks_marf_next::ClarityMarfTrieId as _;

use crate::alloc::{environment, repeat, Counters, Direct, Opts, Samples};
use crate::Impl;

fn bid(n: u32) -> StacksBlockId {
    let mut b = [0u8; 32];
    b[..4].copy_from_slice(&n.to_be_bytes());
    b[31] = 1;
    StacksBlockId(b)
}

#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub blocks: usize,
    pub inserts: usize,
    pub reads: usize,
    pub external: bool,
    pub compress: bool,
    pub result_cache: Option<usize>,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            blocks: 20,
            inserts: 2000,
            reads: 5000,
            external: true,
            compress: true,
            result_cache: None,
        }
    }
}

#[derive(Default)]
struct PhaseAcc {
    /// read or write time per repetition
    time: Samples,
    ops: u64,
    kind: &'static str,
    counters: Counters,
}

struct Rec<'a> {
    phases: &'a mut BTreeMap<(usize, String), PhaseAcc>,
    order: usize,
}

impl Rec<'_> {
    fn add(&mut self, name: &str, kind: &'static str, ops: u64, d: Duration, c: Counters) {
        let key = (self.order, name.to_string());
        let e = self.phases.entry(key).or_default();
        e.time.push(d);
        e.ops = ops;
        e.kind = kind;
        if e.time.len() == 1 {
            e.counters = c;
        }
        self.order += 1;
    }
}

fn value(rng: &mut ChaCha20Rng) -> [u8; 40] {
    let mut v = [0u8; 40];
    rng.fill(&mut v[..]);
    v
}

/// Read `keys` at `tip` into `out` (timed), then check against `model`.
fn timed_reads<M: Direct>(
    marf: &mut M,
    tip: &StacksBlockId,
    keys: &[&String],
    cycles: usize,
    out: &mut [Option<[u8; 40]>],
    model: &HashMap<String, [u8; 40]>,
) -> (u64, Duration, Counters) {
    let _ = marf.counters();
    let t = Instant::now();
    let mut n = 0u64;
    for _ in 0..cycles {
        for (i, k) in keys.iter().enumerate() {
            out[i] = marf.get_value(tip, k);
            n += 1;
        }
    }
    let d = t.elapsed();
    let c = marf.counters();
    for (i, k) in keys.iter().enumerate() {
        assert_eq!(out[i], model.get(k.as_str()).copied(), "key {k} at {tip}");
    }
    (n, d, c)
}

fn run_once<M: Direct>(dir: &Path, cfg: &Config, rec: &mut Rec<'_>) -> ([u8; 32], [u8; 32]) {
    let mut rng = ChaCha20Rng::seed_from_u64(31);
    let opts = Opts {
        external: cfg.external,
        compress: cfg.compress,
        patch_cache_capacity: None,
        result_cache_capacity: cfg.result_cache,
    };
    let mut marf = M::open(&dir.join("marf.sqlite"), &opts);
    if rec.phases.is_empty() {
        println!("impl {}: {}", M::NAME, marf.describe(&opts));
    }
    // base chain, model of committed state per tip we read at
    let mut committed: HashMap<String, [u8; 40]> = HashMap::new();
    let mut at_block10: HashMap<String, [u8; 40]> = HashMap::new();
    let mut all_keys: Vec<String> = vec![];
    let mut parent = StacksBlockId::sentinel();
    let mut tip10 = parent.clone();
    let old_block = cfg.blocks.min(10);
    for b in 0..cfg.blocks {
        let child = bid(b as u32 + 1);
        marf.begin(&parent, &child);
        let keys: Vec<String> = (0..cfg.inserts)
            .map(|i| format!("key::{b}::{i}::{}", rng.gen::<u32>()))
            .collect();
        let values: Vec<[u8; 40]> = (0..cfg.inserts).map(|_| value(&mut rng)).collect();
        marf.insert_batch(&keys, &values);
        marf.seal();
        marf.commit();
        for (k, v) in keys.iter().zip(&values) {
            committed.insert(k.clone(), *v);
        }
        if b + 1 == old_block {
            at_block10 = committed.clone();
            tip10 = child.clone();
        }
        all_keys.extend(keys);
        parent = child;
    }
    let base_tip = parent.clone();
    let child = bid(5000);
    let reads = cfg.reads;
    let mut out: Vec<Option<[u8; 40]>> = vec![None; reads.max(cfg.inserts).max(2000)];
    // the model of the open block's state
    let mut open: HashMap<String, [u8; 40]> = committed.clone();

    let construction = Instant::now();
    let _ = marf.counters();
    let t = Instant::now();
    marf.begin(&base_tip, &child);
    rec.add("begin", "write", 1, t.elapsed(), marf.counters());

    // inherited, hot: 64 keys cycled
    let hot: Vec<&String> = all_keys.iter().take(64).collect();
    let (n, d, c) = timed_reads(&mut marf, &child, &hot, reads / 64, &mut out, &open);
    rec.add("inherited hot64 (cycled)", "read", n, d, c);
    // inherited, wide: `reads` distinct keys once
    let wide: Vec<&String> = (0..reads).map(|_| &all_keys[rng.gen_range(0..all_keys.len())]).collect();
    let (n, d, c) = timed_reads(&mut marf, &child, &wide, 1, &mut out, &open);
    rec.add("inherited wide (distinct once)", "read", n, d, c);

    // insert 500 new keys, then read them (hot 64 cycled, and all once x10)
    let new_keys: Vec<String> = (0..500).map(|i| format!("new::{i}::{}", rng.gen::<u32>())).collect();
    let new_values: Vec<[u8; 40]> = (0..500).map(|_| value(&mut rng)).collect();
    let _ = marf.counters();
    let t = Instant::now();
    marf.insert_batch(&new_keys, &new_values);
    rec.add("insert 500 new", "write", 500, t.elapsed(), marf.counters());
    for (k, v) in new_keys.iter().zip(&new_values) {
        open.insert(k.clone(), *v);
    }
    let hot_new: Vec<&String> = new_keys.iter().take(64).collect();
    let (n, d, c) = timed_reads(&mut marf, &child, &hot_new, reads / 64, &mut out, &open);
    rec.add("inserted hot64 (cycled)", "read", n, d, c);
    let all_new: Vec<&String> = new_keys.iter().collect();
    let (n, d, c) = timed_reads(&mut marf, &child, &all_new, 10, &mut out, &open);
    rec.add("inserted all 500 (x10)", "read", n, d, c);

    // update 200 inherited keys, read them (expect the new values)
    let upd_keys: Vec<String> = (0..200).map(|_| all_keys[rng.gen_range(0..all_keys.len())].clone()).collect();
    let upd_values: Vec<[u8; 40]> = (0..200).map(|_| value(&mut rng)).collect();
    let _ = marf.counters();
    let t = Instant::now();
    marf.insert_batch(&upd_keys, &upd_values);
    rec.add("update 200 inherited", "write", 200, t.elapsed(), marf.counters());
    for (k, v) in upd_keys.iter().zip(&upd_values) {
        open.insert(k.clone(), *v);
    }
    let upd_refs: Vec<&String> = upd_keys.iter().collect();
    let (n, d, c) = timed_reads(&mut marf, &child, &upd_refs, 25, &mut out, &open);
    rec.add("updated 200 (x25)", "read", n, d, c);

    // misses, then insertion of the missing keys, then reads
    let miss_keys: Vec<String> = (0..500).map(|i| format!("absent::{i}")).collect();
    let miss_refs: Vec<&String> = miss_keys.iter().collect();
    let (n, d, c) = timed_reads(&mut marf, &child, &miss_refs, 10, &mut out, &open);
    rec.add("misses 500 (x10)", "read", n, d, c);
    let miss_values: Vec<[u8; 40]> = (0..500).map(|_| value(&mut rng)).collect();
    let _ = marf.counters();
    let t = Instant::now();
    marf.insert_batch(&miss_keys, &miss_values);
    rec.add("insert the 500 missed", "write", 500, t.elapsed(), marf.counters());
    for (k, v) in miss_keys.iter().zip(&miss_values) {
        open.insert(k.clone(), *v);
    }
    let (n, d, c) = timed_reads(&mut marf, &child, &miss_refs, 10, &mut out, &open);
    rec.add("formerly missed 500 (x10)", "read", n, d, c);

    // successive updates of one key with a read after each
    let k = all_keys[7].clone();
    let mut wtime = Duration::ZERO;
    let mut rtime = Duration::ZERO;
    let _ = marf.counters();
    for _ in 0..100 {
        let v = value(&mut rng);
        let t = Instant::now();
        marf.insert_batch(std::slice::from_ref(&k), &[v]);
        wtime += t.elapsed();
        let t = Instant::now();
        let got = marf.get_value(&child, &k);
        rtime += t.elapsed();
        assert_eq!(got, Some(v), "successive update not visible");
        open.insert(k.clone(), v);
    }
    let c = marf.counters();
    rec.add("successive updates x100: writes", "write", 100, wtime, c);
    rec.add("successive updates x100: reads", "read", 100, rtime, Counters::default());

    // ancestor reads while the child is open: the parent tip sees the
    // committed values (updates not visible), block 10's tip its own state
    let anc: Vec<&String> = (0..2000).map(|_| &all_keys[rng.gen_range(0..all_keys.len())]).collect();
    let (n, d, c) = timed_reads(&mut marf, &base_tip, &anc, 1, &mut out, &committed);
    rec.add("ancestor reads: parent tip 2000", "read", n, d, c);
    let old_keys: Vec<&String> = all_keys[..old_block * cfg.inserts].iter().take(2000).collect();
    let (n, d, c) = timed_reads(&mut marf, &tip10, &old_keys, 1, &mut out, &at_block10);
    rec.add("ancestor reads: block-10 tip 2000", "read", n, d, c);
    // and the open block again after those context changes
    let (n, d, c) = timed_reads(&mut marf, &child, &hot, reads / 64, &mut out, &open);
    rec.add("inherited hot64 after ancestor reads", "read", n, d, c);

    let _ = marf.counters();
    let t = Instant::now();
    marf.seal();
    rec.add("seal", "write", 1, t.elapsed(), marf.counters());
    let t = Instant::now();
    marf.commit();
    rec.add("commit", "write", 1, t.elapsed(), marf.counters());
    rec.add("whole construction (begin..commit)", "total", 1, construction.elapsed(), Counters::default());
    let root_child = marf.root_hash(&child).expect("committed child");
    // committed state must be exactly the open model
    let (n, d, c) = timed_reads(&mut marf, &child, &upd_refs, 1, &mut out, &open);
    rec.add("after commit: updated 200", "read", n, d, c);

    // rollback path: build on the child, read what was written, drop it
    let d_block = bid(6000);
    let rb_keys: Vec<String> = (0..500).map(|i| format!("rb::{i}")).collect();
    let rb_values: Vec<[u8; 40]> = (0..500).map(|_| value(&mut rng)).collect();
    let mut rb_model = open.clone();
    for (k, v) in rb_keys.iter().zip(&rb_values) {
        rb_model.insert(k.clone(), *v);
    }
    let t = Instant::now();
    marf.begin(&child, &d_block);
    marf.insert_batch(&rb_keys, &rb_values);
    let rb_refs: Vec<&String> = rb_keys.iter().collect();
    let (n, d, c) = timed_reads(&mut marf, &d_block, &rb_refs, 1, &mut out, &rb_model);
    rec.add("rollback: reads before drop", "read", n, d, c);
    marf.drop_current();
    rec.add("rollback: begin+insert+drop", "write", 500, t.elapsed(), Counters::default());
    assert!(marf.root_hash(&d_block).is_none(), "dropped block still has a root");
    for k in &rb_keys {
        assert_eq!(marf.get_value(&child, k), None, "dropped write visible at parent");
    }
    // the same block is then built for real and committed
    marf.begin(&child, &d_block);
    marf.insert_batch(&rb_keys, &rb_values);
    marf.seal();
    marf.commit();
    let (n, d, c) = timed_reads(&mut marf, &d_block, &rb_refs, 1, &mut out, &rb_model);
    rec.add("rebuilt after rollback: reads", "read", n, d, c);
    let root_d = marf.root_hash(&d_block).expect("committed d");
    (root_child, root_d)
}

pub fn run(which: Impl, cfg: &Config) {
    match which {
        Impl::Baseline => run_direct::<crate::alloc::Baseline>(cfg),
        Impl::Rewrite => run_direct::<crate::alloc::Rewrite>(cfg),
    }
}

fn run_direct<M: Direct>(cfg: &Config) {
    println!("{}", environment());
    println!(
        "construction workload: {} blocks x {} inserts base chain, {} reads per read phase, external={} compress={} result_cache={:?}",
        cfg.blocks, cfg.inserts, cfg.reads, cfg.external, cfg.compress, cfg.result_cache
    );
    let mut phases: BTreeMap<(usize, String), PhaseAcc> = BTreeMap::new();
    let mut roots = None;
    for _ in 0..repeat() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut rec = Rec {
            phases: &mut phases,
            order: 0,
        };
        let r = run_once::<M>(dir.path(), cfg, &mut rec);
        if let Some(prev) = roots {
            assert_eq!(prev, r, "roots differ between repetitions");
        }
        roots = Some(r);
    }
    let (rc, rd) = roots.unwrap();
    println!("final roots: child={} rollback-rebuilt={}", hex(&rc), hex(&rd));
    println!();
    println!("{:<40} {:>5} {:>8} | {:>12} {:>19} {:>12}", "phase", "kind", "ops", "median us/op", "min-max us/op", "median ms");
    let mut read_total = Duration::ZERO;
    let mut write_total = Duration::ZERO;
    for ((_, name), p) in &phases {
        let per = |d: Duration| d.as_secs_f64() * 1e6 / p.ops.max(1) as f64;
        let med = p.time.median();
        match p.kind {
            "read" => read_total += med,
            "write" => write_total += med,
            _ => {}
        }
        println!(
            "{:<40} {:>5} {:>8} | {:>12.2} {:>19} {:>12.3}",
            name,
            p.kind,
            p.ops,
            per(med),
            format!("{:.2}-{:.2}", per(p.time.min()), per(p.time.max())),
            med.as_secs_f64() * 1e3
        );
    }
    println!(
        "sum of medians: reads {:.3} ms, writes {:.3} ms (whole construction above includes both)",
        read_total.as_secs_f64() * 1e3,
        write_total.as_secs_f64() * 1e3
    );
    if phases.values().any(|p| p.counters.available) {
        println!();
        println!("counters per phase (first repetition):");
        for ((_, name), p) in &phases {
            let fields: Vec<String> = p
                .counters
                .fields()
                .into_iter()
                .filter(|(_, v)| *v != 0)
                .map(|(k, v)| format!("{k}={v}"))
                .collect();
            if !fields.is_empty() {
                println!("  {:<40} {}", name, fields.join(" "));
            }
        }
    }
}

fn hex(b: &[u8; 32]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
