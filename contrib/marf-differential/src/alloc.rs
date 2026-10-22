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
        // SAFETY: the caller supplies the valid layout required by GlobalAlloc.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size() as u64, Ordering::Relaxed);
        // SAFETY: the caller guarantees ptr and layout identify a live allocation.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note(layout.size());
        // SAFETY: the caller supplies the valid layout required by GlobalAlloc.
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        LIVE.fetch_sub(layout.size() as u64, Ordering::Relaxed);
        note(new_size);
        // SAFETY: the caller guarantees a live allocation and valid replacement size.
        unsafe { System.realloc(ptr, layout, new_size) }
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

/// Read-path counters normalized across implementations. `available` is
/// false when the build or the implementation has no counters at all (every
/// field is then `None`); a `None` field in an available set means that
/// implementation does not measure that quantity. Meanings:
///
/// * `node_visits`: nodes a walk visited (arena nodes and storage records
///   after any patch resolution), one per lookup step.
/// * `backptr_follows`: back-pointers followed (block changes).
/// * `record_reads`: records parsed from storage bytes (plain nodes, patches
///   and patch bases), not physical I/O: the bytes come from the mapping or
///   SQLite's buffer.
/// * `patch_cache_probes`: lookups in the resolved-patch cache, one per node
///   read while the cache is enabled, before the record is read (both
///   implementations probe by location first).
/// * `patch_cache_hits`: probes answered by the cache.
/// * `patch_cache_probe_miss_plain`: probes that missed and whose record was
///   a plain node (pure probe overhead).
/// * `patch_cache_admissions`: resolved chains stored in the cache.
/// * `patch_cache_clears`: times the cache was emptied for being full
///   (rewrite only; the reference evicts one entry at a time).
/// * `patch_resolutions`: patch chains actually resolved (cache enabled or
///   not); `patch_applications`: individual patches applied in them.
/// * `owned_materializations`: owned node values built on the read path
///   (patch bases decoded for resolution, owned decodes of records).
/// * `location_sql`, `block_id_sql`: SQL lookups for a block's storage
///   location and for a block hash's id (rewrite; the reference does not
///   expose equivalents).
/// * `result_cache_*`: the reference's lookup-result cache (the rewrite and
///   upstream have none).
/// * `cache_occupancy`: entries in the resolved-patch cache when the
///   counters were taken.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Counters {
    pub available: bool,
    pub node_visits: Option<u64>,
    pub backptr_follows: Option<u64>,
    pub record_reads: Option<u64>,
    pub patch_cache_probes: Option<u64>,
    pub patch_cache_hits: Option<u64>,
    pub patch_cache_probe_miss_plain: Option<u64>,
    pub patch_cache_admissions: Option<u64>,
    pub patch_cache_clears: Option<u64>,
    pub patch_resolutions: Option<u64>,
    pub patch_applications: Option<u64>,
    pub owned_materializations: Option<u64>,
    pub location_sql: Option<u64>,
    pub block_id_sql: Option<u64>,
    pub result_cache_hits: Option<u64>,
    pub result_cache_misses: Option<u64>,
    pub result_cache_invalidations: Option<u64>,
    pub cache_occupancy: Option<u64>,
    /// Diagnostic late-probe build only: probes made after the first patch
    /// record of a mapped chain was parsed (work the production order does
    /// not do before a hit). `None` elsewhere.
    pub late_probe_extra_parses: Option<u64>,
}

impl Counters {
    pub fn fields(&self) -> Vec<(&'static str, Option<u64>)> {
        vec![
            ("node_visits", self.node_visits),
            ("backptr_follows", self.backptr_follows),
            ("record_reads", self.record_reads),
            ("patch_cache_probes", self.patch_cache_probes),
            ("patch_cache_hits", self.patch_cache_hits),
            (
                "patch_cache_probe_miss_plain",
                self.patch_cache_probe_miss_plain,
            ),
            ("patch_cache_admissions", self.patch_cache_admissions),
            ("patch_cache_clears", self.patch_cache_clears),
            ("patch_resolutions", self.patch_resolutions),
            ("patch_applications", self.patch_applications),
            ("owned_materializations", self.owned_materializations),
            ("location_sql", self.location_sql),
            ("block_id_sql", self.block_id_sql),
            ("result_cache_hits", self.result_cache_hits),
            ("result_cache_misses", self.result_cache_misses),
            (
                "result_cache_invalidations",
                self.result_cache_invalidations,
            ),
            ("cache_occupancy", self.cache_occupancy),
            ("late_probe_extra_parses", self.late_probe_extra_parses),
        ]
    }

