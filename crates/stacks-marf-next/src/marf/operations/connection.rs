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

//! Connection capabilities and read-only adapters.
use rusqlite::Connection;
use stacks_common::types::chainstate::TrieHash;

use super::{MARF, MARFOpenOpts, MarfTransaction, ReopenedMarf, TrieFileStorage, View};
use crate::archive::PruneInfo;
use crate::proofs::TrieMerkleProof;
use crate::storage::StorageContext;
use crate::{Error, MARFValue, MarfTrieId, storage};

/// Shared implementation of the read API over [`View`]-like handles.
pub trait MarfConnection<T: MarfTrieId> {
    fn with_conn<F, R>(&mut self, exec: F) -> R
    where
        F: FnOnce(&mut View<'_, T>) -> R;

    fn sqlite_conn(&self) -> &Connection;

    /// Resolve a key from the MARF to a MARFValue with respect to the given block height.
    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    fn get(&mut self, block_hash: &T, key: &str) -> Result<Option<MARFValue>, Error> {
        self.with_conn(|c| c.get_by_key(block_hash, key))
    }

    /// Resolve a TrieHash from the MARF to a MARFValue with respect to the given block height.
    fn get_from_hash(&mut self, block_hash: &T, th: &TrieHash) -> Result<Option<MARFValue>, Error> {
        self.with_conn(|c| c.get_by_path(block_hash, th))
    }

    fn get_block_at_height(&mut self, height: u32, tip: &T) -> Result<Option<T>, Error> {
        self.with_conn(|c| c.get_block_at_height(height, tip))
    }

    fn get_block_height(&mut self, ancestor: &T, tip: &T) -> Result<Option<u32>, Error> {
        self.with_conn(|c| c.get_block_height(ancestor, tip))
    }

    /// Get the root trie hash at a particular block
    fn get_root_hash_at(&mut self, block_hash: &T) -> Result<TrieHash, Error> {
        self.with_conn(|c| c.get_root_hash_at(block_hash))
    }

    fn get_with_proof(
        &mut self,
        block_hash: &T,
        key: &str,
    ) -> Result<Option<(MARFValue, TrieMerkleProof<T>)>, Error> {
        self.with_conn(|c| c.get_with_proof(block_hash, key))
    }

    fn get_with_proof_from_hash(
        &mut self,
        block_hash: &T,
        hash: &TrieHash,
    ) -> Result<Option<(MARFValue, TrieMerkleProof<T>)>, Error> {
        self.with_conn(|c| c.get_with_proof_from_hash(block_hash, hash))
    }

    /// Check if a block can open successfully, i.e., it's a known block, the
    /// storage system isn't issuing IOErrors, and it's in the same fork as the
    /// block last opened with `open_block`.
    fn check_ancestor_block_hash(&mut self, bhh: &T) -> Result<(), Error> {
        self.with_conn(|c| c.check_ancestor_block_hash(bhh))
    }
}

impl<T: MarfTrieId> TrieFileStorage<T> {
    fn open_opts(
        db_path: impl AsRef<std::path::Path>,
        readonly: bool,
        unconfirmed: bool,
        opts: MARFOpenOpts,
    ) -> Result<Self, Error> {
        let (db, store) = storage::open::<T>(
            db_path,
            opts.external_blobs,
            opts.compress,
            readonly,
            unconfirmed,
            opts.force_db_migrate,
            opts.patch_cache_capacity,
        )?;
        Ok(TrieFileStorage { db, store })
    }

    pub fn open(db_path: impl AsRef<std::path::Path>, opts: MARFOpenOpts) -> Result<Self, Error> {
        Self::open_opts(db_path, false, false, opts)
    }

    pub fn open_readonly(
        db_path: impl AsRef<std::path::Path>,
        opts: MARFOpenOpts,
    ) -> Result<Self, Error> {
        Self::open_opts(db_path, true, false, opts)
    }

    pub fn open_unconfirmed(
        db_path: impl AsRef<std::path::Path>,
        opts: MARFOpenOpts,
    ) -> Result<Self, Error> {
        Self::open_opts(db_path, false, true, opts)
    }

    pub fn sqlite_conn(&self) -> &Connection {
        &self.db
    }

    pub fn prune_info(&self) -> Option<&PruneInfo> {
        self.store.prune_info()
    }
}

impl<T: MarfTrieId> MarfConnection<T> for MARF<T> {
    fn with_conn<F, R>(&mut self, exec: F) -> R
    where
        F: FnOnce(&mut View<'_, T>) -> R,
    {
        exec(&mut self.view())
    }

    fn sqlite_conn(&self) -> &Connection {
        &self.db
    }
}

impl<T: MarfTrieId> MarfConnection<T> for MarfTransaction<'_, T> {
    fn with_conn<F, R>(&mut self, exec: F) -> R
    where
        F: FnOnce(&mut View<'_, T>) -> R,
    {
        exec(&mut self.view())
    }

    fn sqlite_conn(&self) -> &Connection {
        self.tx()
    }
}

impl<T: MarfTrieId> MarfConnection<T> for ReopenedMarf<'_, T> {
    fn with_conn<F, R>(&mut self, exec: F) -> R
    where
        F: FnOnce(&mut View<'_, T>) -> R,
    {
        exec(&mut View {
            db: self.db,
            inner: &mut self.inner,
        })
    }

    fn sqlite_conn(&self) -> &Connection {
        self.db
    }
}
impl<'a, T: MarfTrieId> ReopenedMarf<'a, T> {
    pub fn db_conn(&self) -> &Connection {
        self.db
    }

    pub fn connection(&mut self) -> View<'_, T> {
        View {
            db: self.db,
            inner: &mut self.inner,
        }
    }
}
