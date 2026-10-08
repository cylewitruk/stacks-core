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

//! canonical Node256 metadata compaction with a direct writer and mmap column reads.

use std::io::Write;

use super::node::{clear_ctrl_bits, is_backptr, TrieNodeID, TrieNodeType, TriePtr};
use super::packed_branch::{self, PackedBranch};
use super::{bits, mapped_node, Error};

const FULL_OCCUPANCY: u8 = 1;
const SPARSE_KINDS: u8 = 2;
const SPARSE_ORIGINS: u8 = 4;
/// Fixed-size metadata for encoding one logical Node256 without an intermediate payload.
struct Node256Plan {
    /// Bytes per resolved child target.
    target_width: usize,
    /// Bytes per stored origin.
    origin_width: usize,
    /// Number of occupied children.
    count: usize,
    /// Number of children with an origin entry.
    origin_count: usize,
    /// Occupied physical slots.
    occupancy: [u8; 32],
    /// Packed logical kind and backpointer flags.
    kinds: [u8; 128],
    /// Origin presence in occupied-child order.
    origin_bitmap: [u8; 32],
    /// Most common kind when sparse kinds are selected.
    kind_default: u8,
    /// Positions differing from the common kind.
    kind_exceptions: [u8; 32],
    /// Packed kinds at exceptional positions.
    kind_values: [u8; 128],
    /// Number of exceptional kinds.
    kind_exception_count: usize,
    /// Most common origin-presence bit.
    origin_default: u8,
    /// Occupied-child positions differing from the common origin bit.
    origin_exceptions: [u8; 256],
    /// Number of exceptional origin bits.
    origin_exception_count: usize,
    /// Selected physical metadata modes.
    mode: u8,
    /// Exact payload size for the planned resolved targets.
    payload_len: usize,
}

impl Node256Plan {
    /// Choose the same compact metadata modes after optional target relocation.
    fn build(
        node: &TrieNodeType,
        mut resolve: impl FnMut(&TriePtr) -> Result<u64, Error>,
    ) -> Result<Self, Error> {
        if !matches!(node, TrieNodeType::Node256(_)) || node.ptrs().len() != 256 {
            return Err(invalid("Direct A writer requires Node256"));
        }
        if node.path_bytes().len() > u8::MAX as usize {
            return Err(invalid("Node256 path exceeds encoded length"));
        }
        let mut plan = Self {
            target_width: 2,
            origin_width: 1,
            count: 0,
            origin_count: 0,
            occupancy: [0; 32],
            kinds: [0; 128],
            origin_bitmap: [0; 32],
            kind_default: 0,
            kind_exceptions: [0; 32],
            kind_values: [0; 128],
            kind_exception_count: 0,
            origin_default: 0,
            origin_exceptions: [0; 256],
            origin_exception_count: 0,
            mode: 0,
            payload_len: 0,
        };
        let mut max_target = 0;
        let mut max_origin = 0;
        let mut frequencies = [0usize; 16];
        for (slot, ptr) in node.ptrs().iter().enumerate() {
            if ptr.is_empty() {
                continue;
            }
            if usize::from(ptr.chr) != slot {
                return Err(invalid("Misplaced packed Node256 child"));
            }
            let kind = clear_ctrl_bits(ptr.id);
            if !(1..=5).contains(&kind) {
                return Err(invalid("Invalid packed child kind"));
            }
            let encoded_kind = kind | if is_backptr(ptr.id) { 8 } else { 0 };
            plan.occupancy[slot / 8] |= 1 << (slot % 8);
            plan.kinds[plan.count / 2] |= encoded_kind << (4 * (plan.count % 2));
            frequencies[usize::from(encoded_kind)] += 1;
            if is_backptr(ptr.id) || ptr.back_block != 0 {
                plan.origin_bitmap[plan.count / 8] |= 1 << (plan.count % 8);
                plan.origin_count += 1;
            }
            plan.count += 1;
            max_target = max_target.max(resolve(ptr)?);
            max_origin = max_origin.max(ptr.back_block);
        }
        plan.target_width = match max_target {
            0..=0xffff => 2,
            0x10000..=0xffffff => 3,
            0x1000000..=0xffffffff => 4,
            _ => 8,
        };
        plan.origin_width = match max_origin {
            0..=0xff => 1,
            0x100..=0xffff => 2,
            0x10000..=0xffffff => 3,
            _ => 4,
        };
        if plan.occupancy.iter().all(|byte| *byte == 0xff) {
            plan.mode |= FULL_OCCUPANCY;
        }
        if plan.count != 0 {
            plan.kind_default = frequencies
                .iter()
                .enumerate()
                .max_by_key(|(_, frequency)| **frequency)
                .expect("nonempty frequencies")
                .0 as u8;
            plan.kind_exception_count = plan.count - frequencies[usize::from(plan.kind_default)];
            if 1 + 32 + plan.kind_exception_count.div_ceil(2) < plan.count.div_ceil(2) {
                plan.mode |= SPARSE_KINDS;
                let mut next = 0;
                for index in 0..plan.count {
                    let kind = (plan.kinds[index / 2] >> (4 * (index % 2))) & 15;
                    if kind != plan.kind_default {
                        plan.kind_exceptions[index / 8] |= 1 << (index % 8);
                        plan.kind_values[next / 2] |= kind << (4 * (next % 2));
                        next += 1;
                    }
                }
            }
        }
        plan.origin_default = u8::from(plan.origin_count * 2 >= plan.count);
        plan.origin_exception_count = plan.origin_count.min(plan.count - plan.origin_count);
        if 3 + plan.origin_exception_count < plan.count.div_ceil(8) {
            plan.mode |= SPARSE_ORIGINS;
            let mut next = 0;
            for index in 0..plan.count {
                let present = (plan.origin_bitmap[index / 8] >> (index % 8)) & 1;
                if present != plan.origin_default {
                    plan.origin_exceptions[next] = index as u8;
                    next += 1;
                }
            }
        }
        let path_len = 1 + node.path_bytes().len();
        let base_len = path_len
            + 1
            + 32
            + plan.count.div_ceil(2)
            + plan.count.div_ceil(8)
            + plan.count * plan.target_width
            + plan.origin_count * plan.origin_width;
        let compact_len = path_len
            + 2
            + if plan.mode & FULL_OCCUPANCY != 0 {
                0
            } else {
                32
            }
            + if plan.mode & SPARSE_KINDS != 0 {
                1 + 32 + plan.kind_exception_count.div_ceil(2)
            } else {
                plan.count.div_ceil(2)
            }
            + if plan.mode & SPARSE_ORIGINS != 0 {
                3 + plan.origin_exception_count
            } else {
                plan.count.div_ceil(8)
            }
            + plan.count * plan.target_width
            + plan.origin_count * plan.origin_width;
        if plan.mode == 0 || compact_len >= base_len {
            plan.mode = 0;
            plan.payload_len = base_len;
        } else {
            plan.payload_len = compact_len;
        }
        Ok(plan)
    }

