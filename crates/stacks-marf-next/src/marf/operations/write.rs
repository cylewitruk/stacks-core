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

//! Pending-trie writes, sealing, persistence and transaction completion.
use rusqlite::Transaction;
use stacks_common::types::chainstate::TrieHash;

use super::super::Building;
use super::{
    BLOCK_HASH_TO_HEIGHT_MAPPING_KEY, BLOCK_HEIGHT_TO_HASH_MAPPING_KEY, MARF, MarfTransaction,
    OWN_BLOCK_HEIGHT_KEY, View,
};
use crate::codec::{self, Patch};
use crate::hash::marf_root_hash;
use crate::node::{BACKPTR, Branch, Kind, Node, Path, Ptr, backptr_children, id};
use crate::storage::StorageContext;
use crate::trie::{Origin, ROOT_OFFSET, Trie, TrieIdentity};
use crate::{Error, MARFValue, MarfTrieId, corrupt};

/// Which table and name a flushed trie is stored under.
enum FlushTarget<'t, T> {
    Confirmed(&'t T),
    Mined(&'t T),
    Unconfirmed,
}

impl<T: MarfTrieId> View<'_, T> {
    /// Reject mutations on a read-only store.
    fn require_writable(&self) -> Result<(), Error> {
        if self.inner.store.policy().readonly {
            return Err(Error::ReadOnlyError);
        }
        Ok(())
    }

    /// Build the delta for a copied-forward node, if a patch applies.
    ///
    /// The delta is computed against the node as resolved at its origin (with
    /// any patches there already applied), so a chain of amendments stays
    /// small. The patch's base points at the origin record: the node itself
    /// when it is a plain node, or the outermost patch of the chain.
    fn make_patch(&mut self, branch: &Branch, origin: &Origin) -> Result<Option<Patch>, Error> {
        let Some((old, _)) = self
            .inner
            .store
            .branch(self.db, origin.block, &origin.ptr)?
        else {
            return Ok(None);
        };
        if old.path != branch.path || old.kind != branch.kind {
            return Ok(None);
        }
        let base = Ptr {
            id: if origin.depth == 0 {
                origin.ptr.kind_id()
            } else {
                id::PATCH
            } | BACKPTR,
            chr: origin.ptr.chr,
            ptr: origin.ptr.ptr,
            back_block: origin.block,
        };
        Ok(codec::make_patch(base, &old.children, branch))
    }

    /// Seal, serialize and store the trie being built.
    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    fn flush(&mut self, target: FlushTarget<'_, T>) -> Result<(), Error> {
        self.require_writable()?;
        if self.inner.building.is_none() {
            return Ok(());
        }
        match target {
            FlushTarget::Unconfirmed if !self.inner.store.policy().unconfirmed => {
                return Err(Error::UnconfirmedError);
            }
            FlushTarget::Confirmed(_) | FlushTarget::Mined(_)
                if self.inner.store.policy().unconfirmed =>
            {
                return Err(Error::UnconfirmedError);
            }
            _ => {}
        }
        self.seal()?;
        let compress =
            self.inner.store.policy().compress && !matches!(target, FlushTarget::Unconfirmed);
        let trie = self.inner.building.take().expect("checked").trie;
        let blob = trie.serialize(compress, |branch, origin, _| {
            self.make_patch(branch, origin)
        })?;
        let built = trie.block_id();
        match target {
            FlushTarget::Confirmed(bhh) => {
                self.inner.store.write_confirmed(self.db, bhh, &blob)?;
                self.inner.current = bhh.clone();
            }
            FlushTarget::Mined(bhh) => {
                self.inner.store.write_mined(self.db, bhh, &blob)?;
            }
            FlushTarget::Unconfirmed => {
                self.inner.store.write_unconfirmed(self.db, built, &blob)?;
            }
        }
        self.inner.store.unlock(self.db, built)?;
        Ok(())
    }
}

impl<'a, T: MarfTrieId> View<'a, T> {
    /// Height of the block that would extend `parent` (also validates that
    /// `parent` exists).
    fn extension_height(&mut self, parent: &T) -> Result<u32, Error> {
        if *parent == T::sentinel() {
            return Ok(0);
        }
        self.site(parent)?;
        let h = self
            .get_block_height(parent, parent)?
            .ok_or_else(|| corrupt!("Failed to find block height for `{parent:?}`"))?;
        h.checked_add(1).ok_or(Error::OverflowError)
    }

