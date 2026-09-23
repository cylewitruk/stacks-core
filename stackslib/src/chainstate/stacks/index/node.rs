// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020-2026 Stacks Open Internet Foundation
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

use super::inline_value::{self, InlineValue};
use super::ValueExtent;
#[cfg(feature = "marf-read-bench-counters")]
use crate::chainstate::stacks::index::read_bench;

use std::io::{Read, Write};
use std::{error, fmt};

use crate::chainstate::stacks::index::bits::{self, SPARSE_PTR_BITMAP_MARKER};
use crate::chainstate::stacks::index::{
    BlockMap, ClarityMarfTrieId, Error, MARFValue, MarfTrieId, NodePath, ReadNodeBacking,
    ReadTrieNode, ReadTrieNodeCursorStep, TrieLeaf, MARF_VALUE_ENCODED_SIZE,
};
use crate::codec::{read_next, write_next, Error as codec_error, StacksMessageCodec};
use crate::types::chainstate::{TrieHash, BLOCK_HEADER_HASH_ENCODED_SIZE, TRIEHASH_ENCODED_SIZE};
use crate::util::hash::to_hex;

#[derive(Debug, Clone, PartialEq)]
pub enum CursorError {
    PathDiverged,
    BackptrEncountered(TriePtr),
    ChrNotFound,
}

impl fmt::Display for CursorError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            CursorError::PathDiverged => write!(f, "Path diverged"),
            CursorError::BackptrEncountered(_) => write!(f, "Back-pointer encountered"),
            CursorError::ChrNotFound => write!(f, "Node child not found"),
        }
    }
}

impl error::Error for CursorError {
    fn cause(&self) -> Option<&dyn error::Error> {
        None
    }
}

impl From<CursorError> for Error {
    fn from(e: CursorError) -> Self {
        Error::CursorError(e)
    }
}

// All numeric values of a Trie node when encoded.
// The low 4 bits encode the base TrieNodeID value (0-6).
// * the 8th bit (0x80) indicates a back-pointer to be followed
// * the 7th bit (0x40) indicates the ptrs are compressed. Cleared on read.
// * the 6th bit (0x20) indicates the ptr offset is encoded as u64, instead of u32. Cleared on read.
// * the 5th bit (0x10) indicates a compressed inline pointer contains a back_block payload. Cleared on read.
define_u8_enum!(TrieNodeID {
    Empty = 0,
    Leaf = 1,
    Node4 = 2,
    Node16 = 3,
    Node48 = 4,
    Node256 = 5,
    Patch = 6,
    ValueLeaf = 7,
    RawLeaf = 8,
    InlineLeaf = 9
});

impl TrieNodeID {
    /// Maximum serialized body length for this node kind, excluding the leading hash.
    ///
    /// Returns `None` for `Empty`, which is a pointer sentinel rather than a serialized node body.
    pub fn max_body_byte_len(self) -> Option<usize> {
        match self {
            TrieNodeID::Empty => None,
            TrieNodeID::Leaf => Some(<TrieLeaf as TrieNode>::MAX_BODY_BYTE_LEN),
            TrieNodeID::InlineLeaf => Some(
                1 + NODE_PATH_MAX_BYTE_LEN + inline_value::LENGTH_BYTES + inline_value::MAX_BYTES,
            ),
            TrieNodeID::RawLeaf => Some(1 + NODE_PATH_MAX_BYTE_LEN + 40),
            TrieNodeID::ValueLeaf => Some(1 + NODE_PATH_MAX_BYTE_LEN + ValueExtent::ENCODED_SIZE),
            TrieNodeID::Node4 => Some(<TrieNode4 as TrieNode>::MAX_BODY_BYTE_LEN),
            TrieNodeID::Node16 => Some(<TrieNode16 as TrieNode>::MAX_BODY_BYTE_LEN),
            TrieNodeID::Node48 => Some(<TrieNode48 as TrieNode>::MAX_BODY_BYTE_LEN),
            TrieNodeID::Node256 => Some(<TrieNode256 as TrieNode>::MAX_BODY_BYTE_LEN),
            TrieNodeID::Patch => Some(TrieNodePatch::MAX_BODY_BYTE_LEN),
        }
    }
}

/// A node ID encodes a back-pointer if its high bit is set
pub fn is_backptr(id: u8) -> bool {
    id & 0x80 != 0
}

/// Set the back-pointer bit
pub fn set_backptr(id: u8) -> u8 {
    id | 0x80
}

/// Clear the back-pointer bit
pub fn clear_backptr(id: u8) -> u8 {
    id & 0x7f
}

/// Is this node compressed?
pub fn is_compressed(id: u8) -> bool {
    id & 0x40 != 0
}

/// Set the compressed bit
pub fn set_compressed(id: u8) -> u8 {
    id | 0x40
}

/// Clear the compressed bit
pub fn clear_compressed(id: u8) -> u8 {
    id & 0xbf
}

/// Is this compressed inline pointer flagged to carry `back_block` payload bytes?
/// This bit is wire-format-only metadata and is cleared after decoding.
pub fn has_inline_back_block(id: u8) -> bool {
    id & 0x10 != 0
}

/// Set the compressed inline `back_block` payload bit.
pub fn set_inline_back_block(id: u8) -> u8 {
    id | 0x10
}

/// Clear the compressed inline `back_block` payload bit.
pub fn clear_inline_back_block(id: u8) -> u8 {
    id & 0xef
}

/// True if a compressed pointer with this encoded id includes a back_block payload.
#[inline]
fn has_back_block_payload_bytes(id: u8) -> bool {
    is_backptr(id) || has_inline_back_block(id)
}

/// Is this pointer encoded with a u64 offset?
pub const fn is_u64_ptr(id: u8) -> bool {
    id & 0x20 != 0
}

/// Set the u64-pointer bit
pub const fn set_u64_ptr(id: u8) -> u8 {
    id | 0x20
}

/// Clear the u64-pointer bit
pub const fn clear_u64_ptr(id: u8) -> u8 {
    id & 0xdf
}

/// Clear all control bits (backptr, compressed, u64-pointer, squash annotation)
pub fn clear_ctrl_bits(id: u8) -> u8 {
    id & 0x0f
}

/// Normalize physical value leaves to the historical leaf ID while retaining control bits.
pub fn logical_node_id(id: u8) -> u8 {
    if matches!(clear_ctrl_bits(id), x if x == TrieNodeID::ValueLeaf as u8 || x == TrieNodeID::RawLeaf as u8 || x == TrieNodeID::InlineLeaf as u8)
    {
        (id & 0xf0) | TrieNodeID::Leaf as u8
    } else {
        id
    }
}

/// Whether an ID denotes either physical representation of a leaf.
pub fn is_leaf_id(id: u8) -> bool {
    clear_ctrl_bits(logical_node_id(id)) == TrieNodeID::Leaf as u8
}

// Byte writing operations for pointer lists, paths.

/// Write out the list of TriePtrs to the given Write object.
/// The written pointers will NOT be compressed.
/// Returns Ok(()) on success
/// Returns Err(IOError(..)) on disk I/O error
fn write_ptrs_to_bytes<W: Write>(ptrs: &[TriePtr], w: &mut W) -> Result<(), Error> {
    for ptr in ptrs.iter() {
        ptr.write_bytes(w)?;
    }
    Ok(())
}

/// Write the list of TriePtrs to the given Write object.
///
/// The given `id` is a node ID with some control bits set -- in particular, the compressed bit.
///
/// If the compressed bit is set, then the TriePtr list will be compressed as best as possible
/// before written.  See `bits::ptrs_to_bytes()` for details.
///
/// Returns:
///
/// * Ok(()) on success
/// * Err(CorruptionError(..)) if the id does not correspond to a valid node ID or is a patch
/// node ID
/// * Err(IOError(..)) on disk I/O error
fn write_ptrs_to_bytes_compressed<W: Write>(
    id: u8,
    ptrs: &[TriePtr],
    w: &mut W,
) -> Result<(), Error> {
    let Some(node_id) = TrieNodeID::from_u8(id) else {
        return Err(Error::CorruptionError(
            "Tried to store invalid trie node ID".to_string(),
        ));
    };

    if node_id == TrieNodeID::Patch {
        // NB the only proper way to store a patch node is to have it dumped as part of a TrieRAM
        return Err(Error::CorruptionError(
            "Tried to store patch node's ptrs improperly".to_string(),
        ));
    }

    let Some((ptrs_size, is_sparse)) = bits::get_compressed_ptrs_size(id, ptrs) else {
        // doesn't apply -- this node has no ptrs
        return Ok(());
    };

    if is_sparse {
        // do a sparse write -- just write the bitmap and the non-empty trieptrs.
        // the first byte is SPARSE_PTR_BITMAP_MARKER to indicate that this is a sparse list, since it cannot be a
        // valid trie node ID
        w.write_all(&[SPARSE_PTR_BITMAP_MARKER])?;

        // compute the bitmap
        let bitmap_size = bits::get_sparse_ptrs_bitmap_size(id).ok_or_else(|| {
            Error::CorruptionError(format!("No bitmap size defined for node id {id}"))
        })?;

        let mut bitmap = vec![0u8; bitmap_size];
        for (i, ptr) in ptrs.iter().enumerate() {
            if ptr.id() != TrieNodeID::Empty as u8 {
                // SAFETY: have checked ptrs.len() against bitmap size
                let bi = i / 8;
                let bt = i % 8;
                let mask = 1u8 << bt;
                let byte_mut = bitmap
                    .get_mut(bi)
                    .ok_or_else(|| Error::CorruptionError("bitmap not long enough".into()))?;
                *byte_mut |= mask;
            }
        }
        trace!(
            "Write sparse compressed ptrs list ({} bytes) for node {}; bitmap {}",
            ptrs_size,
            id,
            to_hex(&bitmap)
        );

        // write out bitmap
        w.write_all(&bitmap)?;

        // write out non-empty ptrs
        for ptr in ptrs.iter() {
            if ptr.id() != TrieNodeID::Empty as u8 {
                trace!("write sparse ptr {}", {
                    let mut byte_buffer = vec![];
                    _ = ptr.write_bytes_compressed(&mut byte_buffer);
                    to_hex(&byte_buffer)
                });
                ptr.write_bytes_compressed(w)?;
            }
        }
        return Ok(());
    }

    // ptrs are not sparse enough.
    // compute a bitmap of which ptrs are non-empty
    trace!(
        "Write dense compressed ptrs list ({} bytes) for node {}",
        ptrs_size,
        id
    );
    for ptr in ptrs.iter() {
        ptr.write_bytes_compressed(w)?;
    }
    Ok(())
}

fn ptrs_consensus_hash<W: Write, M: BlockMap + ?Sized>(
    ptrs: &[TriePtr],
    map: &mut M,
    w: &mut W,
) -> Result<(), Error> {
    for ptr in ptrs.iter() {
        ptr.write_consensus_bytes(map, w)?;
    }
    Ok(())
}

/// Copy-on-write pointer to a node.  When the MARF writes a new key/value pair, it copies
/// intermediate nodes from the parent trie into the new trie being built.  This struct is a
/// pointer stored in the new trie's nodes which point back to the node it was copied from.
///
/// This data is not stored anywhere.  It is used instead to compute TrieNodePatch nodes to write
/// to disk as a space-efficient alternative to copying over the same lightly-modified node over
/// and over again.
///
/// Fields are (trie block hash holding the node, pointer to the node in the trie)
#[derive(Clone, PartialEq, Copy)]
pub struct TrieCowPtr([u8; 32], TriePtr);

impl TrieCowPtr {
    pub fn new<T: MarfTrieId>(trie_id: T, ptr: TriePtr) -> Self {
        Self(trie_id.to_bytes(), ptr)
    }

    pub fn block_id<T: MarfTrieId>(&self) -> T {
        T::from_bytes(self.0)
    }

    pub fn ptr(&self) -> &TriePtr {
        &self.1
    }
}

impl fmt::Debug for TrieCowPtr {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "TrieCowPtr({},{})",
            &to_hex(&self.0),
            &ptrs_fmt(&[self.1])
        )
    }
}

/// Transient metadata from an owned branch trie node.
///
/// This metadata is not carried by [`TrieNodeRef`], because that type is a lightweight structural
/// view over path/ptr storage only. Read paths capture it alongside node refs so conversion back
/// to owned nodes can round-trip without losing COW/patch state.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TrieNodeTransientMeta {
    pub cowptr: Option<TrieCowPtr>,
    pub patch_depth: usize,
    pub last_patch_source: Option<(u32, TriePtr)>,
}

const NODE_PATH_MAX_BYTE_LEN: usize = 1 + TRIEHASH_ENCODED_SIZE;
const NODE_PTR_ID_BYTE_LEN: usize = 1;
const NODE48_INDEX_BYTE_LEN: usize = 256;
const PATCH_DIFF_MAX_PTRS: usize = 256;

/// All Trie nodes implement the following methods:
pub trait TrieNode {
    /// Maximum serialized body length for this node type, excluding the leading hash.
    const MAX_BODY_BYTE_LEN: usize;

    /// Node ID for encoding/decoding
    fn id(&self) -> u8;

    /// Is the node devoid of children?
    fn empty() -> Self;

    /// Follow a path character to a child pointer
    fn walk(&self, chr: u8) -> Option<TriePtr>;

    /// Insert a child pointer if the path character slot is not occupied.
    /// Return true if inserted, false if the slot is already filled
    fn insert(&mut self, ptr: &TriePtr) -> bool;

    /// Replace an existing child pointer with a new one.  Returns true if replaced; false if the
    /// child does not exist.
    fn replace(&mut self, ptr: &TriePtr) -> bool;

