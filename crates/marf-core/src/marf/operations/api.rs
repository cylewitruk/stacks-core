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

//! Public MARF convenience operations.
use super::{Inner, MARF, ReopenedMarf};
#[cfg(any(test, feature = "testing"))]
use super::{View, WalkProfile};
use crate::archive::PruneInfo;
use crate::observe::Observer;
use crate::storage::StorageContext;
use crate::trie::TrieIdentity;
use crate::{Error, MARFValue, TrieHash, TrieHash as Sha512Trunc256Sum, TrieId};

impl<T: TrieId, O: Observer> MARF<T, O> {
    /// Entries in the resolved-patch cache (benchmark instrumentation).
    #[cfg(any(test, feature = "testing", feature = "counters"))]
    pub fn patch_cache_len(&self) -> usize {
        self.inner.store.patch_cache_len()
    }

    /// Target the MARF at a given block (for `check_ancestor_block_hash`).
    pub fn open_block(&mut self, block_hash: &T) -> Result<(), Error> {
        self.connection().open_block(block_hash)
    }

    /// See [`View::lookup_profile`].
    #[cfg(any(test, feature = "testing"))]
    pub fn lookup_profile(&mut self, tip: &T, key: &str) -> Result<WalkProfile, Error> {
        self.connection()
            .lookup_profile(tip, &TrieHash::from_key(key))
    }

    /// The MARF root hash of `block_hash` (also available through `MarfConnection`).
    pub fn get_root_hash_at(&mut self, block_hash: &T) -> Result<TrieHash, Error> {
        self.connection().get_root_hash_at(block_hash)
    }

    pub fn get_bhh_at_height(&mut self, block_hash: &T, height: u32) -> Result<Option<T>, Error> {
        self.connection().get_block_at_height(height, block_hash)
    }

    pub fn get_block_height_of(
        &mut self,
        bhh: &T,
        current_block_hash: &T,
    ) -> Result<Option<u32>, Error> {
        if Some(bhh) == self.get_open_chain_tip() {
            return Ok(self.get_open_chain_tip_height());
        }
        self.connection().get_block_height(bhh, current_block_hash)
    }

    pub fn get_open_chain_tip(&self) -> Option<&T> {
        self.inner.building.as_ref().map(|b| b.trie.block_id())
    }

    pub fn get_open_chain_tip_height(&self) -> Option<u32> {
        self.inner.building.as_ref().map(|b| b.height)
    }

    /// Insert a batch of key/value pairs into the trie being built.
    pub fn insert_batch(&mut self, keys: &[String], values: Vec<MARFValue>) -> Result<(), Error> {
        let _span = observe!(O, MarfInsertBatch);
        let mut tx = self.begin_tx()?;
        tx.insert_batch(keys, values)?;
        tx.commit_tx();
        Ok(())
    }

    pub fn insert(&mut self, key: &str, value: MARFValue) -> Result<(), Error> {
        let _span = observe!(O, MarfInsert);
        self.insert_batch(&[key.to_string()], vec![value])
    }

    pub fn insert_raw(&mut self, path: TrieHash, value: MARFValue) -> Result<(), Error> {
        let _span = observe!(O, MarfInsertRaw);
        let mut tx = self.begin_tx()?;
        tx.connection().insert_raw(path, value)?;
        tx.commit_tx();
        Ok(())
    }

    /// Drop the trie being built and release its extension lock. Unlike the
    /// transaction-level [`MarfTransaction::drop_current`](crate::MarfTransaction::drop_current), which rolls its
    /// transaction back, this commits the lock removal: the begin may already
    /// have been committed by an earlier transaction.
    pub fn drop_current(&mut self) {
        if !self.inner.store.policy().readonly {
            let mut tx = self
                .begin_tx()
                .expect("BUG: failed to start transaction to drop trie");
            tx.connection()
                .drop_extending_trie()
                .expect("Corruption: Failed to drop the extended trie lock");
            tx.inner.current = T::sentinel();
            tx.commit_tx();
        }
    }

