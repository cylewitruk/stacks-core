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

//! Storage-independent verification of inclusion proofs.
use std::collections::HashMap;

use marf_core::{MARFValue, TrieHash, TrieId};

use super::{ProofTrieNode, TrieMerkleProof, TrieMerkleProofType};
use crate::hash::marf_root_hash;
use crate::node::id;

impl<T: TrieId> TrieMerkleProof<T> {
    fn next_shunt_hash(hash: &TrieHash, idx: i64, hashes: &[TrieHash]) -> Option<TrieHash> {
        let mut all_hashes = Vec::with_capacity(hashes.len() + 1);
        let mut hash_idx = 0;
        for i in 0..hashes.len() + 1 {
            if idx == 0 {
                return None;
            }
            if idx - 1 == (i as i64) {
                all_hashes.push(*hash);
            } else {
                all_hashes.push(*hashes.get(hash_idx)?);
                hash_idx += 1;
            }
        }
        Some(TrieHash::from_data_array(&all_hashes))
    }

    fn verify_shunt_proof_head(
        node_root_hash: &TrieHash,
        head: &TrieMerkleProofType<T>,
    ) -> Option<TrieHash> {
        match head {
            TrieMerkleProofType::Shunt((idx, hashes)) => {
                if *idx != 0 {
                    return None;
                }
                Some(marf_root_hash(node_root_hash, hashes))
            }
            _ => None,
        }
    }

    fn verify_shunt_proof_tail(
        initial_hash: &TrieHash,
        shunt_proof: &[TrieMerkleProofType<T>],
    ) -> Option<TrieHash> {
        let mut hash = *initial_hash;
        for proof_node in shunt_proof {
            hash = match proof_node {
                TrieMerkleProofType::Shunt((idx, hashes)) => {
                    if *idx == 0 {
                        return None;
                    }
                    Self::next_shunt_hash(&hash, *idx, hashes)?
                }
                _ => return None,
            };
        }
        Some(hash)
    }

    fn verify_shunt_proof_junction(
        node_root_hash: &TrieHash,
        penultimate_trie_hash: &TrieHash,
        junction: &TrieMerkleProofType<T>,
    ) -> Option<TrieHash> {
        match junction {
            TrieMerkleProofType::Shunt((idx, hashes)) => {
                if *idx == 0 {
                    return None;
                }
                let mut all_hashes = Vec::with_capacity(hashes.len() + 2);
                let mut hash_idx = 0;
                all_hashes.push(*node_root_hash);
                for i in 0..hashes.len() + 1 {
                    if *idx - 1 == (i as i64) {
                        all_hashes.push(*penultimate_trie_hash);
                    } else {
                        all_hashes.push(*hashes.get(hash_idx)?);
                        hash_idx += 1;
                    }
                }
                Some(TrieHash::from_data_array(&all_hashes))
            }
            _ => None,
        }
    }

    fn get_segment_proof_hash(
        node: &ProofTrieNode<T>,
        hash: &TrieHash,
        chr: u8,
        hashes: &[TrieHash],
        count: usize,
    ) -> Option<TrieHash> {
        if node.ptrs.len() != count || count == 0 || hashes.len() != count - 1 {
            return None;
        }
        let mut all_hashes = Vec::with_capacity(count);
        let mut ih = 0;
        for child_ptr in &node.ptrs {
            if child_ptr.id != id::EMPTY && child_ptr.chr == chr {
                all_hashes.push(*hash);
            } else {
                all_hashes.push(*hashes.get(ih)?);
                ih += 1;
            }
        }
        if all_hashes.len() != count {
            return None;
        }
        Some(node.hash(&all_hashes))
    }

    fn verify_segment_proof(
        proof: &[TrieMerkleProofType<T>],
        node_hash: &TrieHash,
    ) -> Option<TrieHash> {
        let mut hash = *node_hash;
        for proof_node in proof {
            hash = match proof_node {
                TrieMerkleProofType::Leaf((_, node)) => node.hash(),
                TrieMerkleProofType::Node4((chr, node, hashes)) => {
                    Self::get_segment_proof_hash(node, &hash, *chr, hashes, 4)?
                }
                TrieMerkleProofType::Node16((chr, node, hashes)) => {
                    Self::get_segment_proof_hash(node, &hash, *chr, hashes, 16)?
                }
                TrieMerkleProofType::Node48((chr, node, hashes)) => {
                    Self::get_segment_proof_hash(node, &hash, *chr, hashes, 48)?
                }
                TrieMerkleProofType::Node256((chr, node, hashes)) => {
                    Self::get_segment_proof_hash(node, &hash, *chr, hashes, 256)?
                }
                TrieMerkleProofType::Shunt(_) => return None,
            };
        }
        Some(hash)
    }

    fn get_segment_proof_path_prefix(segment_proof: &[TrieMerkleProofType<T>]) -> Option<Vec<u8>> {
        let mut path_parts = vec![];
        for proof_node in segment_proof {
            match proof_node {
                TrieMerkleProofType::Leaf((_, node)) => path_parts.push(node.path.clone()),
                TrieMerkleProofType::Node4((chr, node, _))
                | TrieMerkleProofType::Node16((chr, node, _))
                | TrieMerkleProofType::Node48((chr, node, _))
                | TrieMerkleProofType::Node256((chr, node, _)) => {
                    path_parts.push(vec![*chr]);
                    path_parts.push(node.path.clone());
                }
                TrieMerkleProofType::Shunt(_) => return None,
            }
        }
        Some(path_parts.into_iter().rev().flatten().collect())
    }