    /// Emit exactly the planned payload from its resolved logical pointers.
    fn write<W: Write>(&self, writer: &mut W, node: &TrieNodeType) -> Result<(), Error> {
        if self.mode == 0 {
            return packed_branch::write_payload(writer, node);
        }
        bits::write_path_to_bytes(node.path_bytes(), writer)?;
        let target_code = match self.target_width {
            2 => 0,
            3 => 1,
            4 => 2,
            _ => 3,
        };
        let width_code = target_code | ((self.origin_width - 1) << 2);
        writer.write_all(&[(width_code as u8) | 0x80, self.mode])?;
        if self.mode & FULL_OCCUPANCY == 0 {
            writer.write_all(&self.occupancy)?;
        }
        if self.mode & SPARSE_KINDS != 0 {
            writer.write_all(&[self.kind_default])?;
            writer.write_all(&self.kind_exceptions)?;
            writer.write_all(&self.kind_values[..self.kind_exception_count.div_ceil(2)])?;
        } else {
            writer.write_all(&self.kinds[..self.count.div_ceil(2)])?;
        }
        if self.mode & SPARSE_ORIGINS != 0 {
            writer.write_all(&[self.origin_default])?;
            writer.write_all(&(self.origin_exception_count as u16).to_le_bytes())?;
            writer.write_all(&self.origin_exceptions[..self.origin_exception_count])?;
        } else {
            writer.write_all(&self.origin_bitmap[..self.count.div_ceil(8)])?;
        }
        for ptr in node.ptrs().iter().filter(|ptr| !ptr.is_empty()) {
            writer.write_all(&ptr.ptr.to_le_bytes()[..self.target_width])?;
        }
        for ptr in node
            .ptrs()
            .iter()
            .filter(|ptr| !ptr.is_empty() && (is_backptr(ptr.id) || ptr.back_block != 0))
        {
            writer.write_all(&ptr.back_block.to_le_bytes()[..self.origin_width])?;
        }
        Ok(())
    }
}

/// Encode a branch directly from logical pointers.
#[cfg(test)]
fn encode_node(node: &TrieNodeType) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::with_capacity(payload_len(node));
    write_payload(&mut bytes, node)?;
    Ok(bytes)
}

