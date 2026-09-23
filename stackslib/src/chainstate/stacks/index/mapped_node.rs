// Copyright (C) 2026 Stacks Open Internet Foundation
// SPDX-License-Identifier: GPL-3.0-or-later

//! Path-first branch payloads with direct access to individual compressed child pointers.
//!
//! ```text
//! path length | path | occupied-slot bitmap | selectors | u16 offsets | pointer bytes
//! ```
//! Node4/16 selectors contain one byte per occupied slot. Node48 retains its 256-byte
//! selector-to-slot table. Node256 selects a bitmap slot directly. Offsets include a final
//! sentinel and are relative to the pointer bytes. Logical slot order is preserved.

use std::io::Write;

use super::bits;
use super::node::{
    clear_ctrl_bits, TrieNode, TrieNode16, TrieNode256, TrieNode4, TrieNode48, TrieNodeID,
    TrieNodeType, TriePtr,
};
use super::{Error, NodePath};

/// Checked metadata and borrowed payload for one mapped branch node.
#[derive(Clone, Copy, Debug)]
pub struct MappedBranch<'a> {
    /// Logical branch type.
    id: TrieNodeID,
    /// Compressed path prefix.
    path: &'a [u8],
    /// Occupied logical pointer slots.
    bitmap: &'a [u8],
    /// Compact keys for Node4/16 or the Node48 slot table.
    selectors: &'a [u8],
    /// Little-endian offsets into the pointer payload, including the final sentinel.
    offsets: &'a [u8],
    /// Compressed pointers in logical slot order.
    pointers: &'a [u8],
    /// Encoded payload length, excluding the outer marker and hash.
    length: usize,
}

/// Number of logical child slots for a branch type.
fn capacity(id: TrieNodeID) -> Result<usize, Error> {
    match id {
        TrieNodeID::Node4 => Ok(4),
        TrieNodeID::Node16 => Ok(16),
        TrieNodeID::Node48 => Ok(48),
        TrieNodeID::Node256 => Ok(256),
        _ => Err(invalid("Expected branch node")),
    }
}

/// Decode a path prefix without examining the child directory.
pub fn path_prefix(bytes: &[u8]) -> Result<&[u8], Error> {
    let length = usize::from(
        *bytes
            .first()
            .ok_or_else(|| invalid("Missing path length"))?,
    );
    if length > 32 {
        return Err(invalid("Oversized node path"));
    }
    bytes
        .get(1..1 + length)
        .ok_or_else(|| invalid("Truncated node path"))
}

/// Count occupied bits before a slot using at most four word-sized population counts.
fn rank(bitmap: &[u8], slot: usize) -> usize {
    let whole_bytes = slot / 8;
    let prefix = &bitmap[..whole_bytes];
    let mut chunks = prefix.chunks_exact(8);
    let words: usize = chunks
        .by_ref()
        .map(|chunk| {
            u64::from_le_bytes(chunk.try_into().expect("word width")).count_ones() as usize
        })
        .sum();
    let tail: usize = chunks
        .remainder()
        .iter()
        .map(|byte| byte.count_ones() as usize)
        .sum();
    let partial = if slot % 8 == 0 {
        0
    } else {
        (bitmap[whole_bytes] & ((1 << (slot % 8)) - 1)).count_ones() as usize
    };
    words + tail + partial
}

/// Whether one logical slot is occupied.
fn occupied(bitmap: &[u8], slot: usize) -> bool {
    bitmap
        .get(slot / 8)
        .is_some_and(|byte| byte & (1 << (slot % 8)) != 0)
}

/// Read a checked little-endian pointer-directory entry.
fn offset_at(offsets: &[u8], index: usize) -> Result<usize, Error> {
    let start = index.checked_mul(2).ok_or(Error::OverflowError)?;
    let bytes = offsets
        .get(start..start + 2)
        .ok_or_else(|| invalid("Truncated child directory"))?;
    Ok(u16::from_le_bytes(bytes.try_into().expect("directory width")) as usize)
}