    fn is_proof_well_formed(proof: &[TrieMerkleProofType<T>], expected_path: &TrieHash) -> bool {
        let Some(TrieMerkleProofType::Leaf(_)) = proof.first() else {
            return false;
        };
        let mut i = 0;
        let mut path_bytes = vec![];
        while i < proof.len() {
            let mut j = i + 1;
            while let Some(step) = proof.get(j) {
                if let TrieMerkleProofType::Shunt(_) = step {
                    break;
                }
                j += 1;
            }
            let Some(segment_proof) = proof.get(i..j) else {
                return false;
            };
            if i == 0 {
                let Some(p) = Self::get_segment_proof_path_prefix(segment_proof) else {
                    return false;
                };
                path_bytes = p;
                if expected_path.as_bytes() != path_bytes.as_slice() {
                    return false;
                }
            } else {
                let Some(new_path_bytes) = Self::get_segment_proof_path_prefix(segment_proof)
                else {
                    return false;
                };
                let Some(prefix) = path_bytes.get(..new_path_bytes.len()) else {
                    return false;
                };
                if prefix != new_path_bytes.as_slice() {
                    return false;
                }
            }
            i = j;
            if i >= proof.len() {
                return false;
            }
            j = i + 1;
            while let Some(step) = proof.get(j) {
                if let TrieMerkleProofType::Shunt(_) = step {
                    j += 1;
                } else {
                    break;
                }
            }
            i = j;
        }
        true
    }

    /// Verify a proof against `root_hash`, given the mapping from trie root
    /// hashes to the blocks that produced them.
    pub fn verify_proof(
        proof: &[TrieMerkleProofType<T>],
        path: &TrieHash,
        value: &MARFValue,
        root_hash: &TrieHash,
        root_to_block: &HashMap<TrieHash, T>,
    ) -> bool {
        if !Self::is_proof_well_formed(proof, path) {
            return false;
        }
        let (mut node_hash, node_data) = match proof.first() {
            Some(TrieMerkleProofType::Leaf((_, node))) => (node.hash(), node.data.clone()),
            _ => return false,
        };
        if node_data != *value {
            return false;
        }

        let mut i = 0;
        let mut j = i + 1;
        while let Some(step) = proof.get(j) {
            if let TrieMerkleProofType::Shunt(_) = step {
                break;
            }
            j += 1;
        }
        let Some(segment_proof) = proof.get(i..j) else {
            return false;
        };
        let Some(node_root_hash) = Self::verify_segment_proof(segment_proof, &node_hash) else {
            return false;
        };
        i = j;
        let Some(shunt_head) = proof.get(i) else {
            return false;
        };
        let Some(mut trie_hash) = Self::verify_shunt_proof_head(&node_root_hash, shunt_head) else {
            return false;
        };
        i += 1;
        let Some(segment_head) = proof.get(i) else {
            return *root_hash == trie_hash;
        };
        node_hash = match root_to_block.get(&trie_hash) {
            Some(bhh) => TrieHash(bhh.clone().to_bytes()),
            None => return false,
        };
        if let TrieMerkleProofType::Shunt(_) = segment_head {
            return false;
        }

        while i < proof.len() {
            j = i + 1;
            while let Some(step) = proof.get(j) {
                if let TrieMerkleProofType::Shunt(_) = step {
                    break;
                }
                j += 1;
            }
            let Some(segment_proof) = proof.get(i..j) else {
                return false;
            };
            let Some(next_node_root_hash) = Self::verify_segment_proof(segment_proof, &node_hash)
            else {
                return false;
            };
            i = j;
            if i >= proof.len() {
                return false;
            }
            j = i;
            while let Some(step) = proof.get(j) {
                if let TrieMerkleProofType::Shunt((idx, _)) = step {
                    if *idx == 0 {
                        break;
                    }
                    j += 1;
                } else {
                    break;
                }
            }
            if j == 0 {
                return false;
            }
            j -= 1;
            if j < i {
                return false;
            }
            let Some(shunt_tail) = proof.get(i..j) else {
                return false;
            };
            let Some(penultimate) = Self::verify_shunt_proof_tail(&trie_hash, shunt_tail) else {
                return false;
            };
            i = j;
            let Some(junction) = proof.get(i) else {
                return false;
            };
            let Some(next_trie_hash) =
                Self::verify_shunt_proof_junction(&next_node_root_hash, &penultimate, junction)
            else {
                return false;
            };
            trie_hash = next_trie_hash;
            node_hash = match root_to_block.get(&trie_hash) {
                Some(bhh) => TrieHash(bhh.clone().to_bytes()),
                None => return false,
            };
            i += 1;
            if trie_hash == *root_hash {
                break;
            }
        }
        *root_hash == trie_hash
    }

    pub fn verify(
        &self,
        path: &TrieHash,
        marf_value: &MARFValue,
        root_hash: &TrieHash,
        root_to_block: &HashMap<TrieHash, T>,
    ) -> bool {
        Self::verify_proof(&self.0, path, marf_value, root_hash, root_to_block)
    }
}
