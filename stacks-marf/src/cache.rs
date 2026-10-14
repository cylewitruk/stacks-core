// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020-2026 Stacks Open Internet Foundation
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

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rapidhash::RapidHashMap;

use crate::MarfTrieId;

/// A cached hash and whether its reverse lookup has been published.
struct CachedBlockHash<T> {
    /// Hash resolved for the owner-local block ID.
    hash: T,
    /// Whether eager lookup already populated the reverse cache.
    reverse_cached: bool,
}

/// Cache MARF block hash/block ID lookups.
pub struct BlockHashCache<T: MarfTrieId> {
    /// Mapping between trie blob IDs (i.e. rowids) and the [`MarfTrieId`] of the trie.
    ///
    /// Contents are never evicted; the size of this map grows only at the rate of new Stacks
    /// blocks.
    block_hash_cache: RapidHashMap<u32, CachedBlockHash<T>>,

    /// Mapping between trie blob hashes and their IDs
    block_id_cache: HashMap<T, u32>,
}

impl<T: MarfTrieId> BlockHashCache<T> {
    pub fn new() -> BlockHashCache<T> {
        BlockHashCache {
            block_hash_cache: RapidHashMap::default(),
            block_id_cache: HashMap::new(),
        }
    }

    /// Resolve a hash and populate both lookup directions after success.
    pub fn get_block_hash_caching<E, F: FnOnce(u32) -> Result<T, E>>(
        &mut self,
        id: u32,
        lookup: F,
    ) -> Result<&T, E> {
        self.get_block_hash_caching_impl::<E, F, true>(id, lookup)
    }

    /// Resolve a hash without requiring or populating its reverse lookup.
    pub fn get_block_hash_forward_only<E, F: FnOnce(u32) -> Result<T, E>>(
        &mut self,
        id: u32,
        lookup: F,
    ) -> Result<&T, E> {
        self.get_block_hash_caching_impl::<E, F, false>(id, lookup)
    }

    /// Share lookup/error handling while keeping reverse work off forward-only hits.
    fn get_block_hash_caching_impl<E, F, const CACHE_REVERSE: bool>(
        &mut self,
        id: u32,
        lookup: F,
    ) -> Result<&T, E>
    where
        F: FnOnce(u32) -> Result<T, E>,
    {
        match self.block_hash_cache.entry(id) {
            Entry::Occupied(occupied_entry) => {
                let entry = occupied_entry.into_mut();
                if CACHE_REVERSE && !entry.reverse_cached {
                    self.block_id_cache.entry(entry.hash.clone()).or_insert(id);
                    entry.reverse_cached = true;
                }
                Ok(&entry.hash)
            }
            Entry::Vacant(vacant_entry) => {
                let hash = lookup(id)?;
                if CACHE_REVERSE {
                    self.block_id_cache.insert(hash.clone(), id);
                }
                Ok(&vacant_entry
                    .insert(CachedBlockHash {
                        hash,
                        reverse_cached: CACHE_REVERSE,
                    })
                    .hash)
            }
        }
    }

    /// Publish both directions, accepting a matching existing forward entry.
    /// Returns false without mutation when the ID already resolves to another hash.
    pub fn publish_block_hash(&mut self, block_id: u32, block_hash: T) -> bool {
        match self.block_hash_cache.entry(block_id) {
            Entry::Occupied(mut entry) => {
                if entry.get().hash != block_hash {
                    return false;
                }
                entry.get_mut().reverse_cached = true;
            }
            Entry::Vacant(entry) => {
                entry.insert(CachedBlockHash {
                    hash: block_hash.clone(),
                    reverse_cached: true,
                });
            }
        }
        self.block_id_cache.insert(block_hash, block_id);
        true
    }

    /// Cache a new block ID and hash, rejecting a duplicate ID.
    pub fn store_block_hash(&mut self, block_id: u32, block_hash: T) {
        assert!(!self.block_hash_cache.contains_key(&block_id));
        assert!(self.publish_block_hash(block_id, block_hash));
    }

    /// Get an immutable reference to a block hash, given the ID
    pub fn ref_block_hash(&self, block_id: u32) -> Option<&T> {
        self.block_hash_cache
            .get(&block_id)
            .map(|entry| &entry.hash)
    }

    /// Get a block ID provided its hash
    pub fn load_block_id(&self, block_hash: &T) -> Option<u32> {
        self.block_id_cache.get(block_hash).copied()
    }
}

