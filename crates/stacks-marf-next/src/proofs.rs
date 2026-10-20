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

use std::collections::HashMap;
use std::fmt;
use std::io::{Read, Write};
use std::ops::Deref;

use sha2::{Digest, Sha512_256};
use stacks_common::codec::{read_next, write_next, Error as codec_error, StacksMessageCodec};
use stacks_common::types::chainstate::TrieHash;
use stacks_common::util::hash::to_hex;

use crate::hash::marf_root_hash;
use crate::marf::View;
use crate::node::{id, Leaf, Node, Ptr};
use crate::{corrupt, ClarityMarfTrieId, Error, MARFValue, MarfTrieId};

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

    pub(crate) fn from_leaf(leaf: &Leaf) -> TrieLeaf {
        TrieLeaf {
            path: leaf.path.as_slice().to_vec(),
            data: leaf.value.clone(),
        }
    }

    pub(crate) fn hash(&self) -> TrieHash {
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

// ---------------------------------------------------------------------------
// wire format

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

// ---------------------------------------------------------------------------
// generation

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

impl<'a, T: MarfTrieId> View<'a, T> {
    /// Walk `path` inside the trie of `tip` only, stopping at the leaf or at
    /// the first back-pointer.
    fn walk_segment(&mut self, tip: &T, path: &TrieHash) -> Result<Segment, Error> {
        let site = self.proof_site(tip)?;
        let mut ptr = self.proof_root_ptr(site);
        let mut ptrs = vec![ptr];
        let key = path.as_bytes();
        let mut depth = 0usize;
        loop {
            let node = self.proof_view(site, &ptr)?;
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
    fn proof_node(
        &mut self,
        tip: &T,
        ptr: &Ptr,
        prev_chr: u8,
    ) -> Result<TrieMerkleProofType<T>, Error> {
        let site = self.proof_site(tip)?;
        let node = self.proof_read(site, ptr)?;
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
                                let bhh = self.proof_block_hash(p.back_block)?;
                                (bhh.clone(), TrieHash(bhh.to_bytes()))
                            } else {
                                (T::from_bytes([0u8; 32]), self.proof_child_hash(site, p)?)
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

    fn make_segment_proof(
        &mut self,
        tip: &T,
        segment: &Segment,
    ) -> Result<Vec<TrieMerkleProofType<T>>, Error> {
        let mut proof = Vec::with_capacity(segment.ptrs.len());
        let mut prev_chr = segment.chr;
        for ptr in segment.ptrs.iter().rev() {
            proof.push(self.proof_node(tip, ptr, prev_chr)?);
            prev_chr = ptr.chr;
        }
        Ok(proof)
    }

    /// Height of `tip` as recorded in its own trie.
    fn height_of(&mut self, tip: &T) -> Result<u32, Error> {
        self.get_block_height(tip, tip)?
            .ok_or_else(|| corrupt!("Could not find block height of block {tip}"))
    }

    /// The ancestor root hashes hashed into `tip`'s MARF root.
    fn ancestors_of(&mut self, tip: &T) -> Result<Vec<TrieHash>, Error> {
        let h = self.height_of(tip)?;
        self.proof_ancestor_hashes(tip, h)
    }

    /// The shunt proof linking `tip`'s trie to the trie of `backptr.back_block`.
    fn make_backptr_shunt_proof(
        &mut self,
        tip: &T,
        backptr: &Ptr,
    ) -> Result<Vec<TrieMerkleProofType<T>>, Error> {
        let mut proof = vec![];
        let mut block = tip.clone();
        let ancestor_block = self.proof_block_hash(backptr.back_block)?;
        let ancestor_root_hash = self.get_root_hash_at(&ancestor_block)?;
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
    fn proof_root_node_hash(&mut self, tip: &T) -> Result<TrieHash, Error> {
        let site = self.proof_site(tip)?;
        let root_ptr = self.proof_root_ptr(site);
        let (node, hashes) = self.proof_node_full(tip, &root_ptr)?;
        if node.id != id::NODE256 {
            return Err(corrupt!("Root node at {tip} is not a TrieNode256"));
        }
        Ok(node.hash(&hashes))
    }

    /// A branch's proof view with all child hashes.
    fn proof_node_full(
        &mut self,
        tip: &T,
        ptr: &Ptr,
    ) -> Result<(ProofTrieNode<T>, Vec<TrieHash>), Error> {
        let site = self.proof_site(tip)?;
        let node = self.proof_read(site, ptr)?;
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
                        let bhh = self.proof_block_hash(p.back_block)?;
                        (bhh.clone(), TrieHash(bhh.to_bytes()))
                    } else {
                        (T::from_bytes([0u8; 32]), self.proof_child_hash(site, p)?)
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
    pub fn make_proof(
        &mut self,
        path: &TrieHash,
        expected_value: &MARFValue,
        root_block: &T,
    ) -> Result<TrieMerkleProof<T>, Error> {
        if self.is_pruned() {
            return Err(Error::UnsupportedOnPrunedMarf("TrieMerkleProof::from_path"));
        }
        let mut segment_proofs = vec![];
        let mut shunt_proofs = vec![];
        let mut block = root_block.clone();
        loop {
            self.proof_site(&block)?;
            let segment = self.walk_segment(&block, path)?;
            segment_proofs.push(self.make_segment_proof(&block, &segment)?);
            match segment.backptr {
                Some(backptr) => {
                    shunt_proofs.push(self.make_backptr_shunt_proof(&block, &backptr)?);
                    block = self.proof_block_hash(backptr.back_block)?;
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

impl<T: MarfTrieId> TrieMerkleProof<T> {
    pub fn to_hex(&self) -> String {
        let mut bytes = vec![];
        self.consensus_serialize(&mut bytes)
            .expect("Write error on memory buffer");
        to_hex(&bytes)
    }

    /// Make a merkle proof of inclusion from a path.
    pub fn from_path(
        view: &mut View<'_, T>,
        path: &TrieHash,
        expected_value: &MARFValue,
        root_block_header: &T,
    ) -> Result<TrieMerkleProof<T>, Error> {
        view.make_proof(path, expected_value, root_block_header)
    }

    /// Make a merkle proof of inclusion from a key/value pair.
    pub fn from_entry(
        view: &mut View<'_, T>,
        key: &str,
        value: &str,
        root_block_header: &T,
    ) -> Result<TrieMerkleProof<T>, Error> {
        view.make_proof(
            &TrieHash::from_key(key),
            &MARFValue::from_value(value),
            root_block_header,
        )
    }

    pub fn from_raw_entry(
        view: &mut View<'_, T>,
        key: &str,
        value: &MARFValue,
        root_block_header: &T,
    ) -> Result<TrieMerkleProof<T>, Error> {
        view.make_proof(&TrieHash::from_key(key), value, root_block_header)
    }

    // -- verification (ported from the original implementation) -----------------

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trie_leaf_hash_matches_node_leaf_hash() {
        let leaf = Leaf {
            path: crate::node::Path::from_slice(&[1, 2, 3]).unwrap(),
            value: MARFValue([9u8; 40]),
        };
        assert_eq!(
            TrieLeaf::from_leaf(&leaf).hash(),
            crate::hash::leaf_hash(&leaf)
        );
    }
}
