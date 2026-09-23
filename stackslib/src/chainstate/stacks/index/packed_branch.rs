// Copyright (C) 2026 Stacks Open Internet Foundation
// SPDX-License-Identifier: GPL-3.0-or-later

//! Direct fixed-width child columns preserving logical slots and squash annotations.
//!
//! ```text
//! path | widths | occupancy | selectors | kind nibbles | origin bitmap | targets | origins
//! ```
//! Widths are per node. Targets use 2/3/4/8 bytes; origins use 1/2/3/4 bytes.
//! Node48 selectors are six-bit original slots, with 63 denoting absence.

use std::io::Write;

use super::bits;
use super::mapped_node;
use super::node::{
    clear_ctrl_bits, is_backptr, TrieNode, TrieNode16, TrieNode256, TrieNode4, TrieNode48,
    TrieNodeID, TrieNodeType, TriePtr,
};
use super::record::NodeRecordFormat;
use super::{Error, NodePath};

/// Checked metadata and mmap-backed pointer columns for one branch.
#[derive(Clone, Copy, Debug)]
pub struct PackedBranch<'a> {
    /// Logical branch kind.
    id: TrieNodeID,
    /// Shared path prefix.
    path: &'a [u8],
    /// Occupied logical slots.
    occupancy: &'a [u8],
    /// Edge selectors, absent for Node256.
    selectors: &'a [u8],
    /// Logical kind and backpointer flag, two children per byte.
    kinds: &'a [u8],
    /// Origin presence in occupied-slot order, including squash annotations.
    origin_bitmap: &'a [u8],
    /// Directly indexed target offsets.
    targets: &'a [u8],
    /// Origins addressed by rank in the origin bitmap.
    origins: &'a [u8],
    /// Bytes per target.
    target_width: usize,
    /// Bytes per origin.
    origin_width: usize,
    /// Total encoded payload size.
    length: usize,
}

/// Number of logical slots for one branch kind.
fn capacity(id: TrieNodeID) -> Result<usize, Error> {
    match id {
        TrieNodeID::Node4 => Ok(4),
        TrieNodeID::Node16 => Ok(16),
        TrieNodeID::Node48 => Ok(48),
        TrieNodeID::Node256 => Ok(256),
        _ => Err(invalid("Expected packed branch")),
    }
}

/// Construct a malformed-record error.
fn invalid(message: &str) -> Error {
    Error::CorruptionError(message.into())
}

/// Whether one bit is set in a bounded bitmap.
fn present(bitmap: &[u8], slot: usize) -> bool {
    bitmap
        .get(slot / 8)
        .is_some_and(|b| b & (1 << (slot % 8)) != 0)
}

/// Count preceding bits without decoding the pointer array.
fn rank(bitmap: &[u8], slot: usize) -> usize {
    let mut chunks = bitmap[..slot / 8].chunks_exact(8);
    let words: usize = chunks
        .by_ref()
        .map(|c| u64::from_le_bytes(c.try_into().expect("word width")).count_ones() as usize)
        .sum();
    words
        + chunks
            .remainder()
            .iter()
            .map(|b| b.count_ones() as usize)
            .sum::<usize>()
        + if slot % 8 == 0 {
            0
        } else {
            (bitmap[slot / 8] & ((1 << (slot % 8)) - 1)).count_ones() as usize
        }
}

/// Reject nonzero padding bits in a bitmap.
fn check_padding(bitmap: &[u8], count: usize) -> Result<(), Error> {
    if count % 8 != 0 && bitmap[count / 8] >> (count % 8) != 0 {
        return Err(invalid("Nonzero packed bitmap padding"));
    }
    Ok(())
}

/// Read a checked fixed-width unsigned integer.
fn integer(bytes: &[u8], index: usize, width: usize) -> Result<u64, Error> {
    let start = index.checked_mul(width).ok_or(Error::OverflowError)?;
    let mut value = [0; 8];
    value[..width].copy_from_slice(
        bytes
            .get(start..start + width)
            .ok_or_else(|| invalid("Truncated packed integer"))?,
    );
    Ok(u64::from_le_bytes(value))
}

/// Select one six-bit original Node48 slot using at most two bytes.
fn selector48(bytes: &[u8], edge: u8) -> usize {
    let bit = usize::from(edge) * 6;
    let start = bit / 8;
    let word =
        u16::from(bytes[start]) | (u16::from(bytes.get(start + 1).copied().unwrap_or(0)) << 8);
    usize::from((word >> (bit % 8)) & 63)
}