/// A small array-backed (fixed-capacity, stack-allocated) Least-Recently-Used (LRU) cache.
///
/// Entries are stored in MRU-to-LRU order:
///
/// * Index 0 is the most recently accessed entry.
/// * The last occupied slot is the least-recently-used entry.
/// * On lookup or update, a hit is promoted to index 0.
/// * On insert when full, the least-recently-used entry is evicted.
///
/// ## Notes
///
/// The value of `N` must be greater than zero, which is enforced by a compile-time assertion in
/// [`ArrayLru::new()`]. The following will fail to compile:
///
/// ```compile_fail
/// # use crate::cache::ArrayLru;
/// let _cache = ArrayLru::<u8, u8, 0>::new();
/// ```
#[derive(Debug, Clone)]
pub struct ArrayLru<K, V, const N: usize> {
    /// Entries in most-recently-used order.
    entries: [Option<(K, V)>; N],
    /// Number of occupied entries.
    len: usize,
}

impl<K: Eq, V, const N: usize> ArrayLru<K, V, N> {
    /// Construct an empty fixed-capacity cache.
    pub fn new() -> Self {
        const {
            assert!(N > 0, "ArrayLru capacity must be greater than zero");
        }

        Self {
            entries: core::array::from_fn(|_| None),
            len: 0,
        }
    }

    /// Look up by key.
    ///
    /// On hit, promotes the entry to MRU position.
    pub fn get(&mut self, key: &K) -> Option<&V> {
        let pos = self.entries[..self.len]
            .iter()
            .position(|entry| matches!(entry, Some((k, _)) if k == key))?;

        if pos > 0 {
            // Shift entries [0..pos] right by one and move the hit to position 0, preserving
            // recency order of all other entries (true LRU).
            self.entries[..=pos].rotate_right(1);
        }

        debug_assert!(
            self.entries[0].is_some(),
            "entry promoted to position 0 should always be Some"
        );

        self.entries[0].as_ref().map(|(_, v)| v)
    }

    /// Insert a key-value pair.
    ///
    /// If the key is already present, updates the value and promotes it. Otherwise, inserts at MRU
    /// position, evicting the LRU entry if at capacity.
    pub fn put(&mut self, key: K, value: V) {
        // Update existing entry
        if let Some(pos) = self.entries[..self.len]
            .iter()
            .position(|entry| matches!(entry, Some((k, _)) if k == &key))
        {
            debug_assert!(
                pos < self.entries.len(),
                "search position should always be within array bounds"
            );

            if let Some(slot) = self.entries.get_mut(pos) {
                *slot = Some((key, value));
            }

            if pos > 0 {
                // Shift entries [0..pos] right by one and move the updated entry to position 0,
                // preserving recency order of all other entries (true LRU).
                self.entries[..=pos].rotate_right(1);
            }

            return;
        }

        // Grow if not at capacity
        if self.len < N {
            self.len += 1;
        }

        // Rotate right: moves the current LRU (last position) to index 0, then overwrite it
        // with the new entry. Entries [0..len-1] shift right by one, preserving their order.
        self.entries[..self.len].rotate_right(1);
        self.entries[0] = Some((key, value));
    }

    /// Clear all entries from the cache and reset length to zero.
    pub fn clear(&mut self) {
        for entry in &mut self.entries[..self.len] {
            *entry = None;
        }
        self.len = 0;
    }

    /// Get the number of entries currently in the cache.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Check if the cache contains the specified key.
    pub fn contains_key(&self, key: &K) -> bool {
        self.entries[..self.len]
            .iter()
            .any(|entry| matches!(entry, Some((k, _)) if k == key))
    }
}

/// A bounded LRU for a small working set of expensive decoded values.
#[derive(Clone)]
pub struct SmallLru<K, V> {
    /// Entries ordered from least to most recently used; boxing keeps moves small.
    entries: Vec<(K, Box<V>)>,
    /// Maximum number of retained entries; zero disables admission.
    capacity: usize,
}

