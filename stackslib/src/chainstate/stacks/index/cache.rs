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

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::{Arc, Mutex};

use crate::chainstate::stacks::index::MarfTrieId;

/// Cache MARF block hash/block ID lookups.
pub struct BlockHashCache<T: MarfTrieId> {
    /// Mapping between trie blob IDs (i.e. rowids) and the [`MarfTrieId`] of the trie.
    ///
    /// Contents are never evicted; the size of this map grows only at the rate of new Stacks
    /// blocks.
    block_hash_cache: HashMap<u32, T>,

    /// Mapping between trie blob hashes and their IDs
    block_id_cache: HashMap<T, u32>,
}

impl<T: MarfTrieId> BlockHashCache<T> {
    pub fn new() -> BlockHashCache<T> {
        BlockHashCache {
            block_hash_cache: HashMap::new(),
            block_id_cache: HashMap::new(),
        }
    }

    /// Get cached entry for a block hash, given its ID, or, if not found, use `lookup` to get the
    /// corresponding block hash and store it in the cache
    pub fn get_block_hash_caching<E, F: FnOnce(u32) -> Result<T, E>>(
        &mut self,
        id: u32,
        lookup: F,
    ) -> Result<&T, E> {
        match self.block_hash_cache.entry(id) {
            Entry::Occupied(occupied_entry) => Ok(occupied_entry.into_mut()),
            Entry::Vacant(vacant_entry) => {
                let block_hash = lookup(id)?;
                let block_hash_ref = vacant_entry.insert(block_hash.clone());
                self.block_id_cache.insert(block_hash, id);
                Ok(block_hash_ref)
            }
        }
    }

    /// Cache a block hash provided its ID
    pub fn store_block_hash(&mut self, block_id: u32, block_hash: T) {
        assert!(!self.block_hash_cache.contains_key(&block_id));
        self.block_id_cache.insert(block_hash.clone(), block_id);
        self.block_hash_cache.insert(block_id, block_hash);
    }

    /// Get an immutable reference to a block hash, given the ID
    pub fn ref_block_hash(&self, block_id: u32) -> Option<&T> {
        self.block_hash_cache.get(&block_id)
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
/// # use crate::chainstate::stacks::index::cache::ArrayLru;
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
