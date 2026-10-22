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

//! MARF adapters for archival reads and transactional destination construction.
use std::path::{Path as FilePath, PathBuf};

use stacks_common::types::chainstate::TrieHash;

use super::super::{
    MARF, MARFOpenOpts, MarfConnection, MarfTransaction, TrieFileStorage, TrieHashCalculationMode,
};
use crate::archive::{
    Archive, ArchiveBlob, ArchiveDestination, ArchiveSource, ArchiveTransaction, PruneInfo,
};
use crate::node::{Node, Ptr};
use crate::{Error, MarfTrieId, storage};

impl<T: MarfTrieId> Archive<T> for MARF<T> {
    type Source = Self;
    type Destination = Self;
    fn blobs_path(path: &FilePath) -> PathBuf {
        storage::Blobs::path_for(path)
    }
    fn open_archive_source(path: &FilePath, options: MARFOpenOpts) -> Result<Self, Error> {
        Ok(Self::from_storage(TrieFileStorage::open_readonly(
            path, options,
        )?))
    }
    fn create_archive_destination(path: &FilePath) -> Result<Self, Error> {
        let dst = Self::from_path(
            path,
            MARFOpenOpts::new(TrieHashCalculationMode::Deferred, true),
        )?;
        dst.db.pragma_update(None, "journal_mode", "OFF")?;
        dst.db.pragma_update(None, "synchronous", "OFF")?;
        Ok(dst)
    }
}
impl<T: MarfTrieId> ArchiveSource<T> for MARF<T> {
    fn prune_height(&self) -> Option<u32> {
        self.inner.store.prune_height()
    }
    fn block_at_height(&mut self, height: u32, tip: &T) -> Result<Option<T>, Error> {
        self.get_block_at_height(height, tip)
    }
    fn block_id(&mut self, block: &T) -> Result<u32, Error> {
        self.inner.store.block_id(&self.db, block)
    }
    fn block_metadata(&mut self, block: u32) -> Result<(TrieHash, T), Error> {
        let root = self.inner.store.read_root_hash(&self.db, block)?;
        let parent = self.inner.store.read_parent(&self.db, block)?;
        Ok((root, parent))
    }
    fn pruned_blocks(&mut self) -> Result<Vec<(u32, T, TrieHash)>, Error> {
        self.inner.store.pruned_blocks(&self.db)
    }
    fn read_node(&mut self, block: u32, ptr: &Ptr) -> Result<Node, Error> {
        Ok(self.inner.store.read_node(&self.db, block, ptr)?.0)
    }
    fn confirmed_blocks(&mut self) -> Result<Vec<(u32, T)>, Error> {
        self.inner.store.confirmed_blocks(&self.db)
    }
}
impl<T: MarfTrieId> ArchiveDestination<T> for MARF<T> {
    type Transaction<'a>
        = MarfTransaction<'a, T>
    where
        Self: 'a;
    fn begin_archive(&mut self) -> Result<Self::Transaction<'_>, Error> {
        self.begin_tx()
    }
    fn finalize(&mut self) -> Result<(), Error> {
        self.db.pragma_update(None, "journal_mode", "WAL")?;
        self.db.pragma_update(None, "synchronous", "NORMAL")?;
        Ok(())
    }
}
impl<T: MarfTrieId> ArchiveTransaction<T> for MarfTransaction<'_, T> {
    type Blob = storage::Blobs;
    fn insert_placeholder(&mut self, block: &T) -> Result<u32, Error> {
        let v = self.view();
        v.inner.store.insert_placeholder(v.db, block)
    }
    fn write_shared_blob_with(
        &mut self,
        write: impl FnOnce(&Self::Blob, u64) -> Result<u64, Error>,
    ) -> Result<(u64, u64), Error> {
        let v = self.view();
        v.inner.store.write_shared_blob_with(v.db, write)
    }
    fn write_metadata(
        &mut self,
        info: &PruneInfo,
        blocks: &[(u32, T, TrieHash)],
    ) -> Result<(), Error> {
        let v = self.view();
        v.inner.store.write_prune_metadata(
            v.db,
            &info.archival_marf_root_hash,
            &info.prune_root_node_hash,
            info.prune_height,
            blocks,
        )
    }
    fn commit_archive(self) -> Result<(), Error> {
        self.into_sqlite_tx().commit()?;
        Ok(())
    }
}
impl ArchiveBlob for storage::Blobs {
    fn write_at_unsynced(&self, offset: u64, bytes: &[u8]) -> Result<(), Error> {
        Ok(storage::Blobs::write_at_unsynced(self, offset, bytes)?)
    }
}
