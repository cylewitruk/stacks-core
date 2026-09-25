// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020 Stacks Open Internet Foundation
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

use std::collections::HashMap;
use std::fmt;
use std::io::{Read, Write};
use std::ops::Deref;

use stacks_common::codec::{self, Error as codec_error, StacksMessageCodec};
use stacks_common::types::chainstate::TrieHash;
use stacks_common::util::hash::to_hex;

use crate::chainstate::stacks::index::marf::MarfReadCtx;
use crate::chainstate::stacks::index::node::{
    is_backptr, ConsensusSerializable, TrieCursor, TrieNodeID, TrieNodeRef, TriePtr,
};
use crate::chainstate::stacks::index::trie::Trie;
use crate::chainstate::stacks::index::{
    bits, BlockMap, ClarityMarfTrieId, Error, MARFValue, MarfTrieId, NodePatching, ProofTrieNode,
    ProofTriePtr, TrieLeafOrBackptr, TrieMerkleProof, TrieMerkleProofType, TrieReadSession,
    TrieReadStorage,
};

impl<T: MarfTrieId> ConsensusSerializable<()> for ProofTrieNode<T> {
    fn write_consensus_bytes<W: Write>(
        &self,
        _additional_data: &mut (),
        w: &mut W,
    ) -> Result<(), Error> {
        w.write_all(&[self.id])?;
        for ptr in self.ptrs.iter() {
            w.write_all(&[ptr.id, ptr.chr])?;
            w.write_all(ptr.back_block.as_bytes())?;
        }
        bits::write_path_to_bytes(&self.path, w)
    }
}

impl<T: MarfTrieId> ProofTriePtr<T> {
    fn try_from_trie_ptr<M: BlockMap + ?Sized>(
        other: &TriePtr,
        block_map: &mut M,
    ) -> Result<ProofTriePtr<T>, Error> {
        let id = other.id;
        let chr = other.chr;
        let back_block = if is_backptr(id) {
            block_map
                .get_block_hash_caching(other.back_block)?
                .clone()
                .to_bytes()
        } else {
            [0u8; 32]
        };
        Ok(ProofTriePtr {
            id,
            chr,
            back_block: back_block.into(),
        })
    }
}

impl<T: MarfTrieId> ProofTrieNode<T> {
    fn try_from_parts<M: BlockMap + ?Sized>(
        id: u8,
        path: &[u8],
        ptrs: &[TriePtr],
        block_map: &mut M,
    ) -> Result<ProofTrieNode<T>, Error> {
        let ptrs: Result<Vec<_>, Error> = ptrs
            .iter()
            .map(|trie_ptr| ProofTriePtr::try_from_trie_ptr(trie_ptr, block_map))
            .collect();
        Ok(ProofTrieNode {
            id,
            path: path.to_vec(),
            ptrs: ptrs?,
        })
    }

    fn ptrs(&self) -> &[ProofTriePtr<T>] {
        &self.ptrs
    }
}

define_u8_enum!( TrieMerkleProofTypeIndicator {
    Node4 = 0, Node16 = 1, Node48 = 2, Node256 = 3, Leaf = 4, Shunt = 5
});

impl<T: ClarityMarfTrieId> PartialEq for TrieMerkleProofType<T> {
    fn eq(&self, other: &TrieMerkleProofType<T>) -> bool {
        match (self, other) {
            (
                TrieMerkleProofType::Node4((ref chr, ref node, ref hashes)),
                TrieMerkleProofType::Node4((ref other_chr, ref other_node, ref other_hashes)),
            ) => chr == other_chr && node == other_node && hashes == other_hashes,
            (
                TrieMerkleProofType::Node16((ref chr, ref node, ref hashes)),
                TrieMerkleProofType::Node16((ref other_chr, ref other_node, ref other_hashes)),
            ) => chr == other_chr && node == other_node && hashes == other_hashes,
            (
                TrieMerkleProofType::Node48((ref chr, ref node, ref hashes)),
                TrieMerkleProofType::Node48((ref other_chr, ref other_node, ref other_hashes)),
            ) => chr == other_chr && node == other_node && hashes == other_hashes,
            (
                TrieMerkleProofType::Node256((ref chr, ref node, ref hashes)),
                TrieMerkleProofType::Node256((ref other_chr, ref other_node, ref other_hashes)),
            ) => chr == other_chr && node == other_node && hashes == other_hashes,
            (
                TrieMerkleProofType::Leaf((ref chr, ref node)),
                TrieMerkleProofType::Leaf((ref other_chr, ref other_node)),
            ) => chr == other_chr && node == other_node,
            (
                TrieMerkleProofType::Shunt((ref idx_1, ref hashes_1)),
                TrieMerkleProofType::Shunt((ref idx_2, ref hashes_2)),
            ) => idx_1 == idx_2 && hashes_1 == hashes_2,
            (_, _) => false,
        }
    }
}

pub fn hashes_fmt(hashes: &[TrieHash]) -> String {
    let mut strs = vec![];
    let zero = &TrieHash::ZERO;
    if hashes.len() < 48 {
        for i in 0..hashes.len() {
            strs.push(format!("{:?}", hashes.get(i).unwrap_or(zero)));
        }
        strs.join(",")
    } else {
        for i in 0..hashes.len() / 4 {
            strs.push(format!(
                "{:?},{:?},{:?},{:?}",
                hashes.get(4 * i).unwrap_or(zero),
                hashes.get(4 * i + 1).unwrap_or(zero),
                hashes.get(4 * i + 2).unwrap_or(zero),
                hashes.get(4 * i + 3).unwrap_or(zero),
            ));
        }
        format!("\n{}", strs.join("\n"))
    }
}

