//! Allocation and time measurement of the two implementations' hot paths,
//! calling their public APIs directly (no op DSL, no observation encoding)
//! so that what is counted is the MARF's own behaviour.
//!
//! The binary installs [`Counting`] as the global allocator; `measure`
//! brackets a closure and reports allocations, allocated bytes and time.
//! Phases: `insert` (batch inserts into the trie being built), `seal`
//! (hashing), `commit` (serialization + storage write), `warm_get`
//! (repeated lookups of present keys at a committed tip), `patched_get`
//! (lookups through nodes stored as patch chains), `miss_get` (absent
//! keys) and `proof` (lookup with Merkle proof).

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha20Rng;
use stacks_common::types::chainstate::StacksBlockId;
use stacks_marf_next::ClarityMarfTrieId as _;

use crate::Impl;

/// Counts allocations (`alloc` and `realloc`) and the bytes they request.
pub struct Counting;

static ALLOCS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        System.alloc_zeroed(layout)
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        System.realloc(ptr, layout, new_size)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Measure {
    pub allocs: u64,
    pub bytes: u64,
    pub elapsed: Duration,
}

/// Run `f`, returning its result and what it allocated.
#[inline(never)]
pub fn measure<R>(f: impl FnOnce() -> R) -> (R, Measure) {
    let a0 = ALLOCS.load(Ordering::SeqCst);
    let b0 = BYTES.load(Ordering::SeqCst);
    let t = Instant::now();
    let r = f();
    let elapsed = t.elapsed();
    let m = Measure {
        allocs: ALLOCS.load(Ordering::SeqCst) - a0,
        bytes: BYTES.load(Ordering::SeqCst) - b0,
        elapsed,
    };
    (r, m)
}

/// The handful of operations the measurement needs, over either implementation.
pub trait Direct {
    fn open(path: &Path, external: bool, compress: bool) -> Self;
    fn begin(&mut self, parent: &StacksBlockId, child: &StacksBlockId);
    fn insert_batch(&mut self, keys: &[String], values: &[[u8; 40]]);
    fn seal(&mut self);
    fn commit(&mut self);
    /// Returns whether the key was present.
    fn get(&mut self, tip: &StacksBlockId, key: &str) -> bool;
    fn get_with_proof(&mut self, tip: &StacksBlockId, key: &str) -> bool;
}

pub struct Baseline(stacks_marf::MARF<StacksBlockId>);

impl Direct for Baseline {
    fn open(path: &Path, external: bool, compress: bool) -> Self {
        use stacks_marf::storage::TrieHashCalculationMode;
        let opts = stacks_marf::MARFOpenOpts::new(TrieHashCalculationMode::Deferred, external)
            .with_compression(compress);
        Baseline(stacks_marf::MARF::from_path(path.to_str().unwrap(), opts).unwrap())
    }
    fn begin(&mut self, parent: &StacksBlockId, child: &StacksBlockId) {
        self.0.begin(parent, child).unwrap();
    }
    fn insert_batch(&mut self, keys: &[String], values: &[[u8; 40]]) {
        let values: Vec<stacks_marf::MARFValue> =
            values.iter().map(|v| stacks_marf::MARFValue(*v)).collect();
        self.0.insert_batch(keys, values).unwrap();
    }
    fn seal(&mut self) {
        // `MARF::seal` is test-only in the original; this is its body.
        let mut tx = self.0.begin_tx().unwrap();
        tx.seal().unwrap();
        drop(tx);
    }
    fn commit(&mut self) {
        self.0.commit().unwrap();
    }
    fn get(&mut self, tip: &StacksBlockId, key: &str) -> bool {
        use stacks_marf::marf::MarfConnection;
        self.0.get(tip, key).unwrap().is_some()
    }
    fn get_with_proof(&mut self, tip: &StacksBlockId, key: &str) -> bool {
        self.0.get_with_proof(tip, key).unwrap().is_some()
    }
}

pub struct Rewrite(stacks_marf_next::MARF<StacksBlockId>);