    /// Load an encoded instance of this node from bytes into `self`.
    fn load_from_slice(&mut self, bytes: &[u8]) -> Result<usize, Error> {
        let (&marker, body) = bytes
            .split_first()
            .ok_or_else(|| Error::CorruptionError("Missing node marker".into()))?;
        Ok(1 + self.load_from_parts(marker, body)?)
    }

    /// Decode a marker and separately borrowed payload, excluding any stored hash.
    fn load_from_parts(&mut self, marker: u8, body: &[u8]) -> Result<usize, Error>;

    /// Read an encoded instance of this node from bytes and instantiate a new owned value.
    fn from_bytes(bytes: &[u8]) -> Result<(Self, usize), Error>
    where
        Self: Sized,
    {
        let mut node = Self::empty();
        let consumed = node.load_from_slice(bytes)?;
        Ok((node, consumed))
    }

    /// Get a reference to the children of this node.
    fn ptrs(&self) -> &[TriePtr];

    /// Get a mutable reference to the children of this node.
    fn ptrs_mut(&mut self) -> &mut [TriePtr];

    /// Transient COW/patch metadata for branch nodes.
    fn meta(&self) -> Option<&TrieNodeTransientMeta>;

    /// Mutable transient COW/patch metadata for branch nodes.
    fn meta_mut(&mut self) -> Option<&mut TrieNodeTransientMeta>;

    /// Reset transient metadata (COW pointer, patch depth, patch source) to defaults.
    fn reset_transient_meta(&mut self) {
        if let Some(meta) = self.meta_mut() {
            *meta = TrieNodeTransientMeta::default();
        }
    }

    /// Get a reference to the node's compressed path.
    fn path(&self) -> &NodePath;

    /// Construct a TrieNodeType from a TrieNode
    fn as_trie_node_type(&self) -> TrieNodeType;

    /// Get the ptr to the node we were copied from (on COW)
    fn get_cow_ptr(&self) -> Option<&TrieCowPtr> {
        self.meta().and_then(|meta| meta.cowptr.as_ref())
    }

    /// Set the ptr to the node we were copied from (on COW)
    fn set_cow_ptr(&mut self, cowptr: TrieCowPtr) {
        if let Some(meta) = self.meta_mut() {
            meta.cowptr = Some(cowptr);
        }
    }

    /// Encode this node instance into a byte stream and write it to w.
    /// The TriePtrs willl NOT be compressed
    fn write_bytes<W: Write>(&self, w: &mut W) -> Result<(), Error> {
        w.write_all(&[self.id()])?;
        write_ptrs_to_bytes(self.ptrs(), w)?;
        bits::write_path_to_bytes(self.path().as_slice(), w)
    }

    /// Encode this node instance into a byte stream and write it to w.
    /// The TriePtrs will be compressed to the smallest possible size.
    fn write_bytes_compressed<W: Write>(&self, w: &mut W) -> Result<(), Error> {
        w.write_all(&[set_compressed(self.id())])?;
        write_ptrs_to_bytes_compressed(self.id(), self.ptrs(), w)?;
        bits::write_path_to_bytes(self.path().as_slice(), w)
    }

    #[cfg(test)]
    fn to_bytes(&self) -> Vec<u8> {
        let mut r = Vec::new();
        self.write_bytes(&mut r)
            .expect("Failed to write to byte buffer");
        r
    }

    /// Calculate how many bytes this node will take to encode.
    fn byte_len(&self) -> usize {
        bits::get_ptrs_byte_len(self.ptrs()) + bits::get_path_byte_len(self.path())
    }

    /// Calculate how many bytes this node will take to encode.
    fn byte_len_compressed(&self) -> usize {
        bits::get_ptrs_byte_len_compressed(self.id(), self.ptrs())
            + bits::get_path_byte_len(self.path())
    }
}

/// Trait for types that can serialize to consensus bytes
/// This is implemented by `TrieNode`s and `ProofTrieNode`s
///  and allows hash calculation routines to be the same for
///  both types.
/// The type `M` is used for any additional data structures required
///   (BlockHashMap for TrieNode and () for ProofTrieNode)
pub trait ConsensusSerializable<M: ?Sized> {
    /// Encode the consensus-relevant bytes of this node and write it to w.
    fn write_consensus_bytes<W: Write>(
        &self,
        additional_data: &mut M,
        w: &mut W,
    ) -> Result<(), Error>;

    #[cfg(test)]
    fn to_consensus_bytes(&self, additional_data: &mut M) -> Vec<u8> {
        let mut r = Vec::new();
        self.write_consensus_bytes(additional_data, &mut r)
            .expect("Failed to write to byte buffer");
        r
    }
}

impl<T: TrieNode, M: BlockMap + ?Sized> ConsensusSerializable<M> for T {
    fn write_consensus_bytes<W: Write>(&self, map: &mut M, w: &mut W) -> Result<(), Error> {
        w.write_all(&[self.id()])?;
        ptrs_consensus_hash(self.ptrs(), map, w)?;
        bits::write_path_to_bytes(self.path().as_slice(), w)
    }
}

impl<M: BlockMap + ?Sized> ConsensusSerializable<M> for TrieNodeRef<'_> {
    fn write_consensus_bytes<W: Write>(&self, map: &mut M, w: &mut W) -> Result<(), Error> {
        w.write_all(&[self.id()])?;
        ptrs_consensus_hash(self.ptrs(), map, w)?;
        bits::write_path_to_bytes(self.path_bytes(), w)
    }
}

/// Child pointer within a MARF trie node.
///
/// `back_block` has two modes depending on the backptr flag in `id`:
///
/// * Back-pointer (`id & 0x80 != 0`): the child lives in a different block's trie.
///   `back_block` is the `marf_data` row ID of that block, and `ptr` is the byte offset
///   within that block's trie storage.
///
/// * Inline (`id & 0x80 == 0`): the child lives in the same trie storage.
///   `back_block` is normally 0. In a squashed MARF, a non-zero `back_block` is a
///   squash annotation: it records the original archival block ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TriePtr {
    /// Node type ID of the child (see [`TrieNodeID`]). Bit 0x80 marks a back-pointer.
    pub id: u8,
    /// Path character at which this child resides.
    pub chr: u8,
    /// Byte offset of the child's encoded data within the trie storage.
    pub ptr: u64,
    /// Block ID of the trie containing the child. Zero for same-block inline children
    /// (unless carrying a squash annotation).
    pub back_block: u32,
}

pub fn ptrs_fmt(ptrs: &[TriePtr]) -> String {
    let mut strs = vec![];
    for ptr in ptrs.iter() {
        if ptr.id != TrieNodeID::Empty as u8 {
            strs.push(format!(
                "id({})chr({:02x})ptr({})bblk({})",
                ptr.id, ptr.chr, ptr.ptr, ptr.back_block
            ))
        }
    }
    strs.join(",")
}

fn fmt_trie_node(name: &str, path: &[u8], ptrs: &[TriePtr], f: &mut fmt::Formatter) -> fmt::Result {
    write!(f, "{name}(path={} ptrs={})", &to_hex(path), ptrs_fmt(ptrs))
}

impl Default for TriePtr {
    #[inline]
    fn default() -> TriePtr {
        TriePtr {
            id: 0,
            chr: 0,
            ptr: 0,
            back_block: 0,
        }
    }
}

impl TriePtr {
    /// Serialized size of the node-ID byte.
    const ID_BYTE_LEN: usize = 1;
    /// Serialized size of the path-char byte.
    const CHR_BYTE_LEN: usize = 1;
    /// Serialized size of the `back_block` field (only present in uncompressed
    /// form, or in compressed form when the pointer is a backptr).
    const BACK_BLOCK_BYTE_LEN: usize = 4;

    #[inline]
    pub fn new(id: u8, chr: u8, ptr: u64) -> TriePtr {
        TriePtr {
            id,
            chr,
            ptr,
            back_block: 0,
        }
    }

    /// A pointer whose offset forces the widest on-disk encoding: an 8-byte
    /// (u64) offset, as used by a squashed trie that exceeds 4 GiB. The id,
    /// chr, and back_block are irrelevant to the encoded width. Useful for
    /// computing a node type's maximum serialized size.
    pub fn widest_encoded() -> TriePtr {
        TriePtr::new(0, 0, u64::from(u32::MAX) + 1)
    }

    /// Create a back-pointer version of a [`TriePtr`]
    #[cfg(test)]
    pub fn new_backptr(id: u8, chr: u8, ptr: u64, back_block: u32) -> TriePtr {
        TriePtr {
            id: set_backptr(id),
            chr,
            ptr,
            back_block,
        }
    }

    #[inline]
    pub fn id(&self) -> u8 {
        self.id
    }

    #[inline]
    /// Is the TriePtr an unoccupied slot?
    pub fn is_empty(&self) -> bool {
        self.id() == TrieNodeID::Empty as u8
    }

    #[inline]
    pub fn chr(&self) -> u8 {
        self.chr
    }

    #[inline]
    pub fn ptr(&self) -> u64 {
        self.ptr
    }

    /// Convert `self.ptr()` to a `u32` in-memory index, or return an error
    /// if the value exceeds `u32::MAX`.
    #[inline]
    pub fn try_ptr_into_u32(&self) -> Result<u32, Error> {
        u32::try_from(self.ptr).map_err(|_| Error::OverflowError)
    }

    /// Convert `self.ptr()` to a `usize` in-memory index, or return an error
    /// if the value exceeds `usize::MAX`.
    #[inline]
    pub fn try_ptr_into_usize(&self) -> Result<usize, Error> {
        usize::try_from(self.ptr).map_err(|_| Error::OverflowError)
    }

    #[inline]
    pub fn back_block(&self) -> u32 {
        self.back_block
    }

    #[inline]
    pub fn from_backptr(&self) -> TriePtr {
        TriePtr {
            id: clear_backptr(self.id),
            chr: self.chr,
            ptr: self.ptr,
            back_block: 0,
        }
    }

    /// Return the identifier byte that will be emitted on disk for this pointer.
    ///
    /// This preserves the logical node kind while setting or clearing the `0x20`
    /// control bit to match the encoded pointer width.
    #[inline]
    pub fn encoded_id(&self) -> u8 {
        if self.ptr() > u64::from(u32::MAX) {
            set_u64_ptr(self.id())
        } else {
            clear_u64_ptr(self.id())
        }
    }

    /// Return the uncompressed encoded size, in bytes, for a pointer with the
    /// given on-disk identifier byte.
    ///
    /// The `0x20` control bit determines whether the pointer payload is encoded
    /// as `u32` or `u64`.
    ///
    /// Uncompressed layout: `id (1) + chr (1) + ptr (4 or 8) + back_block (4)`.
    #[inline]
    pub const fn encoded_size_for_id(node_id: u8) -> usize {
        let ptr_bytes = if is_u64_ptr(node_id) { 8 } else { 4 };
        Self::ID_BYTE_LEN + Self::CHR_BYTE_LEN + ptr_bytes + Self::BACK_BLOCK_BYTE_LEN
    }

    /// Return the maximum possible uncompressed encoded size for any `TriePtr`.
    #[inline]
    pub const fn max_encoded_size() -> usize {
        Self::encoded_size_for_id(set_u64_ptr(TrieNodeID::Empty as u8))
    }

    /// Return the compressed encoded size, in bytes, for a pointer with the
    /// given on-disk identifier byte.
    ///
    /// The `0x20` control bit determines whether the pointer payload is encoded
    /// as `u32` or `u64`.
    ///
    /// Compressed layout: `id (1) + chr (1) + ptr (4 or 8)`. The `back_block`
    /// field is only appended for backptr nodes (see [`Self::compressed_size_for_id`]).
    #[inline]
    pub const fn encoded_size_compressed_for_id(node_id: u8) -> usize {
        let ptr_bytes = if is_u64_ptr(node_id) { 8 } else { 4 };
        Self::ID_BYTE_LEN + Self::CHR_BYTE_LEN + ptr_bytes
    }

    #[inline]
    pub fn write_bytes<W: Write>(&self, w: &mut W) -> Result<(), Error> {
        let encoded_id = self.encoded_id();
        w.write_all(&[encoded_id, self.chr()])?;
        if is_u64_ptr(encoded_id) {
            w.write_all(&self.ptr().to_be_bytes())?;
        } else {
            let ptr32 = self.try_ptr_into_u32()?;
            w.write_all(&ptr32.to_be_bytes())?;
        }
        w.write_all(&self.back_block().to_be_bytes())?;
        Ok(())
    }

    #[inline]
    pub fn write_bytes_compressed<W: Write>(&self, w: &mut W) -> Result<(), Error> {
        // Preserve squash annotation payload on disk for inline pointers that
        // carry a non-zero back_block, without changing backptr semantics.
        let mut encoded_id = set_compressed(self.encoded_id());
        if !is_backptr(self.id()) && self.back_block() != 0 {
            encoded_id = set_inline_back_block(encoded_id);
        }
        w.write_all(&[encoded_id, self.chr()])?;
        if is_u64_ptr(encoded_id) {
            w.write_all(&self.ptr().to_be_bytes())?;
        } else {
            let ptr32 = self.try_ptr_into_u32()?;
            w.write_all(&ptr32.to_be_bytes())?;
        }
        if has_back_block_payload_bytes(encoded_id) {
            w.write_all(&self.back_block().to_be_bytes())?;
        }
        Ok(())
    }

