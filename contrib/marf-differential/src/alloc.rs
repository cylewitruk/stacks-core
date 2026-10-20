//! Allocation and time measurement of the two implementations' hot paths,
//! calling their public APIs directly (no op DSL, no observation encoding)
//! so that what is counted is the MARF's own behaviour.
//!
//! The binary installs [`Counting`] as the global allocator; `measure`
//! brackets a closure and reports allocations, allocated bytes, the peak of
//! live heap above where it started, and time.
//! Phases: `insert` (batch inserts into the trie being built), `seal`
//! (hashing), `commit` (serialization + storage write), `warm_get`
//! (repeated lookups of present keys at a committed tip), `patched_get`
//! (lookups through nodes stored as patch chains), `miss_get` (absent
//! keys), the lookups bucketed by walk shape (see below), `proof` (lookup
//! with Merkle proof) and `prune` (squash the whole chain into a new
//! database; ops = nodes collected, so the peak column is the squash's
//! high-water mark of live heap).

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
/// Bytes currently allocated, and the highest that has been since `measure`
/// last reset it.
static LIVE: AtomicU64 = AtomicU64::new(0);
static PEAK: AtomicU64 = AtomicU64::new(0);

#[inline]
fn note(size: usize) {
    ALLOCS.fetch_add(1, Ordering::Relaxed);
    BYTES.fetch_add(size as u64, Ordering::Relaxed);
    let live = LIVE.fetch_add(size as u64, Ordering::Relaxed) + size as u64;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note(layout.size());
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size() as u64, Ordering::Relaxed);
        System.dealloc(ptr, layout)
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note(layout.size());
        System.alloc_zeroed(layout)
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        LIVE.fetch_sub(layout.size() as u64, Ordering::Relaxed);
        note(new_size);
        System.realloc(ptr, layout, new_size)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Measure {
    pub allocs: u64,
    pub bytes: u64,
    /// High-water mark of live heap during the measurement, above the live
    /// heap when it started.
    pub peak: u64,
    pub elapsed: Duration,
}

/// Run `f`, returning its result and what it allocated.
#[inline(never)]
pub fn measure<R>(f: impl FnOnce() -> R) -> (R, Measure) {
    let a0 = ALLOCS.load(Ordering::SeqCst);
    let b0 = BYTES.load(Ordering::SeqCst);
    let live0 = LIVE.load(Ordering::SeqCst);
    PEAK.store(live0, Ordering::SeqCst);
    let t = Instant::now();
    let r = f();
    let elapsed = t.elapsed();
    let m = Measure {
        allocs: ALLOCS.load(Ordering::SeqCst) - a0,
        bytes: BYTES.load(Ordering::SeqCst) - b0,
        peak: PEAK.load(Ordering::SeqCst).saturating_sub(live0),
        elapsed,
    };
    (r, m)
}

/// Cache settings an experiment asks for. `None` leaves an implementation's
/// production default in place; the effective settings are reported by
/// [`Direct::describe`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Opts {
    pub external: bool,
    pub compress: bool,
    /// Resolved-patch cache entries (rewrite: `patch_cache_capacity`,
    /// reference: `resolved_patch_cache_capacity`; upstream has none).
    pub patch_cache_capacity: Option<usize>,
    /// Lookup-result cache entries (reference only: serves the active
    /// mutable trie; neither upstream nor the rewrite has one).
    pub result_cache_capacity: Option<usize>,
}

/// Read-path counters normalized across implementations (zero where an
/// implementation has no such thing; `available` is false when the build
/// has no counters at all).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    pub available: bool,
    pub node_visits: u64,
    pub backptr_follows: u64,
    pub record_reads: u64,
    pub patch_cache_lookups: u64,
    pub patch_cache_hits: u64,
    pub patch_cache_misses: u64,
    pub patch_cache_clears: u64,
    pub patch_resolutions: u64,
    pub patch_applications: u64,
    pub owned_materializations: u64,
    pub location_sql: u64,
    pub block_id_sql: u64,
    pub result_cache_hits: u64,
    pub result_cache_misses: u64,
    pub result_cache_invalidations: u64,
}

