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

//! Statically dispatched diagnostic spans; the default compiles to no work.
use std::time::Duration;

/// One logical operation, preserving the existing profiler-site mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum Operation {
    /// MARF::insert_batch.
    MarfInsertBatch,
    /// MARF::insert.
    MarfInsert,
    /// MARF::insert_raw.
    MarfInsertRaw,
    /// MARF::commit.
    MarfCommit,
    /// MARF::commit_mined.
    MarfCommitMined,
    /// MARF::commit_to.
    MarfCommitTo,
    /// MARF::seal.
    MarfSeal,
    /// MARF::begin.
    MarfBegin,
    /// MARF::begin_unconfirmed.
    MarfBeginUnconfirmed,
    /// marf/operations/connection::get.
    Get,
    /// View::flush.
    ViewFlush,
    /// View::begin.
    ViewBegin,
    /// View::begin_unconfirmed.
    ViewBeginUnconfirmed,
    /// View::insert_batch.
    ViewInsertBatch,
    /// View::insert_raw.
    ViewInsertRaw,
    /// View::seal.
    ViewSeal,
    /// View::drop_unconfirmed_trie.
    ViewDropUnconfirmedTrie,
    /// MarfTransaction::begin.
    MarftransactionBegin,
    /// MarfTransaction::begin_unconfirmed.
    MarftransactionBeginUnconfirmed,
    /// MarfTransaction::insert_batch.
    MarftransactionInsertBatch,
    /// MarfTransaction::insert.
    MarftransactionInsert,
    /// MarfTransaction::seal.
    MarftransactionSeal,
    /// MarfTransaction::commit.
    MarftransactionCommit,
    /// MarfTransaction::commit_to.
    MarftransactionCommitTo,
    /// MarfTransaction::commit_mined.
    MarftransactionCommitMined,
    /// View::site.
    ViewSite,
    /// View::read_with_depth.
    ViewReadWithDepth,
    /// View::collect_ancestor_hashes.
    ViewCollectAncestorHashes,
    /// View::lookup.
    ViewLookup,
    /// View::get_by_path.
    ViewGetByPath,
    /// View::get_by_key.
    ViewGetByKey,
    /// View::get_block_at_height.
    ViewGetBlockAtHeight,
    /// Store::insert_placeholder.
    StoreInsertPlaceholder,
    /// Store::write_shared_blob_with.
    StoreWriteSharedBlobWith,
    /// Store::block_id.
    StoreBlockId,
    /// Store::confirmed_block_id.
    StoreConfirmedBlockId,
    /// Store::unconfirmed_block_id.
    StoreUnconfirmedBlockId,
    /// Store::has_block.
    StoreHasBlock,
    /// Store::num_blocks.
    StoreNumBlocks,
    /// Store::location.
    StoreLocation,
    /// Store::read_trie_blob.
    StoreReadTrieBlob,
    /// Store::read_hash.
    StoreReadHash,
    /// Store::node.
    StoreNode,
    /// Store::read_node.
    StoreReadNode,
    /// storage/schema::export_trie_blobs.
    ExportTrieBlobs,
    /// Store::write_confirmed.
    StoreWriteConfirmed,
    /// Store::write_unconfirmed.
    StoreWriteUnconfirmed,
    /// Store::write_mined.
    StoreWriteMined,
    /// Store::drop_unconfirmed.
    StoreDropUnconfirmed,
    /// Store::lock.
    StoreLock,
    /// Store::unlock.
    StoreUnlock,
    /// storage/write::external_blobs_length.
    ExternalBlobsLength,
    /// storage/write::clear_locks.
    ClearLocks,
    /// Trie::seal.
    TrieSeal,
    /// Trie::hash_subtree.
    TrieHashSubtree,
    /// Trie::insert.
    TrieInsert,
    /// Trie::serialize.
    TrieSerialize,
    /// Trie::load.
    TrieLoad,
}

/// Number of fixed logical span sites.
pub const OPERATION_COUNT: usize = 58;