impl<K: Eq, V> SmallLru<K, V> {
    /// Create an empty cache without allocating until its first admission.
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: Vec::new(),
            capacity,
        }
    }

    /// Whether the cache can admit entries.
    pub fn enabled(&self) -> bool {
        self.capacity > 0
    }

    /// Whether a key is present, without changing recency.
    pub fn contains_key(&self, key: &K) -> bool {
        self.entries.iter().rev().any(|(k, _)| k == key)
    }

    /// Get an entry and promote it to most recently used.
    pub fn get(&mut self, key: &K) -> Option<&V> {
        let pos = self.entries.iter().rposition(|(k, _)| k == key)?;
        self.entries[pos..].rotate_left(1);
        self.entries.last().map(|(_, value)| value.as_ref())
    }

    /// Insert or update an entry, evicting the least recently used when full.
    pub fn put(&mut self, key: K, value: V) {
        if !self.enabled() {
            return;
        }
        if let Some(pos) = self.entries.iter().rposition(|(k, _)| k == &key) {
            self.entries.remove(pos);
        } else if self.entries.len() == self.capacity {
            self.entries.remove(0);
        }
        self.entries.push((key, Box::new(value)));
    }

    /// Remove all entries while retaining the entry buffer.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Number of retained entries.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Bounded LRU shared by related storage views; values outlive eviction through `Arc`.
pub struct SharedLru<K: Eq, V> {
    /// Cache state and invalidation generation protected by one short-lived lock.
    state: Arc<Mutex<SharedLruState<K, V>>>,
    /// Immutable capacity, allowing disabled caches to bypass locking.
    capacity: usize,
}

/// Recency state and generation updated together under the cache lock.
struct SharedLruState<K: Eq, V> {
    /// Generation used to reject admissions racing with invalidation.
    generation: u64,
    /// Resolved values ordered by recency.
    entries: SmallLru<K, Arc<V>>,
}

impl<K: Eq, V> Clone for SharedLru<K, V> {
    /// Share entries, recency and invalidation with the original owner.
    fn clone(&self) -> Self {
        Self {
            state: Arc::clone(&self.state),
            capacity: self.capacity,
        }
    }
}

impl<K: Eq, V> SharedLru<K, V> {
    /// Create an empty shared cache without allocating entry storage.
    pub fn new(capacity: usize) -> Self {
        Self {
            state: Arc::new(Mutex::new(SharedLruState {
                generation: 0,
                entries: SmallLru::new(capacity),
            })),
            capacity,
        }
    }

    /// Whether this cache accepts entries.
    pub fn enabled(&self) -> bool {
        self.capacity > 0
    }

    /// Promote a hit and return an independent value handle plus the admission generation.
    pub fn get(&self, key: &K) -> (u64, Option<Arc<V>>) {
        if !self.enabled() {
            return (0, None);
        }
        let mut state = self.state.lock().expect("shared MARF cache lock poisoned");
        (state.generation, state.entries.get(key).map(Arc::clone))
    }

    /// Admit a loaded value only if no invalidation occurred while loading it.
    pub fn put_if_current(&self, generation: u64, key: K, value: Arc<V>) {
        if !self.enabled() {
            return;
        }
        let mut state = self.state.lock().expect("shared MARF cache lock poisoned");
        if state.generation == generation {
            state.entries.put(key, value);
        }
    }

    /// Invalidate entries and pending admissions across all related views.
    pub fn clear(&self) {
        let mut state = self.state.lock().expect("shared MARF cache lock poisoned");
        state.generation = state.generation.wrapping_add(1);
        state.entries.clear();
    }

    /// Invalidate on a failed or implicitly rolled-back storage transaction.
    pub fn rollback_guard(&self) -> SharedLruGuard<K, V> {
        SharedLruGuard {
            cache: self.clone(),
            succeeded: false,
        }
    }

    /// Number of retained entries for correctness checks.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.state
            .lock()
            .expect("shared MARF cache lock poisoned")
            .entries
            .len()
    }
}

/// Clears a shared cache unless its storage transaction commits successfully.
pub struct SharedLruGuard<K: Eq, V> {
    /// Cache shared with the transaction's related views.
    cache: SharedLru<K, V>,
    /// Whether the transaction committed successfully.
    succeeded: bool,
}

impl<K: Eq, V> SharedLruGuard<K, V> {
    /// Preserve cached entries after successful commit.
    pub fn succeed(mut self) {
        self.succeeded = true;
    }
}

impl<K: Eq, V> Drop for SharedLruGuard<K, V> {
    fn drop(&mut self) {
        if !self.succeeded {
            self.cache.clear();
        }
    }
}

#[cfg(test)]
mod block_hash_tests {
    use super::BlockHashCache;
    use stacks_common::types::chainstate::BlockHeaderHash;