impl Counters {
    pub fn fields(&self) -> Vec<(&'static str, u64)> {
        vec![
            ("node_visits", self.node_visits),
            ("backptr_follows", self.backptr_follows),
            ("record_reads", self.record_reads),
            ("patch_cache_lookups", self.patch_cache_lookups),
            ("patch_cache_hits", self.patch_cache_hits),
            ("patch_cache_misses", self.patch_cache_misses),
            ("patch_cache_clears", self.patch_cache_clears),
            ("patch_resolutions", self.patch_resolutions),
            ("patch_applications", self.patch_applications),
            ("owned_materializations", self.owned_materializations),
            ("location_sql", self.location_sql),
            ("block_id_sql", self.block_id_sql),
            ("result_cache_hits", self.result_cache_hits),
            ("result_cache_misses", self.result_cache_misses),
            ("result_cache_invalidations", self.result_cache_invalidations),
        ]
    }
}

/// The operations the experiments need, over either implementation. Every
/// method calls the implementation's public API.
pub trait Direct {
    const NAME: &'static str;
    fn open(path: &Path, opts: &Opts) -> Self;
    /// The effective storage and cache settings, for the log.
    fn describe(&self, opts: &Opts) -> String;
    fn begin(&mut self, parent: &StacksBlockId, child: &StacksBlockId);
    fn insert_batch(&mut self, keys: &[String], values: &[[u8; 40]]);
    fn seal(&mut self);
    fn commit(&mut self);
    /// Abandon the block being built.
    fn drop_current(&mut self);
    /// Returns whether the key was present.
    fn get(&mut self, tip: &StacksBlockId, key: &str) -> bool;
    /// The value stored for `key` at `tip`, if any.
    fn get_value(&mut self, tip: &StacksBlockId, key: &str) -> Option<[u8; 40]>;
    fn get_with_proof(&mut self, tip: &StacksBlockId, key: &str) -> bool;
    /// The MARF root hash at `tip` (`None` if the block is unknown).
    fn root_hash(&mut self, tip: &StacksBlockId) -> Option<[u8; 32]>;
    /// Take the implementation's read-path counters since the last take.
    fn counters(&mut self) -> Counters;
    /// Squash `src` at `height` on `tip`'s fork into `dst`; returns the number
    /// of nodes collected.
    fn prune(src: &Path, dst: &Path, tip: &StacksBlockId, height: u32, external: bool) -> u64;
}

pub struct Baseline(stacks_marf::MARF<StacksBlockId>);

/// Upstream's open options for `opts`. (The reference overlay replaces this
/// to set its mmap, root-cache, patch-cache and result-cache options.)
fn baseline_open_opts(opts: &Opts) -> stacks_marf::MARFOpenOpts {
    use stacks_marf::storage::TrieHashCalculationMode;
    stacks_marf::MARFOpenOpts::new(TrieHashCalculationMode::Deferred, opts.external)
        .with_compression(opts.compress)
}

fn baseline_describe(opts: &Opts) -> String {
    format!(
        "upstream stacks-marf: external={} compress={} (no mmap, no caches; requested patch_cache={:?} result_cache={:?} not applicable)",
        opts.external, opts.compress, opts.patch_cache_capacity, opts.result_cache_capacity
    )
}

/// Upstream has no read counters.
fn baseline_counters() -> Counters {
    Counters::default()
}