impl<T: MarfTrieId> fmt::Debug for TrieMerkleProofType<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            TrieMerkleProofType::Node4((ref chr, ref node, ref hashes)) => write!(
                f,
                "TrieMerkleProofType::Node4(0x{:02x}, node={:?}, hashes={})",
                chr,
                node,
                hashes_fmt(hashes)
            ),
            TrieMerkleProofType::Node16((ref chr, ref node, ref hashes)) => write!(
                f,
                "TrieMerkleProofType::Node16(0x{:02x}, node={:?}, hashes={})",
                chr,
                node,
                hashes_fmt(hashes)
            ),
            TrieMerkleProofType::Node48((ref chr, ref node, ref hashes)) => write!(
                f,
                "TrieMerkleProofType::Node48(0x{:02x}, node={:?}, hashes={})",
                chr,
                node,
                hashes_fmt(hashes)
            ),
            TrieMerkleProofType::Node256((ref chr, ref node, ref hashes)) => write!(
                f,
                "TrieMerkleProofType::Node256(0x{:02x}, node={:?}, hashes={})",
                chr,
                node,
                hashes_fmt(hashes)
            ),
            TrieMerkleProofType::Leaf((ref chr, ref node)) => write!(
                f,
                "TrieMerkleProofType::Leaf(0x{:02x}, node={:?})",
                chr, node
            ),
            TrieMerkleProofType::Shunt((ref idx, ref hashes)) => write!(
                f,
                "TrieMerkleProofType::Shunt(idx={}, hashes={:?})",
                idx, hashes
            ),
        }
    }
}

impl<T: MarfTrieId> Deref for TrieMerkleProof<T> {
    type Target = Vec<TrieMerkleProofType<T>>;
    fn deref(&self) -> &Vec<TrieMerkleProofType<T>> {
        &self.0
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
    for hash in hashes.iter() {
        hash.consensus_serialize(fd)?;
    }
    Ok(())
}

macro_rules! deserialize_id_hash_node {
    ($fd:expr, $HashesArray:expr) => {{
        let id = codec::read_next($fd)?;
        let node = codec::read_next($fd)?;
        let mut array = $HashesArray;
        for slot in array.iter_mut() {
            *slot = codec::read_next($fd)?;
        }
        (id, node, array)
    }};
}

impl<T: MarfTrieId> StacksMessageCodec for ProofTriePtr<T> {
    fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), codec_error> {
        self.id.consensus_serialize(fd)?;
        self.chr.consensus_serialize(fd)?;
        self.back_block.consensus_serialize(fd)
    }

    fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<ProofTriePtr<T>, codec_error> {
        let id = codec::read_next(fd)?;
        let chr = codec::read_next(fd)?;
        let back_block = codec::read_next(fd)?;

        Ok(ProofTriePtr {
            id,
            chr,
            back_block,
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
        let id = codec::read_next(fd)?;
        let path = codec::read_next(fd)?;
        let ptrs = codec::read_next(fd)?;

        Ok(ProofTrieNode { id, path, ptrs })
    }
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

        type_byte.consensus_serialize(fd)?;

        match self {
            TrieMerkleProofType::Node4((id, proof_node, hashes)) => {
                serialize_id_hash_node(fd, id, proof_node, hashes)
            }
            TrieMerkleProofType::Node16((id, proof_node, hashes)) => {
                serialize_id_hash_node(fd, id, proof_node, hashes)
            }
            TrieMerkleProofType::Node48((id, proof_node, hashes)) => {
                serialize_id_hash_node(fd, id, proof_node, hashes)
            }
            TrieMerkleProofType::Node256((id, proof_node, hashes)) => {
                serialize_id_hash_node(fd, id, proof_node, hashes)
            }
            TrieMerkleProofType::Leaf((id, leaf_node)) => {
                id.consensus_serialize(fd)?;
                leaf_node.consensus_serialize(fd)
            }
            TrieMerkleProofType::Shunt((id, hashes)) => {
                id.consensus_serialize(fd)?;
                hashes.consensus_serialize(fd)
            }
        }
    }

    fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<TrieMerkleProofType<T>, codec_error> {
        let type_byte =
            TrieMerkleProofTypeIndicator::from_u8(codec::read_next(fd)?).ok_or_else(|| {
                codec_error::DeserializeError("Bad type byte in Trie Merkle Proof".into())
            })?;

        let codec = match type_byte {
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
                let id = codec::read_next(fd)?;
                let leaf_node = codec::read_next(fd)?;
                TrieMerkleProofType::Leaf((id, leaf_node))
            }
            TrieMerkleProofTypeIndicator::Shunt => {
                let id = codec::read_next(fd)?;
                let hashes = codec::read_next(fd)?;
                TrieMerkleProofType::Shunt((id, hashes))
            }
        };

        Ok(codec)
    }
}

impl<T: MarfTrieId> StacksMessageCodec for TrieMerkleProof<T> {
    fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), codec_error> {
        self.0.consensus_serialize(fd)
    }

    fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<TrieMerkleProof<T>, codec_error> {
        let proof_parts: Vec<TrieMerkleProofType<T>> = codec::read_next(fd)?;
        Ok(TrieMerkleProof(proof_parts))
    }
}

impl<T: MarfTrieId> TrieMerkleProof<T> {
    pub fn to_hex(&self) -> String {
        let mut marf_proof = vec![];
        self.consensus_serialize(&mut marf_proof)
            .expect("Write error on memory buffer");
        to_hex(&marf_proof)
    }

    fn make_proof_hashes_for_ptrs(
        ptrs: &[TriePtr],
        all_hashes: &[TrieHash],
        chr: u8,
    ) -> Result<Vec<TrieHash>, Error> {
        let mut hashes = vec![];
        assert!(all_hashes.len() == ptrs.len());

        for (i, ptr) in ptrs.iter().enumerate() {
            if ptr.id() == TrieNodeID::Empty as u8 {
                hashes.push(TrieHash::EMPTY);
            } else if ptr.chr() != chr {
                let hash = all_hashes.get(i).ok_or_else(|| {
                    Error::CorruptionError("Hash array smaller than node ptrs".into())
                })?;
                hashes.push(*hash);
            }
        }

        if hashes.len() + 1 != ptrs.len() {
            trace!("Char 0x{:02x} does not appear in ptrs: {:?}", chr, ptrs);
            return Err(Error::NotFoundError);
        }

        Ok(hashes)
    }