    /// The parts of a child pointer that are relevant for consensus are only its ID, path
    /// character, and referred-to block hash.  The software doesn't care about the details of how/where
    /// nodes are stored.
    pub fn write_consensus_bytes<W: Write, M: BlockMap + ?Sized>(
        &self,
        block_map: &mut M,
        w: &mut W,
    ) -> Result<(), Error> {
        let hash = if is_backptr(self.id()) {
            Some(
                block_map
                    .get_block_hash_caching(self.back_block())
                    .expect("Block identifier {} refered to an unknown block. Consensus failure.")
                    .as_bytes(),
            )
        } else {
            None
        };
        self.write_consensus_bytes_with_block_hash(hash, w)
    }

    /// Write pointer metadata using an already resolved backpointer identity.
    fn write_consensus_bytes_with_block_hash<W: Write>(
        &self,
        block_hash: Option<&[u8]>,
        w: &mut W,
    ) -> Result<(), Error> {
        w.write_all(&[self.id(), self.chr()])?;
        w.write_all(block_hash.unwrap_or(&[0; BLOCK_HEADER_HASH_ENCODED_SIZE]))?;
        Ok(())
    }

    #[inline]
    #[allow(clippy::indexing_slicing)]
    /// Deserialize a pointer from raw bytes and return it with bytes consumed.
    pub fn from_bytes(bytes: &[u8]) -> (TriePtr, usize) {
        let encoded_id = bytes[0];
        let min_len = TriePtr::encoded_size_for_id(encoded_id);
        assert!(bytes.len() >= min_len);
        let id = clear_u64_ptr(encoded_id);
        let chr = bytes[1];
        // Layout: [id(1)][chr(1)][ptr(4 or 8)][back_block(4)].
        let (ptr, back_block) = if is_u64_ptr(encoded_id) {
            let ptr = u64::from_be_bytes(bytes[2..10].try_into().unwrap());
            let back_block = u32::from_be_bytes(bytes[10..14].try_into().unwrap());
            (ptr, back_block)
        } else {
            let ptr = u64::from(u32::from_be_bytes(bytes[2..6].try_into().unwrap()));
            let back_block = u32::from_be_bytes(bytes[6..10].try_into().unwrap());
            (ptr, back_block)
        };

        (
            TriePtr {
                id,
                chr,
                ptr,
                back_block,
            },
            min_len,
        )
    }

    /// Load a [`TriePtr`] from a slice of bytes, assuming that they represent a compressed
    /// `TriePtr`.
    ///
    /// Returns the pointer and the number of bytes consumed.
    #[inline]
    #[allow(clippy::indexing_slicing)]
    pub fn from_slice_compressed(slice: &[u8]) -> Result<(TriePtr, usize), Error> {
        let encoded_id = clear_compressed(*slice.first().ok_or_else(|| {
            Error::CorruptionError("Failed to read compressed ptr ID".to_string())
        })?);
        let ptr_len = TriePtr::compressed_size_for_id(encoded_id);
        let bytes = slice.get(..ptr_len).ok_or_else(|| {
            Error::CorruptionError(format!("Failed to read {ptr_len} bytes of compressed ptr"))
        })?;

        Ok((Self::from_bytes_compressed(bytes), ptr_len))
    }

    /// Load up this TriePtr from a slice of bytes, assuming that they represent a compressed
    /// TriePtr.
    ///
    /// A compressed TriePtr stores `back_block` bytes if either:
    /// * it is a back-pointer (`is_backptr(id)`), or
    /// * it is an inline pointer with back_block payload
    ///   (`has_inline_back_block(id)`).
    ///
    /// The annotation bit is wire metadata and is cleared on read.
    #[inline]
    #[allow(clippy::indexing_slicing)]
    pub fn from_bytes_compressed(bytes: &[u8]) -> TriePtr {
        let encoded_id = clear_compressed(bytes[0]);
        assert!(bytes.len() >= TriePtr::compressed_size_for_id(encoded_id));
        let id = clear_u64_ptr(clear_inline_back_block(encoded_id));
        let chr = bytes[1];
        // Layout: [id(1)][chr(1)][ptr(4 or 8)]; backptrs append [back_block(4)].
        let ptr = if is_u64_ptr(encoded_id) {
            u64::from_be_bytes(bytes[2..10].try_into().unwrap())
        } else {
            u64::from(u32::from_be_bytes(bytes[2..6].try_into().unwrap()))
        };

        let back_block = if has_back_block_payload_bytes(encoded_id) {
            // Backpointers and squash annotations append a 4-byte `back_block` after the compressed ptr payload.
            let back_block_offset = TriePtr::encoded_size_compressed_for_id(encoded_id);
            let back_block_end = back_block_offset + 4;
            assert!(bytes.len() >= back_block_end);
            u32::from_be_bytes(bytes[back_block_offset..back_block_end].try_into().unwrap())
        } else {
            0
        };

        TriePtr {
            id,
            chr,
            ptr,
            back_block,
        }
    }

    /// Load up a compressed TriePtr from a Read object.
    /// Returns Ok(ptr) on success
    /// Returns Err(codec_error::*) on disk I/O failure, or failure to decode the requested bytes
    #[inline]
    pub fn read_bytes_compressed<R: Read>(fd: &mut R) -> Result<TriePtr, codec_error> {
        let id_bits: u8 = read_next(fd)?;
        let encoded_id = clear_compressed(id_bits);
        let id = clear_u64_ptr(clear_inline_back_block(encoded_id));
        let chr: u8 = read_next(fd)?;
        let ptr = if is_u64_ptr(encoded_id) {
            let hi: [u8; 4] = read_next(fd)?;
            let lo: [u8; 4] = read_next(fd)?;
            u64::from_be_bytes([hi[0], hi[1], hi[2], hi[3], lo[0], lo[1], lo[2], lo[3]])
        } else {
            let ptr_be_bytes: [u8; 4] = read_next(fd)?;
            u64::from(u32::from_be_bytes(ptr_be_bytes))
        };
        let back_block = if has_back_block_payload_bytes(encoded_id) {
            let bytes: [u8; 4] = read_next(fd)?;
            u32::from_be_bytes(bytes)
        } else {
            0
        };

        Ok(TriePtr {
            id,
            chr,
            ptr,
            back_block,
        })
    }

    /// Size of this TriePtr on disk.
    #[inline]
    pub fn encoded_size(&self) -> usize {
        Self::encoded_size_for_id(self.encoded_id())
    }

    /// Size of this TriePtr on disk, if compression is to be used.
    #[inline]
    pub fn compressed_size(&self) -> usize {
        let encoded_id = self.encoded_id();
        if !is_backptr(self.id) && self.back_block != 0 {
            Self::encoded_size_for_id(encoded_id)
        } else {
            Self::compressed_size_for_id(encoded_id)
        }
    }

    /// Returns the size, in bytes, that a node occupies on disk, taking compression into account.
    /// Pointers without a `back_block` payload omit it, while backpointers and
    /// inline-annotation pointers store it.
    #[inline]
    pub fn compressed_size_for_id(node_id: u8) -> usize {
        if !has_back_block_payload_bytes(node_id) {
            Self::encoded_size_compressed_for_id(node_id)
        } else {
            Self::encoded_size_for_id(node_id)
        }
    }
}

/// Cursor structure for walking down one or more Tries.  This structure helps other parts of the
/// codebase remember which nodes were visited, which blocks they came from, and which pointers
/// were walked.  In particular, it's useful for figuring out where to insert a new node, and which
/// nodes to visit when updating the root node hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParkedNodeHandle(usize);

impl ParkedNodeHandle {
    pub fn new(slot: usize) -> Self {
        Self(slot)
    }