/// Return the exact payload size without constructing an encoded buffer.
pub fn payload_len(node: &TrieNodeType) -> usize {
    payload_len_with_targets(node, |ptr| Ok(ptr.ptr)).expect("valid branch in payload_len")
}

/// Size a branch after resolving child offsets, without cloning or serializing it.
pub fn payload_len_with_targets(
    node: &TrieNodeType,
    resolve: impl FnMut(&TriePtr) -> Result<u64, Error>,
) -> Result<usize, Error> {
    if matches!(node, TrieNodeType::Node256(_)) {
        Ok(Node256Plan::build(node, resolve)?.payload_len)
    } else {
        packed_branch::payload_len_with_targets(node, resolve)
    }
}

/// Write compact Node256 metadata and pointer columns directly to the destination.
pub fn write_payload<W: Write>(writer: &mut W, node: &TrieNodeType) -> Result<(), Error> {
    if matches!(node, TrieNodeType::Node256(_)) {
        Node256Plan::build(node, |ptr| Ok(ptr.ptr))?.write(writer, node)
    } else {
        packed_branch::write_payload(writer, node)
    }
}

/// Return a corruption error for a malformed compact branch.
fn invalid(message: &str) -> Error {
    Error::CorruptionError(message.into())
}

/// Read one checked byte span and advance a bounded cursor.
fn take<'a>(bytes: &'a [u8], cursor: &mut usize, len: usize) -> Result<&'a [u8], Error> {
    let end = cursor.checked_add(len).ok_or(Error::OverflowError)?;
    let result = bytes
        .get(*cursor..end)
        .ok_or_else(|| invalid("Truncated canonical branch column"))?;
    *cursor = end;
    Ok(result)
}

/// Select the packed branch capacity.
fn slots(id: TrieNodeID) -> Result<usize, Error> {
    match id {
        TrieNodeID::Node4 => Ok(4),
        TrieNodeID::Node16 => Ok(16),
        TrieNodeID::Node48 => Ok(48),
        TrieNodeID::Node256 => Ok(256),
        _ => Err(invalid("canonical codec requires a branch")),
    }
}

/// Count bits before a position in a checked compact bitmap.
fn rank(bitmap: &[u8], before: usize) -> usize {
    bitmap[..before / 8]
        .iter()
        .map(|byte| byte.count_ones() as usize)
        .sum::<usize>()
        + if before % 8 == 0 {
            0
        } else {
            (bitmap[before / 8] & ((1 << (before % 8)) - 1)).count_ones() as usize
        }
}

/// Extract the fixed target width from the low width-code bits.
fn target_width(code: u8) -> usize {
    [2, 3, 4, 8][usize::from(code & 3)]
}

/// Extract the fixed origin width from the low width-code bits.
fn origin_width(code: u8) -> usize {
    usize::from((code >> 2) & 3) + 1
}

/// Decode one little-endian fixed-width unsigned entry.
fn integer(bytes: &[u8], index: usize, width: usize) -> Result<u64, Error> {
    let start = index.checked_mul(width).ok_or(Error::OverflowError)?;
    let mut value = [0u8; 8];
    value[..width].copy_from_slice(
        bytes
            .get(start..start + width)
            .ok_or_else(|| invalid("Truncated canonical integer"))?,
    );
    Ok(u64::from_le_bytes(value))
}

