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

//! Consensus proof wire encoding.
use std::io::{Read, Write};

use stacks_common::codec::{Error as codec_error, StacksMessageCodec, read_next, write_next};
use stacks_common::types::chainstate::TrieHash;

use super::{ProofTrieNode, ProofTriePtr, TrieMerkleProof, TrieMerkleProofType};
use crate::MarfTrieId;

define_u8_enum!(TrieMerkleProofTypeIndicator {
    Node4 = 0, Node16 = 1, Node48 = 2, Node256 = 3, Leaf = 4, Shunt = 5
});

impl<T: MarfTrieId> StacksMessageCodec for ProofTriePtr<T> {
    fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), codec_error> {
        self.id.consensus_serialize(fd)?;
        self.chr.consensus_serialize(fd)?;
        self.back_block.consensus_serialize(fd)
    }

    fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<ProofTriePtr<T>, codec_error> {
        Ok(ProofTriePtr {
            id: read_next(fd)?,
            chr: read_next(fd)?,
            back_block: read_next(fd)?,
        })
    }
}

impl<T: MarfTrieId> StacksMessageCodec for ProofTrieNode<T> {
    fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), codec_error> {
        self.id.consensus_serialize(fd)?;
        self.path.consensus_serialize(fd)?;
        self.ptrs.consensus_serialize(fd)
    }

    fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<ProofTrieNode<T>, codec_error> {
        Ok(ProofTrieNode {
            id: read_next(fd)?,
            path: read_next(fd)?,
            ptrs: read_next(fd)?,
        })
    }
}

fn serialize_id_hash_node<W: Write, T: MarfTrieId>(
    fd: &mut W,
    id: &u8,
    node: &ProofTrieNode<T>,
    hashes: &[TrieHash],
) -> Result<(), codec_error> {
    id.consensus_serialize(fd)?;
    node.consensus_serialize(fd)?;
    for hash in hashes {
        hash.consensus_serialize(fd)?;
    }
    Ok(())
}

macro_rules! deserialize_id_hash_node {
    ($fd:expr, $HashesArray:expr) => {{
        let id = read_next($fd)?;
        let node = read_next($fd)?;
        let mut array = $HashesArray;
        for slot in array.iter_mut() {
            *slot = read_next($fd)?;
        }
        (id, node, array)
    }};
}

impl<T: MarfTrieId> StacksMessageCodec for TrieMerkleProofType<T> {
    fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), codec_error> {
        let type_byte = match self {
            TrieMerkleProofType::Node4(_) => TrieMerkleProofTypeIndicator::Node4,
            TrieMerkleProofType::Node16(_) => TrieMerkleProofTypeIndicator::Node16,
            TrieMerkleProofType::Node48(_) => TrieMerkleProofTypeIndicator::Node48,
            TrieMerkleProofType::Node256(_) => TrieMerkleProofTypeIndicator::Node256,
            TrieMerkleProofType::Leaf(_) => TrieMerkleProofTypeIndicator::Leaf,
            TrieMerkleProofType::Shunt(_) => TrieMerkleProofTypeIndicator::Shunt,
        } as u8;
        write_next(fd, &type_byte)?;
        match self {
            TrieMerkleProofType::Node4((id, node, hashes)) => {
                serialize_id_hash_node(fd, id, node, hashes)
            }
            TrieMerkleProofType::Node16((id, node, hashes)) => {
                serialize_id_hash_node(fd, id, node, hashes)
            }
            TrieMerkleProofType::Node48((id, node, hashes)) => {
                serialize_id_hash_node(fd, id, node, hashes)
            }
            TrieMerkleProofType::Node256((id, node, hashes)) => {
                serialize_id_hash_node(fd, id, node, hashes)
            }
            TrieMerkleProofType::Leaf((id, leaf)) => {
                id.consensus_serialize(fd)?;
                leaf.consensus_serialize(fd)
            }
            TrieMerkleProofType::Shunt((id, hashes)) => {
                id.consensus_serialize(fd)?;
                hashes.consensus_serialize(fd)
            }
        }
    }

    fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<TrieMerkleProofType<T>, codec_error> {
        let type_byte = TrieMerkleProofTypeIndicator::from_u8(read_next(fd)?).ok_or_else(|| {
            codec_error::DeserializeError("Bad type byte in Trie Merkle Proof".into())
        })?;
        Ok(match type_byte {
            TrieMerkleProofTypeIndicator::Node4 => {
                TrieMerkleProofType::Node4(deserialize_id_hash_node!(fd, [TrieHash::ZERO; 3]))
            }
            TrieMerkleProofTypeIndicator::Node16 => {
                TrieMerkleProofType::Node16(deserialize_id_hash_node!(fd, [TrieHash::ZERO; 15]))
            }
            TrieMerkleProofTypeIndicator::Node48 => {
                TrieMerkleProofType::Node48(deserialize_id_hash_node!(fd, [TrieHash::ZERO; 47]))
            }
            TrieMerkleProofTypeIndicator::Node256 => {
                TrieMerkleProofType::Node256(deserialize_id_hash_node!(fd, [TrieHash::ZERO; 255]))
            }
            TrieMerkleProofTypeIndicator::Leaf => {
                let id = read_next(fd)?;
                let leaf = read_next(fd)?;
                TrieMerkleProofType::Leaf((id, leaf))
            }
            TrieMerkleProofTypeIndicator::Shunt => {
                let id = read_next(fd)?;
                let hashes = read_next(fd)?;
                TrieMerkleProofType::Shunt((id, hashes))
            }
        })
    }
}

impl<T: MarfTrieId> StacksMessageCodec for TrieMerkleProof<T> {
    fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), codec_error> {
        self.0.consensus_serialize(fd)
    }

    fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<TrieMerkleProof<T>, codec_error> {
        Ok(TrieMerkleProof(read_next(fd)?))
    }
}