impl Direct for Rewrite {
    fn open(path: &Path, external: bool, compress: bool) -> Self {
        use stacks_marf_next::marf::TrieHashCalculationMode;
        let opts = stacks_marf_next::MARFOpenOpts::new(TrieHashCalculationMode::Deferred, external)
            .with_compression(compress);
        Rewrite(stacks_marf_next::MARF::from_path(path, opts).unwrap())
    }
    fn begin(&mut self, parent: &StacksBlockId, child: &StacksBlockId) {
        self.0.begin(parent, child).unwrap();
    }
    fn insert_batch(&mut self, keys: &[String], values: &[[u8; 40]]) {
        let values: Vec<stacks_marf_next::MARFValue> =
            values.iter().map(|v| stacks_marf_next::MARFValue(*v)).collect();
        self.0.insert_batch(keys, values).unwrap();
    }
    fn seal(&mut self) {
        self.0.seal().unwrap();
    }
    fn commit(&mut self) {
        self.0.commit().unwrap();
    }
    fn get(&mut self, tip: &StacksBlockId, key: &str) -> bool {
        use stacks_marf_next::MarfConnection;
        self.0.get(tip, key).unwrap().is_some()
    }
    fn get_with_proof(&mut self, tip: &StacksBlockId, key: &str) -> bool {
        use stacks_marf_next::MarfConnection;
        self.0.get_with_proof(tip, key).unwrap().is_some()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub external: bool,
    pub compress: bool,
    /// Blocks of `inserts` fresh keys each.
    pub blocks: usize,
    pub inserts: usize,
    /// Blocks after the bulk ones, each rewriting `patch_inserts` of the
    /// first block's keys, so lookups at the tip go through patch chains.
    pub patch_blocks: usize,
    pub patch_inserts: usize,
    /// Lookups per read phase.
    pub reads: usize,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            external: true,
            compress: true,
            blocks: 20,
            inserts: 2000,
            patch_blocks: 8,
            patch_inserts: 50,
            reads: 5000,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Phase {
    pub name: String,
    pub ops: u64,
    pub m: Measure,
}

impl Phase {
    pub fn allocs_per_op(&self) -> f64 {
        self.m.allocs as f64 / self.ops.max(1) as f64
    }
    pub fn bytes_per_op(&self) -> f64 {
        self.m.bytes as f64 / self.ops.max(1) as f64
    }
    pub fn micros_per_op(&self) -> f64 {
        self.m.elapsed.as_secs_f64() * 1e6 / self.ops.max(1) as f64
    }
}

fn bid(n: u32) -> StacksBlockId {
    let mut b = [0u8; 32];
    b[..4].copy_from_slice(&n.to_be_bytes());
    b[31] = 1;
    StacksBlockId(b)
}

/// Run every phase against one implementation; returns the phases in order.
pub fn run_direct<M: Direct>(dir: &Path, cfg: &Config) -> Vec<Phase> {
    let mut rng = ChaCha20Rng::seed_from_u64(11);
    let mut phases = vec![];
    let mut marf = M::open(&dir.join("marf.sqlite"), cfg.external, cfg.compress);

    let mut all_keys: Vec<String> = vec![];
    let mut insert = Measure::default();
    let mut seal = Measure::default();
    let mut commit = Measure::default();
    let mut parent = StacksBlockId::sentinel();
    let mut tip = parent.clone();
    for b in 0..cfg.blocks {
        let child = bid(b as u32 + 1);
        marf.begin(&parent, &child);
        let keys: Vec<String> = (0..cfg.inserts)
            .map(|i| format!("key::{b}::{i}::{}", rng.gen::<u32>()))
            .collect();
        let values: Vec<[u8; 40]> = (0..cfg.inserts)
            .map(|_| {
                let mut v = [0u8; 40];
                rng.fill(&mut v[..]);
                v
            })
            .collect();
        let (_, m) = measure(|| marf.insert_batch(&keys, &values));
        insert += m;
        let (_, m) = measure(|| marf.seal());
        seal += m;
        let (_, m) = measure(|| marf.commit());
        commit += m;
        all_keys.extend(keys);
        tip = child.clone();
        parent = child;
    }
    phases.push(Phase {
        name: "insert".into(),
        ops: (cfg.blocks * cfg.inserts) as u64,
        m: insert,
    });
    phases.push(Phase {
        name: "seal".into(),
        ops: cfg.blocks as u64,
        m: seal,
    });
    phases.push(Phase {
        name: "commit".into(),
        ops: cfg.blocks as u64,
        m: commit,
    });

    // warm lookups at the bulk tip
    let picks: Vec<&String> = (0..cfg.reads)
        .map(|_| &all_keys[rng.gen_range(0..all_keys.len())])
        .collect();
    for k in &picks {
        assert!(marf.get(&tip, k));
    }
    let (_, m) = measure(|| {
        for k in &picks {
            assert!(marf.get(&tip, k));
        }
    });
    phases.push(Phase {
        name: "warm_get".into(),
        ops: cfg.reads as u64,
        m,
    });

    // misses
    let misses: Vec<String> = (0..cfg.reads).map(|i| format!("absent::{i}")).collect();
    for k in &misses {
        assert!(!marf.get(&tip, k));
    }
    let (_, m) = measure(|| {
        for k in &misses {
            assert!(!marf.get(&tip, k));
        }
    });
    phases.push(Phase {
        name: "miss_get".into(),
        ops: cfg.reads as u64,
        m,
    });

    // patch chains: rewrite a few of the first block's keys in each of
    // several small blocks, then look the first block's keys up at the tip
    let bulk_tip = tip.clone();
    let first_block_keys = &all_keys[..cfg.inserts];
    let mut last_patch_keys: Vec<String> = vec![];
    for p in 0..cfg.patch_blocks {
        let child = bid(1000 + p as u32);
        marf.begin(&parent, &child);
        let keys: Vec<String> = (0..cfg.patch_inserts)
            .map(|_| first_block_keys[rng.gen_range(0..first_block_keys.len())].clone())
            .collect();
        let values: Vec<[u8; 40]> = (0..cfg.patch_inserts)
            .map(|_| {
                let mut v = [0u8; 40];
                rng.fill(&mut v[..]);
                v
            })
            .collect();
        marf.insert_batch(&keys, &values);
        marf.commit();
        last_patch_keys = keys;
        tip = child.clone();
        parent = child;
    }
    let picks: Vec<&String> = (0..cfg.reads)
        .map(|_| &first_block_keys[rng.gen_range(0..first_block_keys.len())])
        .collect();
    for k in &picks {
        assert!(marf.get(&tip, k));
    }
    let (_, m) = measure(|| {
        for k in &picks {
            assert!(marf.get(&tip, k));
        }
    });
    phases.push(Phase {
        name: "patched_get".into(),
        ops: cfg.reads as u64,
        m,
    });

    // qualified lookups: bucket keys by the shape of their walk (back-pointer
    // hops, patch chains), measured with the rewrite's profiler on the same
    // files (the stored trie is identical for every implementation), then
    // time each bucket with two access patterns. Two tips: the bulk tip
    // (large blocks, so its root is a full node) and the final tip (small
    // blocks on top, so the root and its hot children are patch chains).
    {
        use stacks_marf_next::marf::TrieHashCalculationMode as Mode;
        let opts = stacks_marf_next::MARFOpenOpts::new(Mode::Deferred, cfg.external)
            .with_compression(cfg.compress);
        let storage = stacks_marf_next::marf::TrieFileStorage::<StacksBlockId>::open_readonly(
            dir.join("marf.sqlite"),
            opts,
        )
        .expect("open profiler");
        let mut profiler = stacks_marf_next::MARF::from_storage(storage);
        let sample = (cfg.reads * 3).min(all_keys.len());
        let tips: [(&str, &StacksBlockId, Vec<&String>); 2] = [
            (
                "bulk",
                &bulk_tip,
                (0..sample)
                    .map(|_| &all_keys[rng.gen_range(0..all_keys.len())])
                    .chain(all_keys[all_keys.len() - cfg.inserts..].iter().take(sample / 4))
                    .collect(),
            ),
            (
                "patched",
                &tip,
                (0..sample)
                    .map(|_| &all_keys[rng.gen_range(0..all_keys.len())])
                    .chain(last_patch_keys.iter())
                    .collect(),
            ),
        ];
        for (tip_label, at, candidates) in &tips {
            let mut buckets: std::collections::BTreeMap<(u32, u32), Vec<&String>> = Default::default();
            let mut nodes_sum: std::collections::BTreeMap<(u32, u32), u64> = Default::default();
            for k in candidates {
                let p = profiler.lookup_profile(at, k).expect("profile");
                assert!(p.found);
                let hops = p.backptr_hops.min(3);
                let patch = match p.max_patch_depth {
                    0 => 0,
                    1 | 2 => 1,
                    _ => 2,
                };
                buckets.entry((hops, patch)).or_default().push(k);
                *nodes_sum.entry((hops, patch)).or_default() += u64::from(p.nodes);
            }
            for ((hops, patch), keys) in &buckets {
                if keys.len() < 40 {
                    continue;
                }
                let avg_nodes = *nodes_sum.get(&(*hops, *patch)).unwrap() as f64 / keys.len() as f64;
                let hops_label = if *hops == 3 { "3+".to_string() } else { hops.to_string() };
                let patch_label = ["none", "1-2", "3-4"][*patch as usize];
                let label = format!("get@{tip_label} hops={hops_label} patch={patch_label}");
                // spread: every key of the bucket once per pass, several passes
                let n = keys.len().min(cfg.reads);
                let passes = (cfg.reads / n).clamp(2, 50);
                for k in &keys[..n] {
                    assert!(marf.get(at, k));
                }
                let (_, m) = measure(|| {
                    for _ in 0..passes {
                        for k in &keys[..n] {
                            assert!(marf.get(at, k));
                        }
                    }
                });
                phases.push(Phase {
                    name: format!("{label} spread(n={},~{avg_nodes:.1} nodes)", keys.len()),
                    ops: (passes * n) as u64,
                    m,
                });
                // hot: 64 keys cycled
                let hot = &keys[..64.min(keys.len())];
                for k in hot {
                    assert!(marf.get(at, k));
                }
                let (_, m) = measure(|| {
                    for i in 0..cfg.reads {
                        assert!(marf.get(at, hot[i % hot.len()]));
                    }
                });
                phases.push(Phase {
                    name: format!("{label} hot64"),
                    ops: cfg.reads as u64,
                    m,
                });
            }
        }
    }

    // proofs
    let proofs = cfg.reads / 10;
    let picks: Vec<&String> = (0..proofs)
        .map(|_| &all_keys[rng.gen_range(0..all_keys.len())])
        .collect();
    for k in picks.iter().take(10) {
        assert!(marf.get_with_proof(&tip, k));
    }
    let (_, m) = measure(|| {
        for k in &picks {
            assert!(marf.get_with_proof(&tip, k));
        }
    });
    phases.push(Phase {
        name: "proof".into(),
        ops: proofs as u64,
        m,
    });
    phases
}

impl std::ops::AddAssign for Measure {
    fn add_assign(&mut self, rhs: Measure) {
        self.allocs += rhs.allocs;
        self.bytes += rhs.bytes;
        self.elapsed += rhs.elapsed;
    }
}

/// Run once on a fresh directory.
pub fn run_once(which: Impl, cfg: &Config) -> Vec<Phase> {
    let dir = tempfile::tempdir().expect("tempdir");
    match which {
        Impl::Baseline => run_direct::<Baseline>(dir.path(), cfg),
        Impl::Rewrite => run_direct::<Rewrite>(dir.path(), cfg),
    }
}

/// Run `REPEAT` times (default 1): allocation counts come from the first
/// run (they are deterministic), times are the best of all runs.
pub fn run(which: Impl, cfg: &Config) -> Vec<Phase> {
    if std::env::var("ONLY").ok().and_then(|s| Impl::parse(&s)).is_some_and(|o| o != which) {
        return vec![];
    }
    let repeat: usize = std::env::var("REPEAT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1)
        .max(1);
    let mut best = run_once(which, cfg);
    for _ in 1..repeat {
        for (b, p) in best.iter_mut().zip(run_once(which, cfg)) {
            b.m.elapsed = b.m.elapsed.min(p.m.elapsed);
        }
    }
    best
}

/// Print a side-by-side table for both implementations (or one, with
/// `ONLY=baseline|rewrite`).
pub fn report(cfg: &Config) {
    let base = run(Impl::Baseline, cfg);
    let next = run(Impl::Rewrite, cfg);
    println!(
        "storage: {} blobs, {}; {} blocks x {} inserts, {} patch blocks x {} rewrites, {} reads/phase",
        if cfg.external { "external" } else { "sqlite" },
        if cfg.compress { "compressed" } else { "plain" },
        cfg.blocks,
        cfg.inserts,
        cfg.patch_blocks,
        cfg.patch_inserts,
        cfg.reads
    );
    let cols = |label: &str| {
        format!(
            "{:>12} {:>12} {:>10}",
            format!("{label} alloc/op"),
            format!("{label} B/op"),
            format!("{label} us/op")
        )
    };
    let mut header = format!("{:<52} {:>8}", "phase", "ops");
    if !base.is_empty() {
        header.push_str(&format!(" | {}", cols("base")));
    }
    if !next.is_empty() {
        header.push_str(&format!(" | {}", cols("next")));
    }
    println!("{header}");
    let rows = base.len().max(next.len());
    for i in 0..rows {
        let p = base.get(i).or(next.get(i)).expect("row");
        if let (Some(b), Some(n)) = (base.get(i), next.get(i)) {
            assert_eq!(b.name, n.name, "phase order differs between implementations");
        }
        let mut line = format!("{:<52} {:>8}", p.name, p.ops);
        for ph in [base.get(i), next.get(i)].into_iter().flatten() {
            line.push_str(&format!(
                " | {:>12.2} {:>12.0} {:>10.2}",
                ph.allocs_per_op(),
                ph.bytes_per_op(),
                ph.micros_per_op()
            ));
        }
        println!("{line}");
    }
}
