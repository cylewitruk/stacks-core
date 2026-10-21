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

//! Runtime toggles for the `stacks-profiler` instrumentation of this crate
//! (feature `profiler`). The flag bits and function names are those of
//! stackslib's `profiler` module, which re-exports this module so that
//! `set_flags`/`enable`/`disable` from either path control the same state.
//!
//! Instrumented operations (feature `profiler`), each the equivalent of a
//! span the original in-stackslib MARF carried; sampled spans use the
//! original's `sample_rate = 8, unsampled = "count_only"`:
//!
//! | original span | here |
//! |---|---|
//! | `MarfConnection::get` | `MarfConnection::get` |
//! | `MARF::get_by_key` (+ `KEY` record) | `View::get_by_key` (+ `KEY` record) |
//! | `MARF::get_by_hash` (+ `BH` record) | `View::get_by_path` (+ `BH` record) |
//! | `MARF::get_path`, `walk` | `View::lookup` |
//! | `walk_backptr`, `read_node_hash_bytes` (sampled) | `View::read_with_depth` (sampled), `Store::node` (sampled) |
//! | `trie_sql::get_*_block_identifier` (sampled) | `View::site`, `Store::block_id`, `confirmed_block_id`, `unconfirmed_block_id` (sampled) |
//! | `trie_sql::get_node_hash_bytes*`, `get_external_trie_offset_length*`, `get_external_blobs_length`, `count_blocks` (sampled) | `Store::read_hash`, `location`, `external_blobs_length`, `num_blocks`, `has_block` (sampled) |
//! | `MARF_NODE_HASH_DISK_READ` record on a SQLite node read | the same record on a SQLite record read (`storage::trie_bytes`) |
//! | `MARF::get_block_at_height`, `get_with_proof*`, `inner_get_trie_ancestor_hashes_bytes` | `View::get_block_at_height`, `get_with_proof*`, `ancestor_hashes` |
//! | `extend_trie`, `extend_to_block`, `extend_to_unconfirmed_block` | `begin`, `begin_unconfirmed` (`View`, `MarfTransaction`, `MARF`) |
//! | `insert_batch`, `insert`, `insert_raw`, `insert_leaf*` | `insert_batch`, `insert`, `insert_raw` (`View`, `MarfTransaction`, `MARF`), `Trie::insert` |
//! | `seal`, `calculate_node_hashes` (sampled) | `seal` (`View`, `MarfTransaction`, `MARF`), `Trie::seal`, `Trie::hash_subtree` (sampled) |
//! | `commit`, `commit_to`, `commit_mined`, `flush*` | `commit`, `commit_to`, `commit_mined` (`MarfTransaction`, `MARF`), `View::flush` |
//! | `store_trie_blob`, `append_trie_blob`, `write_trie_blob*`, `write_external_trie_blob` | `Trie::serialize`, `Store::insert_placeholder`, `write_shared_blob_with`, `write_confirmed`, `write_unconfirmed`, `write_mined`, `storage::export_trie_blobs` |
//! | `read_trie_blob_from_db` | `Store::read_trie_blob`, `Trie::load` |
//! | `drop_unconfirmed_trie`, `*lock_bhh_for_extension`, `clear_lock_data` | `View::drop_unconfirmed_trie`, `Store::drop_unconfirmed`, `lock`, `unlock`, `storage::clear_locks` |
//!
//! Not mirrored: `walk_cow` (copy-on-write has no separate walk here; the
//! back-pointer fetch is inside `Store::read_node`, which is spanned) and
//! `open_trie_blob*` (no SQLite blob handle is kept open; the per-record
//! read is the span above).

use std::sync::atomic::{AtomicU64, Ordering};

/// Bitflags that control profiler capture behavior.
pub mod flags {
    /// Capture MARF key strings in `get_by_key`.
    pub const CAPTURE_MARF_KEYS: u64 = 1 << 0;
    /// Capture MARF block-hash lookups in `get_by_path` (the original's `get_by_hash`).
    pub const CAPTURE_MARF_HASH_LOOKUPS: u64 = 1 << 1;
    /// Capture MARF disk-read identifiers (block id) for records read through SQLite.
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

#[inline(always)]
pub fn capture_marf_keys() -> bool {
    is_enabled(flags::CAPTURE_MARF_KEYS)
}

#[inline(always)]
pub fn capture_marf_hash_lookups() -> bool {
    is_enabled(flags::CAPTURE_MARF_HASH_LOOKUPS)
}

#[inline(always)]
pub fn capture_marf_disk_reads() -> bool {
    is_enabled(flags::CAPTURE_MARF_DISK_READS)
}