    /// The counters of the later take (used where a cell's warm passes are
    /// taken once, after all of them).
    pub fn max_merge(self, later: Counters) -> Counters {
        later
    }

    /// `name=value` for every measured field, `name=n/a` for unavailable
    /// ones, zeros omitted.
    pub fn summary(&self) -> String {
        if !self.available {
            return "(no counters in this build)".into();
        }
        self.fields()
            .into_iter()
            .filter(|(_, v)| *v != Some(0))
            .map(|(k, v)| match v {
                Some(v) => format!("{k}={v}"),
                None => format!("{k}=n/a"),
            })
            .collect::<Vec<_>>()
            .join(" ")
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
    /// `begin` that reports failure instead of panicking (failed-begin path).
    fn try_begin(&mut self, parent: &StacksBlockId, child: &StacksBlockId) -> bool;
    /// Returns whether the key was present.
    fn get(&mut self, tip: &StacksBlockId, key: &str) -> bool;
    /// The value stored for `key` at `tip`, if any.
    fn get_value(&mut self, tip: &StacksBlockId, key: &str) -> Option<[u8; 40]>;
    fn get_with_proof(&mut self, tip: &StacksBlockId, key: &str) -> bool;
    /// The MARF root hash at `tip` (`None` if the block is unknown).
    fn root_hash(&mut self, tip: &StacksBlockId) -> Option<[u8; 32]>;
    /// Take the implementation's read-path counters since the last take.
    fn counters(&mut self) -> Counters;
    /// Entries in the resolved-patch cache now, if the build exposes it.
    fn cache_occupancy(&mut self) -> Option<u64>;
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
fn baseline_counters(_occupancy: Option<u64>) -> Counters {
    Counters::default()
}

/// Upstream has no patch cache.
fn baseline_occupancy(_m: &mut stacks_marf::MARF<StacksBlockId>) -> Option<u64> {
    None
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
    fn try_begin(&mut self, parent: &StacksBlockId, child: &StacksBlockId) -> bool {
        self.0.begin(parent, child).is_ok()
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
        let occupancy = baseline_occupancy(&mut self.0);
        baseline_counters(occupancy)
    }
    fn cache_occupancy(&mut self) -> Option<u64> {
        baseline_occupancy(&mut self.0)
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
        let mut o =
            stacks_marf_next::MARFOpenOpts::new(TrieHashCalculationMode::Deferred, opts.external)
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
        let values: Vec<stacks_marf_next::MARFValue> = values
            .iter()
            .map(|v| stacks_marf_next::MARFValue(*v))
            .collect();
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
    fn try_begin(&mut self, parent: &StacksBlockId, child: &StacksBlockId) -> bool {
        self.0.begin(parent, child).is_ok()
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
        if !stacks_marf_next::stats::ENABLED {
            return Counters::default();
        }
        let c = stacks_marf_next::stats::take();
        Counters {
            available: true,
            node_visits: Some(c.node_visits),
            backptr_follows: Some(c.backptr_follows),
            record_reads: Some(c.record_reads),
            patch_cache_probes: Some(c.patch_cache_lookups),
            patch_cache_hits: Some(c.patch_cache_hits),
            patch_cache_probe_miss_plain: Some(c.patch_cache_probe_miss_plain),
            patch_cache_admissions: Some(c.patch_cache_misses),
            patch_cache_clears: Some(c.patch_cache_clears),
            patch_resolutions: Some(c.patch_resolutions),
            patch_applications: Some(c.patch_applications),
            owned_materializations: Some(c.owned_materializations),
            location_sql: Some(c.location_sql),
            block_id_sql: Some(c.block_id_sql),
            result_cache_hits: None,
            result_cache_misses: None,
            result_cache_invalidations: None,
            cache_occupancy: self.cache_occupancy(),
            late_probe_extra_parses: None,
        }
    }
    fn cache_occupancy(&mut self) -> Option<u64> {
        #[cfg(feature = "counters")]
        {
            Some(self.0.patch_cache_len() as u64)
        }
        #[cfg(not(feature = "counters"))]
        {
            None
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

/// Identity of this run for the machine-readable output: harness,
/// experiment, source revisions, provider, binary hash, features, arguments.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Manifest {
    pub harness: &'static str,
    pub experiment: String,
    pub args: Vec<String>,
    /// Revision of the tree the harness and the rewrite were built from
    /// (`MARF_BENCH_TREE_REV` at build time).
    pub tree_rev: String,
    /// The oracle's source (`MARF_BENCH_PROVIDER` at build time: upstream
    /// in-tree, or the reference revision for an overlay build).
    pub oracle: String,
    pub binary_sha256: String,
    pub rustc: String,
    pub profile: &'static str,
    pub counters: bool,
    pub alloc_count: bool,
    pub reference_lateprobe: bool,
    pub repeat: usize,
    pub env: String,
    pub started: String,
}

impl Manifest {
    pub fn new(experiment: &str) -> Manifest {
        let exe = std::env::current_exe()
            .ok()
            .and_then(|p| std::fs::read(p).ok())
            .map(|b| stacks_common::util::hash::Sha256Sum::from_data(&b).to_hex())
            .unwrap_or_else(|| "unknown".into());
        Manifest {
            harness: "marf-differential",
            experiment: experiment.to_string(),
            args: std::env::args().skip(1).collect(),
            tree_rev: option_env!("MARF_BENCH_TREE_REV")
                .unwrap_or("unknown")
                .to_string(),
            oracle: option_env!("MARF_BENCH_PROVIDER")
                .unwrap_or("upstream stacks-marf (in tree)")
                .to_string(),
            binary_sha256: exe,
            rustc: option_env!("MARF_BENCH_RUSTC")
                .unwrap_or("unknown")
                .to_string(),
            profile: if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            },
            counters: cfg!(feature = "counters"),
            alloc_count: ALLOC_COUNTING,
            reference_lateprobe: cfg!(feature = "reference-lateprobe"),
            repeat: repeat(),
            env: environment(),
            started: now_rfc3339(),
        }
    }
}

pub fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // civil date from days since epoch (Howard Hinnant's algorithm)
    let days = (secs / 86400) as i64;
    let (h, m, s) = ((secs % 86400) / 3600, (secs % 3600) / 60, secs % 60);
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Write `value` as JSON to the path in `JSON_OUT`, if set.
pub fn write_json<T: serde::Serialize>(value: &T) {
    if let Ok(path) = std::env::var("JSON_OUT") {
        let s = serde_json::to_string_pretty(value).expect("json");
        std::fs::write(&path, s).unwrap_or_else(|e| panic!("write {path}: {e}"));
        eprintln!("[json] wrote {path}");
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
        option_env!("MARF_BENCH_RUSTC").unwrap_or("unknown"),
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
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
            .map(|i| format!("key::{b}::{i}::{}", rng.r#gen::<u32>()))
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
            let mut buckets: std::collections::BTreeMap<(u32, u32), Vec<&String>> =
                Default::default();
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
                let avg_nodes =
                    *nodes_sum.get(&(*hops, *patch)).unwrap() as f64 / keys.len() as f64;
                let hops_label = if *hops == 3 {
                    "3+".to_string()
                } else {
                    hops.to_string()
                };
                let patch_label = ["none", "1-2", "3-4"][*patch as usize];
                let label = format!(
                    "get@{tip_label} hops={hops_label} patch={patch_label} ~{avg_nodes:.1}nodes"
                );
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
            assert_eq!(
                b.name, n.name,
                "phase order differs between implementations"
            );
        }
        let mut line = format!("{:<60} {:>8}", p.name, p.ops);
        for ph in [base.get(i), next.get(i)].into_iter().flatten() {
            let per_op = |d: Duration| d.as_secs_f64() * 1e6 / ph.ops.max(1) as f64;
            line.push_str(&format!(
                " | {:>12.2} {:>12.0} {:>10.2} {:>17} {:>11}",
                ph.allocs_per_op(),
                ph.bytes_per_op(),
                ph.micros_per_op(),
                format!(
                    "{:.2}-{:.2}",
                    per_op(ph.samples.min()),
                    per_op(ph.samples.max())
                ),
                ph.m.peak / 1024
            ));
        }
        println!("{line}");
    }
}