impl Direct for Baseline {
    const NAME: &'static str = "base";
    fn open(path: &Path, opts: &Opts) -> Self {
        Baseline(
            stacks_marf::MARF::from_path(path.to_str().unwrap(), baseline_open_opts(opts)).unwrap(),
        )
    }
    fn describe(&self, opts: &Opts) -> String {
        baseline_describe(opts)
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
    fn drop_current(&mut self) {
        self.0.drop_current();
    }
    fn get(&mut self, tip: &StacksBlockId, key: &str) -> bool {
        use stacks_marf::marf::MarfConnection;
        self.0.get(tip, key).unwrap().is_some()
    }
    fn get_value(&mut self, tip: &StacksBlockId, key: &str) -> Option<[u8; 40]> {
        use stacks_marf::marf::MarfConnection;
        self.0.get(tip, key).unwrap().map(|v| v.0)
    }
    fn get_with_proof(&mut self, tip: &StacksBlockId, key: &str) -> bool {
        self.0.get_with_proof(tip, key).unwrap().is_some()
    }
    fn root_hash(&mut self, tip: &StacksBlockId) -> Option<[u8; 32]> {
        self.0.get_root_hash_at(tip).ok().map(|h| h.0)
    }
    fn counters(&mut self) -> Counters {
        baseline_counters()
    }
    fn prune(src: &Path, dst: &Path, tip: &StacksBlockId, height: u32, external: bool) -> u64 {
        use stacks_marf::storage::TrieHashCalculationMode;
        let opts = stacks_marf::MARFOpenOpts::new(TrieHashCalculationMode::Deferred, external);
        stacks_marf::MARF::<StacksBlockId>::squash_to_path(
            src.to_str().unwrap(),
            dst.to_str().unwrap(),
            opts,
            tip,
            height,
            "alloc",
        )
        .unwrap()
        .node_count
    }
}

pub struct Rewrite(stacks_marf_next::MARF<StacksBlockId>);

impl Direct for Rewrite {
    const NAME: &'static str = "next";
    fn open(path: &Path, opts: &Opts) -> Self {
        use stacks_marf_next::marf::TrieHashCalculationMode;
        let mut o = stacks_marf_next::MARFOpenOpts::new(TrieHashCalculationMode::Deferred, opts.external)
            .with_compression(opts.compress);
        if let Some(c) = opts.patch_cache_capacity {
            o = o.with_patch_cache_capacity(c);
        }
        Rewrite(stacks_marf_next::MARF::from_path(path, o).unwrap())
    }
    fn describe(&self, opts: &Opts) -> String {
        format!(
            "rewrite stacks-marf-next: external={} compress={} mmap=always patch_cache={} result_cache=none{}",
            opts.external,
            opts.compress,
            opts.patch_cache_capacity
                .unwrap_or(stacks_marf_next::storage::PATCH_CACHE_CAPACITY),
            if opts.result_cache_capacity.is_some() {
                " (requested result_cache not applicable)"
            } else {
                ""
            }
        )
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
    fn drop_current(&mut self) {
        self.0.drop_current();
    }
    fn get(&mut self, tip: &StacksBlockId, key: &str) -> bool {
        use stacks_marf_next::MarfConnection;
        self.0.get(tip, key).unwrap().is_some()
    }
    fn get_value(&mut self, tip: &StacksBlockId, key: &str) -> Option<[u8; 40]> {
        use stacks_marf_next::MarfConnection;
        self.0.get(tip, key).unwrap().map(|v| v.0)
    }
    fn get_with_proof(&mut self, tip: &StacksBlockId, key: &str) -> bool {
        use stacks_marf_next::MarfConnection;
        self.0.get_with_proof(tip, key).unwrap().is_some()
    }
    fn root_hash(&mut self, tip: &StacksBlockId) -> Option<[u8; 32]> {
        self.0.get_root_hash_at(tip).ok().map(|h| h.0)
    }
    fn counters(&mut self) -> Counters {
        let c = stacks_marf_next::stats::take();
        Counters {
            available: stacks_marf_next::stats::ENABLED,
            node_visits: c.node_visits,
            backptr_follows: c.backptr_follows,
            record_reads: c.record_reads,
            patch_cache_lookups: c.patch_cache_lookups,
            patch_cache_hits: c.patch_cache_hits,
            patch_cache_misses: c.patch_cache_misses,
            patch_cache_clears: c.patch_cache_clears,
            patch_resolutions: c.patch_resolutions,
            patch_applications: c.patch_applications,
            owned_materializations: c.owned_materializations,
            location_sql: c.location_sql,
            block_id_sql: c.block_id_sql,
            result_cache_hits: 0,
            result_cache_misses: 0,
            result_cache_invalidations: 0,
        }
    }
    fn prune(src: &Path, dst: &Path, tip: &StacksBlockId, height: u32, external: bool) -> u64 {
        use stacks_marf_next::marf::TrieHashCalculationMode;
        use stacks_marf_next::pruning::Prune;
        let opts = stacks_marf_next::MARFOpenOpts::new(TrieHashCalculationMode::Deferred, external);
        <stacks_marf_next::MARF<StacksBlockId> as Prune<StacksBlockId>>::prune_to_path(
            src, dst, opts, tip, height, "alloc",
        )
        .unwrap()
        .node_count
    }
}

/// Whether this build counts allocations.
pub const ALLOC_COUNTING: bool = cfg!(feature = "alloc-count");

/// Timing samples of one measurement across repetitions.
#[derive(Clone, Debug, Default)]
pub struct Samples(pub Vec<Duration>);

impl Samples {
    pub fn push(&mut self, d: Duration) {
        self.0.push(d);
    }
    pub fn median(&self) -> Duration {
        if self.0.is_empty() {
            return Duration::ZERO;
        }
        let mut v = self.0.clone();
        v.sort();
        let n = v.len();
        if n % 2 == 1 {
            v[n / 2]
        } else {
            (v[n / 2 - 1] + v[n / 2]) / 2
        }
    }
    pub fn min(&self) -> Duration {
        self.0.iter().copied().min().unwrap_or(Duration::ZERO)
    }
    pub fn max(&self) -> Duration {
        self.0.iter().copied().max().unwrap_or(Duration::ZERO)
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// `REPEAT` from the environment (default 5, minimum 1).
pub fn repeat() -> usize {
    std::env::var("REPEAT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5)
        .max(1)
}

/// One line describing the build and machine, for every log.
pub fn environment() -> String {
    let cpu = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("model name"))
                .map(|l| l.split(':').nth(1).unwrap_or("").trim().to_string())
        })
        .unwrap_or_else(|| "unknown cpu".into());
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0);
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    format!(
        "env: {cpu} x{cores}, linux {kernel}, rustc {}, profile {}, alloc-count={}, counters={}, repeat={}",
        option_env!("MARF_BENCH_RUSTC").unwrap_or("see build log"),
        if cfg!(debug_assertions) { "debug" } else { "release" },
        ALLOC_COUNTING,
        cfg!(feature = "counters"),
        repeat()
    )
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
    /// Allocation counts and peak from the first repetition; `elapsed` is
    /// the median of all repetitions once [`run`] has merged them.
    pub m: Measure,
    pub samples: Samples,
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
    let opts = Opts {
        external: cfg.external,
        compress: cfg.compress,
        patch_cache_capacity: None,
        result_cache_capacity: None,
    };
    let mut marf = M::open(&dir.join("marf.sqlite"), &opts);
    eprintln!("[alloc] {}", marf.describe(&opts));

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
        samples: Samples::default(),
    });
    phases.push(Phase {
        name: "seal".into(),
        ops: cfg.blocks as u64,
        m: seal,
        samples: Samples::default(),
    });
    phases.push(Phase {
        name: "commit".into(),
        ops: cfg.blocks as u64,
        m: commit,
        samples: Samples::default(),
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
        samples: Samples::default(),
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
        samples: Samples::default(),
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
        samples: Samples::default(),
    });

    // qualified lookups: bucket every key by the shape of its walk
    // (back-pointer hops, patch chains), measured with the rewrite's profiler
    // on the same files (the stored trie is identical for every
    // implementation), then time each bucket under three working-set sizes:
    //   hot64     64 distinct keys cycled
    //   fit       up to 1,024 distinct keys cycled
    //   overflow  every distinct key of the bucket, whole passes, only for
    //             buckets of at least 4,096 keys
    // These are locality variations of committed reads. No lookup-result
    // cache takes part: the reference's result cache serves only the active
    // mutable trie (`cached_result` in its storage.rs), and these reads
    // happen after commit; upstream and the rewrite have none. What the
    // working-set size changes is page-cache and patch-cache locality, and
    // which patched nodes recur (see the `ablate` experiment for the
    // patch-cache matrix with counters).
    // Each bucket holds distinct keys and the label gives the number timed.
    // Two tips: the bulk tip (large blocks, so its root is a full node) and
    // the final tip (small blocks on top, so the root and its hot children
    // are patch chains).
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
        // every key once (keys are distinct by construction)
        let _ = &last_patch_keys;
        let tips: [(&str, &StacksBlockId, Vec<&String>); 2] = [
            ("bulk", &bulk_tip, all_keys.iter().collect()),
            ("patched", &tip, all_keys.iter().collect()),
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
            const FIT: usize = 1024;
            const OVERFLOW_MIN: usize = 4096;
            for ((hops, patch), keys) in &buckets {
                if keys.len() < 64 {
                    continue;
                }
                let avg_nodes = *nodes_sum.get(&(*hops, *patch)).unwrap() as f64 / keys.len() as f64;
                let hops_label = if *hops == 3 { "3+".to_string() } else { hops.to_string() };
                let patch_label = ["none", "1-2", "3-4"][*patch as usize];
                let label =
                    format!("get@{tip_label} hops={hops_label} patch={patch_label} ~{avg_nodes:.1}nodes");
                // cycle `set` for cfg.reads lookups (warmed once first)
                let mut cycle = |set: &[&String], name: String| {
                    for k in set {
                        assert!(marf.get(at, k));
                    }
                    let (_, m) = measure(|| {
                        for i in 0..cfg.reads {
                            assert!(marf.get(at, set[i % set.len()]));
                        }
                    });
                    phases.push(Phase {
                        name,
                        ops: cfg.reads as u64,
                        m,
                        samples: Samples::default(),
                    });
                };
                cycle(&keys[..64], format!("{label} hot64"));
                let fit = &keys[..FIT.min(keys.len())];
                cycle(fit, format!("{label} fit(n={})", fit.len()));
                if keys.len() >= OVERFLOW_MIN {
                    // whole passes over the bucket so every key is timed
                    let passes = (cfg.reads * 2 / keys.len()).max(1);
                    for k in keys {
                        assert!(marf.get(at, k));
                    }
                    let (_, m) = measure(|| {
                        for _ in 0..passes {
                            for k in keys {
                                assert!(marf.get(at, k));
                            }
                        }
                    });
                    phases.push(Phase {
                        name: format!("{label} overflow(n={})", keys.len()),
                        ops: (passes * keys.len()) as u64,
                        m,
                        samples: Samples::default(),
                    });
                }
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
        samples: Samples::default(),
    });

    // prune: squash the whole chain at the final tip into a new database.
    // ops = nodes collected, so B/op is bytes allocated per node and the
    // peak column is the squash's high-water mark of live heap.
    drop(marf);
    let height = (cfg.blocks + cfg.patch_blocks - 1) as u32;
    let (nodes, m) = measure(|| {
        M::prune(
            &dir.join("marf.sqlite"),
            &dir.join("pruned.sqlite"),
            &tip,
            height,
            cfg.external,
        )
    });
    phases.push(Phase {
        name: "prune".into(),
        ops: nodes,
        m,
        samples: Samples::default(),
    });
    phases
}

