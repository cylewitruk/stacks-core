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

//! Pruning: collapse the canonical history up to height `H` into a single
//! trie that holds exactly the state visible at `H`.
//!
//! The on-disk vocabulary (`marf_squash_info`, `marf_squashed_blocks`) keeps
//! the original's "squash" names; everything in this crate says "prune".
//!
//! The pruned MARF keeps one `marf_data` row per historical block (so block
//! ids stay meaningful), all pointing at one shared trie blob. Every child
//! pointer in that blob is inline and annotated with the storage id of the
//! block the child originally came from; when the squashed MARF is extended,
//! copy-on-write turns those annotations back into back-pointers, so the
//! root hashes of new blocks match what the archival MARF would produce.
//! Per-height archival root hashes and block ids live in `marf_squashed_blocks`.
//!
//! Memory: the collected nodes are staged in a temporary file beside the
//! destination (`Staged`), and the output blob is streamed into the blobs
//! file; what stays in memory is per-node bookkeeping (a file offset, a
//! hash, a blob offset: 48 bytes a node in arrays, plus the dedup index
//! while collecting and two 1 MiB buffers; about 95 bytes a node measured
//! at peak), never the decoded trie or the serialized blob. The original
//! does the same (`squash::node_store`, `squash::stream`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use stacks_common::types::chainstate::TrieHash;

pub use crate::archive::PruneInfo;
use crate::marf::MARFOpenOpts;
use crate::node::Ptr;
use crate::{Error, MarfTrieId, corrupt};

/// Pruning as a capability of a MARF type.
pub trait Prune<T: MarfTrieId>: Sized {
    /// Prune the MARF at `src_path` at `height` (on the fork of `tip`) into a
    /// new database at `dst_path`.
    fn prune_to_path(
        src_path: impl AsRef<Path>,
        dst_path: impl AsRef<Path>,
        src_open_opts: MARFOpenOpts,
        tip: &T,
        height: u32,
        label: &str,
    ) -> Result<PruneStats, Error>;
}

/// Summary statistics from a pruning run.
#[derive(Debug, Clone)]
pub struct PruneStats {
    pub node_count: u64,
    pub prune_height: u32,
    pub dst_db_path: PathBuf,
    pub dst_blobs_path: PathBuf,
    pub blob_size: u64,
    pub historical_placeholder_count: u64,
    pub source_root_hash: TrieHash,
    pub prune_root_node_hash: TrieHash,
    pub total_duration: Duration,
}

/// Resolve a child pointer to the `(block_id, offset)` locating it in the
/// source: back-pointers and squash annotations name the block directly,
/// plain inline pointers inherit the node's block.
fn child_location(ptr: &Ptr, origin_block: u32) -> (u32, u64) {
    if ptr.is_back() || ptr.back_block != 0 {
        (ptr.back_block, ptr.ptr)
    } else {
        (origin_block, ptr.ptr)
    }
}

/// Write buffer size for the stage file and the output blob.
const STREAM_BUFFER: usize = 1 << 20;

mod pipeline;
mod stage;
#[cfg(test)]
mod tests;
/// The stage index an inline child pointer of node `parent` refers to; the
/// pre-order invariant (`parent < child < n`) is what lets hashing and
/// serialization run in reverse index order, children before parents.
fn inline_child(p: &Ptr, parent: u32, n: u32) -> Result<u32, Error> {
    if p.is_back() {
        return Err(corrupt!("squash trie has a back-pointer at node {parent}"));
    }
    let child = u32::try_from(p.ptr).map_err(|_| Error::OverflowError)?;
    if child <= parent || child >= n {
        return Err(corrupt!(
            "node {parent}: child index {child} breaks the pre-order invariant (n = {n})"
        ));
    }
    Ok(child)
}
