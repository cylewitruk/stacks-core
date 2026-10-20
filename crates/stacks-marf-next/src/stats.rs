//! Opt-in per-thread operation counters for benchmark experiments
//! (`counters` feature). The hooks (`count!`) compile to nothing without the
//! feature, so release timing runs carry no instrumentation; counter runs
//! are made with a separate build. Same shape as the reference's
//! `read_bench`.

use std::cell::Cell;

/// Counts of what the read path did on the current thread.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counters {
    /// Nodes visited by a walk (arena nodes and storage records, after patch
    /// resolution): one per lookup step.
    pub node_visits: u64,
    /// Back-pointers followed by a walk (each is a change of block context).
    pub backptr_follows: u64,
    /// Records parsed from storage (plain nodes, patches and patch bases).
    pub record_reads: u64,
    /// Records read from SQLite-stored tries (`blob_open` reads).
    pub sqlite_record_reads: u64,
    /// Patch-cache lookups (only when the cache is enabled).
    pub patch_cache_lookups: u64,
    pub patch_cache_hits: u64,
    /// Resolved chains admitted into the cache after a miss.
    pub patch_cache_misses: u64,
    /// Times the cache was emptied for reaching its capacity.
    pub patch_cache_clears: u64,
    /// Patch chains resolved (a read whose record was a patch).
    pub patch_resolutions: u64,
    /// Individual patches applied while resolving chains.
    pub patch_applications: u64,
    /// Owned `Branch`/`Node` values built on the read path: patch bases
    /// decoded for resolution, and `read_node` decodes.
    pub owned_materializations: u64,
    /// `marf_data` location rows fetched (block-location cache misses).
    pub location_sql: u64,
    /// Block hash -> id SQL lookups (identifier cache misses).
    pub block_id_sql: u64,
}

thread_local! {
    static COUNTERS: Cell<Counters> = Cell::new(Counters::default());
}

/// Update the current thread's counters.
#[inline]
pub fn update(f: impl FnOnce(&mut Counters)) {
    COUNTERS.with(|slot| {
        let mut c = slot.get();
        f(&mut c);
        slot.set(c);
    });
}

/// Take the accumulated counts and start a new interval.
pub fn take() -> Counters {
    COUNTERS.with(|slot| slot.replace(Counters::default()))
}

/// Whether this build counts anything.
pub const ENABLED: bool = cfg!(feature = "counters");