/// Minimum supported byte width of one target.
fn target_width(value: u64) -> usize {
    match value {
        0..=0xffff => 2,
        0x10000..=0xffffff => 3,
        0x1000000..=0xffffffff => 4,
        _ => 8,
    }
}

/// Minimum supported byte width of one origin.
fn origin_width(value: u32) -> usize {
    match value {
        0..=0xff => 1,
        0x100..=0xffff => 2,
        0x10000..=0xffffff => 3,
        _ => 4,
    }
}

/// Writer-selected widths and occupied/origin counts.
fn dimensions(node: &TrieNodeType) -> (usize, usize, usize, usize) {
    let (mut target, mut origin, mut count, mut origins) = (0, 0, 0, 0);
    for p in node.ptrs().iter().filter(|p| !p.is_empty()) {
        target = target.max(p.ptr);
        origin = origin.max(p.back_block);
        count += 1;
        origins += usize::from(is_backptr(p.id) || p.back_block != 0);
    }
    (target_width(target), origin_width(origin), count, origins)
}

/// Number of edge selector bytes for a given occupancy.
fn selector_len(slots: usize, count: usize) -> usize {
    match slots {
        4 | 16 => count,
        48 => 192,
        _ => 0,
    }
}

impl<'a> PackedBranch<'a> {
    /// Validate bounded column metadata without materializing child pointers.
    pub fn parse(id: TrieNodeID, bytes: &'a [u8]) -> Result<Self, Error> {
        let slots = capacity(id)?;
        let path = mapped_node::path_prefix(bytes)?;
        let mut cursor = 1 + path.len();
        let widths = *bytes
            .get(cursor)
            .ok_or_else(|| invalid("Missing packed widths"))?;
        cursor += 1;
        if widths & 0xf0 != 0 {
            return Err(invalid("Unsupported packed widths"));
        }
        let target_width = [2, 3, 4, 8][usize::from(widths & 3)];
        let origin_width = usize::from((widths >> 2) & 3) + 1;
        let mut take = |length: usize| -> Result<&'a [u8], Error> {
            let end = cursor.checked_add(length).ok_or(Error::OverflowError)?;
            let result = bytes
                .get(cursor..end)
                .ok_or_else(|| invalid("Truncated packed column"))?;
            cursor = end;
            Ok(result)
        };
        let occupancy = take(slots.div_ceil(8))?;
        check_padding(occupancy, slots)?;
        let count = rank(occupancy, slots);
        let selectors = take(selector_len(slots, count))?;
        if slots <= 16 {
            for (index, edge) in selectors.iter().enumerate() {
                if selectors[..index].contains(edge) {
                    return Err(invalid("Duplicate packed selector"));
                }
            }
        }
        let kinds = take(count.div_ceil(2))?;
        if count % 2 != 0 && kinds[count / 2] & 0xf0 != 0 {
            return Err(invalid("Nonzero packed kind padding"));
        }
        let origin_bitmap = take(count.div_ceil(8))?;
        check_padding(origin_bitmap, count)?;
        let targets = take(count * target_width)?;
        let origins = take(rank(origin_bitmap, count) * origin_width)?;
        Ok(Self {
            id,
            path,
            occupancy,
            selectors,
            kinds,
            origin_bitmap,
            targets,
            origins,
            target_width,
            origin_width,
            length: cursor,
        })
    }

    /// Borrow the unchanged compressed path.
    pub fn path(&self) -> &'a [u8] {
        self.path
    }

    /// Number of payload bytes consumed.
    pub fn byte_len(&self) -> usize {
        self.length
    }

    /// Decode only the selected child from direct columns.
    fn pointer_at(&self, index: usize, chr: u8) -> Result<TriePtr, Error> {
        let kind = (self.kinds[index / 2] >> ((index % 2) * 4)) & 15;
        let id = kind & 7;
        if !(1..=5).contains(&id) {
            return Err(invalid("Invalid packed child kind"));
        }
        let back = kind & 8 != 0;
        let has_origin = present(self.origin_bitmap, index);
        if back && !has_origin {
            return Err(invalid("Missing packed backpointer origin"));
        }
        let back_block = if has_origin {
            integer(
                self.origins,
                rank(self.origin_bitmap, index),
                self.origin_width,
            )? as u32
        } else {
            0
        };
        Ok(TriePtr {
            id: id | if back { 0x80 } else { 0 },
            chr,
            ptr: integer(self.targets, index, self.target_width)?,
            back_block,
        })
    }

    /// Select a child without copying or traversing the other pointers.
    pub fn child(&self, chr: u8) -> Result<Option<TriePtr>, Error> {
        let index = match self.id {
            TrieNodeID::Node4 | TrieNodeID::Node16 => {
                let Some(index) = self.selectors.iter().position(|b| *b == chr) else {
                    return Ok(None);
                };
                index
            }
            TrieNodeID::Node48 => {
                let slot = selector48(self.selectors, chr);
                if slot == 63 {
                    return Ok(None);
                }
                if slot >= 48 || !present(self.occupancy, slot) {
                    return Err(invalid("Invalid packed Node48 selector"));
                }
                rank(self.occupancy, slot)
            }
            TrieNodeID::Node256 => {
                if !present(self.occupancy, usize::from(chr)) {
                    return Ok(None);
                }
                rank(self.occupancy, usize::from(chr))
            }
            _ => return Err(invalid("Expected packed branch")),
        };
        self.pointer_at(index, chr).map(Some)
    }

    /// Reconstruct all logical slots for mutation, hashing or proofs.
    pub fn to_owned_node(&self) -> Result<TrieNodeType, Error> {
        let mut node = match self.id {
            TrieNodeID::Node4 => TrieNodeType::Node4(TrieNode4::empty()),
            TrieNodeID::Node16 => TrieNodeType::Node16(TrieNode16::empty()),
            TrieNodeID::Node48 => TrieNodeType::Node48(Box::new(TrieNode48::empty())),
            TrieNodeID::Node256 => TrieNodeType::Node256(Box::new(TrieNode256::empty())),
            _ => return Err(invalid("Expected packed branch")),
        };
        let mut reverse = [None; 48];
        if let TrieNodeType::Node48(n) = &mut node {
            for edge in 0..=255u8 {
                let slot = selector48(self.selectors, edge);
                if slot == 63 {
                    continue;
                }
                if slot >= 48
                    || !present(self.occupancy, slot)
                    || reverse[slot].replace(edge).is_some()
                {
                    return Err(invalid("Invalid packed Node48 reverse selector"));
                }
                n.indexes[usize::from(edge)] = slot as i8;
            }
        }
        let mut index = 0;
        for (slot, p) in node.ptrs_mut().iter_mut().enumerate() {
            if !present(self.occupancy, slot) {
                continue;
            }
            let edge = match self.id {
                TrieNodeID::Node48 => {
                    reverse[slot].ok_or_else(|| invalid("Missing packed Node48 selector"))?
                }
                TrieNodeID::Node256 => slot as u8,
                _ => self.selectors[index],
            };
            *p = self.pointer_at(index, edge)?;
            index += 1;
        }
        let path = NodePath::from_slice(self.path).expect("checked path length");
        match &mut node {
            TrieNodeType::Node4(n) => n.path = path,
            TrieNodeType::Node16(n) => n.path = path,
            TrieNodeType::Node48(n) => n.path = path,
            TrieNodeType::Node256(n) => n.path = path,
            _ => unreachable!("checked branch"),
        }
        Ok(node)
    }
}