    /// Given a TriePtr to the _currently-visited_ node and the chr of the _previous_ node, calculate a
    /// Merkle proof node.  Include all the children hashes _except_ for the one that corresponds
    /// to the previous node.
    fn ptr_to_segment_proof_node<S: NodePatching, R: TrieReadStorage<T> + ?Sized>(
        read_session: &mut TrieReadSession<'_, T, S, R>,
        ptr: &TriePtr,
        prev_chr: u8,
    ) -> Result<TrieMerkleProofType<T>, Error> {
        trace!(
            "ptr_to_proof_node: ptr={:?}, prev_chr=0x{:02x}",
            ptr,
            prev_chr
        );
        let (node_id, node_path, node_ptrs, leaf_data) = {
            let read = read_session.read_node(ptr)?;
            let leaf_data = read.as_leaf()?.map(|data| data.to_owned());
            (
                read.node_type_u8(),
                read.path_bytes()?.to_vec(),
                read.ptrs()?.to_vec(),
                leaf_data,
            )
        };
        let storage = read_session.storage();
        let all_hashes = Trie::get_children_hashes_by_ptrs(storage, &node_ptrs)?;

        let hashes = if node_id == TrieNodeID::Leaf as u8 {
            vec![]
        } else {
            Self::make_proof_hashes_for_ptrs(&node_ptrs, &all_hashes, prev_chr)?
        };

        let proof_node = match node_id {
            x if x == TrieNodeID::Leaf as u8 => {
                let mut leaf = leaf_data.ok_or_else(|| {
                    Error::CorruptionError("Leaf proof node missing leaf payload".into())
                })?;
                storage.resolve_leaf_value(&mut leaf)?;
                TrieMerkleProofType::Leaf((prev_chr, leaf))
            }
            x if x == TrieNodeID::Node4 as u8 => {
                let mut hash_slice = [TrieHash::EMPTY; 3];
                let copy_data = hashes
                    .get(..3)
                    .ok_or_else(|| Error::CorruptionError("Too few byte in trie node".into()))?;
                hash_slice.copy_from_slice(copy_data);

                TrieMerkleProofType::Node4((
                    prev_chr,
                    ProofTrieNode::try_from_parts(node_id, &node_path, &node_ptrs, storage)?,
                    hash_slice,
                ))
            }
            x if x == TrieNodeID::Node16 as u8 => {
                let mut hash_slice = [TrieHash::EMPTY; 15];
                let copy_data = hashes
                    .get(..15)
                    .ok_or_else(|| Error::CorruptionError("Too few byte in trie node".into()))?;
                hash_slice.copy_from_slice(copy_data);

                TrieMerkleProofType::Node16((
                    prev_chr,
                    ProofTrieNode::try_from_parts(node_id, &node_path, &node_ptrs, storage)?,
                    hash_slice,
                ))
            }
            x if x == TrieNodeID::Node48 as u8 => {
                let mut hash_slice = [TrieHash::EMPTY; 47];
                let copy_data = hashes
                    .get(..47)
                    .ok_or_else(|| Error::CorruptionError("Too few byte in trie node".into()))?;
                hash_slice.copy_from_slice(copy_data);

                TrieMerkleProofType::Node48((
                    prev_chr,
                    ProofTrieNode::try_from_parts(node_id, &node_path, &node_ptrs, storage)?,
                    hash_slice,
                ))
            }
            x if x == TrieNodeID::Node256 as u8 => {
                let mut hash_slice = [TrieHash::EMPTY; 255];
                let copy_data = hashes
                    .get(..255)
                    .ok_or_else(|| Error::CorruptionError("Too few byte in trie node".into()))?;
                hash_slice.copy_from_slice(copy_data);

                TrieMerkleProofType::Node256(
                    // ancestor hashes to be filled in later
                    (
                        prev_chr,
                        ProofTrieNode::try_from_parts(node_id, &node_path, &node_ptrs, storage)?,
                        hash_slice,
                    ),
                )
            }
            _ => {
                return Err(Error::CorruptionError(format!(
                    "Unknown trie node type 0x{node_id:02x}"
                )));
            }
        };
        Ok(proof_node)
    }

    /// Make the initial shunt proof in a MARF merkle proof, for a node that isn't a backptr.
    /// This is a one-item list of a TrieMerkleProofType::Shunt proof entry.
    /// The storage handle must be opened to the block we care about.
    fn make_initial_shunt_proof<S: NodePatching, R: TrieReadStorage<T> + ?Sized>(
        ctx: &mut MarfReadCtx<'_, T, S, R>,
    ) -> Result<Vec<TrieMerkleProofType<T>>, Error> {
        let backptr_ancestor_hashes = ctx.get_trie_ancestor_hashes_bytes()?;

        trace!(
            "First shunt proof node: (0, {:?})",
            &backptr_ancestor_hashes
        );

        let backptr_proof = TrieMerkleProofType::Shunt((0, backptr_ancestor_hashes));
        Ok(vec![backptr_proof])
    }

