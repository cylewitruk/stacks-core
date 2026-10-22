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
use std::path::{Path, PathBuf};

use stacks_common::types::chainstate::TrieHash;

use crate::marf::MARFOpenOpts;
use crate::node::{Node, Ptr};
use crate::{Error, MarfTrieId};

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
    /// Write bytes without syncing; the enclosing blob operation syncs once.
    fn write_at_unsynced(&self, offset: u64, bytes: &[u8]) -> Result<(), Error>;
}

/// Historical state needed to construct a pruned archive.
pub trait ArchiveSource<T: MarfTrieId> {
    /// The existing prune boundary, if any.
    fn prune_height(&self) -> Option<u32>;
    /// Locate the block on the selected fork.
    fn block_at_height(&mut self, height: u32, tip: &T) -> Result<Option<T>, Error>;
    /// Resolve consensus identity to the local id used by node pointers.
    fn block_id(&mut self, block: &T) -> Result<u32, Error>;
    /// Read the archival root hash and parent identity of a block.
    fn block_metadata(&mut self, block: u32) -> Result<(TrieHash, T), Error>;
    /// Existing historical metadata, in height order.
    fn pruned_blocks(&mut self) -> Result<Vec<(u32, T, TrieHash)>, Error>;
    /// Materialize one reachable node, retaining its pointer annotations.
    fn read_node(&mut self, block: u32, ptr: &Ptr) -> Result<Node, Error>;
    /// Enumerate confirmed block identities and their local ids.
    fn confirmed_blocks(&mut self) -> Result<Vec<(u32, T)>, Error>;
}

/// A destination transaction; dropping it without [`commit_archive`](Self::commit_archive)
/// rolls back metadata.
pub trait ArchiveTransaction<T: MarfTrieId>: Sized {
    /// Blob writer used inside a single durable shared-blob operation.
    type Blob: ArchiveBlob;
    /// Reserve a historical block row and return its destination-local id.
    fn insert_placeholder(&mut self, block: &T) -> Result<u32, Error>;
    /// Stream one blob, sync it, and associate all reserved rows with it.
    fn write_shared_blob_with(
        &mut self,
        write: impl FnOnce(&Self::Blob, u64) -> Result<u64, Error>,
    ) -> Result<(u64, u64), Error>;
    /// Record the archive boundary and per-height consensus roots.
    fn write_metadata(
        &mut self,
        info: &PruneInfo,
        blocks: &[(u32, T, TrieHash)],
    ) -> Result<(), Error>;
    /// Commit metadata after the shared blob and its side tables are complete.
    fn commit_archive(self) -> Result<(), Error>;
}

/// A new archive destination, including its offline write/finalization policy.
pub trait ArchiveDestination<T: MarfTrieId> {
    /// Transaction borrowing this destination.
    type Transaction<'a>: ArchiveTransaction<T>
    where
        Self: 'a;
    /// Begin a metadata transaction for the shared blob.
    fn begin_archive(&mut self) -> Result<Self::Transaction<'_>, Error>;
    /// Restore the normal persistence policy after a successful commit.
    fn finalize(&mut self) -> Result<(), Error>;
}

/// Open archival sources and new destinations without exposing their backends.
pub trait Archive<T: MarfTrieId> {
    /// Historical source opened read-only.
    type Source: ArchiveSource<T>;
    /// New offline archive destination.
    type Destination: ArchiveDestination<T>;
    /// Determine the destination's external blob path before creating files.
    fn blobs_path(path: &Path) -> PathBuf;
    /// Open the source without migrating or mutating it.
    fn open_archive_source(path: &Path, options: MARFOpenOpts) -> Result<Self::Source, Error>;
    /// Create the destination with external blobs and the archive write policy.
    fn create_archive_destination(path: &Path) -> Result<Self::Destination, Error>;
}

/// Read archival boundary metadata without exposing storage internals.
pub trait ArchiveMetadata {
    /// Return the completed prune boundary, rejecting incomplete metadata.
    fn read_prune_info(&self) -> Result<Option<PruneInfo>, Error>;
}