impl<'a> MappedBranch<'a> {
    /// Validate branch metadata without decoding or copying the child pointer array.
    pub fn parse(id: TrieNodeID, bytes: &'a [u8]) -> Result<Self, Error> {
        let slots = capacity(id)?;
        let path = path_prefix(bytes)?;
        let bitmap_start = 1 + path.len();
        let bitmap_end = bitmap_start + slots.div_ceil(8);
        let bitmap = bytes
            .get(bitmap_start..bitmap_end)
            .ok_or_else(|| invalid("Truncated slot bitmap"))?;
        if slots == 4 && bitmap[0] & 0xf0 != 0 {
            return Err(invalid("Invalid Node4 occupancy"));
        }
        let count = rank(bitmap, slots);
        let selector_len = match id {
            TrieNodeID::Node4 | TrieNodeID::Node16 => count,
            TrieNodeID::Node48 => 256,
            _ => 0,
        };
        let selector_end = bitmap_end + selector_len;
        let selectors = bytes
            .get(bitmap_end..selector_end)
            .ok_or_else(|| invalid("Truncated child selectors"))?;
        if slots <= 16 {
            for (i, key) in selectors.iter().enumerate() {
                if selectors[..i].contains(key) {
                    return Err(invalid("Duplicate child selector"));
                }
            }
        }
        let directory_end = selector_end + (count + 1) * 2;
        let offsets = bytes
            .get(selector_end..directory_end)
            .ok_or_else(|| invalid("Truncated child directory"))?;
        if offset_at(offsets, 0)? != 0 {
            return Err(invalid("Nonzero first child offset"));
        }
        let pointer_len = offset_at(offsets, count)?;
        let length = directory_end
            .checked_add(pointer_len)
            .ok_or(Error::OverflowError)?;
        let pointers = bytes
            .get(directory_end..length)
            .ok_or_else(|| invalid("Truncated child payload"))?;
        Ok(Self {
            id,
            path,
            bitmap,
            selectors,
            offsets,
            pointers,
            length,
        })
    }

    /// Borrow the compressed path prefix.
    pub fn path(&self) -> &'a [u8] {
        self.path
    }

    /// Number of payload bytes consumed.
    pub fn byte_len(&self) -> usize {
        self.length
    }

    /// Decode exactly one occupied pointer, validating its directory bounds and encoding.
    fn pointer_at(&self, index: usize) -> Result<TriePtr, Error> {
        let start = offset_at(self.offsets, index)?;
        let end = offset_at(self.offsets, index + 1)?;
        let bytes = self
            .pointers
            .get(start..end)
            .ok_or_else(|| invalid("Invalid child extent"))?;
        let (ptr, consumed) = TriePtr::from_slice_compressed(bytes)?;
        if ptr.is_empty() || consumed != bytes.len() {
            return Err(invalid("Invalid occupied child record"));
        }
        let logical = TrieNodeID::from_u8(clear_ctrl_bits(ptr.id()))
            .ok_or_else(|| invalid("Invalid child node type"))?;
        if matches!(
            logical,
            TrieNodeID::Empty | TrieNodeID::Patch | TrieNodeID::ValueLeaf
        ) {
            return Err(invalid("Physical-only type in logical child pointer"));
        }
        Ok(ptr)
    }

    /// Find one child directly in mapped bytes, without materializing other pointers.
    pub fn child(&self, chr: u8) -> Result<Option<TriePtr>, Error> {
        let index = match self.id {
            TrieNodeID::Node4 | TrieNodeID::Node16 => {
                let Some(index) = self.selectors.iter().position(|key| *key == chr) else {
                    return Ok(None);
                };
                index
            }
            TrieNodeID::Node48 => {
                let slot = self.selectors[chr as usize] as usize;
                if slot == 255 {
                    return Ok(None);
                }
                if slot >= 48 || !occupied(self.bitmap, slot) {
                    return Err(invalid("Invalid Node48 selector"));
                }
                rank(self.bitmap, slot)
            }
            TrieNodeID::Node256 => {
                if !occupied(self.bitmap, chr as usize) {
                    return Ok(None);
                }
                rank(self.bitmap, chr as usize)
            }
            _ => return Err(invalid("Expected branch node")),
        };
        let ptr = self.pointer_at(index)?;
        if ptr.chr() != chr {
            return Err(invalid("Child selector disagrees with pointer"));
        }
        Ok(Some(ptr))
    }

    /// Materialize all children for mutation, proof generation or patch resolution.
    pub fn to_owned_node(&self) -> Result<TrieNodeType, Error> {
        let mut node = match self.id {
            TrieNodeID::Node4 => TrieNodeType::Node4(TrieNode4::empty()),
            TrieNodeID::Node16 => TrieNodeType::Node16(TrieNode16::empty()),
            TrieNodeID::Node48 => TrieNodeType::Node48(Box::new(TrieNode48::empty())),
            TrieNodeID::Node256 => TrieNodeType::Node256(Box::new(TrieNode256::empty())),
            _ => return Err(invalid("Expected branch node")),
        };
        let mut index = 0;
        for (slot, destination) in node.ptrs_mut().iter_mut().enumerate() {
            if occupied(self.bitmap, slot) {
                let ptr = self.pointer_at(index)?;
                if self.id == TrieNodeID::Node256 && ptr.chr() as usize != slot {
                    return Err(invalid("Node256 child in wrong slot"));
                }
                if self.id == TrieNodeID::Node48
                    && self.selectors[ptr.chr() as usize] as usize != slot
                {
                    return Err(invalid("Node48 child in wrong slot"));
                }
                if matches!(self.id, TrieNodeID::Node4 | TrieNodeID::Node16)
                    && self.selectors[index] != ptr.chr()
                {
                    return Err(invalid("Child selector disagrees with pointer"));
                }
                *destination = ptr;
                index += 1;
            }
        }
        let path = NodePath::from_slice(self.path).expect("validated path");
        match &mut node {
            TrieNodeType::Node4(n) => n.path = path,
            TrieNodeType::Node16(n) => n.path = path,
            TrieNodeType::Node48(n) => {
                for (chr, slot) in self.selectors.iter().copied().enumerate() {
                    if slot != 255
                        && (slot >= 48
                            || n.ptrs[slot as usize].is_empty()
                            || n.ptrs[slot as usize].chr() as usize != chr)
                    {
                        return Err(invalid("Invalid Node48 reverse selector"));
                    }
                    n.indexes[chr] = slot as i8;
                }
                n.path = path;
            }
            TrieNodeType::Node256(n) => n.path = path,
            _ => return Err(invalid("Expected branch node")),
        }
        Ok(node)
    }
}