    pub fn drop_unconfirmed(&mut self) {
        if !self.inner.store.policy().readonly && self.inner.store.policy().unconfirmed {
            let tx = self
                .begin_tx()
                .expect("BUG: failed to start transaction to drop trie");
            tx.drop_unconfirmed();
        }
    }

    pub fn commit(&mut self) -> Result<(), Error> {
        let _span = observe!(O, MarfCommit);
        self.begin_tx()?.commit()
    }

    pub fn commit_mined(&mut self, bhh: &T) -> Result<(), Error> {
        let _span = observe!(O, MarfCommitMined);
        self.begin_tx()?.commit_mined(bhh)
    }

    pub fn commit_to(&mut self, real_bhh: &T) -> Result<(), Error> {
        let _span = observe!(O, MarfCommitTo);
        self.begin_tx()?.commit_to(real_bhh)
    }

    pub fn seal(&mut self) -> Result<TrieHash, Error> {
        let _span = observe!(O, MarfSeal);
        let mut tx = self.begin_tx()?;
        let h = tx.seal()?;
        tx.commit_tx();
        Ok(h)
    }

    /// Test helper: begin a block in its own committed SQLite transaction,
    /// leaving the trie open in memory for `insert`/`commit` on this handle.
    #[cfg(any(test, feature = "testing"))]
    pub fn begin(&mut self, chain_tip: &T, next_chain_tip: &T) -> Result<(), Error> {
        let _span = observe!(O, MarfBegin);
        let mut tx = self.begin_tx()?;
        tx.begin(chain_tip, next_chain_tip)?;
        tx.commit_tx();
        Ok(())
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn begin_unconfirmed(&mut self, chain_tip: &T) -> Result<T, Error> {
        let _span = observe!(O, MarfBeginUnconfirmed);
        let mut tx = self.begin_tx()?;
        let tip = tx.begin_unconfirmed(chain_tip)?;
        tx.commit_tx();
        Ok(tip)
    }

    /// Access the storage handle (test helper).
    #[cfg(any(test, feature = "testing"))]
    pub fn borrow_storage_backend(&mut self) -> View<'_, T, O> {
        self.connection()
    }

    /// Build a read-only view which reuses this MARF's SQLite connection but
    /// has its own `open_block` target and caches.
    pub fn reopen_connection(&self) -> Result<ReopenedMarf<'_, T, O>, Error> {
        if self.inner.building.is_some() {
            return Err(Error::InProgressError);
        }
        Ok(ReopenedMarf {
            db: &self.db,
            inner: Inner {
                store: self.inner.store.reopen_readonly(),
                building: None,
                current: self.inner.current.clone(),
            },
        })
    }

    /// Reopen this MARF read-only on a fresh connection. Fails if a trie is
    /// being built.
    pub fn reopen_readonly(&self) -> Result<MARF<T, O>, Error> {
        if self.inner.building.is_some() {
            return Err(Error::InProgressError);
        }
        Self::reopen_readonly_inner(&self.inner)
    }

    pub fn get_db_path(&self) -> &std::path::Path {
        self.inner.store.database_path()
    }

    pub fn prune_info(&self) -> Option<&PruneInfo> {
        self.inner.store.prune_info()
    }

    /// Make an unconfirmed chain tip from an existing chain tip, so that it
    /// won't conflict with the "true" chain tip after the state it represents
    /// is later reprocessed and confirmed.
    pub fn make_unconfirmed_chain_tip(chain_tip: &T) -> T {
        let mut bytes = [0u8; 64];
        bytes[0..32].copy_from_slice(chain_tip.as_bytes());
        bytes[32..64].copy_from_slice(chain_tip.as_bytes());
        let h = Sha512Trunc256Sum::from_data(&bytes);
        let mut res = [0u8; 32];
        res.copy_from_slice(h.as_bytes());
        T::from_bytes(res)
    }
}