    pub fn slot(self) -> usize {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum CursorNodeHandle<T: MarfTrieId> {
    Persisted { ptr: TriePtr, block_hash: T },
    Parked(ParkedNodeHandle),
}

#[derive(Debug, Clone, PartialEq)]
pub enum TrieCursorNode<T: MarfTrieId> {
    Handle(CursorNodeHandle<T>),
    Materialized(TrieNodeType),
}

impl<T: MarfTrieId> TrieCursorNode<T> {
    fn as_node(&self) -> Option<&TrieNodeType> {
        match self {
            TrieCursorNode::Handle(_) => None,
            TrieCursorNode::Materialized(node) => Some(node),
        }
    }

    fn as_handle(&self) -> Option<&CursorNodeHandle<T>> {
        match self {
            TrieCursorNode::Handle(handle) => Some(handle),
            TrieCursorNode::Materialized(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrieCursor<T: MarfTrieId> {
    /// The path to walk.
    pub path: TrieHash,
    /// Index into the path.
    pub index: usize,
    /// Index into the currently-visited node's compressed path.
    pub node_path_index: usize,
    /// List of visited nodes, materialized only when needed.
    pub nodes: Vec<TrieCursorNode<T>>,
    /// List of ptr branches this cursor has taken.
    pub node_ptrs: Vec<TriePtr>,
    /// List of Tries we've visited.
    ///
    /// `block_hashes[i]` corresponds to `node_ptrs[i + 1]`.
    pub block_hashes: Vec<T>,
    /// Last error encountered while walking (used to make sure the client calls the right "recovery" method).
    pub last_error: Option<CursorError>,
}

impl<T: MarfTrieId> TrieCursor<T> {
    fn step_from_walk_result(
        walk_result: Result<Option<TriePtr>, CursorError>,
        is_leaf: bool,
    ) -> ReadTrieNodeCursorStep {
        match walk_result {
            Ok(Some(next_ptr)) => ReadTrieNodeCursorStep::Next(next_ptr),
            Ok(None) => ReadTrieNodeCursorStep::EndOfPath { is_leaf },
            Err(CursorError::PathDiverged) => ReadTrieNodeCursorStep::Diverged,
            Err(CursorError::ChrNotFound) => ReadTrieNodeCursorStep::ChrNotFound,
            Err(CursorError::BackptrEncountered(ptr)) => ReadTrieNodeCursorStep::FollowBackptr(ptr),
        }
    }

    pub fn new(path: &TrieHash, root_ptr: TriePtr) -> TrieCursor<T> {
        TrieCursor {
            path: *path,
            index: 0,
            node_path_index: 0,
            nodes: vec![],
            node_ptrs: vec![root_ptr],
            block_hashes: vec![],
            last_error: None,
        }
    }

    pub fn reset(&mut self, path: &TrieHash, root_ptr: TriePtr) {
        self.path = *path;
        self.index = 0;
        self.node_path_index = 0;
        self.nodes.clear();
        self.node_ptrs.clear();
        self.node_ptrs.push(root_ptr);
        self.block_hashes.clear();
        self.last_error = None;
    }

    /// what point in the path are we at now?
    /// Will be None only if we haven't taken a step yet.
    pub fn chr(&self) -> Option<u8> {
        if self.index > 0 {
            self.path.as_bytes().get(self.index - 1).copied()
        } else {
            None
        }
    }

    /// what offset in the path are we at?
    pub fn tell(&self) -> usize {
        self.index
    }

    /// what is the offset in the node's compressed path?
    pub fn ntell(&self) -> usize {
        self.node_path_index
    }

    /// Are we a the [E]nd [O]f [P]ath?
    pub fn eop(&self) -> bool {
        self.index == self.path.len()
    }

    /// last ptr visited
    pub fn ptr(&self) -> TriePtr {
        // should always be true by construction
        assert!(!self.node_ptrs.is_empty());
        *self.node_ptrs.last().unwrap()
    }

    /// last node visited.
    /// Returns None if we haven't taken a step yet, or if the last visited node is still deferred.
    pub fn node(&self) -> Option<TrieNodeType> {
        self.nodes.last().and_then(TrieCursorNode::as_node).cloned()
    }

    pub fn node_handle(&self) -> Option<&CursorNodeHandle<T>> {
        self.nodes.last().and_then(TrieCursorNode::as_handle)
    }

    /// Are we at the [E]nd [O]f a [N]ode's [P]ath?
    pub fn eonp(&self, node: &TrieNodeType) -> bool {
        match node {
            TrieNodeType::Leaf(ref data) => self.node_path_index == data.path.len(),
            TrieNodeType::Node4(ref data) => self.node_path_index == data.path.len(),
            TrieNodeType::Node16(ref data) => self.node_path_index == data.path.len(),
            TrieNodeType::Node48(ref data) => self.node_path_index == data.path.len(),
            TrieNodeType::Node256(ref data) => self.node_path_index == data.path.len(),
        }
    }

    /// Compare the node's compressed path against the cursor path, then look up the child pointer
    /// for the next path byte via `walk_chr`.
    ///
    /// Callers must push the cursor node and reset `node_path_index` before calling this. This
    /// separation lets callers perform fallible operations (e.g. decoding path bytes) between the
    /// push and the walk, preserving cursor state on decode failure.
    fn inner_walk<E>(
        &mut self,
        node_path: &[u8],
        block_hash: &T,
        walk_chr: impl FnOnce(u8) -> Result<Option<TriePtr>, E>,
    ) -> Result<Option<TriePtr>, E>
    where
        E: From<CursorError>,
    {
        if self.index >= self.path.len() {
            trace!("cursor: out of path");
            return Ok(None);
        }

        let path_bytes = self.path.as_bytes();

        // consume as much of the compressed path as we can
        for (_i, path_set) in node_path.iter().enumerate() {
            let Some(path_head) = path_bytes.get(self.index) else {
                trace!("cursor: out of path");
                return Ok(None);
            };

            if path_set != path_head {
                trace!(
                    "cursor: diverged({} != {}): i = {_i}, self.index = {}, self.node_path_index = {}",
                    to_hex(node_path),
                    to_hex(path_bytes),
                    self.index,
                    self.node_path_index
                );

                self.last_error = Some(CursorError::PathDiverged);
                return Err(CursorError::PathDiverged.into());
            }

            self.index += 1;
            self.node_path_index += 1;
        }

        // Walked to end of the node's compressed path. Find the pointer to the next node.
        let Some(chr) = path_bytes.get(self.index) else {
            trace!("cursor: now out of path");
            return Ok(None);
        };

        self.index += 1;
        let ptr_opt = walk_chr(*chr)?;

        match ptr_opt {
            Some(ptr) if !is_backptr(ptr.id()) => {
                self.node_ptrs.push(ptr);
                self.block_hashes.push(block_hash.clone());
                Ok(Some(ptr))
            }
            Some(ptr) => {
                self.last_error = Some(CursorError::BackptrEncountered(ptr));
                trace!(
                    "cursor: not found: chr = 0x{chr:02x}, self.index = {}, self.path = {:?}",
                    self.index - 1,
                    &path_bytes
                );
                Err(CursorError::BackptrEncountered(ptr).into())
            }
            None => {
                self.last_error = Some(CursorError::ChrNotFound);
                trace!(
                    "cursor: not found: chr = 0x{chr:02x}, self.index = {}, self.path = {:?}",
                    self.index - 1,
                    &path_bytes
                );
                Err(CursorError::ChrNotFound.into())
            }
        }
    }

    /// Walk an owned node: follow its compressed path and child pointer.
    ///
    /// Records the node as [`Materialized`](TrieCursorNode::Materialized) in the cursor and returns
    /// `Err(BackptrEncountered)` if a back-pointer is reached (caller must handle via the
    /// `repair_backptr_*` methods).
    pub fn walk(
        &mut self,
        node: &TrieNodeType,
        block_hash: &T,
    ) -> Result<Option<TriePtr>, CursorError> {
        trace!("cursor: walk: node = {node:?} block = {block_hash:?}");
        self.walk_borrowed(
            &TrieNodeRef::from(node),
            block_hash,
            TrieCursorNode::Materialized((*node).clone()),
        )
    }

    /// Like [`walk`](Self::walk), but converts the result into a [`ReadTrieNodeCursorStep`].
    pub fn walk_step(&mut self, node: &TrieNodeType, block_hash: &T) -> ReadTrieNodeCursorStep {
        Self::step_from_walk_result(self.walk(node, block_hash), node.is_leaf())
    }

    /// Walk a borrowed [`TrieNodeRef`] with a caller-specified [`TrieCursorNode`] recording.
    fn walk_borrowed(
        &mut self,
        node: &TrieNodeRef<'_>,
        block_hash: &T,
        cursor_node: TrieCursorNode<T>,
    ) -> Result<Option<TriePtr>, CursorError> {
        assert!(self.last_error.is_none());
        trace!(
            "cursor: walk_ref: node = {:?} block = {:?}",
            node,
            block_hash
        );
        self.nodes.push(cursor_node);
        self.node_path_index = 0;
        self.inner_walk(node.path_bytes(), block_hash, |chr| {
            Ok::<_, CursorError>(node.walk(chr))
        })
    }

    /// Walk a persisted [`TrieNodeRef`], recording it as a
    /// [`Persisted`](CursorNodeHandle::Persisted) handle in the cursor.
    pub fn walk_ref(
        &mut self,
        node: &TrieNodeRef<'_>,
        block_hash: &T,
    ) -> Result<Option<TriePtr>, CursorError> {
        self.walk_borrowed(
            node,
            block_hash,
            TrieCursorNode::Handle(CursorNodeHandle::Persisted {
                ptr: self.ptr(),
                block_hash: block_hash.clone(),
            }),
        )
    }

    /// Walk a parked [`TrieNodeRef`], recording its [`ParkedNodeHandle`] in the cursor.
    pub fn walk_parked(
        &mut self,
        node: &TrieNodeRef<'_>,
        parked_handle: ParkedNodeHandle,
        block_hash: &T,
    ) -> Result<Option<TriePtr>, CursorError> {
        self.walk_borrowed(
            node,
            block_hash,
            TrieCursorNode::Handle(CursorNodeHandle::Parked(parked_handle)),
        )
    }

    /// Walk a [`ReadTrieNode`], dispatching to the appropriate walk variant based on its backing.
    pub fn walk_read(
        &mut self,
        node: &ReadTrieNode<'_>,
        block_hash: &T,
    ) -> Result<Option<TriePtr>, Error> {
        #[cfg(feature = "marf-read-bench-counters")]
        read_bench::update(|c| c.cursor_steps += 1);

        match &node.backing {
            ReadNodeBacking::VolatileDecoded(node_ref)
            | ReadNodeBacking::PersistedDecoded(node_ref) => self
                .walk_ref(node_ref, block_hash)
                .map_err(Error::CursorError),
            ReadNodeBacking::PersistedBytes(_) => self.walk_borrowed_read(
                node,
                block_hash,
                TrieCursorNode::Handle(CursorNodeHandle::Persisted {
                    ptr: self.ptr(),
                    block_hash: block_hash.clone(),
                }),
            ),
            ReadNodeBacking::Owned(node_type) => {
                self.walk(node_type, block_hash).map_err(Error::CursorError)
            }
        }
    }

    /// Like [`walk_borrowed`](Self::walk_borrowed) for [`ReadTrieNode`] (fallible path/walk
    /// accessors).
    fn walk_borrowed_read(
        &mut self,
        node: &ReadTrieNode<'_>,
        block_hash: &T,
        cursor_node: TrieCursorNode<T>,
    ) -> Result<Option<TriePtr>, Error> {
        assert!(self.last_error.is_none());
        // Push cursor node before fallible path_bytes() to preserve cursor state on decode failure.
        self.nodes.push(cursor_node);
        self.node_path_index = 0;
        let node_path = node.path_bytes()?;
        self.inner_walk(node_path, block_hash, |chr| node.walk(chr))
    }

    /// Like [`walk_ref`](Self::walk_ref), but converts the result into a [`ReadTrieNodeCursorStep`].
    pub fn walk_ref_step(
        &mut self,
        node: &TrieNodeRef<'_>,
        block_hash: &T,
    ) -> ReadTrieNodeCursorStep {
        Self::step_from_walk_result(self.walk_ref(node, block_hash), node.is_leaf())
    }

    /// Like [`walk_parked`](Self::walk_parked), but converts the result into a [`ReadTrieNodeCursorStep`].
    pub fn walk_parked_step(
        &mut self,
        node: &TrieNodeRef<'_>,
        parked_handle: ParkedNodeHandle,
        block_hash: &T,
    ) -> ReadTrieNodeCursorStep {
        Self::step_from_walk_result(
            self.walk_parked(node, parked_handle, block_hash),
            node.is_leaf(),
        )
    }

    /// Replace the last cursor node entry with a [`Parked`](CursorNodeHandle::Parked) handle (after
    /// parking in the arena).
    pub fn promote_last_node_to_parked(&mut self, parked_handle: ParkedNodeHandle) {
        if let Some(last_node) = self.nodes.last_mut() {
            *last_node = TrieCursorNode::Handle(CursorNodeHandle::Parked(parked_handle));
        } else {
            panic!("Cursor has no last node to park");
        }
    }

    /// Replace the last-visited node and ptr within this trie. Used when doing a copy-on-write or
    /// promoting a node, so the cursor state accurately reflects the nodes and tries visited.
    #[inline]
    pub fn repair_retarget(&mut self, node: &TrieNodeType, ptr: &TriePtr, hash: &T) {
        // this can only be called if we failed to walk to a node (this method _should not_ be
        // called if we walked to a backptr).
        if Some(CursorError::ChrNotFound) != self.last_error
            && Some(CursorError::PathDiverged) != self.last_error
        {
            eprintln!("{:?}", &self.last_error);
            panic!();
        }

        self.nodes.pop();
        self.node_ptrs.pop();
        self.block_hashes.pop();

        self.nodes.push(TrieCursorNode::Materialized(node.clone()));
        self.node_ptrs.push(*ptr);
        self.block_hashes.push(hash.clone());

        self.last_error = None;
    }

    /// Record that a node was walked to by way of a back-pointer.
    /// next_node should be the node walked to.
    /// ptr is the ptr we'll be walking from, off of next_node.
    /// block_hash is the block where next_node came from.
    #[inline]
    pub fn repair_backptr_step_backptr(
        &mut self,
        next_node: &TrieNodeType,
        ptr: &TriePtr,
        block_hash: T,
    ) {
        // this can only be called if we walked to a backptr.
        // If it's anything else, we're in trouble.
        if Some(CursorError::ChrNotFound) == self.last_error
            || Some(CursorError::PathDiverged) == self.last_error
        {
            eprintln!("{:?}", &self.last_error);
            panic!();
        }

        trace!(
            "Cursor: repair_backptr_step_backptr ptr={:?} block_hash={:?} next_node={:?}",
            ptr,
            &block_hash,
            next_node
        );

        let backptr = TriePtr::new(set_backptr(ptr.id()), ptr.chr(), ptr.ptr()); // set_backptr() informs update_root_hash() to skip this node
        self.node_ptrs.push(backptr);
        self.block_hashes.push(block_hash);

        self.nodes
            .push(TrieCursorNode::Materialized(next_node.clone()));
    }

    #[inline]
    pub fn repair_backptr_step_backptr_deferred(&mut self, ptr: &TriePtr, block_hash: T) {
        if Some(CursorError::ChrNotFound) == self.last_error
            || Some(CursorError::PathDiverged) == self.last_error
        {
            eprintln!("{:?}", &self.last_error);
            panic!();
        }

        trace!(
            "Cursor: repair_backptr_step_backptr_deferred ptr={:?} block_hash={:?}",
            ptr,
            &block_hash
        );

        let backptr = TriePtr::new(set_backptr(ptr.id()), ptr.chr(), ptr.ptr());
        self.node_ptrs.push(backptr);
        self.block_hashes.push(block_hash.clone());
        self.nodes
            .push(TrieCursorNode::Handle(CursorNodeHandle::Persisted {
                ptr: *ptr,
                block_hash,
            }));
    }

    /// Record that we landed on a non-backptr from a backptr.
    /// ptr is a non-backptr that refers to the node we landed on.
    #[inline]
    pub fn repair_backptr_finish(&mut self, ptr: &TriePtr, block_hash: T) {
        // this can only be called if we walked to a backptr.
        // If it's anything else, we're in trouble.
        if Some(CursorError::ChrNotFound) == self.last_error
            || Some(CursorError::PathDiverged) == self.last_error
        {
            eprintln!("{:?}", &self.last_error);
            panic!();
        }
        assert!(!is_backptr(ptr.id()));

        trace!("Cursor: repair_backptr_finish ptr={ptr:?} block_hash={block_hash:?}");

        self.node_ptrs.push(*ptr);
        self.block_hashes.push(block_hash);

        self.last_error = None;
    }
}

impl PartialEq for TrieLeaf {
    fn eq(&self, other: &TrieLeaf) -> bool {
        self.path == other.path
            && match (&self.data, &other.data) {
                (Some(left), Some(right)) => left == right,
                (None, None) => self.extent == other.extent && self.inline == other.inline,
                _ => false,
            }
    }
}

impl TrieLeaf {
    /// Return an already available commitment; unresolved extents require a storage resolver.
    pub fn value(&self) -> Result<&MARFValue, Error> {
        self.data.as_ref().ok_or_else(|| {
            Error::CorruptionError("Leaf commitment requires extent resolution".into())
        })
    }

    /// Encode only consensus-relevant leaf bytes, excluding the physical value locator.
    pub fn write_commitment_bytes<W: Write>(&self, w: &mut W) -> Result<(), Error> {
        w.write_all(&[TrieNodeID::Leaf as u8])?;
        bits::write_path_to_bytes(&self.path, w)?;
        w.write_all(&self.value()?.0)?;
        Ok(())
    }

    pub fn new(path: &[u8], data: &[u8]) -> TrieLeaf {
        assert!(data.len() <= 40);
        let mut bytes = [0u8; 40];
        bytes.copy_from_slice(data);
        TrieLeaf {
            path: NodePath::from_slice(path).expect("node path exceeds 32 bytes"),
            data: Some(MARFValue(bytes)),
            extent: None,
            inline: None,
        }
    }

    pub fn from_value(path: &[u8], value: MARFValue) -> TrieLeaf {
        TrieLeaf {
            path: NodePath::from_slice(path).expect("node path exceeds 32 bytes"),
            data: Some(value),
            extent: None,
            inline: None,
        }
    }
}

impl fmt::Debug for TrieLeaf {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "TrieLeaf(path={} data={:?} extent={:?})",
            &to_hex(&self.path),
            &self.data,
            &self.extent
        )
    }
}

impl StacksMessageCodec for TrieLeaf {
    fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), codec_error> {
        // Wire format: 4-byte big-endian length prefix + path bytes (standard Stacks Vec<u8> codec)
        let path_slice = self.path.as_slice();
        (path_slice.len() as u32).consensus_serialize(fd)?;
        fd.write_all(path_slice)
            .map_err(|e| codec_error::SerializeError(format!("Failed to write path: {e:?}")))?;
        self.value()
            .map_err(|error| codec_error::SerializeError(error.to_string()))?
            .consensus_serialize(fd)
    }

    fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<TrieLeaf, codec_error> {
        // Read the 4-byte big-endian length prefix directly, then into NodePath — no temporary Vec.
        let path_len: u32 = read_next(fd)?;
        if path_len > 32 {
            return Err(codec_error::DeserializeError(format!(
                "TrieLeaf path length {} exceeds maximum of 32",
                path_len
            )));
        }
        let mut path = NodePath::default();
        path.read_from(path_len as u8, fd)
            .map_err(|e| codec_error::DeserializeError(format!("Failed to read path: {e:?}")))?;
        let data = read_next(fd)?;

        Ok(TrieLeaf {
            path,
            data: Some(data),
            extent: None,
            inline: None,
        })
    }
}

/// Trie node with four children
#[derive(Clone, PartialEq)]
pub struct TrieNode4 {
    pub path: NodePath,
    pub ptrs: [TriePtr; 4],
    pub meta: TrieNodeTransientMeta,
}

impl fmt::Debug for TrieNode4 {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt_trie_node("TrieNode4", &self.path, &self.ptrs, f)
    }
}

impl TrieNode4 {
    pub fn new(path: &[u8]) -> TrieNode4 {
        TrieNode4 {
            path: NodePath::from_slice(path).expect("node path exceeds 32 bytes"),
            ptrs: [TriePtr::default(); 4],
            meta: TrieNodeTransientMeta::default(),
        }
    }
}

/// Trie node with 16 children
#[derive(Clone, PartialEq)]
pub struct TrieNode16 {
    pub path: NodePath,
    pub ptrs: [TriePtr; 16],
    pub meta: TrieNodeTransientMeta,
}

impl fmt::Debug for TrieNode16 {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt_trie_node("TrieNode16", &self.path, &self.ptrs, f)
    }
}

impl TrieNode16 {
    pub fn new(path: &[u8]) -> TrieNode16 {
        TrieNode16 {
            path: NodePath::from_slice(path).expect("node path exceeds 32 bytes"),
            ptrs: [TriePtr::default(); 16],
            meta: TrieNodeTransientMeta::default(),
        }
    }

