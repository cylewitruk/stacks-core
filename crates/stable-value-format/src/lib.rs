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

//! Explicit little-endian codecs for the experimental stable-ID value generation.
//!
//! These codecs do not activate the format. A caller must bind every file to
//! one store ID and publish a complete generation before exposing its leaves.

use std::fmt;

pub mod files;

/// Bytes in one fixed-width value-directory entry.
pub const VALUE_ROW_BYTES: usize = 14;
/// Bytes in one fixed-width descriptor-directory entry.
pub const DESCRIPTOR_ROW_BYTES: usize = 8;
/// Bytes in a versioned file header.
pub const FILE_HEADER_BYTES: usize = 32;
/// Maximum partition length, including its header.
pub const MAX_PARTITION_BYTES: u64 = u32::MAX as u64 + 1;
/// Maximum accepted value record length, preserving the existing limit.
pub const MAX_RECORD_BYTES: u32 = 32 * 1024 * 1024;
/// Maximum current Binary V1 descriptor length.
pub const MAX_DESCRIPTOR_BYTES: u32 = 2 * 1024 * 1024 + 1;
/// Number of descriptor IDs assigned to each descriptor-data segment.
pub const DESCRIPTORS_PER_SEGMENT: u32 = 1024;

const VERSION: u32 = 1;
const VALUE_DATA_MAGIC: [u8; 8] = *b"SVIDVAL1";
const VALUE_DIR_MAGIC: [u8; 8] = *b"SVIDDIR1";
const DESCRIPTOR_DATA_MAGIC: [u8; 8] = *b"SVIDDSG1";
const DESCRIPTOR_DIR_MAGIC: [u8; 8] = *b"SVIDDDR1";

/// A malformed or unsupported stable-ID artifact.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FormatError {
    /// An input byte slice has the wrong length.
    Length,
    /// A field is outside the format's permitted range.
    Bounds,
    /// A version, file kind, or store identity disagrees.
    Identity,
    /// A legacy 40-byte Clarity commitment has nonzero extension bytes.
    Commitment,
    /// The finite ID or partition namespace is exhausted.
    Exhausted,
}

impl fmt::Display for FormatError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid stable-value format: {self:?}")
    }
}

impl std::error::Error for FormatError {}

/// Nonzero stable value identifier stored in an external MARF leaf.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ValueId(u32);

impl ValueId {
    /// Reject the reserved zero identifier.
    pub fn new(raw: u32) -> Result<Self, FormatError> {
        if raw == 0 {
            Err(FormatError::Bounds)
        } else {
            Ok(Self(raw))
        }
    }

    /// Return the persisted unsigned identifier.
    pub fn get(self) -> u32 {
        self.0
    }

    /// Allocate the next ID under a configurable limit, before mutating files.
    pub fn next_after(previous: u32, limit: u32) -> Result<Self, FormatError> {
        let next = previous.checked_add(1).ok_or(FormatError::Exhausted)?;
        if next > limit || next == 0 {
            return Err(FormatError::Exhausted);
        }
        Ok(Self(next))
    }
}

/// Nonzero interned descriptor identifier; zero in a value row means absent.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DescriptorId(u32);

impl DescriptorId {
    /// Reject the reserved zero identifier.
    pub fn new(raw: u32) -> Result<Self, FormatError> {
        if raw == 0 {
            Err(FormatError::Bounds)
        } else {
            Ok(Self(raw))
        }
    }

    /// Return the persisted unsigned identifier.
    pub fn get(self) -> u32 {
        self.0
    }

    /// Determine the deterministic data segment and zero-based slot within it.
    pub fn segment(self) -> (u32, u32) {
        (
            (self.0 - 1) / DESCRIPTORS_PER_SEGMENT,
            (self.0 - 1) % DESCRIPTORS_PER_SEGMENT,
        )
    }
}

/// Kind of a versioned generation file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileKind {
    /// One append-only partition of compact value records.
    ValueData,
    /// The fixed-width value-location directory.
    ValueDirectory,
    /// One segment of exact descriptor bytes.
    DescriptorData,
    /// The fixed-width descriptor-location directory.
    DescriptorDirectory,
}

