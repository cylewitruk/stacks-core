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

//! Capabilities for authenticated traversal without exposing backend state.

use stacks_common::types::chainstate::TrieHash;

use crate::node::{Node, Ptr};
use crate::{Error, MarfTrieId};

/// A trie selected for reads, either under construction or persisted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrieLocation {
    /// The current in-memory trie.
    Mem,
    /// A persisted trie identified by its local storage id.
    Block(u32),
}

/// A borrowed node; materialization is explicit and only needed at proof boundaries.
pub trait NodeRead {
    /// The compressed path prefix, borrowed from the source.
    fn path(&self) -> Result<&[u8], Error>;
    /// Whether the selected record is a leaf.
    fn is_leaf(&self) -> Result<bool, Error>;
    /// Select a child without decoding the entire node.
    fn child(&self, chr: u8) -> Result<Option<Ptr>, Error>;
    /// Materialize this node when its complete representation is needed.
    fn to_node(self) -> Result<Node, Error>;
}

/// Authenticated trie reads, block identity and skip-list ancestry.
pub trait AuthenticatedRead<T: MarfTrieId> {
    /// Select a persisted or in-progress trie, validating its existence.
    fn select_trie(&mut self, tip: &T) -> Result<TrieLocation, Error>;
    /// The root pointer in the selected trie.
    fn root_pointer(&self, site: TrieLocation) -> Ptr;
    /// Read an inline node; callers must not pass a back-pointer.
    fn read_node(&mut self, site: TrieLocation, ptr: &Ptr) -> Result<Node, Error>;
    /// Borrow an inline node until the next mutable source access.
    fn borrow_node(&mut self, site: TrieLocation, ptr: &Ptr) -> Result<impl NodeRead + '_, Error>;
    /// Hash of an inline child in the selected trie.
    fn child_hash(&mut self, site: TrieLocation, ptr: &Ptr) -> Result<TrieHash, Error>;
    /// Resolve a local block id to its consensus identity.
    fn block_hash(&mut self, block_id: u32) -> Result<T, Error>;
    /// Ancestor root hashes in the order committed by the MARF root.
    fn ancestor_hashes(&mut self, tip: &T, height: u32) -> Result<Vec<TrieHash>, Error>;
    /// Whether historical nodes have been pruned.
    fn is_pruned(&self) -> bool;
    /// Height of a block within the fork ending at `tip`.
    fn get_block_height(&mut self, block: &T, tip: &T) -> Result<Option<u32>, Error>;
    /// Root committed by the selected block.
    fn get_root_hash_at(&mut self, tip: &T) -> Result<TrieHash, Error>;
    /// Block identity at a height in the selected fork.
    fn get_block_at_height(&mut self, height: u32, tip: &T) -> Result<Option<T>, Error>;
}