    /// Given a node's (non-backptr) ptr, and the node's backptr, make a shunt proof that links
    /// them.  That is, make a proof that the current trie's root node hash and ptr are only reachable from the
    /// corresponding non-backptr root in this trie's ${ptr.back_block()}th ancestor back.
    /// s must point to the block from which we're going to walk back from.
    ///
    /// The first entry of the shunt proof is the set of Trie root hashes _excluding_ the one from
    /// backptr, as well as the index into the list of Trie root hashes into which the backptr hash
    /// should be inserted (this root hash is calculated from the segment proof for that backptr
    /// node).
    ///
    /// The last entry of the shunt proof is the set of root hashes _excluding_ the final root
    /// hash, which will be the root hash for the segment proof for the non-backptr copy of this
    /// node.
    ///
    /// All intermediate shunt proofs will contain all ancestor hashes for each node in-between the
    /// backptr and the non-backptr node.  The intermediate root hashes will be calculated by the verifier.
    fn make_backptr_shunt_proof<S: NodePatching, R: TrieReadStorage<T> + ?Sized>(
        ctx: &mut MarfReadCtx<'_, T, S, R>,
        backptr: &TriePtr,
    ) -> Result<Vec<TrieMerkleProofType<T>>, Error> {
        // the proof is built "backwards" -- starting from the current block all the way back to backptr.
        assert!(is_backptr(backptr.id()));

        let mut proof = vec![];

        let mut block_header = ctx.storage().get_cur_block();

        let ancestor_block_hash = ctx
            .storage()
            .get_block_from_local_id(backptr.back_block())?;
        ctx.storage().open_block(&ancestor_block_hash)?;

        let ancestor_root_hash = bits::read_root_hash(ctx.storage())?;

        let mut found_backptr = false;

        let ancestor_height = ctx
            .get_block_height_miner_tip(&ancestor_block_hash, &block_header)?
            .ok_or_else(|| {
                Error::CorruptionError(format!(
                    "Could not find block height of ancestor block {ancestor_block_hash} from {block_header}"
                ))
            })?;
        let mut current_height = ctx
            .get_block_height_miner_tip(&block_header, &block_header)?
            .ok_or_else(|| {
                Error::CorruptionError(format!(
                    "Could not find block height of current block {block_header} from {block_header}"
                ))
            })?;

        if current_height == ancestor_height {
            debug!(
                "Already at the ancestor: {} =? {}, heights: {} =? {}",
                &ancestor_block_hash, &block_header, ancestor_height, current_height
            );
        }

        // find last and intermediate entries in the shunt proof -- exclude the root hashes; just
        // include the ancestor hashes.
        while current_height > ancestor_height && !found_backptr {
            ctx.open_block(&block_header, None)?;
            let _cur_root_hash = bits::read_root_hash(ctx.storage())?;
            trace!(
                "Shunt proof: walk heights {}->{} from {:?} ({:?})",
                current_height,
                ancestor_height,
                &block_header,
                &_cur_root_hash
            );

            let ancestor_hashes = ctx.get_trie_ancestor_hashes_bytes()?;

            trace!(
                "Ancestors of {:?} ({:?}): {:?}",
                &block_header,
                &_cur_root_hash,
                &ancestor_hashes
            );

            // did we reach the backptr's root hash?
            found_backptr = ancestor_hashes.contains(&ancestor_root_hash);

            // what's the next block we'll shunt to?
            let mut idx = 0;
            while (1u32 << idx) <= current_height
                && current_height - (1u32 << idx) >= ancestor_height
            {
                idx += 1;
            }
            if idx == 0 {
                panic!(
                    "ancestor_height = {}, current_height = {}, but ancestor hash `{}` not found in: [{}]",
                    ancestor_height,
                    current_height,
                    ancestor_root_hash,
                    ancestor_hashes
                        .iter()
                        .map(|x| format!("{}", x))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
            idx -= 1;

            if found_backptr {
                assert_eq!(ancestor_hashes.get(idx).unwrap(), &ancestor_root_hash);
            }

            current_height -= 1u32 << idx;

            block_header = ctx
                .get_block_at_height(current_height, &block_header)?
                .ok_or_else(|| {
                    Error::CorruptionError(format!(
                        "Could not find block at height of {}",
                        current_height
                    ))
                })?
                .clone();

            let mut trimmed_ancestor_hashes = Vec::with_capacity(ancestor_hashes.len() - 1);
            for (i, ancestor_hash) in ancestor_hashes.iter().enumerate() {
                if i == idx {
                    continue;
                }
                trimmed_ancestor_hashes.push(*ancestor_hash);
            }

            idx += 1;

            // need the target node's root trie ptr, unless this is the first proof (in which case
            // it's a junction proof)
            if !proof.is_empty() {
                let root_hash = ctx.with_read_state(|storage, _, decode_scratch| {
                    let root_ptr = storage.root_trieptr();
                    let (root_path, root_ptrs) = {
                        let mut read_session = TrieReadSession::new(storage, decode_scratch);
                        let read = read_session.read_node(&root_ptr)?;
                        if !read.is_node256() {
                            return Err(Error::CorruptionError(format!(
                                "Root node at {:?} is not a TrieNode256",
                                &block_header
                            )));
                        }
                        let mut root_ptrs = [TriePtr::default(); 256];
                        root_ptrs.copy_from_slice(read.ptrs()?);
                        (read.path_bytes()?.to_vec(), root_ptrs)
                    };

                    let child_hashes = Trie::get_children_hashes_by_ptrs(storage, &root_ptrs)?;
                    let root_node = TrieNodeRef::Node256 {
                        path: &root_path,
                        ptrs: &root_ptrs,
                    };
                    Ok::<_, Error>(bits::get_node_hash(&root_node, &child_hashes, storage))
                })?;

                trimmed_ancestor_hashes.insert(0, root_hash);
                idx += 1;

                trace!(
                    "Tail proof: Added intermediate proof node's root data hash is {:?}",
                    &root_hash
                );
            }

            if !found_backptr {
                trace!(
                    "Backptr not found yet: trim ancestor hashes at idx={} from {:?} to {:?}",
                    idx,
                    &ancestor_hashes,
                    &trimmed_ancestor_hashes
                );
                trace!(
                    "Backptr not found yet.  Shunt to {:?} and walk to {}; Add shunt proof ({}, {:?})",
                    &block_header, ancestor_height, idx, &trimmed_ancestor_hashes
                );
            } else {
                trace!(
                    "Backptr found: trim ancestor hashes at idx={} from {:?} to {:?}",
                    idx,
                    &ancestor_hashes,
                    &trimmed_ancestor_hashes
                );
                trace!(
                    "Backptr found (ancestor_height = {}, header = {:?}).  Intermediate shunt proof is ({}, {:?})",
                    ancestor_height, &block_header, idx, &trimmed_ancestor_hashes
                );
            };

            let shunt_proof_node =
                TrieMerkleProofType::Shunt((idx as i64, trimmed_ancestor_hashes));
            proof.push(shunt_proof_node);
        }

        ctx.storage().open_block(&block_header)?;
        proof.reverse();

        // put the proof in the right order. we're done!
        Ok(proof)
    }

    fn next_shunt_hash(hash: &TrieHash, idx: i64, hashes: &[TrieHash]) -> Option<TrieHash> {
        let mut all_hashes = Vec::with_capacity(hashes.len() + 1);
        let mut hash_idx = 0;
        for i in 0..hashes.len() + 1 {
            if idx == 0 {
                trace!("Intermediate shunt proof entry must have idx > 0");
                return None;
            }

            if idx - 1 == (i as i64) {
                all_hashes.push(*hash);
            } else {
                let Some(hash) = hashes.get(hash_idx) else {
                    trace!(
                        "Invalid proof: hash_idx = {hash_idx}, hashes.len() = {}",
                        hashes.len()
                    );
                    return None;
                };
                all_hashes.push(*hash);
                hash_idx += 1;
            }
        }
        trace!("Shunt proof node: idx={idx}, all_hashes={all_hashes:?}");
        let next_hash = TrieHash::from_data_array(&all_hashes);
        Some(next_hash)
    }

    /// Verify the head of a shunt proof
    fn verify_shunt_proof_head(
        node_root_hash: &TrieHash,
        shunt_proof_head: &TrieMerkleProofType<T>,
    ) -> Option<TrieHash> {
        // ancestor hashes are always the first item
        let hash = match shunt_proof_head {
            TrieMerkleProofType::Shunt((ref idx, ref hashes)) => {
                if *idx != 0 {
                    trace!("First shunt proof entry must have idx == 0");
                    return None;
                }

                if hashes.is_empty() {
                    // special case -- if this shunt proof has no hashes (i.e. this is a leaf from the first
                    // block), then we can safely skip this step
                    trace!(
                        "Special case for a 0-ancestor node: hash is just the trie hash: {:?}",
                        node_root_hash
                    );
                    *node_root_hash
                } else {
                    let mut all_hashes = Vec::with_capacity(hashes.len() + 1);
                    all_hashes.push(*node_root_hash);
                    for h in hashes {
                        all_hashes.push(*h);
                    }
                    let ret = TrieHash::from_data_array(&all_hashes);
                    trace!(
                        "Shunt proof head: hash = {:?}, all_hashes = {:?}",
                        &ret,
                        &all_hashes
                    );
                    ret
                }
            }
            _ => {
                trace!("Shunt proof head is not a shunt proof node");
                return None;
            }
        };

        Some(hash)
    }

    /// Verify the tail of a shunt proof, given the backptr root hash.
    /// Calculate the root hash of the next segment proof.
    fn verify_shunt_proof_tail(
        initial_hash: &TrieHash,
        shunt_proof: &[TrieMerkleProofType<T>],
    ) -> Option<TrieHash> {
        let mut hash = *initial_hash;

        // walk subsequent legs of a shunt proof, except for the last (since we need the next
        // segment proof for that)
        for proof_node in shunt_proof.iter() {
            hash = match proof_node {
                TrieMerkleProofType::Shunt((ref idx, ref hashes)) => {
                    if *idx == 0 {
                        trace!("Invalid shunt proof tail: idx == 0");
                        return None;
                    }

                    match TrieMerkleProof::<T>::next_shunt_hash(&hash, *idx, hashes) {
                        Some(h) => h,
                        None => {
                            return None;
                        }
                    }
                }
                _ => {
                    trace!("Shunt proof item is not a shunt proof node");
                    return None;
                }
            };
        }
        Some(hash)
    }

    /// Verify a shunt juncture, where a shunt proof tail and a segment proof meet.
    /// Returns the hash of the root of the junction
    fn verify_shunt_proof_junction(
        node_root_hash: &TrieHash,
        penultimate_trie_hash: &TrieHash,
        shunt_proof_junction: &TrieMerkleProofType<T>,
    ) -> Option<TrieHash> {
        // at the juncture, we include the node root hash (from the subsequent segment proof) as
        // the first hash, and include the penultimate trie hash in its idx
        let hash = match shunt_proof_junction {
            TrieMerkleProofType::Shunt((ref idx, ref hashes)) => {
                if *idx == 0 {
                    trace!("Shunt proof junction entry must not have idx == 0");
                    return None;
                }

                let mut all_hashes = Vec::with_capacity(hashes.len() + 1);
                let mut hash_idx = 0;

                all_hashes.push(*node_root_hash);

                for i in 0..hashes.len() + 1 {
                    if *idx - 1 == (i as i64) {
                        all_hashes.push(*penultimate_trie_hash);
                    } else {
                        let Some(hash) = hashes.get(hash_idx) else {
                            trace!(
                                "ran out of hashes: hash_idx = {hash_idx}, hashes.len() = {}",
                                hashes.len()
                            );
                            return None;
                        };

                        all_hashes.push(*hash);
                        hash_idx += 1;
                    }
                }

                trace!(
                    "idx = {}, hashes = {:?}, penultimate = {:?}, node root = {:?}",
                    *idx,
                    hashes,
                    penultimate_trie_hash,
                    node_root_hash
                );
                trace!("Shunt proof junction: all_hashes = {:?}", &all_hashes);
                TrieHash::from_data_array(&all_hashes)
            }
            _ => {
                trace!("Shunt proof junction is not a shunt proof node");
                return None;
            }
        };

        Some(hash)
    }

    /// Given a list of non-backptr ptrs and a root block header hash, calculate a Merkle proof.
    fn make_segment_proof<S: NodePatching, R: TrieReadStorage<T> + ?Sized>(
        storage: &mut R,
        ptrs: &[TriePtr],
        starting_chr: u8,
        decode_scratch: &mut S,
    ) -> Result<Vec<TrieMerkleProofType<T>>, Error> {
        trace!("make_segment_proof: ptrs = {ptrs:?}");

        assert!(!ptrs.is_empty());
        assert_eq!(ptrs.first().unwrap().clone(), storage.root_trieptr());
        for ptr in ptrs
            .get(1..)
            .ok_or_else(|| Error::CorruptionError("Empty pointers list".into()))?
        {
            assert!(!is_backptr(ptr.id()));
        }

        let cur_block = storage.get_cur_block();
        let mut read_session = TrieReadSession::new(storage, decode_scratch);
        let mut proof_segment = Vec::with_capacity(ptrs.len());
        let mut prev_chr = starting_chr;

        trace!(
            "make_segment_proof: Trie segment from {:?} starting at {starting_chr:?}: {ptrs:?}",
            &cur_block
        );
        for ptr in ptrs.iter().rev() {
            let proof_node =
                TrieMerkleProof::ptr_to_segment_proof_node(&mut read_session, ptr, prev_chr)?;

            trace!(
                "make_segment_proof: Add proof node from {ptr:?} child 0x{prev_chr:02x}: {proof_node:?}"
            );

            proof_segment.push(proof_node);
            prev_chr = ptr.chr();
        }

        Ok(proof_segment)
    }

    /// Given a node in a segment proof, find the hash
    fn get_segment_proof_hash(
        node: &ProofTrieNode<T>,
        hash: &TrieHash,
        chr: u8,
        hashes: &[TrieHash],
        count: usize,
    ) -> Option<TrieHash> {
        let mut all_hashes = vec![];
        let mut ih = 0;

        assert!(node.ptrs().len() == count);
        assert!(count > 0 && hashes.len() == count - 1);

        for child_ptr in node.ptrs() {
            if child_ptr.id != TrieNodeID::Empty as u8 && child_ptr.chr == chr {
                all_hashes.push(*hash);
            } else if ih >= hashes.len() {
                trace!("verify_get_hash: {} >= {}", ih, hashes.len());
                return None;
            } else {
                all_hashes.push(*hashes.get(ih)?);
                ih += 1;
            }
        }
        if all_hashes.len() != count {
            trace!("verify_get_hash: {} != {}", all_hashes.len(), count);
            return None;
        }

        Some(bits::get_node_hash(node, &all_hashes, &mut ()))
    }

    /// Given a segment proof, the deepest node's hash, and the hash of the trie root, verify that
    /// the segment proof is well-formed.
    /// If so, calculate the root hash of the segment and return it.
    fn verify_segment_proof(
        proof: &[TrieMerkleProofType<T>],
        node_hash: &TrieHash,
    ) -> Option<TrieHash> {
        let mut hash = *node_hash;
        for proof_node in proof.iter() {
            let hash_opt = match proof_node {
                TrieMerkleProofType::Leaf((ref _chr, ref node)) => {
                    // special case the leaf hash -- it doesn't
                    //   have any child hashes to check.
                    Some(bits::get_leaf_hash(node))
                }
                TrieMerkleProofType::Node4((ref chr, ref node, ref hashes)) => {
                    TrieMerkleProof::get_segment_proof_hash(node, &hash, *chr, hashes, 4)
                }
                TrieMerkleProofType::Node16((ref chr, ref node, ref hashes)) => {
                    TrieMerkleProof::get_segment_proof_hash(node, &hash, *chr, hashes, 16)
                }
                TrieMerkleProofType::Node48((ref chr, ref node, ref hashes)) => {
                    TrieMerkleProof::get_segment_proof_hash(node, &hash, *chr, hashes, 48)
                }
                TrieMerkleProofType::Node256((ref chr, ref node, ref hashes)) => {
                    TrieMerkleProof::get_segment_proof_hash(node, &hash, *chr, hashes, 256)
                }
                _ => {
                    trace!("Invalid proof -- encountered a non-node proof type");
                    return None;
                }
            };
            hash = match hash_opt {
                None => {
                    return None;
                }
                Some(h) => h,
            };
        }

        trace!("verify segment: calculated root hash = {:?}", hash);
        Some(hash)
    }

    /// Given a segment proof, extract the path prefix it encodes
    fn get_segment_proof_path_prefix(segment_proof: &[TrieMerkleProofType<T>]) -> Option<Vec<u8>> {
        let mut path_parts: Vec<Vec<u8>> = vec![];
        for proof_node in segment_proof {
            match proof_node {
                TrieMerkleProofType::Leaf((ref _chr, ref node)) => {
                    // path_parts.push(vec![*chr]);
                    path_parts.push(node.path.to_vec());
                }
                TrieMerkleProofType::Node4((ref chr, ref node, _)) => {
                    path_parts.push(vec![*chr]);
                    path_parts.push(node.path.clone());
                }
                TrieMerkleProofType::Node16((ref chr, ref node, _)) => {
                    path_parts.push(vec![*chr]);
                    path_parts.push(node.path.clone());
                }
                TrieMerkleProofType::Node48((ref chr, ref node, _)) => {
                    path_parts.push(vec![*chr]);
                    path_parts.push(node.path.clone());
                }
                TrieMerkleProofType::Node256((ref chr, ref node, _)) => {
                    path_parts.push(vec![*chr]);
                    path_parts.push(node.path.clone());
                }
                _ => {
                    trace!("Not a valid segment proof: got a non-node proof node");
                    return None;
                }
            }
        }

        let path = path_parts.into_iter().rev().flatten().collect();
        Some(path)
    }

    /// Verify that a proof is well-formed:
    /// * it must have the same number of segment and shunt proofs
    /// * segment proof i+1 must be a prefix of segment proof i
    /// * segment proof 0 must end in a leaf
    /// * all segment proofs must end in a Node256 (a root)
    fn is_proof_well_formed(proof: &[TrieMerkleProofType<T>], expected_path: &TrieHash) -> bool {
        let Some(proof_head) = proof.first() else {
            trace!("Proof is empty");
            return false;
        };

        match proof_head {
            TrieMerkleProofType::Leaf(_) => {}
            _ => {
                trace!("First proof node is not a leaf");
                return false;
            }
        }

        // must be alternating segment and shunt proofs
        let mut i = 0;
        let mut path_bytes = vec![];

        while i < proof.len() {
            // next segment proof
            let mut j = i + 1;
            while let Some(proof_step) = proof.get(j) {
                if let TrieMerkleProofType::Shunt(_) = proof_step {
                    break;
                }
                j += 1
            }

            let Some(segment_proof) = proof.get(i..j) else {
                return false;
            };

            if i == 0 {
                // detect the path
                let Some(set_path_bytes) =
                    TrieMerkleProof::get_segment_proof_path_prefix(segment_proof)
                else {
                    trace!("Failed to get the path from the proof");
                    return false;
                };
                path_bytes = set_path_bytes;

                // first path bytes must be the expected TrieHash
                if expected_path.as_bytes() != path_bytes.as_slice() {
                    trace!(
                        "Invalid proof -- path bytes {:?} differs from the expected path {:?}",
                        &path_bytes,
                        expected_path
                    );
                    return false;
                }
            } else {
                // make sure that this segment proof is a prefix of the last
                let Some(new_path_bytes) =
                    TrieMerkleProof::get_segment_proof_path_prefix(segment_proof)
                else {
                    trace!("Failed to et the path prefix from the proof");
                    return false;
                };

                let Some(path_bytes_prefix) = path_bytes.get(..new_path_bytes.len()) else {
                    trace!(
                        "Segment proof path is {}, which is longer than the previous segment proof length {}",
                        path_bytes.len(),
                        new_path_bytes.len()
                    );
                    trace!("path_bytes: {:?}", &path_bytes);
                    trace!("new path bytes: {:?}", &new_path_bytes);
                    return false;
                };

                if path_bytes_prefix != new_path_bytes.as_slice() {
                    trace!(
                        "Segment path {:?} is not a prefix of previous segment path {:?}",
                        &new_path_bytes,
                        &path_bytes
                    );
                    return false;
                }
            }

            // next shunt proof
            i = j;
            if i >= proof.len() {
                trace!("Proof is incomplete -- must end with a shunt proof");
                return false;
            }

            j = i + 1;
            while let Some(proof_step) = proof.get(j) {
                if let TrieMerkleProofType::Shunt(_) = proof_step {
                    j += 1;
                } else {
                    break;
                }
            }

            // end of shunt proof
            i = j;
        }

        true
    }

    /// Given a value and the root hash from which this proof was
    /// (supposedly) generated go and verify whether or not it is consistent with the root hash.
    /// For the proof validation to work, the verifier needs to know which Trie roots correspond to
    /// which block headers.  This can be calculated and verified independently from the blockchain
    /// headers.
    /// NOTE: Trie root hashes are globally unique by design, even if they represent the same contents, so the root_to_block map is bijective with high probability.
    pub fn verify_proof(
        proof: &[TrieMerkleProofType<T>],
        path: &TrieHash,
        value: &MARFValue,
        root_hash: &TrieHash,
        root_to_block: &HashMap<TrieHash, T>,
    ) -> bool {
        if !TrieMerkleProof::is_proof_well_formed(proof, path) {
            test_debug!("Invalid proof -- proof is not well-formed");
            return false;
        }

        let (mut node_hash, node_data) = match proof.first() {
            Some(TrieMerkleProofType::Leaf((_, ref node))) => {
                if node.data.is_none() {
                    return false;
                }
                (bits::get_leaf_hash(node), node.data.clone())
            }
            _ => return false,
        };

        // proof must be for this value
        if node_data.as_ref() != Some(value) {
            test_debug!(
                "Invalid proof -- not for value hash {:?}",
                value.to_value_hash()
            );
            return false;
        }

        let mut i = 0;

        // verify the very first segment proof
        let mut j = i + 1;
        while let Some(proof_step) = proof.get(j) {
            if let TrieMerkleProofType::Shunt(_) = proof_step {
                break;
            }
            j += 1
        }

        trace!("verify segment proof in range {}..{}", i, j);
        let Some(segment_proof) = proof.get(i..j) else {
            return false;
        };
        let Some(node_root_hash) = TrieMerkleProof::verify_segment_proof(segment_proof, &node_hash)
        else {
            test_debug!("Unable to verify segment proof in range {}...{}", i, j);
            return false;
        };

        i = j;
        let Some(shunt_proof_head) = proof.get(i) else {
            test_debug!(
                "Proof is too short -- needed at least one shunt proof for the first segment"
            );
            return false;
        };

        // verify the very first shunt proof head.
        trace!("verify shunt proof head at {i}: {shunt_proof_head:?}");
        let Some(mut trie_hash) =
            TrieMerkleProof::verify_shunt_proof_head(&node_root_hash, shunt_proof_head)
        else {
            test_debug!("Unable to verify shunt proof head at {i}: {shunt_proof_head:?}",);
            return false;
        };
        trace!("shunt proof head hash: {:?}", &trie_hash);

        i += 1;
        let Some(segment_proof_head) = proof.get(i) else {
            // done -- no further shunts
            test_debug!("Verify proof: {:?} =?= {:?}", root_hash, &trie_hash);
            return *root_hash == trie_hash;
        };

        // next node hash is the hash of the block from which its root came
        node_hash = match root_to_block.get(&trie_hash) {
            Some(bhh) => {
                trace!("Block hash for {:?} is {:?}", &trie_hash, bhh);

                // safe because block header hashes are 32 bytes long
                TrieHash(bhh.clone().to_bytes())
            }
            None => {
                test_debug!("Trie hash not found in root-to-block map: {:?}", &trie_hash);
                trace!("root-to-block map: {:?}", &root_to_block);
                return false;
            }
        };

        // next proof item should be part of a segment proof
        if let TrieMerkleProofType::Shunt(_) = segment_proof_head {
            test_debug!(
                "Malformed proof -- exepcted segment proof following first shunt proof head at {}",
                i
            );
            return false;
        }

        while i < proof.len() {
            // find the next segment proof
            j = i + 1;
            while let Some(proof_step) = proof.get(j) {
                if let TrieMerkleProofType::Shunt(_) = proof_step {
                    break;
                }
                j += 1
            }

            trace!("verify segment proof in range {}..{}", i, j);
            let Some(segment_proof) = proof.get(i..j) else {
                return false;
            };
            let Some(next_node_root_hash) =
                TrieMerkleProof::verify_segment_proof(segment_proof, &node_hash)
            else {
                test_debug!("Unable to verify segment proof in range {}...{}", i, j);
                return false;
            };

            i = j;
            if i >= proof.len() {
                test_debug!("Proof to short -- no shunt proof tail");
                return false;
            }

            // find the tail end
            j = i;
            while let Some(proof_step) = proof.get(j) {
                if let TrieMerkleProofType::Shunt((ref idx, _)) = proof_step {
                    if *idx == 0 {
                        break;
                    }
                    j += 1
                } else {
                    break;
                }
            }
            j -= 1;

            if j < i {
                test_debug!("Proof is malformed -- no tail or junction proof");
                return false;
            }

            let Some(shunt_proof_tail) = proof.get(i..j) else {
                return false;
            };
            trace!(
                "verify shunt proof tail in range {i}..{j} initial hash = {trie_hash:?}: {shunt_proof_tail:?}",
            );
            let Some(penultimate_trie_hash) =
                TrieMerkleProof::verify_shunt_proof_tail(&trie_hash, shunt_proof_tail)
            else {
                test_debug!("Unable to verify shunt proof tail");
                return false;
            };
            trace!(
                "verify shunt proof tail in range {i}..{j}: penultimate trie hash is {penultimate_trie_hash:?}",
            );

            i = j;
            let Some(shunt_proof_junction) = proof.get(i) else {
                test_debug!("Proof to short -- no junction proof");
                return false;
            };

            trace!(
                "verify shunt junction proof at {i} next_node_root_hash = {next_node_root_hash:?} penultimate hash = {penultimate_trie_hash:?}: {shunt_proof_junction:?}"
            );
            let Some(next_trie_hash) = TrieMerkleProof::verify_shunt_proof_junction(
                &next_node_root_hash,
                &penultimate_trie_hash,
                shunt_proof_junction,
            ) else {
                test_debug!(
                    "Unable to verify shunt junction proof at {i} next_node_root_hash = {next_node_root_hash:?} penultimate hash = {penultimate_trie_hash:?}: {shunt_proof_junction:?}"
                );
                return false;
            };

            // next node hash is the hash of the block from which its root came
            trie_hash = next_trie_hash;
            node_hash = match root_to_block.get(&trie_hash) {
                Some(bhh) => {
                    trace!("Block hash for {:?} is {:?}", &trie_hash, bhh);

                    // safe because block header hashes are 32 bytes long
                    TrieHash(bhh.clone().to_bytes())
                }
                None => {
                    test_debug!("Trie hash not found in root-to-block map: {:?}", &trie_hash);
                    test_debug!("root-to-block map: {:?}", &root_to_block);
                    return false;
                }
            };

            i += 1;

            if trie_hash == *root_hash {
                trace!(
                    "Appeared to find the root hash early, with the remaining proof:\n{:?}",
                    proof.get(i..)
                );
                break;
            }
        }

        test_debug!("Verify proof: {:?} =?= {:?}", root_hash, &trie_hash);
        *root_hash == trie_hash
    }

    /// Verify this proof
    pub fn verify(
        &self,
        path: &TrieHash,
        marf_value: &MARFValue,
        root_hash: &TrieHash,
        root_to_block: &HashMap<TrieHash, T>,
    ) -> bool {
        TrieMerkleProof::<T>::verify_proof(&self.0, path, marf_value, root_hash, root_to_block)
    }

    /// Make a merkle proof of inclusion from a path.
    /// If the path doesn't resolve, return an error (NotFoundError)
    pub fn from_path<S: NodePatching, R: TrieReadStorage<T> + ?Sized>(
        ctx: &mut MarfReadCtx<'_, T, S, R>,
        path: &TrieHash,
        expected_value: &MARFValue,
        root_block_header: &T,
    ) -> Result<TrieMerkleProof<T>, Error> {
        // Squash-aware proofs are not currently supported.
        if ctx.storage().is_squashed() {
            return Err(Error::UnsupportedOnSquashedMarf(
                "TrieMerkleProof::from_path",
            ));
        }

        ctx.with_restored_block_context(|ctx| {
            let mut segment_proofs = vec![];
            let mut shunt_proofs = vec![];
            let mut block_header = root_block_header.clone();
            let mut cursor = TrieCursor::new(path, TriePtr::default());

            loop {
                ctx.open_block(&block_header, None)?;

                trace!(
                    "Walk {:?} path {:?} to leaf or backptr",
                    &ctx.storage().get_cur_block(),
                    path
                );
                let walk_result = ctx.with_read_state(|storage, _cursor_opt, decode_scratch| {
                    let mut read_session = TrieReadSession::new(storage, decode_scratch);
                    trace!(
                        "Walk path {path:?} from {:?} to the first backptr",
                        read_session.storage().get_cur_block()
                    );
                    read_session.walk_to_leaf_or_backptr(path, &mut cursor)
                })?;
                let (backptr, reached_leaf_value) = match walk_result {
                    TrieLeafOrBackptr::Leaf { ptr, mut leaf } => {
                        ctx.storage().resolve_leaf_value(&mut leaf)?;
                        (ptr, Some(leaf.value()?.clone()))
                    }
                    TrieLeafOrBackptr::Backptr(ptr) => {
                        trace!("Found backptr {:?}", &ptr);
                        (ptr, None)
                    }
                };

                let segment_proof =
                    ctx.with_read_state(|storage, _cursor_opt, decode_scratch| {
                        trace!(
                            "Make segment proof at {:?} from {:?}",
                            &storage.get_cur_block(),
                            &cursor.node_ptrs
                        );
                        TrieMerkleProof::make_segment_proof(
                            storage,
                            &cursor.node_ptrs,
                            cursor.chr().unwrap(),
                            decode_scratch,
                        )
                    })?;
                segment_proofs.push(segment_proof);

                let cur_block = ctx.storage().get_cur_block();
                trace!(
                    "Make shunt proof {:?} back to the block containing {:?} (cursor ptrs = {:?})",
                    &cur_block,
                    &backptr,
                    &cursor.node_ptrs
                );

                if is_backptr(backptr.id()) {
                    shunt_proofs.push(TrieMerkleProof::make_backptr_shunt_proof(ctx, &backptr)?);
                } else {
                    shunt_proofs.push(TrieMerkleProof::make_initial_shunt_proof(ctx)?);
                }

                if cursor.ptr().id() == TrieNodeID::Leaf as u8 {
                    match reached_leaf_value {
                        Some(data) => {
                            if data != *expected_value {
                                trace!(
                                    "Did not find leaf {:?} at {:?} (but got {:?})",
                                    expected_value,
                                    path,
                                    data
                                );

                                #[cfg(test)]
                                {
                                    use std::env;
                                    if env::var("BLOCKSTACK_TEST_PROOF_ALLOW_INVALID")
                                        == Ok("1".to_string())
                                    {
                                        break;
                                    }
                                }
                                return Err(Error::NotFoundError);
                            }
                        }
                        None => {
                            trace!("Did not find leaf at {:?}", path);
                            return Err(Error::NotFoundError);
                        }
                    }
                    break;
                }

                ctx.storage().open_block(&block_header)?;

                trace!(
                    "Walk back for {:?} from {:?}",
                    &backptr,
                    &ctx.storage().get_cur_block()
                );
                block_header = ctx
                    .storage()
                    .get_block_from_local_id(backptr.back_block())?;
            }

            assert_eq!(shunt_proofs.len(), segment_proofs.len());

            let mut proof = Vec::with_capacity(segment_proofs.len() + shunt_proofs.len());
            for (mut segment, mut shunt) in segment_proofs
                .into_iter()
                .rev()
                .zip(shunt_proofs.into_iter().rev())
            {
                trace!("Append segment proof\n{:?}", &segment);
                proof.append(&mut segment);
                trace!("Append shunt proof\n{:?}", &shunt);
                proof.append(&mut shunt);
            }

            Ok(TrieMerkleProof(proof))
        })
    }
}
