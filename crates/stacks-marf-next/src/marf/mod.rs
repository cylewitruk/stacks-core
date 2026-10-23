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

//! Stacks identifiers and hash adapters for the concrete core engine.
pub use marf_core::marf::{
    BLOCK_HASH_TO_HEIGHT_MAPPING_KEY, BLOCK_HEIGHT_TO_HASH_MAPPING_KEY, MARFOpenOpts,
    OWN_BLOCK_HEIGHT_KEY, TrieHashCalculationMode, test_override_marf_compression,
};
use marf_core::marf::{
    MARF as CoreMarf, MarfTransaction as CoreTransaction, ReopenedMarf as CoreReopened,
    View as CoreView,
};
#[cfg(any(test, feature = "testing"))]
pub use marf_core::marf::{WalkProfile, fault_injection_marf_compression};
use stacks_common::types::chainstate::TrieHash;

use crate::archive::PruneInfo;
use crate::observer::IntegrationObserver;
use crate::proofs::TrieMerkleProof;
use crate::{Error, Id, MARFValue, MarfTrieId, core_hash, stacks_hash};
/// Stacks MARF backed by the concrete core SQLite engine.
pub struct MARF<T: MarfTrieId> {
    /// Engine state and native SQLite ownership.
    inner: CoreMarf<Id<T>, IntegrationObserver>,
    /// Cached boundary metadata adapted to Stacks hash types.
    info: Option<PruneInfo>,
}
/// A native MARF transaction also used by Clarity metadata operations.
pub struct MarfTransaction<'a, T: MarfTrieId> {
    /// Active native transaction and its borrowed engine state.
    inner: CoreTransaction<'a, Id<T>, IntegrationObserver>,
    /// Boundary metadata borrowed from the owning facade.
    info: &'a Option<PruneInfo>,
}
/// Borrowed logical connection, including an in-progress trie when present.
pub struct View<'a, T: MarfTrieId> {
    /// Borrowed logical connection used for reads and writes.
    inner: CoreView<'a, Id<T>, IntegrationObserver>,
    /// Boundary metadata borrowed from the owning facade.
    info: &'a Option<PruneInfo>,
}
/// Read-only state and caches sharing the engine's native connection.
pub struct ReopenedMarf<'a, T: MarfTrieId> {
    /// Independent read state sharing the native connection.
    inner: CoreReopened<'a, Id<T>, IntegrationObserver>,
    /// Cached boundary metadata adapted to Stacks hash types.
    info: Option<PruneInfo>,
}
/// Concrete native storage opened before constructing a MARF.
pub struct TrieFileStorage<T: MarfTrieId> {
    /// Native storage prepared for engine construction.
    inner: marf_core::marf::TrieFileStorage<Id<T>, IntegrationObserver>,
    /// Cached boundary metadata adapted to Stacks hash types.
    info: Option<PruneInfo>,
}
impl<T: MarfTrieId> MARF<T> {
    /// Delegate to the configured core engine.
    #[cfg(any(test, feature = "testing", feature = "counters"))]
    pub fn patch_cache_len(&self) -> usize {
        self.inner.patch_cache_len()
    }
    /// Delegate to the configured core engine.
    pub fn open_block(&mut self, block_hash: &T) -> Result<(), Error> {
        self.inner.open_block(&Id(block_hash.clone()))
    }
    /// Delegate to the configured core engine.
    #[cfg(any(test, feature = "testing"))]
    pub fn lookup_profile(&mut self, tip: &T, key: &str) -> Result<WalkProfile, Error> {
        self.inner.lookup_profile(&Id(tip.clone()), key)
    }
    /// Delegate to the configured core engine.
    pub fn get_root_hash_at(&mut self, block_hash: &T) -> Result<TrieHash, Error> {
        self.inner
            .get_root_hash_at(&Id(block_hash.clone()))
            .map(stacks_hash)
    }
    /// Delegate to the configured core engine.
    pub fn get_bhh_at_height(&mut self, block_hash: &T, height: u32) -> Result<Option<T>, Error> {
        self.inner
            .get_bhh_at_height(&Id(block_hash.clone()), height)
            .map(|v| v.map(|id| id.0))
    }
    /// Delegate to the configured core engine.
    pub fn get_block_height_of(
        &mut self,
        bhh: &T,
        current_block_hash: &T,
    ) -> Result<Option<u32>, Error> {
        self.inner
            .get_block_height_of(&Id(bhh.clone()), &Id(current_block_hash.clone()))
    }
    /// Delegate to the configured core engine.
    pub fn get_open_chain_tip(&self) -> Option<&T> {
        self.inner.get_open_chain_tip().map(|id| &id.0)
    }
    /// Delegate to the configured core engine.
    pub fn get_open_chain_tip_height(&self) -> Option<u32> {
        self.inner.get_open_chain_tip_height()
    }
    /// Delegate to the configured core engine.
    pub fn insert_batch(&mut self, keys: &[String], values: Vec<MARFValue>) -> Result<(), Error> {
        self.inner.insert_batch(keys, values)
    }
    /// Delegate to the configured core engine.
    pub fn insert(&mut self, key: &str, value: MARFValue) -> Result<(), Error> {
        self.inner.insert(key, value)
    }
    /// Delegate to the configured core engine.
    pub fn insert_raw(&mut self, path: TrieHash, value: MARFValue) -> Result<(), Error> {
        self.inner.insert_raw(core_hash(&path), value)
    }
    /// Delegate to the configured core engine.
    pub fn drop_current(&mut self) {
        self.inner.drop_current()
    }
    /// Delegate to the configured core engine.
    pub fn drop_unconfirmed(&mut self) {
        self.inner.drop_unconfirmed()
    }
    /// Delegate to the configured core engine.
    pub fn commit(&mut self) -> Result<(), Error> {
        self.inner.commit()
    }
    /// Delegate to the configured core engine.
    pub fn commit_mined(&mut self, bhh: &T) -> Result<(), Error> {
        self.inner.commit_mined(&Id(bhh.clone()))
    }
    /// Delegate to the configured core engine.
    pub fn commit_to(&mut self, real_bhh: &T) -> Result<(), Error> {
        self.inner.commit_to(&Id(real_bhh.clone()))
    }
    /// Delegate to the configured core engine.
    pub fn seal(&mut self) -> Result<TrieHash, Error> {
        self.inner.seal().map(stacks_hash)
    }
    /// Delegate to the configured core engine.
    #[cfg(any(test, feature = "testing"))]
    pub fn begin(&mut self, chain_tip: &T, next_chain_tip: &T) -> Result<(), Error> {
        self.inner
            .begin(&Id(chain_tip.clone()), &Id(next_chain_tip.clone()))
    }
    /// Delegate to the configured core engine.
    #[cfg(any(test, feature = "testing"))]
    pub fn begin_unconfirmed(&mut self, chain_tip: &T) -> Result<T, Error> {
        self.inner
            .begin_unconfirmed(&Id(chain_tip.clone()))
            .map(|id| id.0)
    }
    /// Delegate to the configured core engine.
    #[cfg(any(test, feature = "testing"))]
    pub fn borrow_storage_backend(&mut self) -> View<'_, T> {
        self.connection()
    }
    /// Delegate to the configured core engine.
    pub fn get_db_path(&self) -> &std::path::Path {
        self.inner.get_db_path()
    }
    /// Delegate to the configured core engine.
    pub fn prune_info(&self) -> Option<&PruneInfo> {
        self.info.as_ref()
    }
    /// Delegate to the configured core engine.
    pub fn make_unconfirmed_chain_tip(chain_tip: &T) -> T {
        CoreMarf::<Id<T>, IntegrationObserver>::make_unconfirmed_chain_tip(&Id(chain_tip.clone())).0
    }
    /// Borrow the logical connection without exposing mutable engine ownership.
    pub fn connection(&mut self) -> View<'_, T> {
        View {
            inner: self.inner.connection(),
            info: &self.info,
        }
    }
    pub fn reopen_connection(&self) -> Result<ReopenedMarf<'_, T>, Error> {
        Ok(ReopenedMarf {
            inner: self.inner.reopen_connection()?,
            info: self.info.clone(),
        })
    }
    pub fn reopen_readonly(&self) -> Result<Self, Error> {
        Self::wrap(self.inner.reopen_readonly()?)
    }
    fn wrap(inner: CoreMarf<Id<T>, IntegrationObserver>) -> Result<Self, Error> {
        let info = inner.prune_info().map(PruneInfo::from);
        Ok(Self { inner, info })
    }
}
impl<T: MarfTrieId> View<'_, T> {
    /// Delegate to the configured core connection.
    pub fn readonly(&self) -> bool {
        self.inner.readonly()
    }
    /// Delegate to the configured core connection.
    pub fn unconfirmed(&self) -> bool {
        self.inner.unconfirmed()
    }
    /// Delegate to the configured core connection.
    pub fn is_pruned(&self) -> bool {
        self.inner.is_pruned()
    }
    /// Delegate to the configured core connection.
    pub fn prune_info(&self) -> Option<&PruneInfo> {
        self.info.as_ref()
    }
    /// Delegate to the configured core connection.
    pub fn open_chain_tip(&self) -> Option<&T> {
        self.inner.open_chain_tip().map(|id| &id.0)
    }
    /// Delegate to the configured core connection.
    pub fn open_chain_tip_height(&self) -> Option<u32> {
        self.inner.open_chain_tip_height()
    }
    /// Delegate to the configured core connection.
    pub fn has_block(&mut self, bhh: &T) -> Result<bool, Error> {
        self.inner.has_block(&Id(bhh.clone()))
    }
    /// Delegate to the configured core connection.
    pub fn get_cur_block(&self) -> T {
        self.inner.get_cur_block().0
    }
    /// Delegate to the configured core connection.
    pub fn open_block(&mut self, bhh: &T) -> Result<(), Error> {
        self.inner.open_block(&Id(bhh.clone()))
    }
    /// Delegate to the configured core connection.
    #[cfg(any(test, feature = "testing"))]
    pub fn lookup_profile(&mut self, tip: &T, path: &TrieHash) -> Result<WalkProfile, Error> {
        self.inner
            .lookup_profile(&Id(tip.clone()), &core_hash(path))
    }
    /// Delegate to the configured core connection.
    pub fn lookup(&mut self, tip: &T, path: &TrieHash) -> Result<Option<MARFValue>, Error> {
        self.inner.lookup(&Id(tip.clone()), &core_hash(path))
    }
    /// Delegate to the configured core connection.
    pub fn get_by_path(&mut self, tip: &T, path: &TrieHash) -> Result<Option<MARFValue>, Error> {
        self.inner.get_by_path(&Id(tip.clone()), &core_hash(path))
    }
    /// Delegate to the configured core connection.
    pub fn get_by_key(&mut self, tip: &T, key: &str) -> Result<Option<MARFValue>, Error> {
        self.inner.get_by_key(&Id(tip.clone()), key)
    }
    /// Delegate to the configured core connection.
    pub fn read_block_root_hash(&mut self, bhh: &T) -> Result<TrieHash, Error> {
        self.inner
            .read_block_root_hash(&Id(bhh.clone()))
            .map(stacks_hash)
    }
    /// Delegate to the configured core connection.
    pub fn get_root_hash_at(&mut self, tip: &T) -> Result<TrieHash, Error> {
        self.inner
            .get_root_hash_at(&Id(tip.clone()))
            .map(stacks_hash)
    }
    /// Delegate to the configured core connection.
    pub fn for_each_leaf<F>(&mut self, tip: &T, mut handle_leaf: F) -> Result<u64, Error>
    where
        F: FnMut(TrieHash, MARFValue) -> Result<(), Error>,
    {
        self.inner
            .for_each_leaf(&Id(tip.clone()), |h, v| handle_leaf(stacks_hash(h), v))
    }
    /// Delegate to the configured core connection.
    pub fn get_block_height(&mut self, block: &T, tip: &T) -> Result<Option<u32>, Error> {
        self.inner
            .get_block_height(&Id(block.clone()), &Id(tip.clone()))
    }
    /// Delegate to the configured core connection.
    pub fn get_block_at_height(&mut self, height: u32, tip: &T) -> Result<Option<T>, Error> {
        self.inner
            .get_block_at_height(height, &Id(tip.clone()))
            .map(|v| v.map(|id| id.0))
    }
    /// Delegate to the configured core connection.
    pub fn check_ancestor_block_hash(&mut self, bhh: &T) -> Result<(), Error> {
        self.inner.check_ancestor_block_hash(&Id(bhh.clone()))
    }
    /// Delegate to the configured core connection.
    pub fn begin(&mut self, parent: &T, child: &T) -> Result<(), Error> {
        self.inner.begin(&Id(parent.clone()), &Id(child.clone()))
    }
    /// Delegate to the configured core connection.
    pub fn begin_unconfirmed(&mut self, parent: &T) -> Result<T, Error> {
        self.inner
            .begin_unconfirmed(&Id(parent.clone()))
            .map(|id| id.0)
    }
    /// Delegate to the configured core connection.
    pub fn set_block_heights(&mut self, parent: &T, child: &T, height: u32) -> Result<(), Error> {
        self.inner
            .set_block_heights(&Id(parent.clone()), &Id(child.clone()), height)
    }
    /// Delegate to the configured core connection.
    pub fn insert_batch(&mut self, keys: &[String], values: Vec<MARFValue>) -> Result<(), Error> {
        self.inner.insert_batch(keys, values)
    }
    /// Delegate to the configured core connection.
    pub fn insert_raw(&mut self, path: TrieHash, value: MARFValue) -> Result<(), Error> {
        self.inner.insert_raw(core_hash(&path), value)
    }
    /// Delegate to the configured core connection.
    pub fn seal(&mut self) -> Result<TrieHash, Error> {
        self.inner.seal().map(stacks_hash)
    }
    /// Delegate to the configured core connection.
    pub fn drop_unconfirmed_trie(&mut self) -> Result<(), Error> {
        self.inner.drop_unconfirmed_trie()
    }
}
impl<T: MarfTrieId> MarfTransaction<'_, T> {
    /// Delegate to the configured core connection.
    pub fn commit_tx(self) {
        self.inner.commit_tx()
    }
    /// Delegate to the configured core connection.
    pub fn rollback(self) {
        self.inner.rollback()
    }
    /// Delegate to the configured core connection.
    pub fn begin(&mut self, chain_tip: &T, next_chain_tip: &T) -> Result<(), Error> {
        self.inner
            .begin(&Id(chain_tip.clone()), &Id(next_chain_tip.clone()))
    }
    /// Delegate to the configured core connection.
    pub fn begin_unconfirmed(&mut self, chain_tip: &T) -> Result<T, Error> {
        self.inner
            .begin_unconfirmed(&Id(chain_tip.clone()))
            .map(|id| id.0)
    }
    /// Delegate to the configured core connection.
    pub fn set_block_heights(
        &mut self,
        block_hash: &T,
        next_block_hash: &T,
        height: u32,
    ) -> Result<(), Error> {
        self.inner.set_block_heights(
            &Id(block_hash.clone()),
            &Id(next_block_hash.clone()),
            height,
        )
    }
    /// Delegate to the configured core connection.
    pub fn insert_batch(&mut self, keys: &[String], values: Vec<MARFValue>) -> Result<(), Error> {
        self.inner.insert_batch(keys, values)
    }
    /// Delegate to the configured core connection.
    pub fn insert(&mut self, key: &str, value: MARFValue) -> Result<(), Error> {
        self.inner.insert(key, value)
    }
    /// Delegate to the configured core connection.
    pub fn seal(&mut self) -> Result<TrieHash, Error> {
        self.inner.seal().map(stacks_hash)
    }
    /// Delegate to the configured core connection.
    pub fn get_open_chain_tip(&self) -> Option<&T> {
        self.inner.get_open_chain_tip().map(|id| &id.0)
    }
    /// Delegate to the configured core connection.
    pub fn get_open_chain_tip_height(&self) -> Option<u32> {
        self.inner.get_open_chain_tip_height()
    }
    /// Delegate to the configured core connection.
    pub fn get_block_height_of(
        &mut self,
        bhh: &T,
        current_block_hash: &T,
    ) -> Result<Option<u32>, Error> {
        self.inner
            .get_block_height_of(&Id(bhh.clone()), &Id(current_block_hash.clone()))
    }
    /// Delegate to the configured core connection.
    pub fn commit(self) -> Result<(), Error> {
        self.inner.commit()
    }
    /// Delegate to the configured core connection.
    pub fn commit_to(self, real_bhh: &T) -> Result<(), Error> {
        self.inner.commit_to(&Id(real_bhh.clone()))
    }
    /// Delegate to the configured core connection.
    pub fn commit_mined(self, bhh: &T) -> Result<(), Error> {
        self.inner.commit_mined(&Id(bhh.clone()))
    }
    /// Delegate to the configured core connection.
    pub fn drop_current(self) {
        self.inner.drop_current()
    }
    /// Delegate to the configured core connection.
    pub fn drop_unconfirmed(self) {
        self.inner.drop_unconfirmed()
    }
    pub fn connection(&mut self) -> View<'_, T> {
        View {
            inner: self.inner.connection(),
            info: self.info,
        }
    }
    pub fn reopen_readonly(&self) -> Result<MARF<T>, Error> {
        MARF::wrap(self.inner.reopen_readonly()?)
    }
}
impl<T: MarfTrieId> MARF<T> {
    /// Visit leaves without materializing an entire trie.
    pub fn for_each_leaf<F>(view: &mut View<'_, T>, tip: &T, handle_leaf: F) -> Result<u64, Error>
    where
        F: FnMut(TrieHash, MARFValue) -> Result<(), Error>,
    {
        view.for_each_leaf(tip, handle_leaf)
    }
}
pub trait MarfConnection<T: MarfTrieId> {
    fn with_conn<F, R>(&mut self, exec: F) -> R
    where
        F: FnOnce(&mut View<'_, T>) -> R;

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
impl<T: MarfTrieId> MarfConnection<T> for MARF<T> {
    fn with_conn<F, R>(&mut self, exec: F) -> R
    where
        F: FnOnce(&mut View<'_, T>) -> R,
    {
        exec(&mut self.connection())
    }
}
impl<T: MarfTrieId> MarfConnection<T> for MarfTransaction<'_, T> {
    fn with_conn<F, R>(&mut self, exec: F) -> R
    where
        F: FnOnce(&mut View<'_, T>) -> R,
    {
        exec(&mut self.connection())
    }
}
impl<T: MarfTrieId> MarfConnection<T> for ReopenedMarf<'_, T> {
    fn with_conn<F, R>(&mut self, exec: F) -> R
    where
        F: FnOnce(&mut View<'_, T>) -> R,
    {
        exec(&mut self.connection())
    }
}
impl<T: MarfTrieId> ReopenedMarf<'_, T> {
    pub fn connection(&mut self) -> View<'_, T> {
        View {
            inner: self.inner.connection(),
            info: &self.info,
        }
    }
}
mod native;
mod proof;
