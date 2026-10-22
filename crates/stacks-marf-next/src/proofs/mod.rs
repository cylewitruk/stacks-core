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

//! Merkle proofs of inclusion.
//!
//! A proof for `(path, value)` as of block `B` alternates *segment proofs*
//! and *shunt proofs*, starting at the leaf:
//!
//! * a segment proof is the chain of nodes from a trie's root down to either
//!   the leaf or the back-pointer that leads to an older block, each node
//!   carrying its sibling hashes;
//! * a shunt proof links that trie's root to the next trie's root through the
//!   ancestor skip-list that root hashes are computed over.
//!
//! The wire format and the verification algorithm are those of the original
//! implementation; they are visible to RPC clients and must not change.

use std::fmt;
use std::io::{Read, Write};
use std::ops::Deref;

use sha2::{Digest, Sha512_256};
use stacks_common::codec::{Error as codec_error, StacksMessageCodec, read_next};
use stacks_common::types::chainstate::TrieHash;
use stacks_common::util::hash::to_hex;

pub use self::wire::TrieMerkleProofTypeIndicator;
use crate::access::AuthenticatedRead;
use crate::node::{Leaf, id};
use crate::{ClarityMarfTrieId, Error, MARFValue, MarfTrieId};

/// Leaf of a Trie, as it appears in proofs and the public API.
#[derive(Clone, PartialEq, Eq)]
pub struct TrieLeaf {
    pub path: Vec<u8>,
    pub data: MARFValue,
}

impl TrieLeaf {
    pub fn new(path: &[u8], data: &[u8]) -> TrieLeaf {
        assert!(data.len() <= 40);
        let mut bytes = [0u8; 40];
        bytes[..data.len()].copy_from_slice(data);
        TrieLeaf {
            path: path.to_vec(),
            data: MARFValue(bytes),
        }
    }

    pub fn from_value(path: &[u8], value: MARFValue) -> TrieLeaf {
        TrieLeaf {
            path: path.to_vec(),
            data: value,
        }
    }

    fn from_leaf(leaf: &Leaf) -> TrieLeaf {
        TrieLeaf {
            path: leaf.path.as_slice().to_vec(),
            data: leaf.value.clone(),
        }
    }

    fn hash(&self) -> TrieHash {
        let mut h = Sha512_256::new();
        h.update([id::LEAF, self.path.len() as u8]);
        h.update(&self.path);
        h.update(self.data.0);
        TrieHash(h.finalize().into())
    }
}

impl fmt::Debug for TrieLeaf {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "TrieLeaf(path={} data={})",
            to_hex(&self.path),
            self.data.to_hex()
        )
    }
}

impl StacksMessageCodec for TrieLeaf {
    fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), codec_error> {
        self.path.consensus_serialize(fd)?;
        self.data.consensus_serialize(fd)
    }

    fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<TrieLeaf, codec_error> {
        let path = read_next(fd)?;
        let data = read_next(fd)?;
        Ok(TrieLeaf { path, data })
    }
}

/// A child pointer as it appears in a proof: back-pointers carry the block id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProofTriePtr<T> {
    pub id: u8,
    pub chr: u8,
    pub back_block: T,
}

/// A branch node as it appears in a proof: every slot, empty ones included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProofTrieNode<T> {
    pub id: u8,
    pub path: Vec<u8>,
    pub ptrs: Vec<ProofTriePtr<T>>,
}

impl<T: ClarityMarfTrieId> ProofTrieNode<T> {
    /// `H(consensus bytes || child hashes)`.
    fn hash(&self, child_hashes: &[TrieHash]) -> TrieHash {
        let mut h = Sha512_256::new();
        h.update([self.id]);
        for p in &self.ptrs {
            h.update([p.id, p.chr]);
            h.update(p.back_block.as_bytes());
        }
        h.update([self.path.len() as u8]);
        h.update(&self.path);
        for c in child_hashes {
            h.update(c.as_bytes());
        }
        TrieHash(h.finalize().into())
    }
}