/// Largest packed branch payload, independent of writer width policy.
pub fn max_payload_len(id: TrieNodeID) -> Result<usize, Error> {
    let slots = capacity(id)?;
    Ok(34
        + slots.div_ceil(8)
        + selector_len(slots, slots)
        + slots.div_ceil(2)
        + slots.div_ceil(8)
        + slots * 12)
}

/// Exact payload length for the current pointer values.
pub fn payload_len(node: &TrieNodeType) -> usize {
    let (target, origin, count, origins) = dimensions(node);
    let slots = node.ptrs().len();
    2 + node.path_bytes().len()
        + slots.div_ceil(8)
        + selector_len(slots, count)
        + count.div_ceil(2)
        + count.div_ceil(8)
        + count * target
        + origins * origin
}

/// Exact payload size after mapping targets, without cloning nodes or child arrays.
pub fn payload_len_with_targets(
    node: &TrieNodeType,
    mut resolve: impl FnMut(&TriePtr) -> Result<u64, Error>,
) -> Result<usize, Error> {
    let (old_width, _, count, _) = dimensions(node);
    let mut largest = 0;
    for ptr in node.ptrs().iter().filter(|ptr| !ptr.is_empty()) {
        largest = largest.max(resolve(ptr)?);
    }
    Ok(payload_len(node) - count * old_width + count * target_width(largest))
}

