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

use super::super::{MARF, MarfConnection, MarfTransaction};
use crate::archive::{
    ArchiveBlob, ArchiveDestination, ArchiveSource, ArchiveTransaction, PruneInfo,
};
use crate::node::{Node, Ptr};
use crate::observe::Observer;
use crate::{Error, TrieHash, TrieId, storage};

impl<T: TrieId, O: Observer> ArchiveSource<T> for MARF<T, O> {
    type Error = Error;
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
impl<T: TrieId, O: Observer> ArchiveDestination<T> for MARF<T, O> {
    type Error = Error;
    type Transaction<'a>
        = MarfTransaction<'a, T, O>
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
impl<T: TrieId, O: Observer> ArchiveTransaction<T> for MarfTransaction<'_, T, O> {
    type Error = Error;
    type Blob = storage::Blobs;
    fn insert_placeholder(&mut self, block: &T) -> Result<u32, Error> {
        let v = self.connection();
        v.inner.store.insert_placeholder(v.db, block)
    }
    fn write_shared_blob_with<E>(
        &mut self,
        write: impl FnOnce(&Self::Blob, u64) -> Result<u64, E>,
    ) -> Result<(u64, u64), crate::archive::ArchiveWriteError<Error, E>> {
        let v = self.connection();
        v.inner.store.write_shared_blob_with(v.db, write)
    }
    fn write_metadata(
        &mut self,
        info: &PruneInfo,
        blocks: &[(u32, T, TrieHash)],
    ) -> Result<(), Error> {
        let v = self.connection();
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
    type Error = Error;
    fn write_at_unsynced(&self, offset: u64, bytes: &[u8]) -> Result<(), Error> {
        Ok(storage::Blobs::write_at_unsynced(self, offset, bytes)?)
    }
}