    /// The root of a new trie extending `parent`: either a fresh empty root
    /// or the parent's root with its children turned into back-pointers.
    fn root_for(&mut self, parent: &T) -> Result<(Branch, Option<Origin>), Error> {
        if *parent == T::sentinel() || self.inner.store.num_blocks(self.db)? == 0 {
            return Ok((Branch::new(Kind::Node256, Path::EMPTY), None));
        }
        let pid = self.inner.store.block_id(self.db, parent)?;
        let ptr = Ptr {
            id: id::NODE256 | BACKPTR,
            chr: 0,
            ptr: ROOT_OFFSET,
            back_block: pid,
        };
        let (node, _, depth) = self.inner.store.read_node(self.db, pid, &ptr)?;
        let Node::Branch(mut root) = node else {
            return Err(corrupt!("root of {parent} is not a branch"));
        };
        if root.kind != Kind::Node256 {
            return Err(corrupt!("root of {parent} is not a Node256"));
        }
        backptr_children(&mut root.children, pid);
        Ok((
            root,
            Some(Origin {
                block: pid,
                ptr,
                depth,
            }),
        ))
    }

    /// Begin building `child` on top of `parent`.
    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn begin(&mut self, parent: &T, child: &T) -> Result<(), Error> {
        self.require_writable()?;
        if self.inner.building.is_some() {
            return Err(Error::InProgressError);
        }
        if self.inner.store.has_block(self.db, child)? {
            return Err(Error::ExistsError);
        }
        let height = self.extension_height(parent)?;
        if self.inner.store.policy().unconfirmed {
            return Err(Error::UnconfirmedError);
        }
        let (root, origin) = self.root_for(parent)?;
        if !self.inner.store.lock(self.db, child, false)? {
            return Err(Error::ExistsError);
        }
        self.inner.building = Some(Building {
            trie: Trie::new(child.clone(), parent.clone(), root, origin),
            height,
        });
        self.inner.current = child.clone();
        if let Err(e) = self.set_block_heights(parent, child, height) {
            self.inner.building = None;
            return Err(e);
        }
        Ok(())
    }

    /// Begin (or resume) the unconfirmed trie on top of the confirmed `parent`.
    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn begin_unconfirmed(&mut self, parent: &T) -> Result<T, Error> {
        self.require_writable()?;
        if self.inner.building.is_some() {
            return Err(Error::InProgressError);
        }
        if !self.inner.store.policy().unconfirmed {
            return Err(Error::UnconfirmedError);
        }
        if self
            .inner
            .store
            .confirmed_block_id(self.db, parent)?
            .is_none()
        {
            return Err(Error::NotFoundError);
        }
        let tip = MARF::<T>::make_unconfirmed_chain_tip(parent);
        let height = self.extension_height(parent)?;
        let (trie, created) = match self.inner.store.unconfirmed_block_id(self.db, &tip)? {
            Some(id) => {
                let blob = self.inner.store.read_trie_blob(self.db, id)?;
                (Trie::load(&blob, tip.clone())?, false)
            }
            None => {
                let (root, origin) = self.root_for(parent)?;
                (Trie::new(tip.clone(), parent.clone(), root, origin), true)
            }
        };
        if !self.inner.store.lock(self.db, &tip, true)? {
            return Err(Error::ExistsError);
        }
        self.inner.building = Some(Building { trie, height });
        self.inner.current = tip.clone();
        if created && let Err(e) = self.set_block_heights(parent, &tip, height) {
            self.inner.building = None;
            return Err(e);
        }
        Ok(tip)
    }

    /// Record the height mapping keys of a new block. Insertion order is
    /// consensus-relevant (it fixes child slot order) and matches the original.
    pub fn set_block_heights(&mut self, parent: &T, child: &T, height: u32) -> Result<(), Error> {
        let mut keys = vec![
            OWN_BLOCK_HEIGHT_KEY.to_string(),
            format!("{BLOCK_HEIGHT_TO_HASH_MAPPING_KEY}::{height}"),
            format!("{BLOCK_HASH_TO_HEIGHT_MAPPING_KEY}::{child}"),
        ];
        let mut values = vec![
            MARFValue::from(height),
            MARFValue::from(child.clone()),
            MARFValue::from(height),
        ];
        if height > 0 {
            keys.push(format!(
                "{BLOCK_HEIGHT_TO_HASH_MAPPING_KEY}::{}",
                height - 1
            ));
            values.push(MARFValue::from(parent.clone()));
            keys.push(format!("{BLOCK_HASH_TO_HEIGHT_MAPPING_KEY}::{parent}"));
            values.push(MARFValue::from(height - 1));
        }
        self.insert_batch(&keys, values)
    }

