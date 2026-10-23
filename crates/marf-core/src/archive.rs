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

//! Historical reads and transactional archive construction capabilities.
//!
//! Implementations own persistence and transaction policy. Archive algorithms
//! own traversal, staging and byte layout, and never receive a database handle.

use crate::node::{Node, Ptr};
use crate::{TrieHash, TrieId};

/// Metadata of a pruned MARF, cached at open time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PruneInfo {
    /// Archival MARF root hash committed to the chain at the prune boundary.
    pub archival_marf_root_hash: TrieHash,
    /// Root node hash of the pruned trie.
    pub prune_root_node_hash: TrieHash,
    /// MARF height at the prune boundary.
    pub prune_height: u32,
}

/// Positioned writes into an archive blob; durability belongs to the transaction.
pub trait ArchiveBlob {
    /// Error returned by the destination writer.
    type Error: std::error::Error;
    /// Write bytes without syncing; the enclosing blob operation syncs once.
    fn write_at_unsynced(&self, offset: u64, bytes: &[u8]) -> Result<(), Self::Error>;
}

/// Historical state needed to construct a pruned archive.
pub trait ArchiveSource<T: TrieId> {
    /// Error returned by the historical source.
    type Error: std::error::Error;
    /// The existing prune boundary, if any.
    fn prune_height(&self) -> Option<u32>;
    /// Locate the block on the selected fork.
    fn block_at_height(&mut self, height: u32, tip: &T) -> Result<Option<T>, Self::Error>;
    /// Resolve consensus identity to the local id used by node pointers.
    fn block_id(&mut self, block: &T) -> Result<u32, Self::Error>;
    /// Read the archival root hash and parent identity of a block.
    fn block_metadata(&mut self, block: u32) -> Result<(TrieHash, T), Self::Error>;
    /// Existing historical metadata, in height order.
    fn pruned_blocks(&mut self) -> Result<Vec<(u32, T, TrieHash)>, Self::Error>;
    /// Materialize one reachable node, retaining its pointer annotations.
    fn read_node(&mut self, block: u32, ptr: &Ptr) -> Result<Node, Self::Error>;
    /// Enumerate confirmed block identities and their local ids.
    fn confirmed_blocks(&mut self) -> Result<Vec<(u32, T)>, Self::Error>;
}

/// A destination transaction; dropping it without [`commit_archive`](Self::commit_archive)
/// rolls back metadata.
pub trait ArchiveTransaction<T: TrieId>: Sized {
    /// Error returned by the archive transaction.
    type Error: std::error::Error;
    /// Blob writer used inside a single durable shared-blob operation.
    type Blob: ArchiveBlob<Error = Self::Error>;
    /// Reserve a historical block row and return its destination-local id.
    fn insert_placeholder(&mut self, block: &T) -> Result<u32, Self::Error>;
    /// Stream one blob, sync it, and associate all reserved rows with it.
    fn write_shared_blob_with<E>(
        &mut self,
        write: impl FnOnce(&Self::Blob, u64) -> Result<u64, E>,
    ) -> Result<(u64, u64), ArchiveWriteError<Self::Error, E>>;
    /// Record the archive boundary and per-height consensus roots.
    fn write_metadata(
        &mut self,
        info: &PruneInfo,
        blocks: &[(u32, T, TrieHash)],
    ) -> Result<(), Self::Error>;
    /// Commit metadata after the shared blob and its side tables are complete.
    fn commit_archive(self) -> Result<(), Self::Error>;
}

/// A new archive destination, including its offline write/finalization policy.
pub trait ArchiveDestination<T: TrieId> {
    /// Error returned by the destination.
    type Error: std::error::Error;
    /// Transaction borrowing this destination.
    type Transaction<'a>: ArchiveTransaction<T, Error = Self::Error>
    where
        Self: 'a;
    /// Begin a metadata transaction for the shared blob.
    fn begin_archive(&mut self) -> Result<Self::Transaction<'_>, Self::Error>;
    /// Restore the normal persistence policy after a successful commit.
    fn finalize(&mut self) -> Result<(), Self::Error>;
}

/// Read archival boundary metadata without exposing storage internals.
pub trait ArchiveMetadata {
    /// Error returned by the metadata source.
    type Error: std::error::Error;
    /// Return the completed prune boundary, rejecting incomplete metadata.
    fn read_prune_info(&self) -> Result<Option<PruneInfo>, Self::Error>;
}

/// Failure while streaming and publishing one durable archive blob.
#[derive(Debug)]
pub enum ArchiveWriteError<D, C> {
    /// Destination I/O, synchronization or metadata failure.
    Destination(D),
    /// Algorithm/staging failure before publication.
    Callback(C),
}
impl<D, C> From<D> for ArchiveWriteError<D, C> {
    fn from(e: D) -> Self {
        Self::Destination(e)
    }
}
impl<D: std::fmt::Display, C: std::fmt::Display> std::fmt::Display for ArchiveWriteError<D, C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Destination(e) => e.fmt(f),
            Self::Callback(e) => e.fmt(f),
        }
    }
}
impl<D: std::error::Error + 'static, C: std::error::Error + 'static> std::error::Error
    for ArchiveWriteError<D, C>
{
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Destination(e) => Some(e),
            Self::Callback(e) => Some(e),
        }
    }
}
