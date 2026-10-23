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
use super::{MARF, MarfTransaction, ReopenedMarf, View};
use crate::observe::Observer;
use crate::{Error, MARFValue, TrieHash, TrieId};

/// Shared implementation of the read API over [`View`]-like handles.
pub trait MarfConnection<T: TrieId, O: Observer = crate::observe::Noop> {
    fn with_conn<F, R>(&mut self, exec: F) -> R
    where
        F: FnOnce(&mut View<'_, T, O>) -> R;

    /// Resolve a key from the MARF to a MARFValue with respect to the given block height.
    fn get(&mut self, block_hash: &T, key: &str) -> Result<Option<MARFValue>, Error> {
        let _span = observe!(O, Get);
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

    /// Check if a block can open successfully, i.e., it's a known block, the
    /// storage system isn't issuing IOErrors, and it's in the same fork as the
    /// block last opened with `open_block`.
    fn check_ancestor_block_hash(&mut self, bhh: &T) -> Result<(), Error> {
        self.with_conn(|c| c.check_ancestor_block_hash(bhh))
    }
}

impl<T: TrieId, O: Observer> MarfConnection<T, O> for MARF<T, O> {
    fn with_conn<F, R>(&mut self, exec: F) -> R
    where
        F: FnOnce(&mut View<'_, T, O>) -> R,
    {
        exec(&mut self.connection())
    }
}

impl<T: TrieId, O: Observer> MarfConnection<T, O> for MarfTransaction<'_, T, O> {
    fn with_conn<F, R>(&mut self, exec: F) -> R
    where
        F: FnOnce(&mut View<'_, T, O>) -> R,
    {
        exec(&mut self.connection())
    }
}

impl<T: TrieId, O: Observer> MarfConnection<T, O> for ReopenedMarf<'_, T, O> {
    fn with_conn<F, R>(&mut self, exec: F) -> R
    where
        F: FnOnce(&mut View<'_, T, O>) -> R,
    {
        exec(&mut View {
            db: self.db,
            inner: &mut self.inner,
        })
    }
}