/// Largest valid payload, including the directory and wide annotated pointers.
pub fn max_payload_len(id: TrieNodeID) -> Result<usize, Error> {
    let slots = capacity(id)?;
    let selectors = match slots {
        4 | 16 => slots,
        48 => 256,
        _ => 0,
    };
    Ok(33 + slots.div_ceil(8) + selectors + (slots + 1) * 2 + slots * TriePtr::max_encoded_size())
}

/// Compute the path-first payload size without allocating a serialization buffer.
pub fn payload_len(node: &TrieNodeType) -> usize {
    let ptrs = node.ptrs();
    let count = ptrs.iter().filter(|ptr| !ptr.is_empty()).count();
    let selectors = match ptrs.len() {
        4 | 16 => count,
        48 => 256,
        _ => 0,
    };
    1 + node.path_bytes().len()
        + ptrs.len().div_ceil(8)
        + selectors
        + (count + 1) * 2
        + ptrs
            .iter()
            .filter(|ptr| !ptr.is_empty())
            .map(TriePtr::compressed_size)
            .sum::<usize>()
}

/// Write a path-first branch while retaining compressed pointers and their logical slot order.
pub fn write_payload<W: Write>(writer: &mut W, node: &TrieNodeType) -> Result<(), Error> {
    let id = TrieNodeID::from_u8(node.id()).ok_or_else(|| invalid("Invalid branch type"))?;
    let slots = capacity(id)?;
    let ptrs = node.ptrs();
    if ptrs.len() != slots {
        return Err(invalid("Invalid branch capacity"));
    }
    bits::write_path_to_bytes(node.path_bytes(), writer)?;
    let mut bitmap = [0u8; 32];
    for (slot, ptr) in ptrs.iter().enumerate() {
        if !ptr.is_empty() {
            bitmap[slot / 8] |= 1 << (slot % 8);
        }
    }
    writer.write_all(&bitmap[..slots.div_ceil(8)])?;
    if slots <= 16 {
        for ptr in ptrs.iter().filter(|ptr| !ptr.is_empty()) {
            writer.write_all(&[ptr.chr()])?;
        }
    } else if let TrieNodeType::Node48(n) = node {
        for index in n.indexes() {
            writer.write_all(&[*index as u8])?;
        }
    }
    let mut offset = 0u16;
    writer.write_all(&offset.to_le_bytes())?;
    for ptr in ptrs.iter().filter(|ptr| !ptr.is_empty()) {
        offset = offset
            .checked_add(u16::try_from(ptr.compressed_size()).map_err(|_| Error::OverflowError)?)
            .ok_or(Error::OverflowError)?;
        writer.write_all(&offset.to_le_bytes())?;
    }
    for ptr in ptrs.iter().filter(|ptr| !ptr.is_empty()) {
        ptr.write_bytes_compressed(writer)?;
    }
    Ok(())
}

