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

//! Proof conveniences compose core reads and neutral proof generation.
use marf_core::TrieHash as CoreHash;
use marf_core::access::{AuthenticatedRead, NodeRead, TrieLocation};
use marf_core::node::{Node, Ptr};
use stacks_common::types::chainstate::TrieHash;

use super::View;
use crate::proofs::{Prove, TrieMerkleProof};
use crate::{Error, Id, MARFValue, MarfTrieId};
impl<T: MarfTrieId> AuthenticatedRead<Id<T>> for View<'_, T> {
    type Error = Error;
    fn select_trie(&mut self, tip: &Id<T>) -> Result<TrieLocation, Self::Error> {
        self.inner.select_trie(tip)
    }
    fn root_pointer(&self, site: TrieLocation) -> Ptr {
        self.inner.root_pointer(site)
    }
    fn read_node(&mut self, site: TrieLocation, ptr: &Ptr) -> Result<Node, Self::Error> {
        self.inner.read_node(site, ptr)
    }
    fn borrow_node(
        &mut self,
        site: TrieLocation,
        ptr: &Ptr,
    ) -> Result<impl NodeRead<Error = Self::Error> + '_, Self::Error> {
        self.inner.borrow_node(site, ptr)
    }
    fn child_hash(&mut self, site: TrieLocation, ptr: &Ptr) -> Result<CoreHash, Self::Error> {
        self.inner.child_hash(site, ptr)
    }
    fn block_hash(&mut self, block_id: u32) -> Result<Id<T>, Self::Error> {
        self.inner.block_hash(block_id)
    }
    fn ancestor_hashes(&mut self, tip: &Id<T>, height: u32) -> Result<Vec<CoreHash>, Self::Error> {
        self.inner.ancestor_hashes(tip, height)
    }
    fn is_pruned(&self) -> bool {
        self.inner.is_pruned()
    }
    fn get_block_height(&mut self, block: &Id<T>, tip: &Id<T>) -> Result<Option<u32>, Self::Error> {
        self.inner.get_block_height(block, tip)
    }
    fn get_root_hash_at(&mut self, tip: &Id<T>) -> Result<CoreHash, Self::Error> {
        self.inner.get_root_hash_at(tip)
    }
    fn get_block_at_height(
        &mut self,
        height: u32,
        tip: &Id<T>,
    ) -> Result<Option<Id<T>>, Self::Error> {
        self.inner.get_block_at_height(height, tip)
    }
}
impl<T: MarfTrieId> View<'_, T> {
    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn get_with_proof(
        &mut self,
        block_hash: &T,
        key: &str,
    ) -> Result<Option<(MARFValue, TrieMerkleProof<T>)>, Error> {
        if self.is_pruned() {
            return Err(Error::UnsupportedOnPrunedMarf("get_with_proof"));
        }
        let Some(value) = self.get_by_key(block_hash, key)? else {
            return Ok(None);
        };
        let proof = self.make_proof(&TrieHash::from_key(key), &value, block_hash)?;
        Ok(Some((value, proof)))
    }

    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn get_with_proof_from_hash(
        &mut self,
        block_hash: &T,
        path: &TrieHash,
    ) -> Result<Option<(MARFValue, TrieMerkleProof<T>)>, Error> {
        if self.is_pruned() {
            return Err(Error::UnsupportedOnPrunedMarf("get_with_proof_from_hash"));
        }
        let Some(value) = self.get_by_path(block_hash, path)? else {
            return Ok(None);
        };
        let proof = self.make_proof(path, &value, block_hash)?;
        Ok(Some((value, proof)))
    }
}
impl<T: MarfTrieId> View<'_, T> {
    /// Construct an inclusion proof through the authenticated-read capability.
    pub fn make_proof(
        &mut self,
        path: &TrieHash,
        value: &MARFValue,
        block: &T,
    ) -> Result<TrieMerkleProof<T>, Error> {
        self.prove(path, value, block)
    }
}