    /// Promote a Node4 to a Node16
    pub fn from_node4(node4: &TrieNode4) -> TrieNode16 {
        let mut ptrs = [TriePtr::default(); 16];
        ptrs[..4].copy_from_slice(&node4.ptrs[..4]);
        TrieNode16 {
            path: node4.path,
            ptrs,
            meta: TrieNodeTransientMeta::default(),
        }
    }
}

/// Trie node with 48 children
#[derive(Clone)]
pub struct TrieNode48 {
    pub path: NodePath,
    /// If indexes[i] is non-negative, then it is an index into ptrs.
    pub indexes: [i8; 256],
    pub ptrs: [TriePtr; 48],
    pub meta: TrieNodeTransientMeta,
}

impl fmt::Debug for TrieNode48 {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt_trie_node("TrieNode48", &self.path, &self.ptrs, f)
    }
}

impl PartialEq for TrieNode48 {
    fn eq(&self, other: &TrieNode48) -> bool {
        self.path == other.path && self.ptrs == other.ptrs && self.indexes == other.indexes
    }
}

impl TrieNode48 {
    pub fn new(path: &[u8]) -> TrieNode48 {
        TrieNode48 {
            path: NodePath::from_slice(path).expect("node path exceeds 32 bytes"),
            indexes: [-1; 256],
            ptrs: [TriePtr::default(); 48],
            meta: TrieNodeTransientMeta::default(),
        }
    }

    pub fn indexes(&self) -> &[i8; 256] {
        &self.indexes
    }

    fn validate_indexes(
        ptrs_slice: &[TriePtr; 48],
        indexes_slice: &[i8; 256],
    ) -> Result<(), Error> {
        // SAFETY: ptr.chr() is a u8, so it is always in bounds for the 256-entry index array.
        #[allow(clippy::indexing_slicing)]
        let all_ptrs_valid = ptrs_slice.iter().all(|ptr| {
            ptr.is_empty()
                || indexes_slice[ptr.chr() as usize] >= 0 && indexes_slice[ptr.chr() as usize] < 48
        });

        if !all_ptrs_valid {
            return Err(Error::CorruptionError(
                "Node48: corrupt index array: invalid index value".to_string(),
            ));
        }

        let all_indexes_valid = indexes_slice.iter().all(|index| {
            let Ok(index) = usize::try_from(*index) else {
                return true;
            };
            let Some(ptr) = ptrs_slice.get(index) else {
                return false;
            };
            !ptr.is_empty()
        });

        if !all_indexes_valid {
            return Err(Error::CorruptionError(
                "Node48: corrupt index array: index points to empty node".to_string(),
            ));
        }

        Ok(())
    }

    /// Promote a node16 to a node48
    // allow indexing: this function only indexes constant-size arrays
    // with constant-sized indexes
    #[allow(clippy::indexing_slicing)]
    pub fn from_node16(node16: &TrieNode16) -> TrieNode48 {
        let mut ptrs = [TriePtr::default(); 48];
        let mut indexes = [-1i8; 256];
        for i in 0..16 {
            ptrs[i] = node16.ptrs[i];
            indexes[ptrs[i].chr() as usize] = i as i8;
        }
        TrieNode48 {
            path: node16.path,
            indexes,
            ptrs,
            meta: TrieNodeTransientMeta::default(),
        }
    }
}

/// Trie node with 256 children
#[derive(Clone)]
pub struct TrieNode256 {
    pub path: NodePath,
    pub ptrs: [TriePtr; 256],
    pub meta: TrieNodeTransientMeta,
}

impl fmt::Debug for TrieNode256 {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt_trie_node("TrieNode256", &self.path, &self.ptrs, f)
    }
}

impl PartialEq for TrieNode256 {
    fn eq(&self, other: &TrieNode256) -> bool {
        self.path == other.path && self.ptrs == other.ptrs
    }
}

impl TrieNode256 {
    pub fn new(path: &[u8]) -> TrieNode256 {
        TrieNode256 {
            path: NodePath::from_slice(path).expect("node path exceeds 32 bytes"),
            ptrs: [TriePtr::default(); 256],
            meta: TrieNodeTransientMeta::default(),
        }
    }

    // allow indexing because this function operates on
    //  fixed size arrays (256 array can always be indexed by u8)
    #[allow(clippy::indexing_slicing)]
    pub fn from_node4(node4: &TrieNode4) -> TrieNode256 {
        let mut ptrs = [TriePtr::default(); 256];
        for node4_ptr in node4.ptrs.iter() {
            let c = node4_ptr.chr();
            ptrs[c as usize] = *node4_ptr;
        }
        TrieNode256 {
            path: node4.path,
            ptrs,
            meta: TrieNodeTransientMeta::default(),
        }
    }

    /// Promote a node48 to a node256
    // allow indexing because this function operates on
    //  fixed size arrays (256 array can always be indexed by u8)
    #[allow(clippy::indexing_slicing)]
    pub fn from_node48(node48: &TrieNode48) -> TrieNode256 {
        let mut ptrs = [TriePtr::default(); 256];
        for node48_ptr in node48.ptrs.iter() {
            let c = node48_ptr.chr();
            ptrs[c as usize] = *node48_ptr;
        }
        TrieNode256 {
            path: node48.path,
            ptrs,
            meta: TrieNodeTransientMeta::default(),
        }
    }
}

/// This is a non-consensus "patch node" that applies a diff atop a base node.  There can be up to
/// MAX_PATCH_DEPTH patch nodes applied atop the base node.
#[derive(Clone, PartialEq)]
pub struct TrieNodePatch {
    /// Pointer to the node we're patching (will always be a back-block ptr)
    pub ptr: TriePtr,
    /// Field of ptrs to insert atop the base node
    pub ptr_diff: Vec<TriePtr>,
}

impl TrieNodePatch {
    pub const MAX_BODY_BYTE_LEN: usize =
        1 + TriePtr::max_encoded_size() + 1 + PATCH_DIFF_MAX_PTRS * TriePtr::max_encoded_size();
}

impl fmt::Debug for TrieNodePatch {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "TrieNodePatch(ptr={} ptr_diff={})",
            &ptrs_fmt(&[self.ptr]),
            ptrs_fmt(&self.ptr_diff)
        )
    }
}

impl StacksMessageCodec for TrieNodePatch {
    /// Serializes this [`TrieNodePatch`] to the given writer, with the following format:
    ///
    /// 0    1        1+P      2+P              2+P+N
    /// |----|--------|----------|----------------|
    ///   id     ptr    diff len     ptr diffs
    ///   (1)    (P)       (1)          (N)
    ///
    /// where:
    /// - `id` is [`TrieNodeID::Patch`]
    /// - `ptr` is a compressed [`TriePtr`]
    /// - `diff len` is the number of diffs, serialized as `len - 1`
    /// - `ptr diffs` are the patch diffs written in compressed format
    ///
    /// # Invariants
    ///
    /// The number of diffs must be in the range `1..=256`. A patch is valid only
    /// if it contains at least one diff (see the factory methods
    /// [`TrieNodePatch::try_from_nodetype`] and [`TrieNodePatch::try_from_patch`]).
    ///
    /// To fit in a `u8`, the diff count is normalized to `len - 1` before
    /// serialization.
    ///
    /// # Errors
    ///
    /// Returns `Err(codec_error::SerializeError)` if:
    /// * Writing to `fd` fails.
    /// * `ptr` fails to serialize.
    /// * Any pointer in `ptr diffs` fails to serialize.
    /// * The diff count is `0` or greater than `256`.
    fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), codec_error> {
        write_next(fd, &(TrieNodeID::Patch as u8))?;
        self.ptr
            .write_bytes_compressed(fd)
            .map_err(|e| codec_error::SerializeError(format!("Failed to serialize .ptr: {e:?}")))?;

        let num_ptrs = self.ptr_diff.len();
        if num_ptrs == 0 || num_ptrs > 256 {
            return Err(codec_error::SerializeError(format!(
                "Cannot serialize TrieNodePatch with invalid ptrs len {num_ptrs} (expected 1..=256)"
            )));
        }
        // normalize num_ptrs to range [0, 255] to fit in u8
        let num_ptrs_norm = num_ptrs.checked_sub(1).expect("infallible");
        let num_ptrs_u8 = u8::try_from(num_ptrs_norm).expect("infallible");
        write_next(fd, &num_ptrs_u8).map_err(|e| {
            codec_error::SerializeError(format!("Failed to serialize .ptr_diff.len(): {e:?}"))
        })?;

        for ptr in self.ptr_diff.iter() {
            ptr.write_bytes_compressed(fd).map_err(|e| {
                codec_error::SerializeError(format!("Failed to serialize ptr in .ptr_diff: {e:?}"))
            })?;
        }
        Ok(())
    }

    /// Deserializes a [`TrieNodePatch`] from the given reader.
    ///
    /// This method expects the byte stream to be in the exact format produced by
    /// [`TrieNodePatch::consensus_serialize`] (see that method for the detailed
    /// wire format description)
    ///
    /// During deserialization, the stored diff length is de-normalized by
    /// adding `1`, reversing the `len - 1` normalization applied during
    /// serialization.
    ///
    /// # Errors
    ///
    /// Returns `Err(codec_error::DeserializeError)` if:
    /// * The node identifier does not match [`TrieNodeID::Patch`].
    /// * Reading from `fd` fails.
    /// * The pointer or any pointer diff fails to deserialize.
    fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<Self, codec_error> {
        let id: u8 = read_next(fd)?;
        if id != TrieNodeID::Patch as u8 {
            return Err(codec_error::DeserializeError(
                "Did not read a TrieNodeID::Patch".to_string(),
            ));
        }

        let ptr = TriePtr::read_bytes_compressed(fd)?;
        let num_ptrs_u8: u8 = read_next(fd)?;
        let num_ptrs_norm = usize::try_from(num_ptrs_u8).expect("infallible");
        // denormalize num_ptrs to range [1, 256] (reversing the -1 introduced during serialization)
        let num_ptrs = num_ptrs_norm.checked_add(1).expect("infallible");

        let mut ptr_diff: Vec<TriePtr> = Vec::with_capacity(num_ptrs);
        for _ in 0..num_ptrs {
            ptr_diff.push(TriePtr::read_bytes_compressed(fd)?);
        }
        Ok(Self { ptr, ptr_diff })
    }
}

/// Turn each non-empty, non-backptr in `ptrs` into a backptr.
/// If `back_block` is already non-zero (squash annotation), it is preserved;
/// otherwise it is set to `child_block_id`.
pub fn node_copy_update_ptrs(ptrs: &mut [TriePtr], child_block_id: u32) {
    for pointer in ptrs.iter_mut() {
        // if the node is empty, do nothing, if it's a back pointer,
        if pointer.id() == TrieNodeID::Empty as u8 || is_backptr(pointer.id()) {
            continue;
        }
        if pointer.back_block == 0 {
            pointer.back_block = child_block_id;
        }
        pointer.id = set_backptr(pointer.id());
    }
}

/// Given the current block ID, convert every backptr pointer whose back_block is equal to
/// `cur_block_id` to a normal pointer.  This is used when applying patches.
fn node_normalize_ptrs(ptrs: &mut [TriePtr], cur_block_id: u32) {
    for ptr in ptrs.iter_mut() {
        if is_backptr(ptr.id) && ptr.back_block == cur_block_id {
            // normalize
            ptr.id = clear_backptr(ptr.id);
            ptr.back_block = 0;
        }
    }
}

