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

//! Copy-on-write insertion and branch splitting.
use super::{Fetched, Origin, Trie};
use crate::node::{Branch, Kind, Leaf, Node, Path, Ptr, backptr_children, id};
use crate::observe::Observer;
use crate::{Error, MARFValue, TrieId, corrupt};

fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}
impl<T: TrieId, O: Observer> Trie<T, O> {
    /// Insert `value` at `key`, copying ancestor nodes forward as needed.
    /// `fetch` reads the node a back-pointer refers to.
    pub fn insert(
        &mut self,
        key: &[u8; 32],
        value: MARFValue,
        mut fetch: impl FnMut(&Ptr) -> Result<Fetched, Error>,
    ) -> Result<(), Error> {
        let _span = observe!(O, TrieInsert);
        if self.sealed {
            return Err(Error::ReadOnlyError);
        }
        let mut idx = 0u32;
        let mut depth = 0usize;
        loop {
            let node = &self.slots[idx as usize].node;
            let node_path = node.path().as_slice();
            let common = common_prefix(node_path, &key[depth..]);
            if common < node_path.len() {
                return self.split(idx, depth, common, key, value);
            }
            depth += common;
            let chr = match &self.slots[idx as usize].node {
                Node::Leaf(_) => {
                    if depth != key.len() {
                        return Err(corrupt!("leaf path does not reach the end of the key"));
                    }
                    if let Node::Leaf(leaf) = &mut self.slots[idx as usize].node {
                        leaf.value = value;
                    }
                    return Ok(());
                }
                Node::Branch(_) => {
                    let Some(chr) = key.get(depth) else {
                        return Err(corrupt!("key exhausted at a branch node"));
                    };
                    *chr
                }
            };
            let child = self.slots[idx as usize]
                .node
                .as_branch()
                .and_then(|b| b.child(chr))
                .copied();
            match child {
                None => return self.attach_leaf(idx, chr, &key[depth + 1..], value),
                Some(p) if p.is_back() => {
                    let fetched = fetch(&p)?;
                    let mut node = fetched.node;
                    if node.kind_id() != p.kind_id() {
                        return Err(corrupt!(
                            "back-pointer {p:?} led to a node of type {:#x}",
                            node.kind_id()
                        ));
                    }
                    if let Node::Branch(b) = &mut node {
                        backptr_children(&mut b.children, p.back_block);
                    }
                    let new_idx = self.push(
                        node,
                        Some(Origin {
                            block: p.back_block,
                            ptr: p,
                            depth: fetched.depth,
                        }),
                    );
                    let parent = self.slots[idx as usize]
                        .node
                        .as_branch_mut()
                        .expect("branch");
                    parent.replace(Ptr::new(p.kind_id(), chr, u64::from(new_idx)));
                    idx = new_idx;
                    depth += 1;
                }
                Some(p) => {
                    idx = u32::try_from(p.ptr).map_err(|_| Error::OverflowError)?;
                    depth += 1;
                }
            }
        }
    }

    /// Add a leaf under the branch at `idx`, promoting the branch if full.
    fn attach_leaf(
        &mut self,
        idx: u32,
        chr: u8,
        rest: &[u8],
        value: MARFValue,
    ) -> Result<(), Error> {
        let leaf = Leaf {
            path: Path::from_slice(rest).ok_or(Error::OverflowError)?,
            value,
        };
        let leaf_idx = self.push(Node::Leaf(leaf), None);
        let ptr = Ptr::new(id::LEAF, chr, u64::from(leaf_idx));
        let branch = self.slots[idx as usize]
            .node
            .as_branch_mut()
            .expect("branch");
        if !branch.insert(ptr) {
            let mut promoted = branch
                .promoted()
                .ok_or_else(|| corrupt!("could not insert into a full Node256"))?;
            assert!(promoted.insert(ptr));
            *branch = promoted;
        }
        Ok(())
    }

    /// The node at `idx` diverges from `key` after `common` bytes of its
    /// path: replace it with a small branch holding the shortened node and a
    /// new leaf.
    fn split(
        &mut self,
        idx: u32,
        depth: usize,
        common: usize,
        key: &[u8; 32],
        value: MARFValue,
    ) -> Result<(), Error> {
        let slot = &mut self.slots[idx as usize];
        let old_path = *slot.node.path();
        let old_chr = old_path.as_slice()[common];
        let shared = Path::from_slice(&old_path.as_slice()[..common]).expect("<= 32");
        let old_rest = Path::from_slice(&old_path.as_slice()[common + 1..]).expect("<= 32");
        let old_is_leaf = slot.node.is_leaf();

        // Move the old node to a new arena slot (it keeps its origin).
        let kind = if idx == 0 { Kind::Node256 } else { Kind::Node4 };
        let mut parent = Node::Branch(Branch::new(kind, shared));
        std::mem::swap(&mut slot.node, &mut parent);
        let mut old_node = parent;
        old_node.set_path(old_rest);
        let old_kind = old_node.kind_id();
        let old_origin = slot.origin.take();
        let old_idx = self.push(old_node, old_origin);
        let old_ptr = Ptr::new(old_kind, old_chr, u64::from(old_idx));

        let leaf_chr = key[depth + common];
        let leaf = Leaf {
            path: Path::from_slice(&key[depth + common + 1..]).expect("<= 32"),
            value,
        };
        let leaf_idx = self.push(Node::Leaf(leaf), None);
        let leaf_ptr = Ptr::new(id::LEAF, leaf_chr, u64::from(leaf_idx));

        // Slot order is consensus-relevant and follows the original
        // implementation: splitting a leaf puts the old leaf first, splitting
        // a branch puts the new leaf first.
        let branch = self.slots[idx as usize]
            .node
            .as_branch_mut()
            .expect("branch");
        if old_is_leaf {
            assert!(branch.insert(old_ptr));
            assert!(branch.insert(leaf_ptr));
        } else {
            assert!(branch.insert(leaf_ptr));
            assert!(branch.insert(old_ptr));
        }
        Ok(())
    }
}
