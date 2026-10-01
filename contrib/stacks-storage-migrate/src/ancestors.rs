// Copyright (C) 2026 Stacks Open Internet Foundation
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

//! Rebuildable ancestor offset cache with immutable mapped chunks and a bounded mutable tail.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;

use memmap2::{Mmap, MmapOptions};

use crate::Result;

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

/// Append-only relocation maps for already emitted ancestors.
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
    /// Create private all-or-nothing scratch; no published output references this file.
    pub fn create(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;
        Ok(Self {
            file,
            chunks: vec![],
            tail: Vec::with_capacity(CHUNK),
            descriptors: vec![],
            pairs: 0,
        })
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