impl TrieNodePatch {
    /// Compute the difference between `old_ptrs` and `new_ptrs`. In particular, if a pointer in
    /// `new_ptrs` is in the same block as indicated by `old_node_ptr`, this code will first need to
    /// normalize it (i.e. convert it into a non-backpointer) in order to compare it against the
    /// corresponding pointer in `old_ptrs` (which might have that very same pointer, but not yet
    /// made into a backptr by a COW)
    fn make_ptr_diff(
        old_node_ptr: &TriePtr,
        old_ptrs: &[TriePtr],
        new_ptrs: &[TriePtr],
    ) -> Vec<TriePtr> {
        let mut ret = Vec::with_capacity(new_ptrs.len());
        let mut mapped: [Option<&TriePtr>; 256] = [None; 256];
        for old_ptr in old_ptrs.iter() {
            // SAFETY: chr() is a u8, so it's in range [0, 256)
            if !old_ptr.is_empty() {
                let mapped_ptr = mapped
                    .get_mut(old_ptr.chr() as usize)
                    .expect("infallible: mapped has 256 elements and .chr() is a u8");
                *mapped_ptr = Some(old_ptr);
            }
        }

        for new_ptr in new_ptrs.iter() {
            if new_ptr.is_empty() {
                continue;
            }
            // SAFETY: chr() is a u8, so it's in range [0, 256)
            if let Some(old_ptr) = *mapped
                .get(new_ptr.chr() as usize)
                .expect("infallible: mapped has 256 elements and .chr() is a u8")
            {
                if !is_backptr(old_ptr.id())
                    && is_backptr(new_ptr.id())
                    && new_ptr.back_block == old_node_ptr.back_block
                {
                    // new_ptr may be the backptr-ified version of old_ptr
                    let mut normalized_new_ptr =
                        TriePtr::new(clear_ctrl_bits(new_ptr.id()), new_ptr.chr(), new_ptr.ptr());
                    normalized_new_ptr.back_block = 0;
                    if *old_ptr != normalized_new_ptr {
                        trace!(
                            "new overwritten ptr (old_ptr != normalized_new_ptr): {:?} != {:?}",
                            &normalized_new_ptr,
                            old_ptr
                        );
                        ret.push(*new_ptr);
                    }
                } else {
                    if old_ptr != new_ptr {
                        trace!(
                            "new overwritten ptr (old_ptr != new_ptr): {:?} != {:?}",
                            &new_ptr,
                            old_ptr
                        );
                        ret.push(*new_ptr);
                    } else if !is_backptr(new_ptr.id()) {
                        trace!(
                            "new overwritten ptr (new_ptr not backptr): {:?} != {:?}",
                            &new_ptr,
                            old_ptr
                        );
                        ret.push(*new_ptr);
                    }
                }
            } else {
                ret.push(*new_ptr);
            }
        }
        debug_assert!(
            ret.len() <= new_ptrs.len(),
            "TrieNodePatch has {} diffs for {} pointer slots",
            ret.len(),
            new_ptrs.len()
        );
        ret
    }

    /// Test-only wrapper exposing the private [`TrieNodePatch::make_ptr_diff`] for unit testing
    #[cfg(test)]
    pub fn make_ptr_diff_for_test(
        old_node_ptr: &TriePtr,
        old_ptrs: &[TriePtr],
        new_ptrs: &[TriePtr],
    ) -> Vec<TriePtr> {
        TrieNodePatch::make_ptr_diff(old_node_ptr, old_ptrs, new_ptrs)
    }

    /// Create a patch by diffing the pointer arrays of two nodes.
    pub fn from_nodes(old_node_ptr: TriePtr, old_ptrs: &[TriePtr], new_ptrs: &[TriePtr]) -> Self {
        Self {
            ptr: old_node_ptr,
            ptr_diff: Self::make_ptr_diff(&old_node_ptr, old_ptrs, new_ptrs),
        }
    }

    /// Create a patch from one nodetype to another. Returns `None` if the types differ, either
    /// is a leaf, or the resulting patch has no diffs.
    pub fn try_from_nodetype(
        old_node_ptr: TriePtr,
        old_node: &TrieNodeType,
        new_node: &TrieNodeType,
    ) -> Option<Self> {
        if clear_ctrl_bits(old_node.id()) != clear_ctrl_bits(new_node.id()) {
            trace!("Cannot produce TrieNodePatch: old node and new node are not the same type!");
            return None;
        }
        if old_node.is_leaf() {
            trace!("Cannot produce TrieNodePatch: old node and new node are type leaf!");
            return None;
        }
        let patch = Self::from_nodes(old_node_ptr, old_node.ptrs(), new_node.ptrs());
        if patch.ptr_diff.is_empty() {
            trace!("Cannot produce TrieNodePatch: patch has no diffs!");
            return None;
        }
        Some(patch)
    }

    /// Create a patch from a borrowed node reference to another node. If they're not the same
    /// nodetype, then this function returns None.
    pub fn try_from_noderef(
        old_node_ptr: TriePtr,
        old_node: TrieNodeRef<'_>,
        new_node: &TrieNodeType,
    ) -> Option<Self> {
        if clear_ctrl_bits(old_node.id()) != clear_ctrl_bits(new_node.id()) {
            trace!("Cannot produce TrieNodePatch: old node and new node are not the same type!");
            return None;
        }

        if old_node.is_leaf() {
            trace!("Cannot produce TrieNodePatch: old node and new node are type leaf!");
            return None;
        }

        let patch = Self {
            ptr: old_node_ptr,
            ptr_diff: Self::make_ptr_diff(&old_node_ptr, old_node.ptrs(), new_node.ptrs()),
        };

        if patch.ptr_diff.is_empty() {
            trace!("Cannot produce TrieNodePatch: patch has no diffs!");
            return None;
        }

        Some(patch)
    }

    /// Create a patch from one patch to a node
    pub fn try_from_patch(
        old_patch_ptr: TriePtr,
        old_patch: &TrieNodePatch,
        new_node: &TrieNodeType,
    ) -> Option<Self> {
        if clear_ctrl_bits(old_patch.ptr.id) != clear_ctrl_bits(new_node.id()) {
            trace!("Cannot produce TrieNodePatch: old node and new node are not the same type!");
            return None;
        }

        let ptr_diff = Self::make_ptr_diff(&old_patch_ptr, &old_patch.ptr_diff, new_node.ptrs());
        let patch = Self {
            ptr: old_patch_ptr,
            ptr_diff,
        };
        if patch.ptr_diff.is_empty() {
            trace!("Cannot produce TrieNodePatch: patch has no diffs!");
            return None;
        }
        return Some(patch);
    }

    /// Apply this patch to a node, given the node, block ID where the patch was found, and block
    /// ID where the node was written.
    pub fn apply_to<N: TrieNode + fmt::Debug>(
        &self,
        old_node: &mut N,
        patch_block_id: u32,
        cur_block_id: u32,
    ) -> bool {
        trace!("Apply patch {self:?} read from block ID {patch_block_id} to {old_node:?}");
        node_copy_update_ptrs(old_node.ptrs_mut(), self.ptr.back_block);
        for ptr in self.ptr_diff.iter() {
            if !old_node.insert(ptr) {
                return false;
            }
        }
        node_copy_update_ptrs(old_node.ptrs_mut(), patch_block_id);
        node_normalize_ptrs(old_node.ptrs_mut(), cur_block_id);
        trace!("Patched up to {old_node:?}");
        true
    }

    /// Compute the size of the TriePatchNode. Its pointers are always compressed.
    #[inline]
    pub fn size(&self) -> usize {
        // ID
        let mut sz = 1;
        // previous node ptr
        sz += self.ptr.compressed_size();
        // length prefix
        sz += 1;
        // ptr_diff
        for ptr in self.ptr_diff.iter() {
            sz += ptr.compressed_size();
        }
        sz
    }
    /// Load a TrieNodePatch from a byte slice.
    /// Returns the number of bytes consumed on success.
    pub fn load_from_slice(&mut self, bytes: &[u8]) -> Result<usize, Error> {
        let (&marker, body) = bytes
            .split_first()
            .ok_or_else(|| Error::CorruptionError("Missing patch marker".into()))?;
        Ok(1 + self.load_from_parts(marker, body)?)
    }

    /// Decode a patch payload following a separately read marker and hash.
    pub fn load_from_parts(&mut self, id: u8, bytes: &[u8]) -> Result<usize, Error> {
        if id != TrieNodeID::Patch as u8 {
            return Err(Error::CorruptionError(
                "Did not read a TrieNodeID::Patch".to_string(),
            ));
        }

        let mut offset = 0usize;
        let (ptr, ptr_consumed) =
            TriePtr::from_slice_compressed(bytes.get(offset..).ok_or_else(|| {
                Error::CorruptionError("Patch ptr starts past encoded node bytes".to_string())
            })?)?;
        self.ptr = ptr;
        offset = offset
            .checked_add(ptr_consumed)
            .ok_or(Error::OverflowError)?;

        let num_ptrs_norm = *bytes
            .get(offset)
            .ok_or_else(|| Error::CorruptionError("Failed to read patch diff length".to_string()))?
            as usize;
        offset = offset.checked_add(1).ok_or(Error::OverflowError)?;
        let num_ptrs = num_ptrs_norm.checked_add(1).ok_or(Error::OverflowError)?;

        self.ptr_diff.clear();
        if self.ptr_diff.capacity() < num_ptrs {
            self.ptr_diff.reserve(num_ptrs);
        }
        for _ in 0..num_ptrs {
            let (ptr, ptr_consumed) =
                TriePtr::from_slice_compressed(bytes.get(offset..).ok_or_else(|| {
                    Error::CorruptionError(
                        "Patch diff ptr starts past encoded node bytes".to_string(),
                    )
                })?)?;
            self.ptr_diff.push(ptr);
            offset = offset
                .checked_add(ptr_consumed)
                .ok_or(Error::OverflowError)?;
        }
        Ok(offset)
    }

    pub fn from_slice(bytes: &[u8]) -> Result<(Self, usize), Error> {
        let mut patch = Self {
            ptr: TriePtr::default(),
            ptr_diff: Vec::new(),
        };
        let consumed = patch.load_from_slice(bytes)?;
        Ok((patch, consumed))
    }
}

impl TrieNode for TrieNode4 {
    const MAX_BODY_BYTE_LEN: usize =
        NODE_PTR_ID_BYTE_LEN + 4 * TriePtr::max_encoded_size() + NODE_PATH_MAX_BYTE_LEN;

    fn id(&self) -> u8 {
        TrieNodeID::Node4 as u8
    }

    fn empty() -> TrieNode4 {
        TrieNode4 {
            path: NodePath::default(),
            ptrs: [TriePtr::default(); 4],
            meta: TrieNodeTransientMeta::default(),
        }
    }

    fn walk(&self, chr: u8) -> Option<TriePtr> {
        for ptr in self.ptrs.iter() {
            if !ptr.is_empty() && ptr.chr() == chr {
                return Some(*ptr);
            }
        }
        None
    }

    fn load_from_parts(&mut self, marker: u8, bytes: &[u8]) -> Result<usize, Error> {
        let (_, ptrs_consumed) =
            bits::ptrs_from_parts_into(TrieNodeID::Node4 as u8, marker, bytes, &mut self.ptrs)?;
        let remaining = bytes.get(ptrs_consumed..).ok_or_else(|| {
            Error::CorruptionError("Node4: path starts past encoded node bytes".to_string())
        })?;
        let path_consumed = bits::path_from_bytes_slice_into(remaining, &mut self.path)?;
        self.reset_transient_meta();
        Ok(ptrs_consumed + path_consumed)
    }

    fn insert(&mut self, ptr: &TriePtr) -> bool {
        if self.replace(ptr) {
            return true;
        }

        for slot in self.ptrs.iter_mut() {
            if slot.is_empty() {
                *slot = *ptr;
                return true;
            }
        }
        false
    }

    fn replace(&mut self, ptr: &TriePtr) -> bool {
        for slot in self.ptrs.iter_mut() {
            if !slot.is_empty() && slot.chr() == ptr.chr() {
                *slot = *ptr;
                return true;
            }
        }
        false
    }

    fn ptrs(&self) -> &[TriePtr] {
        &self.ptrs
    }

    fn ptrs_mut(&mut self) -> &mut [TriePtr] {
        &mut self.ptrs
    }

    fn meta(&self) -> Option<&TrieNodeTransientMeta> {
        Some(&self.meta)
    }

    fn meta_mut(&mut self) -> Option<&mut TrieNodeTransientMeta> {
        Some(&mut self.meta)
    }

    fn path(&self) -> &NodePath {
        &self.path
    }

    fn as_trie_node_type(&self) -> TrieNodeType {
        TrieNodeType::Node4(self.clone())
    }
}

impl TrieNode for TrieNode16 {
    const MAX_BODY_BYTE_LEN: usize =
        NODE_PTR_ID_BYTE_LEN + 16 * TriePtr::max_encoded_size() + NODE_PATH_MAX_BYTE_LEN;

    fn id(&self) -> u8 {
        TrieNodeID::Node16 as u8
    }

    fn empty() -> TrieNode16 {
        TrieNode16 {
            path: NodePath::default(),
            ptrs: [TriePtr::default(); 16],
            meta: TrieNodeTransientMeta::default(),
        }
    }

    fn walk(&self, chr: u8) -> Option<TriePtr> {
        for ptr in self.ptrs.iter() {
            if !ptr.is_empty() && ptr.chr() == chr {
                return Some(*ptr);
            }
        }
        None
    }

    fn load_from_parts(&mut self, marker: u8, bytes: &[u8]) -> Result<usize, Error> {
        let (_, ptrs_consumed) =
            bits::ptrs_from_parts_into(TrieNodeID::Node16 as u8, marker, bytes, &mut self.ptrs)?;
        let remaining = bytes.get(ptrs_consumed..).ok_or_else(|| {
            Error::CorruptionError("Node16: path starts past encoded node bytes".to_string())
        })?;
        let path_consumed = bits::path_from_bytes_slice_into(remaining, &mut self.path)?;
        self.reset_transient_meta();
        Ok(ptrs_consumed + path_consumed)
    }

