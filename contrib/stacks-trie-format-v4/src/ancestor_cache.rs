//! Rebuildable ancestor offset cache with immutable mapped chunks and a bounded mutable tail.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;

use memmap2::{Mmap, MmapOptions};
use rusqlite::Connection;

use crate::migration::Result;

/// Chunk size is page aligned and divisible by the fixed 16-byte relocation pair width.
const CHUNK: usize = 64 * 1024 * 1024;
/// Bound metadata independently of sparse database-local block IDs.
const MAX_PLANS: usize = 16 * 1024 * 1024;

/// Location of one finalized offset map in the append-only cache.
struct Descriptor {
    /// Positive MARF database-local ID; mined rows never serve as ancestors.
    block: u32,
    /// Starting pair number.
    start: u64,
    /// Number of relocation pairs.
    count: u64,
}

/// Cache rebuilt from committed SQL plans on each converter launch.
pub struct AncestorCache {
    /// Converter-owned disposable file; mapped prefixes never change.
    file: File,
    /// Completed immutable chunks.
    chunks: Vec<Mmap>,
    /// Unpublished tail, capped at one chunk.
    tail: Vec<u8>,
    /// Sorted block descriptors, independent of the largest block ID.
    descriptors: Vec<Descriptor>,
    /// Total appended pairs.
    pairs: u64,
}