/// Expand compact metadata for full-node mutation, sealing and proofs.
fn expand_packed(id: TrieNodeID, bytes: &[u8]) -> Result<Vec<u8>, Error> {
    let capacity = slots(id)?;
    let path = mapped_node::path_prefix(bytes)?;
    let mut cursor = 1 + path.len();
    let widths = take(bytes, &mut cursor, 1)?[0];
    if widths & 0x80 == 0 {
        let parsed = PackedBranch::parse(id, bytes)?;
        if parsed.byte_len() != bytes.len() {
            return Err(invalid("Trailing packed branch bytes"));
        }
        return Ok(bytes.to_vec());
    }
    if widths & 0x70 != 0 {
        return Err(invalid("Reserved canonical width bits"));
    }
    let mode = take(bytes, &mut cursor, 1)?[0];
    if mode == 0 || mode & !(FULL_OCCUPANCY | SPARSE_KINDS | SPARSE_ORIGINS) != 0 || capacity != 256
    {
        return Err(invalid("Unsupported canonical Node256 metadata mode"));
    }
    let mut occupancy = [0u8; 32];
    if mode & FULL_OCCUPANCY != 0 {
        occupancy.fill(0xff);
    } else {
        occupancy.copy_from_slice(take(bytes, &mut cursor, 32)?);
    }
    let count = rank(&occupancy, 256);
    let kinds = if mode & SPARSE_KINDS != 0 {
        let default = take(bytes, &mut cursor, 1)?[0];
        if default > 15 {
            return Err(invalid("Invalid canonical kind default"));
        }
        let bitmap = take(bytes, &mut cursor, 32)?;
        if rank(bitmap, 256) > count || rank(bitmap, 256) != rank(bitmap, count) {
            return Err(invalid("Invalid canonical kind exceptions"));
        }
        let exceptions = take(bytes, &mut cursor, rank(bitmap, count).div_ceil(2))?;
        let mut result = vec![0u8; count.div_ceil(2)];
        for index in 0..count {
            let kind = if bitmap[index / 8] & (1 << (index % 8)) != 0 {
                let number = rank(bitmap, index);
                (exceptions[number / 2] >> (4 * (number % 2))) & 15
            } else {
                default
            };
            result[index / 2] |= kind << (4 * (index % 2));
        }
        result
    } else {
        take(bytes, &mut cursor, count.div_ceil(2))?.to_vec()
    };
    let origin_bitmap = if mode & SPARSE_ORIGINS != 0 {
        let default = take(bytes, &mut cursor, 1)?[0];
        if default > 1 {
            return Err(invalid("Invalid sparse origin default"));
        }
        let count_bytes = take(bytes, &mut cursor, 2)?;
        let exceptions = u16::from_le_bytes(count_bytes.try_into().expect("two bytes")) as usize;
        if exceptions > count {
            return Err(invalid("Too many sparse origin exceptions"));
        }
        let indexes = take(bytes, &mut cursor, exceptions)?;
        let mut result = vec![if default == 1 { 0xff } else { 0 }; count.div_ceil(8)];
        if default == 1 && count % 8 != 0 {
            *result.last_mut().expect("nonempty bitmap") &= (1 << (count % 8)) - 1;
        }
        let mut previous = None;
        for &index in indexes {
            if usize::from(index) >= count || previous.is_some_and(|old| index <= old) {
                return Err(invalid("Unsorted sparse origin exception"));
            }
            result[usize::from(index) / 8] ^= 1 << (index % 8);
            previous = Some(index);
        }
        result
    } else {
        take(bytes, &mut cursor, count.div_ceil(8))?.to_vec()
    };
    let targets = take(bytes, &mut cursor, count * target_width(widths))?;
    let origins = take(
        bytes,
        &mut cursor,
        rank(&origin_bitmap, count) * origin_width(widths),
    )?;
    if cursor != bytes.len() {
        return Err(invalid("Trailing canonical branch bytes"));
    }
    let mut result = Vec::with_capacity(bytes.len() + 128);
    result.push(path.len() as u8);
    result.extend_from_slice(path);
    result.push(widths & 0x0f);
    result.extend_from_slice(&occupancy[..capacity.div_ceil(8)]);
    result.extend_from_slice(&kinds);
    result.extend_from_slice(&origin_bitmap);
    result.extend_from_slice(&targets);
    result.extend_from_slice(&origins);
    let parsed = PackedBranch::parse(id, &result)?;
    if parsed.byte_len() != result.len() {
        return Err(invalid("Invalid reconstructed packed branch"));
    }
    Ok(result)
}

/// Direct selected-child reader for an compact branch payload.
#[derive(Clone, Copy, Debug)]
pub enum BranchView<'a> {
    /// An unchanged packed payload.
    Packed(PackedBranch<'a>),
    /// Selected child access over compact canonical columns.
    Compact(CompactBranch<'a>),
}

/// Borrowed compact metadata with direct target/origin columns.
#[derive(Clone, Copy, Debug)]
pub struct CompactBranch<'a> {
    /// Exact encoded record bytes for full-node decoding.
    payload: &'a [u8],
    /// Physical branch kind.
    id: TrieNodeID,
    /// Validated compact metadata flags.
    mode: u8,
    /// Physical child occupancy bitmap.
    occupancy: [u8; 32],
    /// Packed kinds or sparse exception kinds.
    kinds: &'a [u8],
    /// Common kind for sparse metadata.
    kind_default: u8,
    /// Occupied-child positions whose kinds differ.
    kind_bitmap: &'a [u8],
    /// Origin presence when stored densely.
    origin_bitmap: &'a [u8],
    /// Common origin-presence bit.
    origin_default: bool,
    /// Sorted positions differing from the common origin bit.
    origin_exceptions: &'a [u8],
    /// Borrowed target pointer column.
    targets: &'a [u8],
    /// Borrowed ancestor identifier column.
    origins: &'a [u8],
    /// Bytes per target pointer.
    target_width: usize,
    /// Bytes per ancestor identifier.
    origin_width: usize,
    /// Exact encoded payload length.
    length: usize,
}

