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

use std::path::PathBuf;
use std::time::Duration;

pub use marf_core::archive::PruneInfo;
use marf_core::node::Ptr;
use marf_core::{Error as FormatError, TrieHash, corrupt};

/// Stage-file placement and human-readable progress label.
#[derive(Clone, Debug)]
pub struct PruneOptions {
    /// Parent directory for temporary node staging.
    pub staging_dir: PathBuf,
    /// Label included in optional progress diagnostics.
    pub label: String,
}

pub use pipeline::prune_with_progress;
mod error;
pub use error::Error;
pub use marf_core::{archive, codec, hash, node, trie};
mod positioned_io;

/// Report an algorithm invariant failure without naming a storage error.
macro_rules! prune_corrupt { ($($arg:tt)*) => { $crate::Error::CorruptionError(format!($($arg)*)) }; }

/// Summary statistics from a pruning run.
#[derive(Debug, Clone)]
pub struct PruneStats {
    pub node_count: u64,
    pub prune_height: u32,
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
/// The stage index an inline child pointer of node `parent` refers to; the
/// pre-order invariant (`parent < child < n`) is what lets hashing and
/// serialization run in reverse index order, children before parents.
fn inline_child(p: &Ptr, parent: u32, n: u32) -> Result<u32, FormatError> {
    if p.is_back() {
        return Err(corrupt!("squash trie has a back-pointer at node {parent}"));
    }
    let child = u32::try_from(p.ptr).map_err(|_| FormatError::OverflowError)?;
    if child <= parent || child >= n {
        return Err(corrupt!(
            "node {parent}: child index {child} breaks the pre-order invariant (n = {n})"
        ));
    }
    Ok(child)
}

/// Integrator-owned progress reporting, called outside node traversal loops.
pub trait Progress {
    /// Historical height metadata has been indexed.
    fn indexed(&mut self, _label: &str, _heights: usize) {}
    /// The durable archive and metadata have completed.
    fn completed(&mut self, _label: &str, _nodes: u64, _bytes: u64) {}
}
/// Default progress sink with no state or work.
pub struct NoProgress;
impl Progress for NoProgress {}
/// Construct an archive without progress reporting. The source is consumed and
/// released before output streaming to bound its cache and mapped-file lifetime.
pub fn prune<T, S, D>(
    source: S,
    destination: &mut D,
    options: &PruneOptions,
    tip: &T,
    height: u32,
) -> Result<PruneStats, Error<S::Error, D::Error>>
where
    T: marf_core::TrieId,
    S: archive::ArchiveSource<T>,
    D: archive::ArchiveDestination<T>,
{
    prune_with_progress(source, destination, options, tip, height, &mut NoProgress)
}
