//! Verified prefix hints with padding-free locations and bounded clock eviction.

use crate::chainstate::stacks::index::ValueExtent;
use rapidhash::RapidHashMap;

/// Clock second-chance bit, stored above the bounded record length.
const REFERENCED: u32 = 1 << 31;
/// Prefix collision marker; this slot must use the full SQLite index.
const MULTIPLE: u32 = 1 << 30;
/// Low bits available for a length (records are bounded to 32 MiB).
const LENGTH: u32 = MULTIPLE - 1;

/// A sixteen-byte slot with no alignment padding or per-entry generation identifier.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct Entry {
    /// Byte offset in the owner's immutable extent generation.
    offset: u64,
    /// First four bytes of the full commitment, used only as a lookup hint.
    prefix: u32,
    /// Record length plus clock and collision flags.
    length_flags: u32,
}

/// Prefix-indexed cache whose returned locations require full-commitment verification.
#[derive(Debug)]
pub struct DedupCache {
    /// Four-byte prefix to four-byte slot index; neither field needs padding.
    index: RapidHashMap<u32, u32>,
    /// Compact immutable locations and eviction state.
    slots: Vec<Entry>,
    /// Next eviction candidate.
    hand: usize,
    /// Maximum occupied slots; zero disables admission.
    capacity: usize,
    /// Generation shared by every location in this cache.
    store_id: [u8; 16],
}

/// Interpret the first four digest bytes consistently on every host.
fn prefix(hash: &[u8; 40]) -> u32 {
    u32::from_le_bytes(hash[..4].try_into().expect("fixed commitment prefix"))
}

impl DedupCache {
    /// Create a lazily allocated cache belonging to one generation.
    pub fn new(capacity: usize, store_id: [u8; 16]) -> Self {
        assert!(capacity <= u32::MAX as usize);
        Self {
            index: RapidHashMap::default(),
            slots: Vec::new(),
            hand: 0,
            capacity,
            store_id,
        }
    }

    /// Return an unverified candidate, or no hint for unknown/colliding prefixes.
    pub fn get(&mut self, hash: &[u8; 40]) -> Option<ValueExtent> {
        let entry = &mut self.slots[*self.index.get(&prefix(hash))? as usize];
        entry.length_flags |= REFERENCED;
        if entry.length_flags & MULTIPLE != 0 {
            stacks_profiler::diagnostics::count("dedup_prefix_multiple", 1);
            return None;
        }
        Some(ValueExtent {
            store_id: self.store_id,
            offset: entry.offset,
            length: u64::from(entry.length_flags & LENGTH),
        })
    }

    /// Force full-index lookup after a verified commitment mismatch.
    pub fn mark_multiple(&mut self, hash: &[u8; 40]) {
        if let Some(&slot) = self.index.get(&prefix(hash)) {
            self.slots[slot as usize].length_flags |= MULTIPLE;
        }
    }

    /// Admit a bounded location, conservatively marking different locations as collisions.
    pub fn insert(&mut self, hash: [u8; 40], extent: ValueExtent) {
        assert_eq!(extent.store_id, self.store_id);
        assert!(extent.length > 0 && extent.length <= u64::from(LENGTH));
        self.admit(Entry {
            offset: extent.offset,
            prefix: prefix(&hash),
            length_flags: extent.length as u32,
        });
    }

    /// Merge a candidate from the same generation after successful database commit.
    pub fn admit(&mut self, mut entry: Entry) {
        if self.capacity == 0 {
            return;
        }
        entry.length_flags &= !REFERENCED;
        if let Some(&slot) = self.index.get(&entry.prefix) {
            let existing = &mut self.slots[slot as usize];
            if existing.offset != entry.offset
                || (existing.length_flags & LENGTH) != (entry.length_flags & LENGTH)
            {
                existing.length_flags |= MULTIPLE;
            }
            existing.length_flags |= REFERENCED | (entry.length_flags & MULTIPLE);
            return;
        }
        let slot = if self.slots.len() < self.capacity {
            let slot = self.slots.len();
            self.slots.push(entry);
            slot
        } else {
            while self.slots[self.hand].length_flags & REFERENCED != 0 {
                self.slots[self.hand].length_flags &= !REFERENCED;
                self.hand = (self.hand + 1) % self.capacity;
            }
            let slot = self.hand;
            self.index.remove(&self.slots[slot].prefix);
            self.slots[slot] = entry;
            self.hand = (self.hand + 1) % self.capacity;
            stacks_profiler::diagnostics::count("dedup_cache_evictions", 1);
            slot
        };
        self.index.insert(entry.prefix, slot as u32);
    }

    /// Yield compact occupied candidates without reconstructing discarded digest bytes.
    pub fn entries(&self) -> impl Iterator<Item = Entry> + '_ {
        self.slots.iter().copied()
    }

    /// Discard pending candidates while retaining allocated storage.
    pub fn clear(&mut self) {
        self.index.clear();
        self.slots.clear();
        self.hand = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Layout uses every byte; collisions and admission preserve ambiguity until eviction.
    #[test]
    fn packed_layout_and_collision_states() {
        assert_eq!(std::mem::size_of::<Entry>(), 16);
        assert_eq!(std::mem::offset_of!(Entry, offset), 0);
        assert_eq!(std::mem::offset_of!(Entry, prefix), 8);
        assert_eq!(std::mem::offset_of!(Entry, length_flags), 12);
        assert_eq!(std::mem::size_of::<(u32, u32)>(), 8);
        let extent = |offset| ValueExtent {
            store_id: [0; 16],
            offset,
            length: 1 << 25,
        };
        let mut a = [0; 40];
        a[4] = 1;
        let mut b = a;
        b[4] = 2;
        let mut cache = DedupCache::new(1, [0; 16]);
        cache.insert(a, extent(u64::MAX - (1 << 25)));
        assert_eq!(cache.get(&b), Some(extent(u64::MAX - (1 << 25))));
        cache.mark_multiple(&b);
        assert_eq!(cache.get(&a), None);
        let mut published = DedupCache::new(1, [0; 16]);
        for entry in cache.entries() {
            published.admit(entry);
        }
        assert_eq!(published.get(&a), None);
        published.insert([9; 40], extent(99));
        assert_eq!(published.get(&[9; 40]), Some(extent(99)));
        assert_eq!(published.get(&a), None);
    }

    /// Clock eviction preserves refreshed entries, exact identity and bounded storage.
    #[test]
    fn bounded_clock_eviction_and_clear() {
        let extent = |offset| ValueExtent {
            store_id: [7; 16],
            offset,
            length: 88,
        };
        let mut cache = DedupCache::new(2, [7; 16]);
        cache.insert([1; 40], extent(100));
        cache.insert([2; 40], extent(200));
        cache.insert([3; 40], extent(300));
        assert_eq!(cache.get(&[1; 40]), None);
        assert_eq!(cache.get(&[2; 40]), Some(extent(200)));
        cache.insert([4; 40], extent(400));
        assert_eq!(cache.get(&[3; 40]), None);
        assert_eq!(cache.get(&[2; 40]), Some(extent(200)));
        cache.insert([2; 40], extent(201));
        assert_eq!(cache.get(&[2; 40]), None);
        assert_eq!(cache.entries().count(), 2);
        cache.clear();
        assert_eq!(cache.get(&[2; 40]), None);
        cache.insert([9; 40], extent(900));
        assert_eq!(cache.get(&[9; 40]), Some(extent(900)));
        let mut disabled = DedupCache::new(0, [7; 16]);
        disabled.insert([1; 40], extent(1));
        assert_eq!(disabled.get(&[1; 40]), None);
    }
}
