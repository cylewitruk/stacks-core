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

//! Runtime capture controls for MARF instrumentation.
//!
//! This module is feature-gated behind `profiler` and provides low-overhead
//! toggles for profiler capture modes.

use std::sync::atomic::{AtomicU64, Ordering};

/// Bitflags that control profiler capture behavior.
pub mod flags {
    /// Capture MARF key strings in `get_by_key`.
    pub const CAPTURE_MARF_KEYS: u64 = 1 << 0;
    /// Capture MARF block-hash lookups in `get_by_path`.
    pub const CAPTURE_MARF_HASH_LOOKUPS: u64 = 1 << 1;
    /// Capture MARF disk-read identifiers (block id).
    pub const CAPTURE_MARF_DISK_READS: u64 = 1 << 3;
}

const DEFAULT_FLAGS: u64 =
    flags::CAPTURE_MARF_KEYS | flags::CAPTURE_MARF_HASH_LOOKUPS | flags::CAPTURE_MARF_DISK_READS;

static PROFILER_FLAGS: AtomicU64 = AtomicU64::new(DEFAULT_FLAGS);

/// Replace the current profiler flags and return the previous value.
#[inline(always)]
pub fn set_flags(flags: u64) -> u64 {
    PROFILER_FLAGS.swap(flags, Ordering::Relaxed)
}

/// Enable the given flags.
#[inline(always)]
pub fn enable(flags: u64) {
    PROFILER_FLAGS.fetch_or(flags, Ordering::Relaxed);
}

/// Disable the given flags.
#[inline(always)]
pub fn disable(flags: u64) {
    PROFILER_FLAGS.fetch_and(!flags, Ordering::Relaxed);
}

/// Returns true if all given flags are enabled.
#[inline(always)]
pub fn is_enabled(flags: u64) -> bool {
    (PROFILER_FLAGS.load(Ordering::Relaxed) & flags) == flags
}

/// Whether MARF key capture is enabled.
#[inline(always)]
pub fn capture_marf_keys() -> bool {
    is_enabled(flags::CAPTURE_MARF_KEYS)
}

/// Whether MARF block-hash capture is enabled.
#[inline(always)]
pub fn capture_marf_hash_lookups() -> bool {
    is_enabled(flags::CAPTURE_MARF_HASH_LOOKUPS)
}

/// Whether MARF disk-read capture is enabled.
#[inline(always)]
pub fn capture_marf_disk_reads() -> bool {
    is_enabled(flags::CAPTURE_MARF_DISK_READS)
}

#[cfg(test)]
mod tests {
    use stacks_common::types::chainstate::StacksBlockId;
    use stacks_profiler::{ProfileStats, Profiler, RecordValue};

    use super::{set_flags, DEFAULT_FLAGS};
    use crate::{ClarityMarfTrieId, MARFOpenOpts, MARFValue, MarfConnection, MARF};

    /// Flatten captured spans for assertions across the MARF call tree.
    fn flatten(spans: &[ProfileStats]) -> Vec<&ProfileStats> {
        spans
            .iter()
            .flat_map(|span| {
                let mut entries = vec![span];
                entries.extend(flatten(&span.children));
                entries
            })
            .collect()
    }

    /// Exercise persisted reads, sampling and capture flags through the extracted crate.
    #[test]
    fn persisted_reads_emit_spans_and_gated_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("marf.sqlite");
        let block = StacksBlockId([1; 32]);
        let value = MARFValue::from_value("value");
        let mut marf = MARF::from_path(path.to_str().unwrap(), MARFOpenOpts::default()).unwrap();
        marf.begin(&StacksBlockId::sentinel(), &block).unwrap();
        marf.insert("key", value.clone()).unwrap();
        marf.commit().unwrap();
        drop(marf);

        let previous = set_flags(DEFAULT_FLAGS);
        for capture in [true, false] {
            set_flags(if capture { DEFAULT_FLAGS } else { 0 });
            let mut marf =
                MARF::from_path(path.to_str().unwrap(), MARFOpenOpts::default()).unwrap();
            Profiler::clear();
            assert_eq!(marf.get(&block, "key").unwrap(), Some(value.clone()));
            marf.with_conn(|conn| {
                conn.open_block(&block).unwrap();
                let ptr = conn.root_trieptr();
                for _ in 0..16 {
                    conn.read_node_hash_bytes(&ptr).unwrap();
                }
            });
            let results = Profiler::take_results().unwrap();
            let spans = flatten(&results);
            assert!(spans.iter().any(|span| span.name() == "get_by_key"));
            let reads = spans
                .iter()
                .find(|span| span.name() == "read_node_hash_bytes")
                .unwrap();
            assert_eq!(reads.entered_count, 16);
            assert_eq!(reads.sampled_count, 2);
            let records: Vec<_> = spans.iter().flat_map(|span| &span.records).collect();
            if capture {
                let key = records.iter().find(|record| record.key == "KEY").unwrap();
                std::assert_matches!(&key.value, RecordValue::Str(key) if key.as_ref() == "key");
                let hash = records.iter().find(|record| record.key == "BH").unwrap();
                std::assert_matches!(&hash.value, RecordValue::Bytes(bytes) if bytes.as_ref() == block.as_bytes());
                assert!(records
                    .iter()
                    .any(|record| record.key == "MARF_NODE_HASH_DISK_READ"));
            } else {
                assert!(records.is_empty());
            }
        }
        set_flags(previous);
    }
}