    fn insert(&mut self, ptr: &TriePtr) -> bool {
        if self.replace(ptr) {
            return true;
        }

        for slot in self.ptrs.iter_mut() {
            if slot.is_empty() {
                *slot = *ptr;
                return true;
            }
        }
        false
    }

    fn replace(&mut self, ptr: &TriePtr) -> bool {
        for slot in self.ptrs.iter_mut() {
            if !slot.is_empty() && slot.chr() == ptr.chr() {
                *slot = *ptr;
                return true;
            }
        }
        false
    }

    fn ptrs(&self) -> &[TriePtr] {
        &self.ptrs
    }

    fn ptrs_mut(&mut self) -> &mut [TriePtr] {
        &mut self.ptrs
    }

    fn meta(&self) -> Option<&TrieNodeTransientMeta> {
        Some(&self.meta)
    }

    fn meta_mut(&mut self) -> Option<&mut TrieNodeTransientMeta> {
        Some(&mut self.meta)
    }

    fn path(&self) -> &NodePath {
        &self.path
    }

    fn as_trie_node_type(&self) -> TrieNodeType {
        TrieNodeType::Node16(self.clone())
    }
}

impl TrieNode for TrieNode48 {
    const MAX_BODY_BYTE_LEN: usize = NODE_PTR_ID_BYTE_LEN
        + 48 * TriePtr::max_encoded_size()
        + NODE48_INDEX_BYTE_LEN
        + NODE_PATH_MAX_BYTE_LEN;

    fn id(&self) -> u8 {
        TrieNodeID::Node48 as u8
    }

    fn empty() -> TrieNode48 {
        TrieNode48 {
            path: NodePath::default(),
            indexes: [-1; 256],
            ptrs: [TriePtr::default(); 48],
            meta: TrieNodeTransientMeta::default(),
        }
    }

    // allow indexing here because self.indexes is an array of
    // 256, so it can always return a u8
    #[allow(clippy::indexing_slicing)]
    fn walk(&self, chr: u8) -> Option<TriePtr> {
        let idx = self.indexes[chr as usize];
        let ptr = self.ptrs.get(usize::try_from(idx).ok()?)?;
        if ptr.is_empty() {
            return None;
        }
        Some(*ptr)
    }

    fn write_bytes<W: Write>(&self, w: &mut W) -> Result<(), Error> {
        w.write_all(&[self.id()])?;
        write_ptrs_to_bytes(self.ptrs(), w)?;

        for i in self.indexes.iter() {
            w.write_all(&[*i as u8])?;
        }

        bits::write_path_to_bytes(self.path().as_slice(), w)
    }

    fn write_bytes_compressed<W: Write>(&self, w: &mut W) -> Result<(), Error> {
        w.write_all(&[set_compressed(self.id())])?;
        write_ptrs_to_bytes_compressed(self.id(), self.ptrs(), w)?;

        for i in self.indexes.iter() {
            w.write_all(&[*i as u8])?;
        }

        bits::write_path_to_bytes(self.path().as_slice(), w)
    }

    fn byte_len(&self) -> usize {
        bits::get_ptrs_byte_len(&self.ptrs) + 256 + bits::get_path_byte_len(&self.path)
    }

    fn byte_len_compressed(&self) -> usize {
        bits::get_ptrs_byte_len_compressed(self.id(), &self.ptrs)
            + 256
            + bits::get_path_byte_len(&self.path)
    }

    fn load_from_parts(&mut self, marker: u8, bytes: &[u8]) -> Result<usize, Error> {
        let (_, ptrs_consumed) =
            bits::ptrs_from_parts_into(TrieNodeID::Node48 as u8, marker, bytes, &mut self.ptrs)?;

        let indexes_bytes = bytes
            .get(ptrs_consumed..ptrs_consumed + 256)
            .ok_or_else(|| {
                Error::CorruptionError("I/O error reading TrieNode48 indexes".to_string())
            })?;
        let mut indexes = [0i8; 256];
        for (dst, src) in indexes.iter_mut().zip(indexes_bytes.iter()) {
            *dst = *src as i8;
        }

        let path_offset = ptrs_consumed + 256;
        let remaining = bytes.get(path_offset..).ok_or_else(|| {
            Error::CorruptionError("Node48: path starts past encoded node bytes".to_string())
        })?;
        let path_consumed = bits::path_from_bytes_slice_into(remaining, &mut self.path)?;

        Self::validate_indexes(&self.ptrs, &indexes)?;

        self.indexes = indexes;
        self.reset_transient_meta();
        Ok(path_offset + path_consumed)
    }

    #[allow(clippy::indexing_slicing)]
    fn insert(&mut self, ptr: &TriePtr) -> bool {
        if self.replace(ptr) {
            return true;
        }

        let c = ptr.chr();
        for i in 0..48 {
            if self.ptrs[i].is_empty() {
                self.indexes[c as usize] = i as i8;
                self.ptrs[i] = *ptr;
                return true;
            }
        }
        false
    }

    #[allow(clippy::indexing_slicing)]
    fn replace(&mut self, ptr: &TriePtr) -> bool {
        let i = self.indexes[ptr.chr() as usize];
        if i >= 0 {
            self.ptrs[i as usize] = *ptr;
            true
        } else {
            false
        }
    }

    fn ptrs(&self) -> &[TriePtr] {
        &self.ptrs
    }

    fn ptrs_mut(&mut self) -> &mut [TriePtr] {
        &mut self.ptrs
    }

    fn meta(&self) -> Option<&TrieNodeTransientMeta> {
        Some(&self.meta)
    }

    fn meta_mut(&mut self) -> Option<&mut TrieNodeTransientMeta> {
        Some(&mut self.meta)
    }

    fn path(&self) -> &NodePath {
        &self.path
    }

    fn as_trie_node_type(&self) -> TrieNodeType {
        TrieNodeType::Node48(Box::new(self.clone()))
    }
}

impl TrieNode for TrieNode256 {
    const MAX_BODY_BYTE_LEN: usize =
        NODE_PTR_ID_BYTE_LEN + 256 * TriePtr::max_encoded_size() + NODE_PATH_MAX_BYTE_LEN;

    fn id(&self) -> u8 {
        TrieNodeID::Node256 as u8
    }

    fn empty() -> TrieNode256 {
        TrieNode256 {
            path: NodePath::default(),
            ptrs: [TriePtr::default(); 256],
            meta: TrieNodeTransientMeta::default(),
        }
    }

    fn walk(&self, chr: u8) -> Option<TriePtr> {
        let ptr = self.ptrs.get(chr as usize)?;
        if ptr.is_empty() {
            return None;
        }
        Some(*ptr)
    }

    fn load_from_parts(&mut self, marker: u8, bytes: &[u8]) -> Result<usize, Error> {
        let (_, ptrs_consumed) =
            bits::ptrs_from_parts_into(TrieNodeID::Node256 as u8, marker, bytes, &mut self.ptrs)?;
        let remaining = bytes.get(ptrs_consumed..).ok_or_else(|| {
            Error::CorruptionError("Node256: path starts past encoded node bytes".to_string())
        })?;
        let path_consumed = bits::path_from_bytes_slice_into(remaining, &mut self.path)?;
        self.reset_transient_meta();
        Ok(ptrs_consumed + path_consumed)
    }

    #[allow(clippy::indexing_slicing)]
    fn insert(&mut self, ptr: &TriePtr) -> bool {
        if self.replace(ptr) {
            return true;
        }
        let c = ptr.chr() as usize;
        self.ptrs[c] = *ptr;
        true
    }

    #[allow(clippy::indexing_slicing)]
    fn replace(&mut self, ptr: &TriePtr) -> bool {
        let c = ptr.chr() as usize;
        if !self.ptrs[c].is_empty() && self.ptrs[c].chr() == ptr.chr() {
            self.ptrs[c] = *ptr;
            true
        } else {
            false
        }
    }

    fn ptrs(&self) -> &[TriePtr] {
        &self.ptrs
    }

    fn ptrs_mut(&mut self) -> &mut [TriePtr] {
        &mut self.ptrs
    }

    fn meta(&self) -> Option<&TrieNodeTransientMeta> {
        Some(&self.meta)
    }

    fn meta_mut(&mut self) -> Option<&mut TrieNodeTransientMeta> {
        Some(&mut self.meta)
    }

    fn path(&self) -> &NodePath {
        &self.path
    }

    fn as_trie_node_type(&self) -> TrieNodeType {
        TrieNodeType::Node256(Box::new(self.clone()))
    }
}

impl TrieNode for TrieLeaf {
    const MAX_BODY_BYTE_LEN: usize =
        1 + NODE_PATH_MAX_BYTE_LEN + MARF_VALUE_ENCODED_SIZE as usize + ValueExtent::ENCODED_SIZE;

    fn id(&self) -> u8 {
        TrieNodeID::Leaf as u8
    }

    fn empty() -> TrieLeaf {
        TrieLeaf::new(&[], &[0u8; 40])
    }

    fn walk(&self, _chr: u8) -> Option<TriePtr> {
        None
    }

    fn write_bytes<W: Write>(&self, w: &mut W) -> Result<(), Error> {
        if self.inline.is_some() {
            return Err(Error::CorruptionError(
                "Inline leaves require format version 3".into(),
            ));
        }
        // 0x10 is physical header metadata. Leaf pointers retain the ordinary Leaf ID.
        let id = self.id() | if self.extent.is_some() { 0x10 } else { 0 };
        w.write_all(&[id])?;
        bits::write_path_to_bytes(&self.path, w)?;
        w.write_all(&self.value()?.0)?;
        if let Some(extent) = self.extent {
            extent.write_to(w)?;
        }
        Ok(())
    }

    fn write_bytes_compressed<W: Write>(&self, w: &mut W) -> Result<(), Error> {
        self.write_bytes(w)
    }

    fn byte_len(&self) -> usize {
        1 + bits::get_path_byte_len(&self.path)
            + MARF_VALUE_ENCODED_SIZE as usize
            + if self.extent.is_some() {
                ValueExtent::ENCODED_SIZE
            } else {
                0
            }
    }

    fn byte_len_compressed(&self) -> usize {
        self.byte_len()
    }

    fn load_from_parts(&mut self, marker: u8, body: &[u8]) -> Result<usize, Error> {
        let physical_id = clear_ctrl_bits(marker);
        if !is_leaf_id(marker) {
            return Err(Error::CorruptionError(format!(
                "Leaf: bad ID 0x{marker:02x}"
            )));
        }
        if (physical_id == TrieNodeID::ValueLeaf as u8
            || physical_id == TrieNodeID::InlineLeaf as u8)
            && marker != physical_id
        {
            return Err(Error::CorruptionError(
                "Unsupported value-leaf flags".into(),
            ));
        }
        if physical_id == TrieNodeID::RawLeaf as u8 {
            if marker != physical_id {
                return Err(Error::CorruptionError("Unsupported raw-leaf flags".into()));
            }
            return super::raw_leaf::load(self, body);
        }
        let path_consumed = bits::path_from_bytes_slice_into(body, &mut self.path)?;
        let remaining = body.get(path_consumed..).ok_or(Error::OverflowError)?;
        self.inline = None;
        if physical_id == TrieNodeID::InlineLeaf as u8 {
            let (inline, consumed) = InlineValue::decode(remaining)?;
            self.data = None;
            self.extent = None;
            self.inline = Some(inline);
            return Ok(path_consumed + consumed);
        }
        if physical_id == TrieNodeID::ValueLeaf as u8 {
            self.data = None;
            self.extent = Some(ValueExtent::read_from(&mut &remaining[..])?);
            return Ok(path_consumed + ValueExtent::ENCODED_SIZE);
        }
        let (data_bytes, remaining) = remaining
            .split_at_checked(MARF_VALUE_ENCODED_SIZE as usize)
            .ok_or_else(|| Error::CorruptionError("Leaf: truncated MARF value".into()))?;
        self.data = Some(MARFValue(
            data_bytes.try_into().expect("checked value length"),
        ));
        self.extent = if marker & 0x10 != 0 {
            Some(ValueExtent::read_from(&mut &remaining[..])?)
        } else {
            None
        };
        Ok(path_consumed
            + MARF_VALUE_ENCODED_SIZE as usize
            + self.extent.map_or(0, |_| ValueExtent::ENCODED_SIZE))
    }

    fn insert(&mut self, _ptr: &TriePtr) -> bool {
        panic!("can't insert into a leaf");
    }

    fn replace(&mut self, _ptr: &TriePtr) -> bool {
        panic!("can't replace in a leaf");
    }

    fn ptrs(&self) -> &[TriePtr] {
        &[]
    }

    fn ptrs_mut(&mut self) -> &mut [TriePtr] {
        &mut []
    }

    fn meta(&self) -> Option<&TrieNodeTransientMeta> {
        None
    }

    fn meta_mut(&mut self) -> Option<&mut TrieNodeTransientMeta> {
        None
    }

    fn path(&self) -> &NodePath {
        &self.path
    }