    /// Successful lookups populate both directions and hits never call storage again.
    #[test]
    fn block_hash_hits_and_reverse_lookup() {
        let mut cache = BlockHashCache::new();
        for (i, id) in [0, 1, 256, 65536, u32::MAX].into_iter().enumerate() {
            let expected = BlockHeaderHash([i as u8; 32]);
            let found = cache
                .get_block_hash_caching(id, |requested| {
                    assert_eq!(requested, id);
                    Ok::<_, ()>(expected.clone())
                })
                .unwrap();
            assert_eq!(found, &expected);
            assert_eq!(cache.ref_block_hash(id), Some(&expected));
            assert_eq!(cache.load_block_id(&expected), Some(id));
            assert_eq!(
                cache.get_block_hash_caching(id, |_| Err(())).unwrap(),
                &expected
            );
        }
    }

    /// Storage errors remain retryable and cannot publish a cache entry.
    #[test]
    fn failed_block_hash_lookup_is_not_cached() {
        let mut cache = BlockHashCache::new();
        let expected = BlockHeaderHash([7; 32]);
        assert_eq!(
            cache.get_block_hash_caching(7, |_| Err("missing")),
            Err("missing")
        );
        assert_eq!(cache.ref_block_hash(7), None);
        assert_eq!(cache.load_block_id(&expected), None);
        assert_eq!(
            cache
                .get_block_hash_caching(7, |_| Ok::<_, &str>(expected.clone()))
                .unwrap(),
            &expected
        );
        assert_eq!(cache.load_block_id(&expected), Some(7));
    }

    /// Independent storage owners may resolve the same local ID to different hashes.
    #[test]
    fn block_hash_cache_owners_remain_independent() {
        let mut first = BlockHashCache::new();
        let mut second = BlockHashCache::new();
        let a = BlockHeaderHash([1; 32]);
        let b = BlockHeaderHash([2; 32]);
        first.store_block_hash(1, a.clone());
        second.store_block_hash(1, b.clone());
        assert_eq!(first.get_block_hash_caching(1, |_| Err(())).unwrap(), &a);
        assert_eq!(second.get_block_hash_caching(1, |_| Err(())).unwrap(), &b);
        assert_eq!(first.load_block_id(&b), None);
        assert_eq!(second.load_block_id(&a), None);
    }

    /// Forward-only lookups cache sparse IDs without allocating a reverse entry.
    #[test]
    fn forward_only_hits_and_sparse_ids() {
        let mut cache = BlockHashCache::new();
        for (i, id) in [0, 1, 256, 65536, u32::MAX].into_iter().enumerate() {
            let expected = BlockHeaderHash([i as u8; 32]);
            assert_eq!(
                cache
                    .get_block_hash_forward_only(id, |requested| {
                        assert_eq!(requested, id);
                        Ok::<_, ()>(expected.clone())
                    })
                    .unwrap(),
                &expected
            );
            assert_eq!(cache.ref_block_hash(id), Some(&expected));
            assert_eq!(cache.load_block_id(&expected), None);
            assert_eq!(
                cache.get_block_hash_forward_only(id, |_| Err(())).unwrap(),
                &expected
            );
            assert!(!cache.block_hash_cache.get(&id).unwrap().reverse_cached);
        }
        assert_eq!(cache.block_hash_cache.len(), 5);
        assert_eq!(cache.block_id_cache.len(), 0);
    }

    /// Eager hits promote forward-only entries without invoking their lookup closure.
    #[test]
    fn eager_lookup_promotes_forward_only_entry() {
        let mut cache = BlockHashCache::new();
        let expected = BlockHeaderHash([7; 32]);
        cache
            .get_block_hash_forward_only(7, |_| Ok::<_, ()>(expected.clone()))
            .unwrap();
        assert_eq!(cache.load_block_id(&expected), None);
        assert_eq!(
            cache.get_block_hash_caching(7, |_| Err(())).unwrap(),
            &expected
        );
        assert_eq!(cache.load_block_id(&expected), Some(7));
        assert!(cache.block_hash_cache.get(&7).unwrap().reverse_cached);
        assert_eq!(
            cache.get_block_hash_caching(7, |_| Err(())).unwrap(),
            &expected
        );
        assert_eq!(cache.block_hash_cache.len(), 1);
        assert_eq!(cache.block_id_cache.len(), 1);
    }