impl FileKind {
    /// Select the distinct eight-byte discriminator for this file kind.
    fn magic(self) -> [u8; 8] {
        match self {
            Self::ValueData => VALUE_DATA_MAGIC,
            Self::ValueDirectory => VALUE_DIR_MAGIC,
            Self::DescriptorData => DESCRIPTOR_DATA_MAGIC,
            Self::DescriptorDirectory => DESCRIPTOR_DIR_MAGIC,
        }
    }
}

/// Versioned file header: magic, version, partition or segment number, UUID.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileHeader {
    /// The role of this file in the generation.
    pub kind: FileKind,
    /// Partition or descriptor-segment number; zero for a directory.
    pub number: u32,
    /// UUID binding related files to one Clarity value generation.
    pub store_id: [u8; 16],
}

impl FileHeader {
    /// Encode the header without persisting Rust layout or native endianness.
    pub fn encode(self) -> [u8; FILE_HEADER_BYTES] {
        let mut bytes = [0; FILE_HEADER_BYTES];
        bytes[..8].copy_from_slice(&self.kind.magic());
        bytes[8..12].copy_from_slice(&VERSION.to_le_bytes());
        bytes[12..16].copy_from_slice(&self.number.to_le_bytes());
        bytes[16..32].copy_from_slice(&self.store_id);
        bytes
    }

    /// Verify a file against the required kind, number and owning store.
    pub fn decode(
        bytes: &[u8],
        kind: FileKind,
        number: u32,
        store_id: [u8; 16],
    ) -> Result<Self, FormatError> {
        let bytes: &[u8; FILE_HEADER_BYTES] = bytes.try_into().map_err(|_| FormatError::Length)?;
        if bytes[..8] != kind.magic()
            || u32::from_le_bytes(bytes[8..12].try_into().unwrap()) != VERSION
            || u32::from_le_bytes(bytes[12..16].try_into().unwrap()) != number
            || bytes[16..32] != store_id
        {
            return Err(FormatError::Identity);
        }
        Ok(Self {
            kind,
            number,
            store_id,
        })
    }
}

/// Packed 14-byte mapping from one stable ID to a partition record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ValueDirectoryRow {
    /// Number of the values partition.
    pub partition: u16,
    /// Byte offset within that partition.
    pub offset: u32,
    /// Full compact record length, including its 32-byte commitment.
    pub record_length: u32,
    /// Zero for no descriptor; otherwise an interned ID.
    pub descriptor_id: u32,
}

impl ValueDirectoryRow {
    /// Encode the exact unpadded row.
    pub fn encode(self) -> [u8; VALUE_ROW_BYTES] {
        let mut bytes = [0; VALUE_ROW_BYTES];
        bytes[..2].copy_from_slice(&self.partition.to_le_bytes());
        bytes[2..6].copy_from_slice(&self.offset.to_le_bytes());
        bytes[6..10].copy_from_slice(&self.record_length.to_le_bytes());
        bytes[10..14].copy_from_slice(&self.descriptor_id.to_le_bytes());
        bytes
    }

    /// Parse an unaligned row and check its physical partition bounds.
    pub fn decode(bytes: &[u8], partition_length: u64) -> Result<Self, FormatError> {
        let bytes: &[u8; VALUE_ROW_BYTES] = bytes.try_into().map_err(|_| FormatError::Length)?;
        let row = Self {
            partition: u16::from_le_bytes(bytes[..2].try_into().unwrap()),
            offset: u32::from_le_bytes(bytes[2..6].try_into().unwrap()),
            record_length: u32::from_le_bytes(bytes[6..10].try_into().unwrap()),
            descriptor_id: u32::from_le_bytes(bytes[10..14].try_into().unwrap()),
        };
        let end = u64::from(row.offset) + u64::from(row.record_length);
        if row.offset < FILE_HEADER_BYTES as u32
            || !(33..=MAX_RECORD_BYTES).contains(&row.record_length)
            || partition_length > MAX_PARTITION_BYTES
            || end > partition_length
        {
            return Err(FormatError::Bounds);
        }
        Ok(row)
    }