impl<'a> BranchView<'a> {
    /// Parse one record from a bounded read window that may include the next record.
    pub fn parse_prefix(id: TrieNodeID, bytes: &'a [u8]) -> Result<Self, Error> {
        let capacity = slots(id)?;
        let path = mapped_node::path_prefix(bytes)?;
        let mut cursor = 1 + path.len();
        let widths = take(bytes, &mut cursor, 1)?[0];
        if widths & 0x80 == 0 {
            let view = PackedBranch::parse(id, bytes)?;
            return Ok(Self::Packed(view));
        }
        if widths & 0x70 != 0 {
            return Err(invalid("Reserved canonical width bits"));
        }
        let mode = take(bytes, &mut cursor, 1)?[0];
        if mode == 0
            || mode & !(FULL_OCCUPANCY | SPARSE_KINDS | SPARSE_ORIGINS) != 0
            || capacity != 256
        {
            return Err(invalid("Unsupported canonical Node256 metadata mode"));
        }
        let mut occupancy = [0u8; 32];
        if mode & FULL_OCCUPANCY != 0 {
            occupancy.fill(0xff);
        } else {
            occupancy.copy_from_slice(take(bytes, &mut cursor, 32)?);
        }
        let count = rank(&occupancy, 256);
        let (kinds, kind_default, kind_bitmap) = if mode & SPARSE_KINDS != 0 {
            let default = take(bytes, &mut cursor, 1)?[0];
            if default > 15 {
                return Err(invalid("Invalid sparse kind default"));
            }
            let bitmap = take(bytes, &mut cursor, 32)?;
            if rank(bitmap, 256) != rank(bitmap, count) {
                return Err(invalid("Sparse kind exception beyond child count"));
            }
            let packed = take(bytes, &mut cursor, rank(bitmap, count).div_ceil(2))?;
            (packed, default, bitmap)
        } else {
            (take(bytes, &mut cursor, count.div_ceil(2))?, 0, &[][..])
        };
        let (origin_bitmap, origin_default, origin_exceptions, origin_count) =
            if mode & SPARSE_ORIGINS != 0 {
                let default = take(bytes, &mut cursor, 1)?[0];
                if default > 1 {
                    return Err(invalid("Invalid sparse origin default"));
                }
                let len =
                    u16::from_le_bytes(take(bytes, &mut cursor, 2)?.try_into().expect("two bytes"))
                        as usize;
                if len > count {
                    return Err(invalid("Too many sparse origin exceptions"));
                }
                let exceptions = take(bytes, &mut cursor, len)?;
                let mut previous = None;
                for &index in exceptions {
                    if usize::from(index) >= count || previous.is_some_and(|old| index <= old) {
                        return Err(invalid("Unsorted sparse origin exception"));
                    }
                    previous = Some(index);
                }
                (
                    &[][..],
                    default == 1,
                    exceptions,
                    if default == 1 { count - len } else { len },
                )
            } else {
                let bitmap = take(bytes, &mut cursor, count.div_ceil(8))?;
                (bitmap, false, &[][..], rank(bitmap, count))
            };
        let target_width = target_width(widths);
        let targets = take(bytes, &mut cursor, count * target_width)?;
        let origin_width = origin_width(widths);
        let origins = take(bytes, &mut cursor, origin_count * origin_width)?;
        Ok(Self::Compact(CompactBranch {
            payload: &bytes[..cursor],
            id,
            mode,
            occupancy,
            kinds,
            kind_default,
            kind_bitmap,
            origin_bitmap,
            origin_default,
            origin_exceptions,
            targets,
            origins,
            target_width,
            origin_width,
            length: cursor,
        }))
    }

    /// Parse a record whose caller knows its exact byte length.
    pub fn parse(id: TrieNodeID, bytes: &'a [u8]) -> Result<Self, Error> {
        let view = Self::parse_prefix(id, bytes)?;
        if view.byte_len() != bytes.len() {
            return Err(invalid("Trailing canonical branch bytes"));
        }
        Ok(view)
    }

    /// Return the encoded payload length.
    pub fn byte_len(&self) -> usize {
        match self {
            Self::Packed(view) => view.byte_len(),
            Self::Compact(view) => view.length,
        }
    }