    /// Insert key/value pairs into the trie being built.
    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn insert_batch(&mut self, keys: &[String], values: Vec<MARFValue>) -> Result<(), Error> {
        self.require_writable()?;
        assert_eq!(keys.len(), values.len());
        let inner = &mut *self.inner;
        let Some(building) = inner.building.as_mut() else {
            return Err(Error::WriteNotBegunError);
        };
        let db = self.db;
        for (key, value) in keys.iter().zip(values) {
            let path = TrieHash::from_key(key);
            building
                .trie
                .insert(&path.0, value, |ptr| inner.store.fetch(db, ptr))?;
        }
        Ok(())
    }

    /// Insert a single already-hashed path.
    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn insert_raw(&mut self, path: TrieHash, value: MARFValue) -> Result<(), Error> {
        self.require_writable()?;
        let inner = &mut *self.inner;
        let Some(building) = inner.building.as_mut() else {
            return Err(Error::WriteNotBegunError);
        };
        let db = self.db;
        building
            .trie
            .insert(&path.0, value, |ptr| inner.store.fetch(db, ptr))
    }

    /// Compute all hashes of the trie being built and return its MARF root hash.
    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn seal(&mut self) -> Result<TrieHash, Error> {
        self.require_writable()?;
        let (tip, height, sealed) = {
            let b = self
                .inner
                .building
                .as_ref()
                .ok_or(Error::WriteNotBegunError)?;
            (b.trie.block_id().clone(), b.height, b.trie.is_sealed())
        };
        if sealed {
            return Ok(self
                .inner
                .building
                .as_ref()
                .expect("checked")
                .trie
                .root_hash());
        }
        let root_node_hash = {
            let inner = &mut *self.inner;
            let db = self.db;
            let b = inner.building.as_mut().expect("checked");
            b.trie.seal(|block_id| {
                inner
                    .store
                    .block_hash(db, block_id)
                    .map(|h| h.clone().to_bytes())
            })?
        };
        let ancestors = self.collect_ancestor_hashes(&tip, height)?;
        let root = marf_root_hash(&root_node_hash, &ancestors);
        self.inner
            .building
            .as_mut()
            .expect("checked")
            .trie
            .set_root_hash(root);
        Ok(root)
    }

    /// Abandon and delete the unconfirmed trie being built.
    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn drop_unconfirmed_trie(&mut self) -> Result<(), Error> {
        if self.inner.store.policy().readonly || !self.inner.store.policy().unconfirmed {
            return Ok(());
        }
        if let Some(b) = self.inner.building.take() {
            self.inner
                .store
                .drop_unconfirmed(self.db, b.trie.block_id())?;
            self.inner.store.unlock(self.db, b.trie.block_id())?;
        }
        Ok(())
    }
}

