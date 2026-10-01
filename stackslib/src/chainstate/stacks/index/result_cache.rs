//! Bounded lookup results belonging to one mutable trie, with mutation guards.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use rapidhash::RapidHashMap;
use stacks_common::types::chainstate::TrieHash;

use super::TrieLeaf;

/// Default maximum number of complete lookup results per mutable trie.
pub const DEFAULT_RESULT_CACHE_CAPACITY: usize = 2048;

/// Shared invalidation signal for a storage owner and its in-flight mutations.
#[derive(Default)]
pub struct ResultCacheControl {
    /// Changes when cached results must be discarded.
    generation: AtomicU64,
    /// Number of key mutations during which results cannot be read or admitted.
    writes: AtomicUsize,
}

impl ResultCacheControl {
    /// Discard results at the next access without borrowing their owner.
    pub fn invalidate(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    /// Invalidate on drop unless the enclosing operation completes successfully.
    pub fn guard(self: &Arc<Self>) -> ResultCacheGuard {
        ResultCacheGuard {
            control: Arc::clone(self),
            writing: false,
            succeeded: false,
        }
    }
}

/// Invalidates on error, unwinding or an uncommitted storage transaction's drop.
pub struct ResultCacheGuard {
    /// Invalidation signal shared with the owning mutable trie.
    control: Arc<ResultCacheControl>,
    /// Whether this guard also suspends cache reads and admission.
    writing: bool,
    /// Whether the guarded operation completed successfully.
    succeeded: bool,
}

impl ResultCacheGuard {
    /// Mark successful completion; key eviction still takes effect.
    pub fn succeed(mut self) {
        self.succeeded = true;
    }
}

impl Drop for ResultCacheGuard {
    fn drop(&mut self) {
        if !self.succeeded {
            self.control.invalidate();
        }
        if self.writing {
            self.control.writes.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// An occupied slot in the indexed doubly linked LRU.
struct CachedResult {
    /// Hashed MARF key, also retained to remove its hash-table entry on eviction.
    key: TrieHash,
    /// Successful lookup result; `None` caches absence, never a storage error.
    value: Option<TrieLeaf>,
    /// Previous slot toward the least recently used end.
    previous: Option<usize>,
    /// Next slot toward the most recently used end.
    next: Option<usize>,
}

/// Hash-indexed lookup cache; hits, invalidation and LRU promotion require no scan.
pub struct ResultCache {
    /// Key-to-slot index, allocated lazily.
    index: RapidHashMap<TrieHash, usize>,
    /// Stable slots holding results and recency links.
    slots: Vec<Option<CachedResult>>,
    /// Vacant slots available for reuse after per-key invalidation.
    free: Vec<usize>,
    /// Least recently used occupied slot.
    oldest: Option<usize>,
    /// Most recently used occupied slot.
    newest: Option<usize>,
    /// Maximum number of occupied slots; zero disables the cache.
    capacity: usize,
    /// Owner's state-reset and mutation signal.
    control: Arc<ResultCacheControl>,
    /// Last generation observed by this cache.
    generation: u64,
}

impl Clone for ResultCache {
    /// A cloned trie starts with an independent empty result cache.
    fn clone(&self) -> Self {
        Self::new(self.capacity, Arc::default())
    }
}

impl Default for ResultCache {
    fn default() -> Self {
        Self::new(0, Arc::default())
    }
}

impl ResultCache {
    /// Create an empty cache attached to its owner's invalidation signal.
    pub fn new(capacity: usize, control: Arc<ResultCacheControl>) -> Self {
        let generation = control.generation.load(Ordering::Relaxed);
        Self {
            index: RapidHashMap::default(),
            slots: Vec::new(),
            free: Vec::new(),
            oldest: None,
            newest: None,
            capacity,
            control,
            generation,
        }
    }

    /// Discard all results while retaining allocated buffers.
    pub fn clear(&mut self) {
        self.index.clear();
        self.slots.clear();
        self.free.clear();
        self.oldest = None;
        self.newest = None;
    }

    /// Apply pending invalidation and check whether lookup/admission is enabled.
    fn ready(&mut self) -> bool {
        if self.capacity == 0 {
            return false;
        }
        let generation = self.control.generation.load(Ordering::Relaxed);
        if generation != self.generation {
            self.clear();
            self.generation = generation;
        }
        self.control.writes.load(Ordering::Relaxed) == 0
    }

    /// Return a cached value/absence, or outer `None` when no entry is usable.
    pub fn get(&mut self, key: &TrieHash) -> Option<Option<TrieLeaf>> {
        if !self.ready() {
            return None;
        }
        let slot = *self.index.get(key)?;
        self.unlink(slot);
        self.append(slot);
        Some(
            self.slots[slot]
                .as_ref()
                .expect("occupied result")
                .value
                .clone(),
        )
    }

    /// Admit a successful lookup result, evicting the oldest entry when full.
    pub fn put(&mut self, key: TrieHash, value: Option<TrieLeaf>) {
        if !self.ready() {
            return;
        }
        self.remove(&key);
        if self.index.len() == self.capacity {
            let oldest = self.oldest.expect("full result cache");
            let key = self.slots[oldest].as_ref().expect("oldest result").key;
            self.remove(&key);
        }
        let slot = self.free.pop().unwrap_or_else(|| {
            self.slots.push(None);
            self.slots.len() - 1
        });
        self.slots[slot] = Some(CachedResult {
            key,
            value,
            previous: None,
            next: None,
        });
        self.index.insert(key, slot);
        self.append(slot);
    }

    /// Evict the changed key and suspend all result reuse until mutation ends.
    pub fn begin_write(&mut self, key: &TrieHash) -> ResultCacheGuard {
        self.ready();
        self.remove(key);
        self.control.writes.fetch_add(1, Ordering::Relaxed);
        ResultCacheGuard {
            control: Arc::clone(&self.control),
            writing: true,
            succeeded: false,
        }
    }

    /// Invalidate all results before an untracked low-level node mutation.
    pub fn before_raw_write(&mut self) {
        if self.control.writes.load(Ordering::Relaxed) == 0 {
            self.clear();
        }
    }

    /// Remove a key and recycle its slot.
    fn remove(&mut self, key: &TrieHash) {
        if let Some(slot) = self.index.remove(key) {
            self.unlink(slot);
            self.slots[slot] = None;
            self.free.push(slot);
        }
    }

    /// Detach an occupied slot from its current position.
    fn unlink(&mut self, slot: usize) {
        let entry = self.slots[slot].as_ref().expect("occupied result");
        let (previous, next) = (entry.previous, entry.next);
        if let Some(p) = previous {
            self.slots[p].as_mut().expect("previous result").next = next;
        } else {
            self.oldest = next;
        }
        if let Some(n) = next {
            self.slots[n].as_mut().expect("next result").previous = previous;
        } else {
            self.newest = previous;
        }
    }

    /// Place an occupied slot at the most recently used end.
    fn append(&mut self, slot: usize) {
        let entry = self.slots[slot].as_mut().expect("occupied result");
        entry.previous = self.newest;
        entry.next = None;
        if let Some(n) = self.newest {
            self.slots[n].as_mut().expect("newest result").next = Some(slot);
        } else {
            self.oldest = Some(slot);
        }
        self.newest = Some(slot);
    }

    /// Number of currently usable entries for correctness checks.
    #[cfg(test)]
    pub fn len(&mut self) -> usize {
        if self.ready() { self.index.len() } else { 0 }
    }
}

#[cfg(test)]
mod tests {
    use super::super::MARFValue;
    /// Create a logical-only leaf for recency/invalidation model tests.
    fn leaf(value: u32) -> TrieLeaf {
        TrieLeaf::from_value(&[], MARFValue::from(value))
    }

    use std::collections::VecDeque;
    use std::panic::{self, AssertUnwindSafe};

    use super::*;

    /// Cached absence, eviction, invalidation and slot reuse agree with a simple LRU model.
    #[test]
    fn indexed_lru_matches_model() {
        let mut cache = ResultCache::new(7, Arc::default());
        let mut model: VecDeque<(TrieHash, Option<TrieLeaf>)> = VecDeque::new();
        let mut seed = 71u64;
        for _ in 0..20_000 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let key = TrieHash([(seed >> 32) as u8 % 19; 32]);
            let pos = model.iter().position(|(k, _)| *k == key);
            match seed % 4 {
                0 => {
                    let expected = pos.map(|p| {
                        let entry = model.remove(p).unwrap();
                        let value = entry.1.clone();
                        model.push_back(entry);
                        value
                    });
                    assert_eq!(cache.get(&key), expected);
                }
                1 | 2 => {
                    let value = (seed % 4 == 1).then(|| leaf(seed as u32));
                    if let Some(p) = pos {
                        model.remove(p);
                    }
                    if model.len() == 7 {
                        model.pop_front();
                    }
                    model.push_back((key, value.clone()));
                    cache.put(key, value);
                }
                _ => {
                    if let Some(p) = pos {
                        model.remove(p);
                    }
                    let guard = cache.begin_write(&key);
                    assert_eq!(cache.get(&key), None);
                    cache.put(key, None);
                    guard.succeed();
                }
            }
            assert_eq!(cache.len(), model.len());
            assert!(cache.slots.len() <= 7);
        }
    }

    /// Failed mutations, rollback guards and unwinding discard unrelated entries too.
    #[test]
    fn failure_and_clone_invalidation() {
        let control = Arc::new(ResultCacheControl::default());
        let mut cache = ResultCache::new(8, Arc::clone(&control));
        let a = TrieHash([1; 32]);
        let b = TrieHash([2; 32]);
        cache.put(a, None);
        cache.put(b, Some(leaf(2)));
        let guard = cache.begin_write(&a);
        assert_eq!(cache.get(&b), None);
        guard.succeed();
        assert_eq!(cache.get(&b), Some(Some(leaf(2))));
        assert_eq!(cache.clone().len(), 0);
        drop(control.guard());
        assert_eq!(cache.len(), 0);
        cache.put(b, None);
        drop(cache.begin_write(&a));
        assert_eq!(cache.len(), 0);
        cache.put(b, None);
        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            let _guard = cache.begin_write(&a);
            panic!("injected mutation failure");
        }));
        assert!(result.is_err());
        assert_eq!(cache.len(), 0);
        cache.put(a, None);
        cache.before_raw_write();
        assert_eq!(cache.len(), 0);
        let mut disabled = ResultCache::default();
        disabled.put(a, None);
        assert_eq!(disabled.get(&a), None);
    }
}