    /// Byte offset of this row in the unpadded directory, using wide arithmetic.
    pub fn directory_offset(id: ValueId) -> u64 {
        FILE_HEADER_BYTES as u64 + (u64::from(id.get()) - 1) * VALUE_ROW_BYTES as u64
    }
}

/// Packed eight-byte descriptor location within its deterministic segment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DescriptorDirectoryRow {
    /// Byte offset within the descriptor data segment.
    pub offset: u32,
    /// Exact variable-length descriptor byte count.
    pub length: u32,
}

impl DescriptorDirectoryRow {
    /// Encode the exact unpadded row.
    pub fn encode(self) -> [u8; DESCRIPTOR_ROW_BYTES] {
        let mut bytes = [0; DESCRIPTOR_ROW_BYTES];
        bytes[..4].copy_from_slice(&self.offset.to_le_bytes());
        bytes[4..8].copy_from_slice(&self.length.to_le_bytes());
        bytes
    }

    /// Parse a row and check its data segment bounds.
    pub fn decode(bytes: &[u8], segment_length: u64) -> Result<Self, FormatError> {
        let bytes: &[u8; DESCRIPTOR_ROW_BYTES] =
            bytes.try_into().map_err(|_| FormatError::Length)?;
        let row = Self {
            offset: u32::from_le_bytes(bytes[..4].try_into().unwrap()),
            length: u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
        };
        let end = u64::from(row.offset) + u64::from(row.length);
        if row.offset < FILE_HEADER_BYTES as u32
            || row.length == 0
            || row.length > MAX_DESCRIPTOR_BYTES
            || segment_length > MAX_PARTITION_BYTES
            || end > segment_length
        {
            return Err(FormatError::Bounds);
        }
        Ok(row)
    }

    /// Byte offset of this row in the descriptor directory.
    pub fn directory_offset(id: DescriptorId) -> u64 {
        FILE_HEADER_BYTES as u64 + (u64::from(id.get()) - 1) * DESCRIPTOR_ROW_BYTES as u64
    }
}

/// Choose the next partition slot without ever crossing the 4-GiB file limit.
pub fn next_value_slot(
    partition: u16,
    length: u64,
    record_length: u32,
) -> Result<(u16, u32), FormatError> {
    if length < FILE_HEADER_BYTES as u64
        || length > MAX_PARTITION_BYTES
        || !(33..=MAX_RECORD_BYTES).contains(&record_length)
    {
        return Err(FormatError::Bounds);
    }
    if length + u64::from(record_length) <= MAX_PARTITION_BYTES && length <= u32::MAX as u64 {
        return Ok((partition, length as u32));
    }
    Ok((
        partition.checked_add(1).ok_or(FormatError::Exhausted)?,
        FILE_HEADER_BYTES as u32,
    ))
}

/// Convert the Clarity-specific 40-byte commitment to its stored digest.
pub fn compact_commitment(commitment: [u8; 40]) -> Result<[u8; 32], FormatError> {
    if commitment[32..].iter().any(|byte| *byte != 0) {
        return Err(FormatError::Commitment);
    }
    Ok(commitment[..32].try_into().unwrap())
}

/// Restore the exact logical 40-byte commitment from a compact record digest.
pub fn expand_commitment(digest: [u8; 32]) -> [u8; 40] {
    let mut commitment = [0; 40];
    commitment[..32].copy_from_slice(&digest);
    commitment
}

