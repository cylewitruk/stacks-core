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

//! Post-order hashing of the arena.
use super::Trie;
use crate::hash::{ChildHash, HashScratch, branch_hash, leaf_hash};
use crate::node::Node;
use crate::observe::Observer;
use crate::{Error, TrieHash, TrieId};

impl<T: TrieId, O: Observer> Trie<T, O> {
    /// Compute every node hash post-order and return the root node's hash
    /// (before the ancestor skip-list is mixed in). `block_id` maps a
    /// back-pointer's block id to that block's 32-byte identifier.
    pub fn seal(
        &mut self,
        mut block_id: impl FnMut(u32) -> Result<[u8; 32], Error>,
    ) -> Result<TrieHash, Error> {
        let _span = observe!(O, TrieSeal);
        let mut scratch = HashScratch::default();
        let root = self.hash_subtree(0, &mut block_id, &mut scratch)?;
        self.sealed = true;
        Ok(root)
    }

    fn hash_subtree(
        &mut self,
        idx: u32,
        block_id: &mut impl FnMut(u32) -> Result<[u8; 32], Error>,
        scratch: &mut HashScratch,
    ) -> Result<TrieHash, Error> {
        let _span = observe!(O, TrieHashSubtree);
        let n_children = match &self.slots[idx as usize].node {
            Node::Leaf(leaf) => {
                let hash = leaf_hash(leaf);
                self.slots[idx as usize].hash = hash;
                return Ok(hash);
            }
            Node::Branch(b) => b.children.len(),
        };
        // Hash inline children first (post-order), and normalize their pointer
        // ids to the children's actual kinds: a child may have been promoted
        // since the pointer was written.
        for i in 0..n_children {
            let p = self.slots[idx as usize]
                .node
                .as_branch()
                .expect("branch")
                .children[i];
            if p.is_back() {
                continue;
            }
            let child_idx = u32::try_from(p.ptr).map_err(|_| Error::OverflowError)?;
            self.hash_subtree(child_idx, block_id, scratch)?;
            let kind = self.slots[child_idx as usize].node.kind_id();
            self.slots[idx as usize]
                .node
                .as_branch_mut()
                .expect("branch")
                .children[i]
                .id = kind;
        }
        let slots = &self.slots;
        let branch = slots[idx as usize].node.as_branch().expect("branch");
        let hash = branch_hash::<Error>(
            branch,
            |p| {
                if p.is_back() {
                    Ok(ChildHash::back(block_id(p.back_block)?))
                } else {
                    Ok(ChildHash::inline(slots[p.ptr as usize].hash))
                }
            },
            scratch,
        )?;
        self.slots[idx as usize].hash = hash;
        Ok(hash)
    }
}
