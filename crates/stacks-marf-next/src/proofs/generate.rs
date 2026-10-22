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

//! Inclusion proof construction over authenticated reads.
use stacks_common::types::chainstate::TrieHash;

use super::{ProofTrieNode, ProofTriePtr, TrieLeaf, TrieMerkleProof, TrieMerkleProofType};
use crate::access::{AuthenticatedRead, NodeRead};
use crate::node::{Node, Ptr, id};
use crate::{Error, MARFValue, MarfTrieId, corrupt};

/// One trie's part of a proof walk: the inline pointers followed from the
/// root, the node they end at, and the back-pointer that ends the segment
/// (`None` when the segment ends at the leaf).
struct Segment {
    ptrs: Vec<Ptr>,
    node: Node,
    /// Path byte consumed last: the back-pointer's or the leaf's final byte.
    chr: u8,
    backptr: Option<Ptr>,
}

/// A borrowed authenticated source used for one proof construction.
pub struct Generator<'a, S: ?Sized>(pub &'a mut S);
impl<T: MarfTrieId> TrieMerkleProof<T> {
    /// Construct an inclusion proof from a source capability.
    pub fn from_source(
        source: &mut (impl AuthenticatedRead<T> + ?Sized),
        path: &TrieHash,
        value: &MARFValue,
        block: &T,
    ) -> Result<Self, Error> {
        Generator(source).make_proof(path, value, block)
    }
}
impl<S: ?Sized> Generator<'_, S> {
    /// Walk `path` inside the trie of `tip` only, stopping at the leaf or at
    /// the first back-pointer.
    fn walk_segment<T: MarfTrieId>(&mut self, tip: &T, path: &TrieHash) -> Result<Segment, Error>
    where
        S: AuthenticatedRead<T>,
    {
        let site = self.0.select_trie(tip)?;
        let mut ptr = self.0.root_pointer(site);
        let mut ptrs = vec![ptr];
        let key = path.as_bytes();
        let mut depth = 0usize;
        loop {
            let node = self.0.borrow_node(site, &ptr)?;
            let np = node.path()?;
            if key.len() < depth + np.len() || &key[depth..depth + np.len()] != np {
                return Err(Error::NotFoundError);
            }
            depth += np.len();
            let Some(&chr) = key.get(depth) else {
                // end of path: must be at the leaf
                return Ok(Segment {
                    ptrs,
                    node: node.to_node()?,
                    chr: key[key.len() - 1],
                    backptr: None,
                });
            };
            depth += 1;
            if node.is_leaf()? {
                return Err(corrupt!("leaf path does not reach the end of the key"));
            }
            let Some(child) = node.child(chr)? else {
                return Err(Error::NotFoundError);
            };
            if child.is_back() {
                return Ok(Segment {
                    ptrs,
                    node: node.to_node()?,
                    chr,
                    backptr: Some(child),
                });
            }
            ptr = child;
            ptrs.push(ptr);
        }
    }

    /// The proof node for the node at `ptr`, omitting the child at `prev_chr`.
    fn proof_node<T: MarfTrieId>(
        &mut self,
        tip: &T,
        ptr: &Ptr,
        prev_chr: u8,
    ) -> Result<TrieMerkleProofType<T>, Error>
    where
        S: AuthenticatedRead<T>,
    {
        let site = self.0.select_trie(tip)?;
        let node = self.0.read_node(site, ptr)?;
        match node {
            Node::Leaf(leaf) => Ok(TrieMerkleProofType::Leaf((
                prev_chr,
                TrieLeaf::from_leaf(&leaf),
            ))),
            Node::Branch(branch) => {
                let mut hashes = Vec::with_capacity(branch.kind.capacity());
                let mut proof_ptrs = Vec::with_capacity(branch.kind.capacity());
                let mut found = false;
                for slot in branch.slots() {
                    match slot {
                        None => {
                            hashes.push(TrieHash::EMPTY);
                            proof_ptrs.push(ProofTriePtr {
                                id: 0,
                                chr: 0,
                                back_block: T::from_bytes([0u8; 32]),
                            });
                        }
                        Some(p) => {
                            let (block_bytes, child_hash) = if p.is_back() {
                                let bhh = self.0.block_hash(p.back_block)?;
                                (bhh.clone(), TrieHash(bhh.to_bytes()))
                            } else {
                                (T::from_bytes([0u8; 32]), self.0.child_hash(site, p)?)
                            };
                            proof_ptrs.push(ProofTriePtr {
                                id: p.id,
                                chr: p.chr,
                                back_block: block_bytes,
                            });
                            if p.chr == prev_chr {
                                found = true;
                            } else {
                                hashes.push(child_hash);
                            }
                        }
                    }
                }
                if !found {
                    return Err(Error::NotFoundError);
                }
                let node = ProofTrieNode {
                    id: branch.kind.id(),
                    path: branch.path.as_slice().to_vec(),
                    ptrs: proof_ptrs,
                };
                Ok(match branch.kind {
                    crate::node::Kind::Node4 => TrieMerkleProofType::Node4((
                        prev_chr,
                        node,
                        hashes.try_into().expect("3 hashes"),
                    )),
                    crate::node::Kind::Node16 => TrieMerkleProofType::Node16((
                        prev_chr,
                        node,
                        hashes.try_into().expect("15 hashes"),
                    )),
                    crate::node::Kind::Node48 => TrieMerkleProofType::Node48((
                        prev_chr,
                        node,
                        hashes.try_into().expect("47 hashes"),
                    )),
                    crate::node::Kind::Node256 => TrieMerkleProofType::Node256((
                        prev_chr,
                        node,
                        hashes.try_into().expect("255 hashes"),
                    )),
                })
            }
        }
    }

    fn make_segment_proof<T: MarfTrieId>(
        &mut self,
        tip: &T,
        segment: &Segment,
    ) -> Result<Vec<TrieMerkleProofType<T>>, Error>
    where
        S: AuthenticatedRead<T>,
    {
        let mut proof = Vec::with_capacity(segment.ptrs.len());
        let mut prev_chr = segment.chr;
        for ptr in segment.ptrs.iter().rev() {
            proof.push(self.proof_node(tip, ptr, prev_chr)?);
            prev_chr = ptr.chr;
        }
        Ok(proof)
    }

    /// Height of `tip` as recorded in its own trie.
    fn height_of<T: MarfTrieId>(&mut self, tip: &T) -> Result<u32, Error>
    where
        S: AuthenticatedRead<T>,
    {
        self.0
            .get_block_height(tip, tip)?
            .ok_or_else(|| corrupt!("Could not find block height of block {tip}"))
    }

    /// The ancestor root hashes hashed into `tip`'s MARF root.
    fn ancestors_of<T: MarfTrieId>(&mut self, tip: &T) -> Result<Vec<TrieHash>, Error>
    where
        S: AuthenticatedRead<T>,
    {
        let h = self.height_of(tip)?;
        self.0.ancestor_hashes(tip, h)
    }

    /// The shunt proof linking `tip`'s trie to the trie of `backptr.back_block`.
    fn make_backptr_shunt_proof<T: MarfTrieId>(
        &mut self,
        tip: &T,
        backptr: &Ptr,
    ) -> Result<Vec<TrieMerkleProofType<T>>, Error>
    where
        S: AuthenticatedRead<T>,
    {
        let mut proof = vec![];
        let mut block = tip.clone();
        let ancestor_block = self.0.block_hash(backptr.back_block)?;
        let ancestor_root_hash = self.0.get_root_hash_at(&ancestor_block)?;
        let ancestor_height = self.height_of(&ancestor_block)?;
        let mut current_height = self.height_of(&block)?;
        let mut found = false;

        while current_height > ancestor_height && !found {
            let ancestor_hashes = self.ancestors_of(&block)?;
            found = ancestor_hashes.contains(&ancestor_root_hash);

            let mut idx = 0u32;
            while (1u32 << idx) <= current_height
                && current_height - (1u32 << idx) >= ancestor_height
            {
                idx += 1;
            }
            if idx == 0 {
                return Err(corrupt!(
                    "ancestor_height = {ancestor_height}, current_height = {current_height}, but ancestor hash {ancestor_root_hash} not found"
                ));
            }
            idx -= 1;
            if found && ancestor_hashes.get(idx as usize) != Some(&ancestor_root_hash) {
                return Err(corrupt!(
                    "ancestor hash is not at the expected skip-list index"
                ));
            }
            current_height -= 1u32 << idx;
            let next_block = self
                .0
                .get_block_at_height(current_height, &block)?
                .ok_or_else(|| corrupt!("Could not find block at height of {current_height}"))?;

            let mut trimmed: Vec<TrieHash> = ancestor_hashes
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != idx as usize)
                .map(|(_, h)| *h)
                .collect();
            let mut idx = i64::from(idx) + 1;

            if !proof.is_empty() {
                // the root node hash (without ancestors) of the block we are leaving
                let root_hash = self.proof_root_node_hash(&block)?;
                trimmed.insert(0, root_hash);
                idx += 1;
            }
            proof.push(TrieMerkleProofType::Shunt((idx, trimmed)));
            block = next_block;
        }
        proof.reverse();
        Ok(proof)
    }

    /// Hash of `tip`'s root node (without the ancestor skip-list).
    fn proof_root_node_hash<T: MarfTrieId>(&mut self, tip: &T) -> Result<TrieHash, Error>
    where
        S: AuthenticatedRead<T>,
    {
        let site = self.0.select_trie(tip)?;
        let root_ptr = self.0.root_pointer(site);
        let (node, hashes) = self.proof_node_full(tip, &root_ptr)?;
        if node.id != id::NODE256 {
            return Err(corrupt!("Root node at {tip} is not a TrieNode256"));
        }
        Ok(node.hash(&hashes))
    }

    /// A branch's proof view with all child hashes.
    fn proof_node_full<T: MarfTrieId>(
        &mut self,
        tip: &T,
        ptr: &Ptr,
    ) -> Result<(ProofTrieNode<T>, Vec<TrieHash>), Error>
    where
        S: AuthenticatedRead<T>,
    {
        let site = self.0.select_trie(tip)?;
        let node = self.0.read_node(site, ptr)?;
        let Node::Branch(branch) = node else {
            return Err(corrupt!("expected a branch"));
        };
        let mut hashes = Vec::with_capacity(branch.kind.capacity());
        let mut proof_ptrs = Vec::with_capacity(branch.kind.capacity());
        for slot in branch.slots() {
            match slot {
                None => {
                    hashes.push(TrieHash::EMPTY);
                    proof_ptrs.push(ProofTriePtr {
                        id: 0,
                        chr: 0,
                        back_block: T::from_bytes([0u8; 32]),
                    });
                }
                Some(p) => {
                    let (block_bytes, child_hash) = if p.is_back() {
                        let bhh = self.0.block_hash(p.back_block)?;
                        (bhh.clone(), TrieHash(bhh.to_bytes()))
                    } else {
                        (T::from_bytes([0u8; 32]), self.0.child_hash(site, p)?)
                    };
                    proof_ptrs.push(ProofTriePtr {
                        id: p.id,
                        chr: p.chr,
                        back_block: block_bytes,
                    });
                    hashes.push(child_hash);
                }
            }
        }
        let node = ProofTrieNode {
            id: branch.kind.id(),
            path: branch.path.as_slice().to_vec(),
            ptrs: proof_ptrs,
        };
        Ok((node, hashes))
    }

    /// Make a Merkle proof of inclusion for `path` as of `root_block`.
    pub fn make_proof<T: MarfTrieId>(
        &mut self,
        path: &TrieHash,
        expected_value: &MARFValue,
        root_block: &T,
    ) -> Result<TrieMerkleProof<T>, Error>
    where
        S: AuthenticatedRead<T>,
    {
        if self.0.is_pruned() {
            return Err(Error::UnsupportedOnPrunedMarf("TrieMerkleProof::from_path"));
        }
        let mut segment_proofs = vec![];
        let mut shunt_proofs = vec![];
        let mut block = root_block.clone();
        loop {
            self.0.select_trie(&block)?;
            let segment = self.walk_segment(&block, path)?;
            segment_proofs.push(self.make_segment_proof(&block, &segment)?);
            match segment.backptr {
                Some(backptr) => {
                    shunt_proofs.push(self.make_backptr_shunt_proof(&block, &backptr)?);
                    block = self.0.block_hash(backptr.back_block)?;
                }
                None => {
                    let ancestors = self.ancestors_of(&block)?;
                    shunt_proofs.push(vec![TrieMerkleProofType::Shunt((0, ancestors))]);
                    match &segment.node {
                        Node::Leaf(leaf) if leaf.value == *expected_value => {}
                        _ => return Err(Error::NotFoundError),
                    }
                    break;
                }
            }
        }
        segment_proofs.reverse();
        shunt_proofs.reverse();
        let mut proof = Vec::with_capacity(segment_proofs.len() * 2);
        for (mut segment, mut shunt) in segment_proofs.into_iter().zip(shunt_proofs) {
            proof.append(&mut segment);
            proof.append(&mut shunt);
        }
        Ok(TrieMerkleProof(proof))
    }
}