    /// Publication promotes equal pairs and rejects conflicts without changing either map.
    #[test]
    fn pair_publication_checks_existing_forward_hash() {
        let mut cache = BlockHashCache::new();
        let expected = BlockHeaderHash([7; 32]);
        let other = BlockHeaderHash([8; 32]);
        cache
            .get_block_hash_forward_only(7, |_| Ok::<_, ()>(expected.clone()))
            .unwrap();
        assert!(!cache.publish_block_hash(7, other.clone()));
        assert_eq!(cache.ref_block_hash(7), Some(&expected));
        assert_eq!(cache.load_block_id(&expected), None);
        assert_eq!(cache.load_block_id(&other), None);
        assert!(!cache.block_hash_cache.get(&7).unwrap().reverse_cached);
        assert!(cache.publish_block_hash(7, expected.clone()));
        assert_eq!(cache.load_block_id(&expected), Some(7));
        assert!(cache.block_hash_cache.get(&7).unwrap().reverse_cached);
        assert!(cache.publish_block_hash(8, other.clone()));
        assert_eq!(cache.ref_block_hash(8), Some(&other));
        assert_eq!(cache.load_block_id(&other), Some(8));
    }

    /// Failed forward-only reads publish nothing and remain retryable.
    #[test]
    fn failed_forward_only_lookup_is_not_cached() {
        let mut cache = BlockHashCache::new();
        let expected = BlockHeaderHash([7; 32]);
        std::assert_matches!(
            cache.get_block_hash_forward_only(7, |_| Err::<BlockHeaderHash, _>("missing")),
            Err("missing")
        );
        assert_eq!(cache.ref_block_hash(7), None);
        assert_eq!(cache.load_block_id(&expected), None);
        assert_eq!(cache.block_hash_cache.len(), 0);
        assert_eq!(cache.block_id_cache.len(), 0);
        assert_eq!(
            cache
                .get_block_hash_forward_only(7, |_| Ok::<_, &str>(expected.clone()))
                .unwrap(),
            &expected
        );
        assert_eq!(cache.load_block_id(&expected), None);
    }

    /// Forward-only promotion remains local to the storage owner.
    #[test]
    fn forward_only_cache_owners_remain_independent() {
        let mut first = BlockHashCache::new();
        let mut second = BlockHashCache::new();
        let a = BlockHeaderHash([1; 32]);
        let b = BlockHeaderHash([2; 32]);
        first
            .get_block_hash_forward_only(1, |_| Ok::<_, ()>(a.clone()))
            .unwrap();
        second
            .get_block_hash_forward_only(1, |_| Ok::<_, ()>(b.clone()))
            .unwrap();
        assert_eq!(first.get_block_hash_caching(1, |_| Err(())).unwrap(), &a);
        assert_eq!(first.load_block_id(&a), Some(1));
        assert_eq!(second.load_block_id(&b), None);
        assert_eq!(first.load_block_id(&b), None);
        assert_eq!(second.ref_block_hash(1), Some(&b));
        assert_eq!(second.get_block_hash_caching(1, |_| Err(())).unwrap(), &b);
        assert_eq!(second.load_block_id(&b), Some(1));
        assert_eq!(second.load_block_id(&a), None);
    }

    /// Existing eager hits preserve the reverse map's last-published ID for equal hashes.
    #[test]
    fn eager_promotion_preserves_existing_reverse_alias() {
        let mut cache = BlockHashCache::new();
        let hash = BlockHeaderHash([1; 32]);
        cache.store_block_hash(1, hash.clone());
        cache.store_block_hash(2, hash.clone());
        assert_eq!(cache.get_block_hash_caching(1, |_| Err(())).unwrap(), &hash);
        assert_eq!(cache.load_block_id(&hash), Some(2));
        cache
            .get_block_hash_forward_only(3, |_| Ok::<_, ()>(hash.clone()))
            .unwrap();
        assert_eq!(cache.get_block_hash_caching(3, |_| Err(())).unwrap(), &hash);
        assert_eq!(cache.load_block_id(&hash), Some(2));
        cache
            .get_block_hash_caching(4, |_| Ok::<_, ()>(hash.clone()))
            .unwrap();
        assert_eq!(cache.load_block_id(&hash), Some(4));
    }

    /// Direct store retains its duplicate-ID assertion after a forward-only insertion.
    #[test]
    #[should_panic]
    fn store_rejects_existing_forward_only_id() {
        let mut cache = BlockHashCache::new();
        let hash = BlockHeaderHash([1; 32]);
        cache
            .get_block_hash_forward_only(1, |_| Ok::<_, ()>(hash.clone()))
            .unwrap();
        cache.store_block_hash(1, hash);
    }
}