/// Borrow a compact value record's digest and Binary V1 payload.
pub fn decode_value_record(record: &[u8]) -> Result<([u8; 32], &[u8]), FormatError> {
    if !(33..=MAX_RECORD_BYTES as usize).contains(&record.len()) {
        return Err(FormatError::Length);
    }
    Ok((record[..32].try_into().unwrap(), &record[32..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden bytes and offsets remain independent of host alignment and endianness.
    #[test]
    fn exact_rows_and_large_directory_offsets() {
        let row = ValueDirectoryRow {
            partition: 0x1234,
            offset: 0x0102_0304,
            record_length: 0x0506_0708,
            descriptor_id: 0x090a_0b0c,
        };
        assert_eq!(
            row.encode(),
            [0x34, 0x12, 4, 3, 2, 1, 8, 7, 6, 5, 12, 11, 10, 9]
        );
        assert_eq!(
            ValueDirectoryRow::directory_offset(ValueId::new(u32::MAX).unwrap()),
            60_129_542_148
        );
        let descriptor = DescriptorDirectoryRow {
            offset: 0x0102_0304,
            length: 0x0506_0708,
        };
        assert_eq!(descriptor.encode(), [4, 3, 2, 1, 8, 7, 6, 5]);
        assert!(
            DescriptorDirectoryRow::directory_offset(DescriptorId::new(u32::MAX).unwrap())
                > u32::MAX as u64
        );
    }

    /// Header roles and store identities fail closed under swapped files.
    #[test]
    fn headers_bind_file_role_and_store() {
        let header = FileHeader {
            kind: FileKind::ValueData,
            number: 17,
            store_id: [7; 16],
        };
        let bytes = header.encode();
        assert_eq!(
            FileHeader::decode(&bytes, FileKind::ValueData, 17, [7; 16]),
            Ok(header)
        );
        assert_eq!(
            FileHeader::decode(&bytes, FileKind::DescriptorData, 17, [7; 16]),
            Err(FormatError::Identity)
        );
        assert_eq!(
            FileHeader::decode(&bytes, FileKind::ValueData, 17, [8; 16]),
            Err(FormatError::Identity)
        );
        assert_eq!(
            FileHeader::decode(&bytes[..31], FileKind::ValueData, 17, [7; 16]),
            Err(FormatError::Length)
        );
    }

    /// Directory parsing rejects truncation, oversized records and escaping boundaries.
    #[test]
    fn rows_validate_bounds() {
        let row = ValueDirectoryRow {
            partition: 1,
            offset: 32,
            record_length: 33,
            descriptor_id: 0,
        };
        assert_eq!(ValueDirectoryRow::decode(&row.encode(), 65), Ok(row));
        assert_eq!(
            ValueDirectoryRow::decode(&row.encode(), 64),
            Err(FormatError::Bounds)
        );
        assert_eq!(
            ValueDirectoryRow::decode(&row.encode()[..13], 65),
            Err(FormatError::Length)
        );
        let descriptor = DescriptorDirectoryRow {
            offset: 32,
            length: 67_588,
        };
        assert_eq!(
            DescriptorDirectoryRow::decode(&descriptor.encode(), 67_620),
            Ok(descriptor)
        );
        assert_eq!(
            DescriptorDirectoryRow::decode(&descriptor.encode(), 67_619),
            Err(FormatError::Bounds)
        );
    }

    /// ID and partition exhaustion are explicit before any writer mutation.
    #[test]
    fn finite_namespaces_and_rollover() {
        assert_eq!(ValueId::new(0), Err(FormatError::Bounds));
        assert_eq!(ValueId::next_after(3, 3), Err(FormatError::Exhausted));
        assert_eq!(
            ValueId::next_after(u32::MAX, u32::MAX),
            Err(FormatError::Exhausted)
        );
        assert_eq!(
            next_value_slot(1, MAX_PARTITION_BYTES - 33, 33),
            Ok((1, u32::MAX - 32))
        );
        assert_eq!(
            next_value_slot(1, MAX_PARTITION_BYTES - 32, 33),
            Ok((2, 32))
        );
        assert_eq!(
            next_value_slot(u16::MAX, MAX_PARTITION_BYTES, 33),
            Err(FormatError::Exhausted)
        );
        assert_eq!(DescriptorId::new(1025).unwrap().segment(), (1, 0));
    }

    /// The compact commitment never truncates a nonzero historical extension.
    #[test]
    fn commitment_parity_and_payload_boundary() {
        let mut logical = [0; 40];
        logical[..32].fill(9);
        let digest = compact_commitment(logical).unwrap();
        assert_eq!(expand_commitment(digest), logical);
        logical[39] = 1;
        assert_eq!(compact_commitment(logical), Err(FormatError::Commitment));
        let mut bytes = digest.to_vec();
        bytes.extend_from_slice(&[1, 2, 3]);
        assert_eq!(decode_value_record(&bytes), Ok((digest, &[1, 2, 3][..])));
        assert_eq!(decode_value_record(&bytes[..32]), Err(FormatError::Length));
    }
}