/// Serialize direct pointer columns in unchanged logical slot order.
pub fn write_payload<W: Write>(writer: &mut W, node: &TrieNodeType) -> Result<(), Error> {
    let id = TrieNodeID::from_u8(node.id()).ok_or_else(|| invalid("Invalid packed branch kind"))?;
    let slots = capacity(id)?;
    let ptrs = node.ptrs();
    if ptrs.len() != slots {
        return Err(invalid("Invalid packed branch capacity"));
    }
    let (target, origin, count, _) = dimensions(node);
    let width_code = match target {
        2 => 0,
        3 => 1,
        4 => 2,
        _ => 3,
    } | ((origin - 1) << 2);
    let mut seen_edges = [false; 256];
    let mut occupancy = [0u8; 32];
    let mut origins = [0u8; 32];
    let mut kinds = [0u8; 128];
    let mut index = 0;
    for (slot, p) in ptrs.iter().enumerate().filter(|(_, p)| !p.is_empty()) {
        let edge = usize::from(p.chr);
        if seen_edges[edge] {
            return Err(invalid("Duplicate packed child edge"));
        }
        seen_edges[edge] = true;
        if let TrieNodeType::Node48(n) = node {
            if n.indexes()[edge] != slot as i8 {
                return Err(invalid("Missing packed Node48 selector"));
            }
        }
        let kind = clear_ctrl_bits(p.id);
        if !(1..=5).contains(&kind) {
            return Err(invalid("Invalid packed child kind"));
        }
        if slots == 256 && usize::from(p.chr) != slot {
            return Err(invalid("Misplaced packed Node256 child"));
        }
        occupancy[slot / 8] |= 1 << (slot % 8);
        kinds[index / 2] |= (kind | if is_backptr(p.id) { 8 } else { 0 }) << ((index % 2) * 4);
        if is_backptr(p.id) || p.back_block != 0 {
            origins[index / 8] |= 1 << (index % 8);
        }
        index += 1;
    }
    bits::write_path_to_bytes(node.path_bytes(), writer)?;
    writer.write_all(&[width_code as u8])?;
    writer.write_all(&occupancy[..slots.div_ceil(8)])?;
    if slots <= 16 {
        for p in ptrs.iter().filter(|p| !p.is_empty()) {
            writer.write_all(&[p.chr])?;
        }
    } else if let TrieNodeType::Node48(n) = node {
        let mut selectors = [0u8; 192];
        for (edge, slot) in n.indexes().iter().enumerate() {
            let value = if *slot == -1 {
                63
            } else {
                let s =
                    usize::try_from(*slot).map_err(|_| invalid("Invalid packed Node48 slot"))?;
                if s >= 48 || ptrs[s].is_empty() || usize::from(ptrs[s].chr) != edge {
                    return Err(invalid("Invalid packed Node48 selector"));
                }
                s as u16
            };
            let bit = edge * 6;
            selectors[bit / 8] |= (value << (bit % 8)) as u8;
            if bit % 8 > 2 {
                selectors[bit / 8 + 1] |= (value >> (8 - bit % 8)) as u8;
            }
        }
        writer.write_all(&selectors)?;
    }
    writer.write_all(&kinds[..count.div_ceil(2)])?;
    writer.write_all(&origins[..count.div_ceil(8)])?;
    for p in ptrs.iter().filter(|p| !p.is_empty()) {
        writer.write_all(&p.ptr.to_le_bytes()[..target])?;
    }
    for p in ptrs
        .iter()
        .filter(|p| !p.is_empty() && (is_backptr(p.id) || p.back_block != 0))
    {
        writer.write_all(&p.back_block.to_le_bytes()[..origin])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Make a branch with reverse edge order, holey slots and squash annotations.
    fn sample(slots: usize, mode: usize, target: u64, origin: u32) -> TrieNodeType {
        let mut node = match slots {
            4 => TrieNodeType::Node4(TrieNode4::empty()),
            16 => TrieNodeType::Node16(TrieNode16::empty()),
            48 => TrieNodeType::Node48(Box::new(TrieNode48::empty())),
            _ => TrieNodeType::Node256(Box::new(TrieNode256::empty())),
        };
        for slot in 0..slots {
            if mode == 0 || (mode == 1 && slot % 3 == 1) {
                continue;
            }
            let edge = if slots == 256 {
                slot as u8
            } else {
                (255 - slot) as u8
            };
            node.ptrs_mut()[slot] = TriePtr {
                id: (slot % 5 + 1) as u8 | if slot % 3 == 0 { 0x80 } else { 0 },
                chr: edge,
                ptr: target,
                back_block: if slot % 3 == 2 { 0 } else { origin },
            };
            if let TrieNodeType::Node48(n) = &mut node {
                n.indexes[usize::from(edge)] = slot as i8;
            }
        }
        node
    }

    /// All widths, kinds, occupancies and edges reconstruct exact logical pointers.
    #[test]
    fn columns_roundtrip_and_direct_selection() {
        for slots in [4, 16, 48, 256] {
            for mode in 0..3 {
                for target in [
                    0,
                    65535,
                    65536,
                    16777215,
                    16777216,
                    u64::from(u32::MAX),
                    u64::from(u32::MAX) + 1,
                    u64::MAX,
                ] {
                    for origin in [0, 255, 256, 65535, 65536, 16777215, 16777216, u32::MAX] {
                        let node = sample(slots, mode, target, origin);
                        let mut bytes = vec![];
                        write_payload(&mut bytes, &node).unwrap();
                        let id = TrieNodeID::from_u8(node.id()).unwrap();
                        assert_eq!(bytes.len(), payload_len(&node));
                        assert!(bytes.len() <= max_payload_len(id).unwrap());
                        let view = PackedBranch::parse(id, &bytes).unwrap();
                        assert_eq!(view.byte_len(), bytes.len());
                        assert_eq!(view.to_owned_node().unwrap(), node);
                        for edge in 0..=255u8 {
                            assert_eq!(view.child(edge).unwrap(), node.walk(edge));
                        }
                    }
                }
            }
        }
    }

    /// A selected lookup need not decode an unrelated child's malformed kind.
    #[test]
    fn point_lookup_reads_only_selected_pointer() {
        let node = sample(256, 2, 42, 256);
        let mut bytes = vec![];
        write_payload(&mut bytes, &node).unwrap();
        // Empty path, width byte, 32-byte occupancy: first kind lives at byte 34.
        bytes[34] &= 0xf0;
        let view = PackedBranch::parse(TrieNodeID::Node256, &bytes).unwrap();
        assert_eq!(view.child(255).unwrap(), node.walk(255));
        assert!(view.child(0).is_err());
        assert!(view.to_owned_node().is_err());
    }

    /// Column bounds reject every truncation, flags and incomplete origins.
    #[test]
    fn truncated_and_malformed_columns() {
        for slots in [4, 16, 48, 256] {
            let node = sample(slots, 2, u64::MAX, u32::MAX);
            let id = TrieNodeID::from_u8(node.id()).unwrap();
            let mut bytes = vec![];
            write_payload(&mut bytes, &node).unwrap();
            for end in 0..bytes.len() {
                assert!(PackedBranch::parse(id, &bytes[..end]).is_err());
            }
            let mut corrupt = bytes.clone();
            corrupt[1] |= 0x10;
            assert!(PackedBranch::parse(id, &corrupt).is_err());
            let mut corrupt = bytes.clone();
            corrupt[0] = 33;
            assert!(PackedBranch::parse(id, &corrupt).is_err());
            if slots == 48 {
                // Selector edge zero is absent in this reverse-edge sample.
                let mut corrupt = bytes.clone();
                corrupt[8] = (corrupt[8] & 0xc0) | 48;
                let view = PackedBranch::parse(id, &corrupt).unwrap();
                assert!(view.child(0).is_err());
                assert!(view.to_owned_node().is_err());
            }
        }
        let node = sample(4, 2, 42, 0);
        let mut bytes = vec![];
        write_payload(&mut bytes, &node).unwrap();
        // 2-byte prefix + 1-byte occupancy + 4 selectors + 2 kind bytes.
        bytes[9] = 0;
        let view = PackedBranch::parse(TrieNodeID::Node4, &bytes).unwrap();
        assert!(view.child(255).is_err());
    }

    /// Packing reduces actual emitted bytes without per-child reservation padding.
    #[test]
    fn compact_columns_reduce_record_sizes() {
        for slots in [4, 16, 48, 256] {
            let node = sample(slots, 2, 4000, 1024);
            assert!(payload_len(&node) < mapped_node::payload_len(&node));
        }
    }
}

/// Borrowed branch dispatch retaining support for earlier directory records.
#[derive(Clone, Copy, Debug)]
pub enum BranchView<'a> {
    /// TypeFirstV1–V3 per-child offset directory.
    Directory(mapped_node::MappedBranch<'a>),
    /// TypeFirstV4 fixed-width pointer columns.
    Packed(PackedBranch<'a>),
}

impl<'a> BranchView<'a> {
    /// Select the database's explicitly versioned branch codec.
    pub fn parse(format: NodeRecordFormat, id: TrieNodeID, bytes: &'a [u8]) -> Result<Self, Error> {
        match format {
            NodeRecordFormat::TypeFirstV4 => PackedBranch::parse(id, bytes).map(Self::Packed),
            NodeRecordFormat::Legacy => Err(invalid("Legacy branch has no type-first view")),
            _ => mapped_node::MappedBranch::parse(id, bytes).map(Self::Directory),
        }
    }

    /// Select one child without materializing either branch representation.
    pub fn child(&self, edge: u8) -> Result<Option<TriePtr>, Error> {
        match self {
            Self::Directory(v) => v.child(edge),
            Self::Packed(v) => v.child(edge),
        }
    }

    /// Encoded branch payload size.
    pub fn byte_len(&self) -> usize {
        match self {
            Self::Directory(v) => v.byte_len(),
            Self::Packed(v) => v.byte_len(),
        }
    }

    /// Reconstruct logical slots when mutation or proof generation requires them.
    pub fn to_owned_node(&self) -> Result<TrieNodeType, Error> {
        match self {
            Self::Directory(v) => v.to_owned_node(),
            Self::Packed(v) => v.to_owned_node(),
        }
    }
}

#[cfg(test)]
mod record_tests {
    use super::*;
    use crate::chainstate::stacks::index::scratch::MarfReadState;
    use crate::chainstate::stacks::index::{BorrowedNodeBytes, ReadTrieNode};
    use stacks_common::types::chainstate::TrieHash;
    use std::io::{Cursor, Seek};

    /// The versioned record dispatch keeps mapped reads lazy and buffered reads exact.
    #[test]
    fn versioned_mapped_and_buffered_reads() {
        for path_len in [0, 1, 31, 32] {
            let mut branch = TrieNode16::empty();
            branch.path = NodePath::from_slice(&vec![9; path_len]).unwrap();
            branch.ptrs[7] = TriePtr {
                id: 0x81,
                chr: 144,
                ptr: 0x10001,
                back_block: 0xffffff,
            };
            let node = TrieNodeType::Node16(branch);
            for format in [
                NodeRecordFormat::TypeFirstV1,
                NodeRecordFormat::TypeFirstV2,
                NodeRecordFormat::TypeFirstV3,
                NodeRecordFormat::TypeFirstV4,
            ] {
                let mut bytes = vec![];
                format
                    .write_node(&mut bytes, &node, TrieHash([17; 32]), true)
                    .unwrap();
                assert_eq!(bytes.len(), format.node_len(&node, true));
                assert!(bytes.len() <= format.max_record_len(node.id()).unwrap());
                let record = format.parse(&bytes).unwrap();
                let read = ReadTrieNode::from_stable_bytes(
                    BorrowedNodeBytes::from_record(record),
                    record.hash,
                );
                assert_eq!(read.path_bytes().unwrap(), node.path_bytes());
                for edge in 0..=255 {
                    assert_eq!(read.walk(edge).unwrap(), node.walk(edge));
                }
                assert!(read.decoded_bytes.as_ref().unwrap().get().is_none());
                assert_eq!(read.ptrs().unwrap(), node.ptrs());
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
                .unwrap()
                .into_owned_node()
                .unwrap()
                .0;
                assert_eq!(buffered, node);
                assert_eq!(
                    input.stream_position().unwrap() as usize,
                    input.get_ref().len()
                );
            }
        }
    }
}