    fn as_trie_node_type(&self) -> TrieNodeType {
        TrieNodeType::Leaf(self.clone())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum TrieNodeType {
    Node4(TrieNode4),
    Node16(TrieNode16),
    Node48(Box<TrieNode48>),
    Node256(Box<TrieNode256>),
    Leaf(TrieLeaf),
}

#[derive(Debug, Clone, Copy)]
pub struct TrieLeafRef<'a> {
    pub path: &'a [u8],
    pub data: Option<&'a MARFValue>,
    /// Physical value locator retained across borrowed node reads.
    pub extent: Option<ValueExtent>,
    /// Inline bytes borrowed from a retained leaf owner.
    pub inline: Option<&'a InlineValue>,
}

impl TrieLeafRef<'_> {
    /// Retain both logical data and the physical locator when materializing a borrowed leaf.
    pub fn to_owned(&self) -> TrieLeaf {
        TrieLeaf {
            path: NodePath::from_slice(self.path).expect("validated leaf path"),
            data: self.data.cloned(),
            extent: self.extent,
            inline: self.inline.cloned(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum TrieNodeRef<'a> {
    Node4 {
        path: &'a [u8],
        ptrs: &'a [TriePtr; 4],
    },
    Node16 {
        path: &'a [u8],
        ptrs: &'a [TriePtr; 16],
    },
    Node48 {
        path: &'a [u8],
        indexes: &'a [i8; 256],
        ptrs: &'a [TriePtr; 48],
    },
    Node256 {
        path: &'a [u8],
        ptrs: &'a [TriePtr; 256],
    },
    Leaf(TrieLeafRef<'a>),
}

impl<'a> TrieNodeRef<'a> {
    pub fn is_leaf(&self) -> bool {
        matches!(self, Self::Leaf(_))
    }

    pub fn is_node256(&self) -> bool {
        matches!(self, Self::Node256 { .. })
    }

    pub fn id(&self) -> u8 {
        match self {
            Self::Node4 { .. } => TrieNodeID::Node4 as u8,
            Self::Node16 { .. } => TrieNodeID::Node16 as u8,
            Self::Node48 { .. } => TrieNodeID::Node48 as u8,
            Self::Node256 { .. } => TrieNodeID::Node256 as u8,
            Self::Leaf(_) => TrieNodeID::Leaf as u8,
        }
    }

    pub fn ptrs(&self) -> &[TriePtr] {
        match self {
            Self::Node4 { ptrs, .. } => &ptrs[..],
            Self::Node16 { ptrs, .. } => &ptrs[..],
            Self::Node48 { ptrs, .. } => &ptrs[..],
            Self::Node256 { ptrs, .. } => &ptrs[..],
            Self::Leaf(_) => &[],
        }
    }

    pub fn path_bytes(&self) -> &[u8] {
        match self {
            Self::Node4 { path, .. } => path,
            Self::Node16 { path, .. } => path,
            Self::Node48 { path, .. } => path,
            Self::Node256 { path, .. } => path,
            Self::Leaf(leaf) => leaf.path,
        }
    }

    pub fn walk(&self, chr: u8) -> Option<TriePtr> {
        match self {
            Self::Node4 { ptrs, .. } => {
                for ptr in ptrs.iter() {
                    if !ptr.is_empty() && ptr.chr() == chr {
                        return Some(*ptr);
                    }
                }
                None
            }
            Self::Node16 { ptrs, .. } => {
                for ptr in ptrs.iter() {
                    if !ptr.is_empty() && ptr.chr() == chr {
                        return Some(*ptr);
                    }
                }
                None
            }
            Self::Node48 { indexes, ptrs, .. } => {
                // SAFETY: chr is a u8, so it always indexes the 256-entry index table.
                #[allow(clippy::indexing_slicing)]
                let ptr_index = indexes[chr as usize];
                if ptr_index >= 0 {
                    // SAFETY: Node48 invariants guarantee non-negative index entries are in 0..48.
                    #[allow(clippy::indexing_slicing)]
                    Some(ptrs[ptr_index as usize])
                } else {
                    None
                }
            }
            Self::Node256 { ptrs, .. } => {
                // SAFETY: chr is a u8, so it always indexes the 256-entry pointer array.
                #[allow(clippy::indexing_slicing)]
                let ptr = ptrs[chr as usize];
                if !ptr.is_empty() {
                    Some(ptr)
                } else {
                    None
                }
            }
            Self::Leaf(_) => None,
        }
    }

    pub fn as_leaf(&self) -> Option<TrieLeafRef<'a>> {
        match self {
            Self::Leaf(leaf) => Some(*leaf),
            _ => None,
        }
    }

    pub fn to_owned_node(&self) -> TrieNodeType {
        match self {
            Self::Node4 { path, ptrs } => TrieNodeType::Node4(TrieNode4 {
                path: NodePath::from_slice(path).expect("node path exceeds 32 bytes"),
                ptrs: **ptrs,
                meta: TrieNodeTransientMeta::default(),
            }),
            Self::Node16 { path, ptrs } => TrieNodeType::Node16(TrieNode16 {
                path: NodePath::from_slice(path).expect("node path exceeds 32 bytes"),
                ptrs: **ptrs,
                meta: TrieNodeTransientMeta::default(),
            }),
            Self::Node48 {
                path,
                indexes,
                ptrs,
            } => TrieNodeType::Node48(Box::new(TrieNode48 {
                path: NodePath::from_slice(path).expect("node path exceeds 32 bytes"),
                indexes: **indexes,
                ptrs: **ptrs,
                meta: TrieNodeTransientMeta::default(),
            })),
            Self::Node256 { path, ptrs } => TrieNodeType::Node256(Box::new(TrieNode256 {
                path: NodePath::from_slice(path).expect("node path exceeds 32 bytes"),
                ptrs: **ptrs,
                meta: TrieNodeTransientMeta::default(),
            })),
            Self::Leaf(leaf) => TrieNodeType::Leaf(TrieLeaf {
                path: NodePath::from_slice(leaf.path).expect("node path exceeds 32 bytes"),
                data: leaf.data.cloned(),
                extent: leaf.extent,
                inline: leaf.inline.cloned(),
            }),
        }
    }
}

impl TrieNodeTransientMeta {
    /// Extract transient metadata from an owned `TrieNodeType`.
    pub fn from_node(node: &TrieNodeType) -> Self {
        node.transient_meta().unwrap_or_default()
    }

    /// Apply this metadata to an owned `TrieNodeType`.
    pub fn apply_to(self, node: &mut TrieNodeType) {
        if let Some(meta) = node.transient_meta_mut() {
            *meta = self;
        }
    }
}

impl<'a> From<&'a TrieNodeType> for TrieNodeRef<'a> {
    fn from(node: &'a TrieNodeType) -> Self {
        match node {
            TrieNodeType::Node4(data) => Self::Node4 {
                path: data.path.as_slice(),
                ptrs: &data.ptrs,
            },
            TrieNodeType::Node16(data) => Self::Node16 {
                path: data.path.as_slice(),
                ptrs: &data.ptrs,
            },
            TrieNodeType::Node48(data) => Self::Node48 {
                path: data.path.as_slice(),
                indexes: data.indexes(),
                ptrs: &data.ptrs,
            },
            TrieNodeType::Node256(data) => Self::Node256 {
                path: data.path.as_slice(),
                ptrs: &data.ptrs,
            },
            TrieNodeType::Leaf(data) => Self::Leaf(TrieLeafRef {
                path: data.path.as_slice(),
                data: data.data.as_ref(),
                extent: data.extent,
                inline: data.inline.as_ref(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chainstate::stacks::index::BorrowedNodeBytes;
    use crate::types::chainstate::BlockHeaderHash;

    #[test]
    fn byte_backed_walk_propagates_decode_corruption() {
        let path = TrieHash::from_key("corrupt-node");
        let root_ptr = TriePtr::new(TrieNodeID::Node4 as u8, 0, 0);
        let mut cursor = TrieCursor::<BlockHeaderHash>::new(&path, root_ptr);
        let corrupt_node_bytes = [];
        let read_node = ReadTrieNode::from_stable_bytes(
            BorrowedNodeBytes::new(TrieNodeID::Node4, &corrupt_node_bytes),
            Some(TrieHash([0; 32])),
        );

        let err = cursor
            .walk_read(&read_node, &BlockHeaderHash::sentinel())
            .expect_err("corrupt persisted bytes must not look like a missing child");

        assert!(
            matches!(err, Error::CorruptionError(_)),
            "unexpected error: {err:?}"
        );
    }
}

macro_rules! with_node {
    ($self: expr, $pat:pat, $s:expr) => {
        match $self {
            TrieNodeType::Node4($pat) => $s,
            TrieNodeType::Node16($pat) => $s,
            TrieNodeType::Node48($pat) => $s,
            TrieNodeType::Node256($pat) => $s,
            TrieNodeType::Leaf($pat) => $s,
        }
    };
}

impl TrieNodeType {
    /// Encode branch metadata, reusing the backpointer hashes in child commitments.
    pub fn write_consensus_bytes_with_child_hashes<W: Write>(
        &self,
        child_hashes: &[TrieHash],
        w: &mut W,
    ) -> Result<(), Error> {
        assert!(!self.is_leaf());
        assert_eq!(child_hashes.len(), self.ptrs().len());
        w.write_all(&[self.id()])?;
        for (ptr, hash) in self.ptrs().iter().zip(child_hashes) {
            ptr.write_consensus_bytes_with_block_hash(
                is_backptr(ptr.id()).then_some(hash.as_bytes()),
                w,
            )?;
        }
        bits::write_path_to_bytes(self.path_bytes(), w)
    }

    pub fn is_leaf(&self) -> bool {
        matches!(self, TrieNodeType::Leaf(_))
    }

    pub fn is_node4(&self) -> bool {
        matches!(self, TrieNodeType::Node4(_))
    }

    pub fn is_node16(&self) -> bool {
        matches!(self, TrieNodeType::Node16(_))
    }

    pub fn is_node48(&self) -> bool {
        matches!(self, TrieNodeType::Node48(_))
    }

    pub fn is_node256(&self) -> bool {
        matches!(self, TrieNodeType::Node256(_))
    }

    pub fn id(&self) -> u8 {
        with_node!(self, ref data, data.id())
    }

    pub fn walk(&self, chr: u8) -> Option<TriePtr> {
        with_node!(self, ref data, data.walk(chr))
    }

    pub fn write_bytes<W: Write>(&self, w: &mut W) -> Result<(), Error> {
        with_node!(self, ref data, data.write_bytes(w))
    }

    pub fn write_bytes_compressed<W: Write>(&self, w: &mut W) -> Result<(), Error> {
        with_node!(self, ref data, data.write_bytes_compressed(w))
    }

    pub fn write_consensus_bytes<W: Write, M: BlockMap>(
        &self,
        map: &mut M,
        w: &mut W,
    ) -> Result<(), Error> {
        with_node!(self, ref data, data.write_consensus_bytes(map, w))
    }

    pub fn byte_len(&self) -> usize {
        with_node!(self, ref data, data.byte_len())
    }

    pub fn byte_len_compressed(&self) -> usize {
        with_node!(self, ref data, data.byte_len_compressed())
    }

    pub fn insert(&mut self, ptr: &TriePtr) -> bool {
        with_node!(self, ref mut data, data.insert(ptr))
    }

    pub fn replace(&mut self, ptr: &TriePtr) -> bool {
        with_node!(self, ref mut data, data.replace(ptr))
    }

    pub fn ptrs(&self) -> &[TriePtr] {
        with_node!(self, ref data, data.ptrs())
    }

    pub fn ptrs_mut(&mut self) -> &mut [TriePtr] {
        match self {
            TrieNodeType::Node4(ref mut data) => &mut data.ptrs,
            TrieNodeType::Node16(ref mut data) => &mut data.ptrs,
            TrieNodeType::Node48(ref mut data) => &mut data.ptrs,
            TrieNodeType::Node256(ref mut data) => &mut data.ptrs,
            TrieNodeType::Leaf(_) => panic!("Leaf has no ptrs"),
        }
    }

    pub fn max_ptrs(&self) -> usize {
        match self {
            TrieNodeType::Node4(_) => 4,
            TrieNodeType::Node16(_) => 16,
            TrieNodeType::Node48(_) => 48,
            TrieNodeType::Node256(_) => 256,
            TrieNodeType::Leaf(_) => 0,
        }
    }

    pub fn path_bytes(&self) -> &[u8] {
        with_node!(self, ref data, data.path.as_slice())
    }

    pub fn set_path(&mut self, new_path: NodePath) {
        with_node!(self, ref mut data, data.path = new_path)
    }

    pub fn get_cow_ptr(&self) -> Option<&TrieCowPtr> {
        with_node!(self, ref data, data.get_cow_ptr())
    }

    pub fn set_cow_ptr(&mut self, cowptr: TrieCowPtr) {
        with_node!(self, ref mut data, data.set_cow_ptr(cowptr))
    }

    pub fn clear_cow_ptr(&mut self) {
        if let Some(meta) = self.transient_meta_mut() {
            meta.cowptr = None;
        }
    }

    pub fn patch_depth(&self) -> usize {
        self.transient_meta().map_or(0, |meta| meta.patch_depth)
    }

    pub fn last_patch_source(&self) -> Option<(u32, TriePtr)> {
        self.transient_meta()
            .and_then(|meta| meta.last_patch_source)
    }

    pub fn set_patch_depth(&mut self, depth: usize) {
        if let Some(meta) = self.transient_meta_mut() {
            meta.patch_depth = depth;
        }
    }

    pub fn set_last_patch_source(&mut self, source: Option<(u32, TriePtr)>) {
        if let Some(meta) = self.transient_meta_mut() {
            meta.last_patch_source = source;
        }
    }

    pub fn transient_meta(&self) -> Option<TrieNodeTransientMeta> {
        with_node!(self, ref data, data.meta().copied())
    }

    pub fn transient_meta_mut(&mut self) -> Option<&mut TrieNodeTransientMeta> {
        with_node!(self, ref mut data, data.meta_mut())
    }
}