impl AncestorCache {
    /// Rebuild from authoritative SQL plans; callers hold the exclusive converter lock.
    pub fn rebuild(path: &Path, db: &Connection) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        let mut result = Self {
            file,
            chunks: vec![],
            tail: Vec::with_capacity(CHUNK),
            descriptors: vec![],
            pairs: 0,
        };
        let mut query = db.prepare("SELECT block_id,offsets,length FROM migration.plans WHERE block_id>0 ORDER BY block_id")?;
        let mut rows = query.query([])?;
        while let Some(row) = rows.next()? {
            let block: u32 = row.get(0)?;
            let bytes: Vec<u8> = row.get(1)?;
            result.publish(i64::from(block), &bytes, row.get(2)?)?;
        }
        Ok(result)
    }

    /// Append a finalized plan without mapping or allocating on each trie.
    pub fn publish(&mut self, block: i64, bytes: &[u8], length: u64) -> Result<()> {
        if block < 0 {
            return Ok(());
        }
        let block = u32::try_from(block)?;
        if block == 0
            || bytes.is_empty()
            || bytes.len() % 16 != 0
            || self.descriptors.len() == MAX_PLANS
        {
            return Err("invalid or oversized ancestor cache entry".into());
        }
        let index = self
            .descriptors
            .binary_search_by_key(&block, |entry| entry.block)
            .err()
            .ok_or("duplicate ancestor plan")?;
        let mut previous = None;
        for pair in bytes.chunks_exact(16) {
            let old = u64::from_le_bytes(pair[..8].try_into()?);
            let new = u64::from_le_bytes(pair[8..].try_into()?);
            if previous.is_none() && (old != 36 || new != 36) {
                return Err("invalid ancestor root relocation".into());
            }
            if new >= length || previous.is_some_and(|(a, b)| old <= a || new <= b) {
                return Err("invalid ancestor relocation ordering".into());
            }
            previous = Some((old, new));
        }
        let start = self.pairs;
        let mut remaining = bytes;
        while !remaining.is_empty() {
            let take = remaining.len().min(CHUNK - self.tail.len());
            self.tail.extend_from_slice(&remaining[..take]);
            remaining = &remaining[take..];
            if self.tail.len() == CHUNK {
                let offset = self.chunks.len() as u64 * CHUNK as u64;
                self.file.write_all(&self.tail)?;
                // SAFETY: completed prefixes are never modified or truncated while this owner lives.
                let map = unsafe {
                    MmapOptions::new()
                        .offset(offset)
                        .len(CHUNK)
                        .map(&self.file)?
                };
                self.chunks.push(map);
                self.tail.clear();
            }
        }
        let count = bytes.len() as u64 / 16;
        self.pairs = self
            .pairs
            .checked_add(count)
            .ok_or("ancestor pair overflow")?;
        self.descriptors.insert(
            index,
            Descriptor {
                block,
                start,
                count,
            },
        );
        Ok(())
    }

    /// Read one exact pair from an immutable chunk or the owned tail.
    fn pair(&self, index: u64) -> (u64, u64) {
        let position = usize::try_from(index).expect("cache fits address space") * 16;
        let chunk = position / CHUNK;
        let offset = position % CHUNK;
        let bytes = if chunk < self.chunks.len() {
            &self.chunks[chunk][offset..offset + 16]
        } else {
            &self.tail[offset..offset + 16]
        };
        (
            u64::from_le_bytes(bytes[..8].try_into().expect("pair width")),
            u64::from_le_bytes(bytes[8..].try_into().expect("pair width")),
        )
    }

    /// Resolve an already finalized ancestor without SQLite calls or per-lookup allocations.
    pub fn resolve(&self, block: u32, offset: u64) -> std::result::Result<u64, String> {
        let index = self
            .descriptors
            .binary_search_by_key(&block, |entry| entry.block)
            .map_err(|_| format!("ancestor {block} must be planned before its dependent trie"))?;
        let entry = &self.descriptors[index];
        let (mut low, mut high) = (0, entry.count);
        while low < high {
            let mid = low + (high - low) / 2;
            let pair = self.pair(entry.start + mid);
            match pair.0.cmp(&offset) {
                std::cmp::Ordering::Less => low = mid + 1,
                std::cmp::Ordering::Greater => high = mid,
                std::cmp::Ordering::Equal => return Ok(pair.1),
            }
        }
        Err(format!(
            "ancestor {block} offset {offset} is not a record boundary"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mapped chunks, tail lookups, sparse IDs and checkpoint-only recovery share one representation.
    #[test]
    fn chunk_boundary_and_rebuild_preserve_only_committed_plans() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cache.tmp");
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("ATTACH DATABASE ':memory:' AS migration; CREATE TABLE migration.plans(block_id INTEGER PRIMARY KEY,offsets BLOB,length INTEGER)").unwrap();
        let count = CHUNK / 16 + 2;
        let mut bytes = Vec::with_capacity(count * 16);
        for index in 0..count as u64 {
            bytes.extend((36 + index * 3).to_le_bytes());
            bytes.extend((36 + index * 2).to_le_bytes());
        }
        let length = 36 + count as u64 * 2;
        let mut cache = AncestorCache::rebuild(&path, &db).unwrap();
        cache.publish(1, &bytes, length).unwrap();
        assert_eq!(cache.chunks.len(), 1);
        assert_eq!(cache.tail.len(), 32);
        for index in [0, count / 2, count - 3, count - 2, count - 1] {
            assert_eq!(
                cache.resolve(1, 36 + index as u64 * 3).unwrap(),
                36 + index as u64 * 2
            );
        }
        assert!(cache.resolve(1, 37).is_err());
        assert!(cache.resolve(2, 36).is_err());
        assert!(cache.publish(1, &bytes, length).is_err());
        let small = [36u64.to_le_bytes(), 36u64.to_le_bytes()].concat();
        cache.publish(i64::from(u32::MAX), &small, 40).unwrap();
        assert_eq!(cache.descriptors.len(), 2);
        assert_eq!(cache.resolve(u32::MAX, 36).unwrap(), 36);
        db.execute(
            "INSERT INTO migration.plans VALUES(1,?1,?2)",
            rusqlite::params![bytes, length],
        )
        .unwrap();
        drop(cache);
        let cache = AncestorCache::rebuild(&path, &db).unwrap();
        assert_eq!(
            cache.resolve(1, 36 + (count as u64 - 1) * 3).unwrap(),
            36 + (count as u64 - 1) * 2
        );
        assert!(
            cache.resolve(u32::MAX, 36).is_err(),
            "uncheckpointed cache entry survived restart"
        );
    }
}