impl std::ops::AddAssign for Measure {
    fn add_assign(&mut self, rhs: Measure) {
        self.allocs += rhs.allocs;
        self.bytes += rhs.bytes;
        self.peak = self.peak.max(rhs.peak);
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

/// Merge one repetition's phases into the accumulated ones: allocation
/// counts and peaks from the first repetition (they are deterministic),
/// every repetition's time kept as a sample, `elapsed` = median.
fn merge(acc: &mut Vec<Phase>, run: Vec<Phase>) {
    if acc.is_empty() {
        *acc = run
            .into_iter()
            .map(|mut p| {
                p.samples.push(p.m.elapsed);
                p
            })
            .collect();
        return;
    }
    for (a, p) in acc.iter_mut().zip(run) {
        assert_eq!(a.name, p.name);
        a.samples.push(p.m.elapsed);
        a.m.elapsed = a.samples.median();
    }
}

/// Run `REPEAT` times (default 5), alternating the implementation order
/// between repetitions when both are measured; times are reported as
/// medians with the min-max spread.
pub fn run_both(cfg: &Config) -> (Vec<Phase>, Vec<Phase>) {
    let only = std::env::var("ONLY").ok().and_then(|s| Impl::parse(&s));
    let mut base = vec![];
    let mut next = vec![];
    for r in 0..repeat() {
        let order = if r % 2 == 0 {
            [Impl::Baseline, Impl::Rewrite]
        } else {
            [Impl::Rewrite, Impl::Baseline]
        };
        for which in order {
            if only.is_some_and(|o| o != which) {
                continue;
            }
            let run = run_once(which, cfg);
            match which {
                Impl::Baseline => merge(&mut base, run),
                Impl::Rewrite => merge(&mut next, run),
            }
        }
    }
    (base, next)
}

/// Print a side-by-side table for both implementations (or one, with
/// `ONLY=baseline|rewrite`).
pub fn report(cfg: &Config) {
    println!("{}", environment());
    let (base, next) = run_both(cfg);
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
            "{:>12} {:>12} {:>10} {:>17} {:>11}",
            format!("{label} alloc/op"),
            format!("{label} B/op"),
            format!("{label} us/op"),
            format!("{label} min-max"),
            format!("{label} peakKB")
        )
    };
    let mut header = format!("{:<60} {:>8}", "phase", "ops");
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
        let mut line = format!("{:<60} {:>8}", p.name, p.ops);
        for ph in [base.get(i), next.get(i)].into_iter().flatten() {
            let per_op = |d: Duration| d.as_secs_f64() * 1e6 / ph.ops.max(1) as f64;
            line.push_str(&format!(
                " | {:>12.2} {:>12.0} {:>10.2} {:>17} {:>11}",
                ph.allocs_per_op(),
                ph.bytes_per_op(),
                ph.micros_per_op(),
                format!("{:.2}-{:.2}", per_op(ph.samples.min()), per_op(ph.samples.max())),
                ph.m.peak / 1024
            ));
        }
        println!("{line}");
    }
}