static OPERATIONS: &[(&str, &str, u32)] = &[
    ("insert_batch", "MARF", 1),
    ("insert", "MARF", 1),
    ("insert_raw", "MARF", 1),
    ("commit", "MARF", 1),
    ("commit_mined", "MARF", 1),
    ("commit_to", "MARF", 1),
    ("seal", "MARF", 1),
    ("begin", "MARF", 1),
    ("begin_unconfirmed", "MARF", 1),
    ("get", "MarfConnection", 1),
    ("flush", "View", 1),
    ("begin", "View", 1),
    ("begin_unconfirmed", "View", 1),
    ("insert_batch", "View", 1),
    ("insert_raw", "View", 1),
    ("seal", "View", 1),
    ("drop_unconfirmed_trie", "View", 1),
    ("begin", "MarfTransaction", 1),
    ("begin_unconfirmed", "MarfTransaction", 1),
    ("insert_batch", "MarfTransaction", 1),
    ("insert", "MarfTransaction", 1),
    ("seal", "MarfTransaction", 1),
    ("commit", "MarfTransaction", 1),
    ("commit_to", "MarfTransaction", 1),
    ("commit_mined", "MarfTransaction", 1),
    ("site", "View", 8),
    ("read_with_depth", "View", 8),
    ("collect_ancestor_hashes", "View", 1),
    ("lookup", "View", 1),
    ("get_by_path", "View", 1),
    ("get_by_key", "View", 1),
    ("get_block_at_height", "View", 1),
    ("insert_placeholder", "Store", 1),
    ("write_shared_blob_with", "Store", 1),
    ("block_id", "Store", 8),
    ("confirmed_block_id", "Store", 8),
    ("unconfirmed_block_id", "Store", 8),
    ("has_block", "Store", 8),
    ("num_blocks", "Store", 8),
    ("location", "Store", 8),
    ("read_trie_blob", "Store", 1),
    ("read_hash", "Store", 8),
    ("node", "Store", 8),
    ("read_node", "Store", 1),
    ("export_trie_blobs", "storage/schema", 1),
    ("write_confirmed", "Store", 1),
    ("write_unconfirmed", "Store", 1),
    ("write_mined", "Store", 1),
    ("drop_unconfirmed", "Store", 1),
    ("lock", "Store", 1),
    ("unlock", "Store", 1),
    ("external_blobs_length", "storage/write", 8),
    ("clear_locks", "storage/write", 1),
    ("seal", "Trie", 1),
    ("hash_subtree", "Trie", 8),
    ("insert", "Trie", 1),
    ("serialize", "Trie", 1),
    ("load", "Trie", 1),
];

impl Operation {
    /// Stable index for integrator-owned span-id caches.
    pub fn index(self) -> usize {
        self as usize
    }
    /// Existing function/span name.
    pub fn name(self) -> &'static str {
        OPERATIONS[self.index()].0
    }
    /// Logical owning type or module.
    pub fn context(self) -> &'static str {
        OPERATIONS[self.index()].1
    }
    /// One timed sample per this many calls; other samples count only.
    pub fn sample_rate(self) -> u32 {
        OPERATIONS[self.index()].2
    }
}

/// Capture payloads borrowed only for the duration of the callback.
pub enum Record<'a> {
    /// Key selected for lookup.
    Key(&'a str),
    /// Block selected for lookup.
    BlockHash(&'a [u8]),
    /// Local block whose SQLite record is read.
    DiskRead(u32),
}

/// Diagnostics selected statically for an engine; spans must own their state.
pub trait Observer {
    /// No-op observers bypass sampling counters and adapter calls entirely.
    const ENABLED: bool;
    /// Whether an integrator temporarily suppresses spans and sampling.
    fn is_suppressed() -> bool {
        false
    }
    /// Guard dropped at the operation boundary, including error returns.
    type Span;
    /// Begin a sampled or count-only span without borrowing the engine.
    fn enter(operation: Operation, timed: bool) -> Self::Span;
    /// Capture an explicitly selected payload.
    fn record(_record: Record<'_>) {}
    /// Report native SQL timings when profile-sqlite is compiled.
    fn sqlite_profile(_query: &str, _elapsed: Duration) {}
    /// Record a transaction lock for integrator-owned deadlock diagnostics.
    fn sqlite_locked(_connection: std::fmt::Arguments<'_>) {}
    /// Observe existing exponential/jittered busy retries.
    fn sqlite_busy(_run_count: u32, _sleep_ms: u64) {}
}

/// Default observer with no state, sampling or allocations.
#[derive(Clone, Copy, Debug, Default)]
pub struct Noop;

impl Observer for Noop {
    const ENABLED: bool = false;
    type Span = ();
    #[inline(always)]
    fn enter(_: Operation, _: bool) {}
}