/// Construct a malformed-record error.
fn invalid(message: &str) -> Error {
    Error::CorruptionError(message.into())
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Seek};

    use super::*;
    use crate::chainstate::stacks::index::record::NodeRecordFormat;
    use crate::chainstate::stacks::index::scratch::MarfReadState;
    use crate::chainstate::stacks::index::{BorrowedNodeBytes, ReadTrieNode};
    use stacks_common::types::chainstate::TrieHash;

    /// Exercise every child selector, holes, wide offsets and both inline and historical children.
    #[test]
    fn mapped_selection_preserves_logical_slots() {
        let nodes = [
            TrieNodeType::Node4(TrieNode4::empty()),
            TrieNodeType::Node16(TrieNode16::empty()),
            TrieNodeType::Node48(Box::new(TrieNode48::empty())),
            TrieNodeType::Node256(Box::new(TrieNode256::empty())),
        ];
        for mut node in nodes {
            let slots = node.ptrs().len();
            for slot in 0..slots {
                if slot % 3 == 1 {
                    continue;
                }
                let chr = if slots == 256 {
                    slot as u8
                } else {
                    (255 - slot) as u8
                };
                node.ptrs_mut()[slot] = TriePtr {
                    id: TrieNodeID::Leaf as u8 | if slot % 2 == 0 { 0x80 } else { 0 },
                    chr,
                    ptr: if slot % 5 == 0 {
                        u64::from(u32::MAX) + 4
                    } else {
                        100 + slot as u64
                    },
                    back_block: slot as u32 + 1,
                };
                if let TrieNodeType::Node48(n) = &mut node {
                    n.indexes[chr as usize] = slot as i8;
                }
            }
            let format = NodeRecordFormat::TypeFirstV1;
            let mut bytes = Vec::new();
            format
                .write_node(&mut bytes, &node, TrieHash([4; 32]), true)
                .unwrap();
            assert_eq!(bytes.len(), format.node_len(&node, true));
            assert!(bytes.len() <= format.max_record_len(node.id()).unwrap());
            let record = format.parse(&bytes).unwrap();
            let view = MappedBranch::parse(record.logical_type(), record.payload).unwrap();
            assert_eq!(view.to_owned_node().unwrap(), node);
            let read = ReadTrieNode::from_stable_bytes(
                BorrowedNodeBytes::from_record(record),
                record.hash,
            );
            assert_eq!(read.path_bytes().unwrap(), node.path_bytes());
            for chr in 0..=255 {
                assert_eq!(view.child(chr).unwrap(), node.walk(chr));
                assert_eq!(read.walk(chr).unwrap(), node.walk(chr));
            }
            assert!(
                read.decoded_bytes.as_ref().unwrap().get().is_none(),
                "point traversal materialized a node"
            );
            assert_eq!(read.ptrs().unwrap(), node.ptrs());
            assert!(read.decoded_bytes.as_ref().unwrap().get().is_some());
            let mut input = Cursor::new(bytes);
            let mut scratch = MarfReadState::new();
            let buffered = bits::read_trie_item_at_head_ref_format(
                &mut input,
                node.id(),
                format,
                &mut scratch,
            )
            .unwrap()
            .into_node()
            .unwrap();
            assert_eq!(buffered.ptrs().unwrap(), node.ptrs());
            assert_eq!(
                input.stream_position().unwrap(),
                format.node_len(&node, true) as u64
            );
        }
    }

    /// Invalid selected offsets and selectors fail without dereferencing unchecked file bytes.
    #[test]
    fn invalid_directory_is_rejected() {
        let mut node = TrieNode4::new(&[9]);
        node.insert(&TriePtr::new(TrieNodeID::Leaf as u8, 7, 50));
        let mut bytes = Vec::new();
        write_payload(&mut bytes, &TrieNodeType::Node4(node)).unwrap();
        for end in 0..bytes.len() {
            assert!(MappedBranch::parse(TrieNodeID::Node4, &bytes[..end]).is_err());
        }
        // path(2), bitmap(1), selector(1), first offset(2), final offset(2)
        bytes[4] = 1;
        assert!(MappedBranch::parse(TrieNodeID::Node4, &bytes).is_err());
        bytes[4] = 0;
        bytes[3] = 8;
        let view = MappedBranch::parse(TrieNodeID::Node4, &bytes).unwrap();
        assert!(view.child(8).is_err());
        assert!(view.to_owned_node().is_err());
    }
}