    /// Read just the requested child and its origin, if present.
    pub fn child(&self, edge: u8) -> Result<Option<TriePtr>, Error> {
        match self {
            Self::Packed(view) => view.child(edge),
            Self::Compact(view) => view.child(edge),
        }
    }

    /// Materialize logical slots for mutation, sealing, or proof generation.
    pub fn to_owned_node(&self) -> Result<TrieNodeType, Error> {
        match self {
            Self::Packed(view) => view.to_owned_node(),
            Self::Compact(view) => {
                let packed = expand_packed(view.id, view.payload)?;
                PackedBranch::parse(view.id, &packed)?.to_owned_node()
            }
        }
    }
}

impl CompactBranch<'_> {
    /// Resolve a selected edge to the occupied-slot ordinal.
    fn child_index(&self, edge: u8) -> Result<Option<usize>, Error> {
        if self.occupancy[usize::from(edge) / 8] & (1 << (edge % 8)) == 0 {
            return Ok(None);
        }
        Ok(Some(rank(&self.occupancy, usize::from(edge))))
    }

    /// Decode one selected child without materializing other pointers.
    fn child(&self, edge: u8) -> Result<Option<TriePtr>, Error> {
        let Some(index) = self.child_index(edge)? else {
            return Ok(None);
        };
        let kind = if self.mode & SPARSE_KINDS != 0 {
            if self.kind_bitmap[index / 8] & (1 << (index % 8)) == 0 {
                self.kind_default
            } else {
                let exception = rank(self.kind_bitmap, index);
                (self.kinds[exception / 2] >> (4 * (exception % 2))) & 15
            }
        } else {
            (self.kinds[index / 2] >> (4 * (index % 2))) & 15
        };
        let id = kind & 7;
        if !(1..=5).contains(&id) {
            return Err(invalid("Invalid canonical child kind"));
        }
        let (has_origin, origin_rank) = if self.mode & SPARSE_ORIGINS != 0 {
            let before = self
                .origin_exceptions
                .partition_point(|candidate| usize::from(*candidate) < index);
            let exception = self
                .origin_exceptions
                .get(before)
                .is_some_and(|value| usize::from(*value) == index);
            (
                self.origin_default ^ exception,
                if self.origin_default {
                    index - before
                } else {
                    before
                },
            )
        } else {
            (
                self.origin_bitmap[index / 8] & (1 << (index % 8)) != 0,
                rank(self.origin_bitmap, index),
            )
        };
        if kind & 8 != 0 && !has_origin {
            return Err(invalid("Missing canonical backpointer origin"));
        }
        let ptr = integer(self.targets, index, self.target_width)?;
        let back_block = if has_origin {
            u32::try_from(integer(self.origins, origin_rank, self.origin_width)?)
                .map_err(|_| Error::OverflowError)?
        } else {
            0
        };
        Ok(Some(TriePtr {
            id: id | if kind & 8 != 0 { 0x80 } else { 0 },
            chr: edge,
            ptr,
            back_block,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Checked borrowed packed branch columns used only by the independent test oracle.
    struct PackedColumns<'a> {
        path: &'a [u8],
        widths: u8,
        occupancy: &'a [u8],
        kinds: &'a [u8],
        origin_bitmap: &'a [u8],
        targets: &'a [u8],
        origins: &'a [u8],
        count: usize,
        origin_count: usize,
    }

    /// Inspect one packed payload without materializing a trie node.
    fn packed_columns(id: TrieNodeID, bytes: &[u8]) -> Result<PackedColumns<'_>, Error> {
        let parsed = PackedBranch::parse(id, bytes)?;
        if parsed.byte_len() != bytes.len() {
            return Err(invalid("Trailing packed branch bytes"));
        }
        let capacity = slots(id)?;
        let path = mapped_node::path_prefix(bytes)?;
        let mut cursor = 1 + path.len();
        let widths = take(bytes, &mut cursor, 1)?[0];
        let occupancy = take(bytes, &mut cursor, capacity.div_ceil(8))?;
        let count = rank(occupancy, capacity);
        let _selectors = take(
            bytes,
            &mut cursor,
            match capacity {
                4 | 16 => count,
                48 => 192,
                _ => 0,
            },
        )?;
        let kinds = take(bytes, &mut cursor, count.div_ceil(2))?;
        let origin_bitmap = take(bytes, &mut cursor, count.div_ceil(8))?;
        let origin_count = rank(origin_bitmap, count);
        let targets = take(bytes, &mut cursor, count * target_width(widths))?;
        let origins = take(bytes, &mut cursor, origin_count * origin_width(widths))?;
        Ok(PackedColumns {
            path,
            widths,
            occupancy,
            kinds,
            origin_bitmap,
            targets,
            origins,
            count,
            origin_count,
        })
    }

    /// Compute the sparse kind bitmap and packed exception nibbles.
    fn sparse_kinds(columns: &PackedColumns<'_>) -> Option<(u8, [u8; 32], Vec<u8>)> {
        if columns.count == 0 {
            return None;
        }
        let mut frequencies = [0usize; 16];
        for index in 0..columns.count {
            frequencies[usize::from((columns.kinds[index / 2] >> (4 * (index % 2))) & 15)] += 1;
        }
        let default = frequencies
            .iter()
            .enumerate()
            .max_by_key(|(_, frequency)| **frequency)?
            .0 as u8;
        let exceptions = columns.count - frequencies[usize::from(default)];
        if 1 + 32 + exceptions.div_ceil(2) >= columns.kinds.len() {
            return None;
        }
        let mut bitmap = [0u8; 32];
        let mut values = vec![0u8; exceptions.div_ceil(2)];
        let mut next = 0;
        for index in 0..columns.count {
            let value = (columns.kinds[index / 2] >> (4 * (index % 2))) & 15;
            if value != default {
                bitmap[index / 8] |= 1 << (index % 8);
                values[next / 2] |= value << (4 * (next % 2));
                next += 1;
            }
        }
        Some((default, bitmap, values))
    }

    /// Encode the minority origin bits as sorted occupied-child indexes.
    fn sparse_origins(columns: &PackedColumns<'_>) -> Option<(u8, Vec<u8>)> {
        let default = u8::from(columns.origin_count * 2 >= columns.count);
        let exceptions = columns
            .origin_count
            .min(columns.count - columns.origin_count);
        if 3 + exceptions >= columns.origin_bitmap.len() {
            return None;
        }
        let mut indexes = Vec::with_capacity(exceptions);
        for index in 0..columns.count {
            let present = (columns.origin_bitmap[index / 8] >> (index % 8)) & 1;
            if present != default {
                indexes.push(index as u8);
            }
        }
        Some((default, indexes))
    }

    /// Reference A encoder used only to compare the direct writer's physical bytes.
    fn reference_a(id: TrieNodeID, bytes: &[u8]) -> Result<Vec<u8>, Error> {
        if id != TrieNodeID::Node256 {
            return Ok(bytes.to_vec());
        }
        let columns = packed_columns(id, bytes)?;
        let full = columns.occupancy.iter().all(|b| *b == 255);
        let kinds = sparse_kinds(&columns);
        let origins = sparse_origins(&columns);
        let mode = u8::from(full)
            | if kinds.is_some() { SPARSE_KINDS } else { 0 }
            | if origins.is_some() { SPARSE_ORIGINS } else { 0 };
        if mode == 0 {
            return Ok(bytes.to_vec());
        }
        let mut result = vec![columns.path.len() as u8];
        result.extend_from_slice(columns.path);
        result.extend_from_slice(&[columns.widths | 0x80, mode]);
        if !full {
            result.extend_from_slice(columns.occupancy);
        }
        if let Some((default, bitmap, values)) = kinds {
            result.push(default);
            result.extend_from_slice(&bitmap);
            result.extend_from_slice(&values);
        } else {
            result.extend_from_slice(columns.kinds);
        }
        if let Some((default, indexes)) = origins {
            result.push(default);
            result.extend_from_slice(&(indexes.len() as u16).to_le_bytes());
            result.extend_from_slice(&indexes);
        } else {
            result.extend_from_slice(columns.origin_bitmap);
        }
        result.extend_from_slice(columns.targets);
        result.extend_from_slice(columns.origins);
        Ok(if result.len() < bytes.len() {
            result
        } else {
            bytes.to_vec()
        })
    }

    use crate::chainstate::stacks::index::node::{
        TrieNode, TrieNode256, TrieNode48, TrieNodeType, TriePtr,
    };

    /// Build a branch with high absolute offsets, clustered origins and holes.
    fn sample(id: TrieNodeID, count: usize) -> TrieNodeType {
        let mut node = match id {
            TrieNodeID::Node48 => TrieNodeType::Node48(Box::new(TrieNode48::empty())),
            TrieNodeID::Node256 => TrieNodeType::Node256(Box::new(TrieNode256::empty())),
            _ => unreachable!(),
        };
        for slot in 0..count {
            let edge = if id == TrieNodeID::Node48 {
                (255 - slot * 3) as u8
            } else {
                slot as u8
            };
            let ptr = TriePtr {
                id: if slot % 9 == 0 { 0x81 } else { 5 },
                chr: edge,
                ptr: u64::from(u32::MAX) + 512 + slot as u64,
                back_block: if slot % 9 == 0 {
                    9_000_000 + slot as u32
                } else {
                    0
                },
            };
            node.ptrs_mut()[slot] = ptr;
            if let TrieNodeType::Node48(branch) = &mut node {
                branch.indexes[usize::from(edge)] = slot as i8;
            }
        }
        node
    }

    /// Direct A planning and writing preserve the established physical bytes.
    #[test]
    fn direct_node256_a_matches_legacy_transform() {
        for count in [0, 1, 17, 63, 128, 173, 255, 256] {
            for case in 0..8u64 {
                let mut node = sample(TrieNodeID::Node256, count);
                for (slot, ptr) in node.ptrs_mut().iter_mut().enumerate() {
                    if ptr.is_empty() || (case == 7 && slot % 3 == 0) {
                        *ptr = TriePtr::default();
                        continue;
                    }
                    ptr.ptr = [
                        0,
                        0xffff,
                        0x10000,
                        0xffffff,
                        0x1000000,
                        u32::MAX as u64,
                        u32::MAX as u64 + 1,
                        u64::MAX - 256,
                    ][case as usize]
                        + slot as u64;
                    ptr.id = if case % 3 == 0 && slot % 11 == 0 {
                        0x81
                    } else {
                        5
                    };
                    ptr.back_block = if case % 2 == 0 && slot % 7 == 0 {
                        123_456
                    } else {
                        0
                    };
                }
                let mut v4 = Vec::new();
                packed_branch::write_payload(&mut v4, &node).unwrap();
                let expected = reference_a(TrieNodeID::Node256, &v4).unwrap();
                let actual = encode_node(&node).unwrap();
                assert_eq!(actual, expected, "count {count}, case {case}");
                assert_eq!(
                    actual.len(),
                    Node256Plan::build(&node, |ptr| Ok(ptr.ptr))
                        .unwrap()
                        .payload_len
                );
                let relocated_len =
                    payload_len_with_targets(&node, |ptr| Ok(ptr.ptr.wrapping_add(257))).unwrap();
                let mut relocated = node.clone();
                for ptr in relocated
                    .ptrs_mut()
                    .iter_mut()
                    .filter(|ptr| !ptr.is_empty())
                {
                    ptr.ptr = ptr.ptr.wrapping_add(257);
                }
                assert_eq!(relocated_len, encode_node(&relocated).unwrap().len());
                let view = BranchView::parse(TrieNodeID::Node256, &actual).unwrap();
                for edge in 0..=255 {
                    assert_eq!(view.child(edge).unwrap(), node.walk(edge));
                }
                assert_eq!(view.to_owned_node().unwrap(), node);
                let restored = expand_packed(TrieNodeID::Node256, &actual).unwrap();
                assert_eq!(restored, v4);
            }
        }
        let node48 = sample(TrieNodeID::Node48, 37);
        let mut v4 = Vec::new();
        packed_branch::write_payload(&mut v4, &node48).unwrap();
        assert_eq!(encode_node(&node48).unwrap(), v4);
    }

    /// Truncation, incompatible experimental modes and reserved bits fail closed.
    #[test]
    fn compact_metadata_rejects_invalid_records() {
        let node = sample(TrieNodeID::Node256, 256);
        let bytes = encode_node(&node).unwrap();
        assert_ne!(bytes[1] & 0x80, 0);
        for length in 0..bytes.len() {
            assert!(BranchView::parse(TrieNodeID::Node256, &bytes[..length]).is_err());
        }
        for mode in [0, 8, 16, 32, 64, 128, 255] {
            let mut invalid = bytes.clone();
            invalid[2] = mode;
            assert!(BranchView::parse(TrieNodeID::Node256, &invalid).is_err());
            assert!(expand_packed(TrieNodeID::Node256, &invalid).is_err());
        }
        let mut reserved = bytes.clone();
        reserved[1] |= 0x10;
        assert!(BranchView::parse(TrieNodeID::Node256, &reserved).is_err());
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(BranchView::parse(TrieNodeID::Node256, &trailing).is_err());
        assert_eq!(
            BranchView::parse_prefix(TrieNodeID::Node256, &trailing)
                .unwrap()
                .byte_len(),
            bytes.len()
        );
        assert!(BranchView::parse(TrieNodeID::Node48, &bytes).is_err());
        assert!(super::packed_branch::BranchView::parse(
            crate::chainstate::stacks::index::record::NodeRecordFormat::Legacy,
            TrieNodeID::Node256,
            &bytes
        )
        .is_err());
    }
}
