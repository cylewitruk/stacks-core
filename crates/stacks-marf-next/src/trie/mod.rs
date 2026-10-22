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

//! The in-memory trie under construction for one block.
//!
//! Nodes live in a flat arena indexed by `u32`; an inline child pointer's
//! `ptr` field is an arena index until [`Trie::serialize`] assigns byte
//! offsets. Writes never touch persistent storage: a node that lives in an
//! ancestor block is copied into the arena on first touch (copy-on-write) and
//! its own children become back-pointers into that ancestor block.
//!
//! Hashes are computed once, post-order, by [`Trie::seal`].

use stacks_common::types::chainstate::TrieHash;

use crate::node::{BACKPTR, Branch, Kind, Node, Ptr};
use crate::{Error, MarfTrieId, corrupt};

/// Fixed blob header: `parent block id (32) || reserved u32 LE (0)`.
pub const HEADER_LEN: usize = 36;
/// Byte offset of the root node (and therefore of the root hash) in a blob.
pub const ROOT_OFFSET: u64 = HEADER_LEN as u64;

/// Where a node copied into this trie came from, for delta encoding at flush.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Origin {
    /// Storage id of the block the node was read from.
    pub block: u32,
    /// Back-pointer form of the pointer that was followed to read it.
    pub ptr: Ptr,
    /// Number of patches that were stacked on the base node when it was read.
    pub depth: u32,
}

#[derive(Clone, Debug)]
pub struct Slot {
    pub node: Node,
    pub hash: TrieHash,
    pub origin: Option<Origin>,
}

#[derive(Clone)]
pub struct Trie<T> {
    block: T,
    parent: T,
    slots: Vec<Slot>,
    sealed: bool,
}

/// A node read from an ancestor block, with the patch depth of its location.
pub struct Fetched {
    pub node: Node,
    pub depth: u32,
}

mod hashing;
mod insert;
mod persist;
#[cfg(test)]
mod tests;

impl<T: MarfTrieId> Trie<T> {
    /// A trie whose root is `root` (a `Node256`), copied from `origin` if any.
    pub fn new(block: T, parent: T, root: Branch, origin: Option<Origin>) -> Trie<T> {
        debug_assert_eq!(root.kind, Kind::Node256);
        Trie {
            block,
            parent,
            slots: vec![Slot {
                node: Node::Branch(root),
                hash: TrieHash::ZERO,
                origin,
            }],
            sealed: false,
        }
    }

    /// A trie over an existing node list whose inline pointers are arena
    /// indices and whose first node is the `Node256` root.
    pub fn from_nodes(block: T, parent: T, nodes: Vec<Node>) -> Result<Trie<T>, Error> {
        match nodes.first() {
            Some(Node::Branch(b)) if b.kind == Kind::Node256 => {}
            _ => return Err(corrupt!("first node is not a Node256 root")),
        }
        Ok(Trie {
            block,
            parent,
            slots: nodes
                .into_iter()
                .map(|node| Slot {
                    node,
                    hash: TrieHash::ZERO,
                    origin: None,
                })
                .collect(),
            sealed: false,
        })
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    pub fn is_sealed(&self) -> bool {
        self.sealed
    }

    pub fn node(&self, idx: u32) -> Result<&Node, Error> {
        self.slots
            .get(idx as usize)
            .map(|s| &s.node)
            .ok_or_else(|| corrupt!("arena index {idx} out of range ({})", self.slots.len()))
    }

    /// The hash stored for node `idx` (zero until sealed).
    pub fn hash(&self, idx: u32) -> Result<TrieHash, Error> {
        self.slots
            .get(idx as usize)
            .map(|s| s.hash)
            .ok_or_else(|| corrupt!("arena index {idx} out of range ({})", self.slots.len()))
    }

    pub fn root_hash(&self) -> TrieHash {
        self.slots[0].hash
    }

    pub fn set_root_hash(&mut self, hash: TrieHash) {
        self.slots[0].hash = hash;
    }
}
/// Make the back-pointer bit explicit on a pointer in `block`.
pub fn as_backptr(p: Ptr, block: u32) -> Ptr {
    Ptr {
        id: p.id | BACKPTR,
        back_block: block,
        ..p
    }
}
impl<T: MarfTrieId> Trie<T> {
    fn push(&mut self, node: Node, origin: Option<Origin>) -> u32 {
        self.slots.push(Slot {
            node,
            hash: TrieHash::ZERO,
            origin,
        });
        (self.slots.len() - 1) as u32
    }
}

/// Identity of the block represented by an in-memory trie.
pub trait TrieIdentity<T> {
    /// The block being constructed or loaded.
    fn block_id(&self) -> &T;
}
impl<T> TrieIdentity<T> for Trie<T> {
    fn block_id(&self) -> &T {
        &self.block
    }
}