#[derive(Clone)]
#[allow(clippy::large_enum_variant)] // wire format: fixed-size sibling hash arrays
pub enum TrieMerkleProofType<T> {
    Node4((u8, ProofTrieNode<T>, [TrieHash; 3])),
    Node16((u8, ProofTrieNode<T>, [TrieHash; 15])),
    Node48((u8, ProofTrieNode<T>, [TrieHash; 47])),
    Node256((u8, ProofTrieNode<T>, [TrieHash; 255])),
    Leaf((u8, TrieLeaf)),
    Shunt((i64, Vec<TrieHash>)),
}

impl<T: ClarityMarfTrieId> PartialEq for TrieMerkleProofType<T> {
    fn eq(&self, other: &Self) -> bool {
        use TrieMerkleProofType::*;
        match (self, other) {
            (Node4((a, b, c)), Node4((x, y, z))) => a == x && b == y && c == z,
            (Node16((a, b, c)), Node16((x, y, z))) => a == x && b == y && c == z,
            (Node48((a, b, c)), Node48((x, y, z))) => a == x && b == y && c[..] == z[..],
            (Node256((a, b, c)), Node256((x, y, z))) => a == x && b == y && c[..] == z[..],
            (Leaf((a, b)), Leaf((x, y))) => a == x && b == y,
            (Shunt((a, b)), Shunt((x, y))) => a == x && b == y,
            _ => false,
        }
    }
}

impl<T: ClarityMarfTrieId> fmt::Debug for TrieMerkleProofType<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let fmt_hashes = |h: &[TrieHash]| {
            h.iter()
                .map(|x| format!("{x:?}"))
                .collect::<Vec<_>>()
                .join(",")
        };
        match self {
            TrieMerkleProofType::Node4((chr, node, hashes)) => {
                write!(
                    f,
                    "TrieMerkleProofType::Node4(0x{chr:02x}, node={node:?}, hashes={})",
                    fmt_hashes(hashes)
                )
            }
            TrieMerkleProofType::Node16((chr, node, hashes)) => {
                write!(
                    f,
                    "TrieMerkleProofType::Node16(0x{chr:02x}, node={node:?}, hashes={})",
                    fmt_hashes(hashes)
                )
            }
            TrieMerkleProofType::Node48((chr, node, hashes)) => {
                write!(
                    f,
                    "TrieMerkleProofType::Node48(0x{chr:02x}, node={node:?}, hashes={})",
                    fmt_hashes(hashes)
                )
            }
            TrieMerkleProofType::Node256((chr, node, hashes)) => {
                write!(
                    f,
                    "TrieMerkleProofType::Node256(0x{chr:02x}, node={node:?}, hashes={})",
                    fmt_hashes(hashes)
                )
            }
            TrieMerkleProofType::Leaf((chr, node)) => {
                write!(f, "TrieMerkleProofType::Leaf(0x{chr:02x}, node={node:?})")
            }
            TrieMerkleProofType::Shunt((idx, hashes)) => write!(
                f,
                "TrieMerkleProofType::Shunt(idx={idx}, hashes={hashes:?})"
            ),
        }
    }
}

#[derive(Debug)]
pub struct TrieMerkleProof<T: MarfTrieId>(pub Vec<TrieMerkleProofType<T>>);

impl<T: MarfTrieId> Deref for TrieMerkleProof<T> {
    type Target = Vec<TrieMerkleProofType<T>>;
    fn deref(&self) -> &Vec<TrieMerkleProofType<T>> {
        &self.0
    }
}

mod compatibility;
mod generate;
#[cfg(test)]
mod tests;
mod verify;
mod wire;

/// Proof generation over an authenticated source, without owning its storage.
pub trait Prove<T: MarfTrieId>: AuthenticatedRead<T> {
    /// Construct an inclusion proof, checking the expected leaf value.
    fn prove(
        &mut self,
        path: &TrieHash,
        value: &MARFValue,
        block: &T,
    ) -> Result<TrieMerkleProof<T>, Error> {
        generate::Generator(self).make_proof(path, value, block)
    }
}
impl<T: MarfTrieId, S: AuthenticatedRead<T> + ?Sized> Prove<T> for S {}
