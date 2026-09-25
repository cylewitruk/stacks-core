//! Opt-in per-thread operation counters for isolated MARF benchmark experiments.

use std::cell::Cell;

/// Counts of logical lookup operations and backing-record access.
#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
pub struct Counters {
    /// Calls to the storage node-read API, including cache hits and RAM nodes.
    pub logical_node_reads: u64,
    /// Node reads served from an open uncommitted trie.
    pub ram_node_reads: u64,
    /// Eligible committed-root read requests, regardless of cache enablement.
    pub committed_root_requests: u64,
    /// Committed-root requests served by the decoded root LRU.
    pub root_cache_hits: u64,
    /// Eligible cache-enabled requests requiring a new root entry.
    pub root_cache_misses: u64,
    /// Non-patch persisted nodes returned by the borrowed mmap fast path.
    pub mmap_node_returns: u64,
    /// Node or patch records read by the patch-resolution loop.
    pub resolved_item_reads: u64,
    /// Patch records encountered during patch resolution.
    pub patch_records: u64,
    /// Node requests served by the resolved patched-node LRU.
    pub resolved_patch_cache_hits: u64,
    /// Patched-node requests admitted after a cache miss.
    pub resolved_patch_cache_misses: u64,
    /// Read-node visits by the MARF lookup cursor.
    pub cursor_steps: u64,
    /// Inter-trie backpointers followed by the lookup walker.
    pub backpointer_follows: u64,
}

thread_local! {
    /// Counters belonging to the current benchmark thread.
    static COUNTERS: Cell<Counters> = Cell::new(Counters::default());
}

/// Update the current thread's counters without allocating.
#[inline]
pub fn update(f: impl FnOnce(&mut Counters)) {
    COUNTERS.with(|slot| {
        let mut value = slot.get();
        f(&mut value);
        slot.set(value);
    });
}

/// Take accumulated counts and start a new measurement interval.
pub fn take() -> Counters {
    COUNTERS.with(|slot| slot.replace(Counters::default()))
}