impl<T: MarfTrieId> Drop for MarfTransaction<'_, T> {
    fn drop(&mut self) {
        // dropping an open SQLite transaction rolls it back; anything cached
        // from rows written inside it must go with it
        if self.tx.take().is_some() {
            self.inner.store.invalidate();
        }
    }
}
impl<'a, T: MarfTrieId> MarfTransaction<'a, T> {
    pub fn sqlite_tx(&self) -> &Transaction<'a> {
        self.tx()
    }

    pub fn sqlite_tx_mut(&mut self) -> &mut Transaction<'a> {
        self.tx.as_mut().expect("transaction still open")
    }
    fn finish(mut self) -> Result<(), Error> {
        match self.take_tx().commit() {
            Ok(()) => Ok(()),
            Err(e) => {
                self.inner.store.invalidate();
                Err(e.into())
            }
        }
    }
    /// Commit the SQLite transaction without flushing the trie being built.
    pub fn commit_tx(mut self) {
        self.take_tx()
            .commit()
            .expect("CORRUPTION: Failed to commit MARF");
    }

    pub fn rollback(mut self) {
        self.take_tx()
            .rollback()
            .expect("CORRUPTION: Failed to roll back MARF");
        self.inner.store.invalidate();
    }

    /// Begin writing the next trie in the MARF. Fails if the block already exists.
    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn begin(&mut self, chain_tip: &T, next_chain_tip: &T) -> Result<(), Error> {
        self.view().begin(chain_tip, next_chain_tip)
    }

    /// Begin extending the MARF to an unconfirmed trie built on `chain_tip`.
    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn begin_unconfirmed(&mut self, chain_tip: &T) -> Result<T, Error> {
        self.view().begin_unconfirmed(chain_tip)
    }

    pub fn set_block_heights(
        &mut self,
        block_hash: &T,
        next_block_hash: &T,
        height: u32,
    ) -> Result<(), Error> {
        self.view()
            .set_block_heights(block_hash, next_block_hash, height)
    }

    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn insert_batch(&mut self, keys: &[String], values: Vec<MARFValue>) -> Result<(), Error> {
        self.view().insert_batch(keys, values)
    }

    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn insert(&mut self, key: &str, value: MARFValue) -> Result<(), Error> {
        self.view().insert_batch(&[key.to_string()], vec![value])
    }

    /// Seal the trie being built and return the MARF root hash.
    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn seal(&mut self) -> Result<TrieHash, Error> {
        self.view().seal()
    }

    pub fn get_open_chain_tip(&self) -> Option<&T> {
        self.inner.building.as_ref().map(|b| b.trie.block_id())
    }

    pub fn get_open_chain_tip_height(&self) -> Option<u32> {
        self.inner.building.as_ref().map(|b| b.height)
    }

    pub fn get_block_height_of(
        &mut self,
        bhh: &T,
        current_block_hash: &T,
    ) -> Result<Option<u32>, Error> {
        if Some(bhh) == self.get_open_chain_tip() {
            return Ok(self.get_open_chain_tip_height());
        }
        self.view().get_block_height(bhh, current_block_hash)
    }

    /// Persist the trie being built under the block it was begun with and commit.
    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn commit(mut self) -> Result<(), Error> {
        if self.inner.store.policy().readonly {
            return Err(Error::ReadOnlyError);
        }
        if let Some(tip) = self.get_open_chain_tip().cloned() {
            let target = if self.inner.store.policy().unconfirmed {
                FlushTarget::Unconfirmed
            } else {
                FlushTarget::Confirmed(&tip)
            };
            self.view().flush(target)?;
        }
        self.finish()
    }

    /// Persist the trie being built under `real_bhh` and commit.
    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn commit_to(mut self, real_bhh: &T) -> Result<(), Error> {
        if self.inner.store.policy().readonly {
            return Err(Error::ReadOnlyError);
        }
        if self.inner.store.policy().unconfirmed {
            return Err(Error::UnconfirmedError);
        }
        if self.inner.building.is_some() {
            self.view().flush(FlushTarget::Confirmed(real_bhh))?;
            self.finish()?;
        }
        Ok(())
    }

    /// Persist the trie being built into the mined-blocks table and commit.
    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn commit_mined(mut self, bhh: &T) -> Result<(), Error> {
        if self.inner.store.policy().readonly {
            return Err(Error::ReadOnlyError);
        }
        if self.inner.store.policy().unconfirmed {
            return Err(Error::UnconfirmedError);
        }
        if self.inner.building.is_some() {
            self.view().flush(FlushTarget::Mined(bhh))?;
            self.finish()?;
        }
        Ok(())
    }

    /// Drop the trie being built and roll back.
    pub fn drop_current(mut self) {
        if !self.inner.store.policy().readonly {
            self.view()
                .drop_extending_trie()
                .expect("Corruption: Failed to drop the extended trie lock");
            self.inner.current = T::sentinel();
            self.rollback();
        }
    }

    /// Drop the unconfirmed trie being built, delete its persisted state, and commit.
    pub fn drop_unconfirmed(mut self) {
        if !self.inner.store.policy().readonly
            && self.inner.store.policy().unconfirmed
            && self.inner.building.is_some()
        {
            self.view()
                .drop_unconfirmed_trie()
                .expect("Corruption: Failed to drop unconfirmed trie");
            self.inner.current = T::sentinel();
            // unconfirmed state may already be on disk, so this cannot be a rollback
            self.commit_tx();
        }
    }

    /// Reopen this MARF read-only on a fresh connection. Fails if a trie is
    /// being built.
    pub fn reopen_readonly(&self) -> Result<MARF<T>, Error> {
        if self.inner.building.is_some() {
            return Err(Error::InProgressError);
        }
        MARF::reopen_readonly_inner(self.inner)
    }
}
