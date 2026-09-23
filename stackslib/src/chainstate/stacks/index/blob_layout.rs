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

//! Shared constants for the MARF trie blob header.
//!
//! ```text
//! offset  size  field
//!      0    32  parent_hash         (T::sentinel for the squash blob)
//!     32     4  reserved            (historically a local block-id; written as 0u32 LE)
//!     36   ...  root node           (first 32 bytes are its TrieHash)
//! ```
//!
//! Type-first stores use a checked version tag and put the root marker before its hash.

use std::io::{Read, Seek, SeekFrom, Write};

use stacks_common::types::chainstate::{
    TrieHash, BLOCK_HEADER_HASH_ENCODED_SIZE, TRIEHASH_ENCODED_SIZE,
};

use super::node::TrieNodeID;
use super::record::NodeRecordFormat;
use crate::chainstate::stacks::index::{Error, MarfTrieId};

/// Offset of the reserved 4-byte field.
pub const RESERVED_FIELD_OFFSET: usize = BLOCK_HEADER_HASH_ENCODED_SIZE;

/// Length of the reserved field.
pub const RESERVED_FIELD_LEN: usize = 4;

/// Offset where the root node, and therefore the root hash, begins.
pub const ROOT_NODE_OFFSET: usize = RESERVED_FIELD_OFFSET + RESERVED_FIELD_LEN;

/// Bytes needed to read `parent_hash || reserved || root_hash`.
pub const READER_PREFIX_LEN: usize = ROOT_NODE_OFFSET + TRIEHASH_ENCODED_SIZE;

// If these values change, update the blob writers in `storage.rs`
// (`TrieRAM::dump_consume`, `TrieRAM::dump_compressed_consume`) and
// [`BlobHeader::parse`] below.
const _: () = {
    assert!(BLOCK_HEADER_HASH_ENCODED_SIZE == 32);
    assert!(TRIEHASH_ENCODED_SIZE == 32);
    assert!(RESERVED_FIELD_OFFSET == 32);
    assert!(ROOT_NODE_OFFSET == 36);
    assert!(READER_PREFIX_LEN == 68);
};

/// The parsed fixed-layout prefix of a trie blob (see the module doc):
/// the parent block hash and the trie root hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BlobHeader<T> {
    pub parent_hash: T,
    pub root_hash: TrieHash,
}

impl<T: MarfTrieId> BlobHeader<T> {
    /// Parse the first [`READER_PREFIX_LEN`] bytes of a trie blob.
    pub(super) fn parse(buf: &[u8; READER_PREFIX_LEN]) -> BlobHeader<T> {
        let mut parent_bytes = [0u8; BLOCK_HEADER_HASH_ENCODED_SIZE];
        parent_bytes.copy_from_slice(&buf[..BLOCK_HEADER_HASH_ENCODED_SIZE]);
        let mut root_bytes = [0u8; TRIEHASH_ENCODED_SIZE];
        root_bytes
            .copy_from_slice(&buf[ROOT_NODE_OFFSET..ROOT_NODE_OFFSET + TRIEHASH_ENCODED_SIZE]);
        BlobHeader {
            parent_hash: T::from_bytes(parent_bytes),
            root_hash: TrieHash(root_bytes),
        }
    }
}

/// Version tag checked only after database metadata selects the type-first format.
const TYPE_FIRST_TAG: [u8; 4] = *b"MRF\x01";

/// Largest prefix needed to read a parent identity and stored root hash.
pub const MAX_READER_PREFIX_LEN: usize = READER_PREFIX_LEN + 1;

impl NodeRecordFormat {
    /// Number of bytes needed for a root-hash probe in this layout.
    pub const fn reader_prefix_len(self) -> usize {
        match self {
            Self::Legacy => READER_PREFIX_LEN,
            Self::TypeFirstV1 | Self::TypeFirstV2 | Self::TypeFirstV3 | Self::TypeFirstV4 => {
                MAX_READER_PREFIX_LEN
            }
        }
    }

    /// Write the parent identity and explicit layout tag before a trie root.
    pub fn write_trie_header<T: MarfTrieId, W: Write>(
        self,
        writer: &mut W,
        parent: &T,
    ) -> Result<(), Error> {
        writer.write_all(parent.as_bytes())?;
        writer.write_all(&match self {
            Self::Legacy => [0; 4],
            Self::TypeFirstV1 => TYPE_FIRST_TAG,
            Self::TypeFirstV2 => *b"MRF\x02",
            Self::TypeFirstV3 => *b"MRF\x03",
            Self::TypeFirstV4 => *b"MRF\x04",
        })?;
        Ok(())
    }

    /// Check a newly opened inline blob before reading records from it.
    pub fn validate_reader_header<R: Read + Seek>(self, reader: &mut R) -> Result<(), Error> {
        if self == Self::Legacy {
            return Ok(());
        }
        reader.seek(SeekFrom::Start(RESERVED_FIELD_OFFSET as u64))?;
        let mut tag = [0u8; RESERVED_FIELD_LEN];
        reader.read_exact(&mut tag)?;
        if tag != [b'M', b'R', b'F', self.version()] {
            return Err(Error::CorruptionError(
                "Trie layout version disagrees with database metadata".into(),
            ));
        }
        Ok(())
    }

    /// Check the selected format without interpreting historical local block IDs as versions.
    pub fn validate_trie_header(self, bytes: &[u8]) -> Result<(), Error> {
        let reserved = bytes
            .get(RESERVED_FIELD_OFFSET..ROOT_NODE_OFFSET)
            .ok_or_else(|| Error::CorruptionError("Truncated trie header".into()))?;
        if self.is_type_first() && reserved != [b'M', b'R', b'F', self.version()] {
            return Err(Error::CorruptionError(
                "Trie layout version disagrees with database metadata".into(),
            ));
        }
        Ok(())
    }
}

impl<T: MarfTrieId> BlobHeader<T> {
    /// Read a root hash using an explicitly selected layout.
    pub fn parse_format(format: NodeRecordFormat, bytes: &[u8]) -> Result<Self, Error> {
        format.validate_trie_header(bytes)?;
        let parent_bytes = bytes
            .get(..BLOCK_HEADER_HASH_ENCODED_SIZE)
            .ok_or_else(|| Error::CorruptionError("Truncated parent identity".into()))?;
        let hash_start = ROOT_NODE_OFFSET + usize::from(format.is_type_first());
        if format.is_type_first() {
            let record =
                format.parse(bytes.get(ROOT_NODE_OFFSET..).ok_or(Error::OverflowError)?)?;
            if !matches!(
                record.logical_type(),
                TrieNodeID::Node256 | TrieNodeID::Patch
            ) {
                return Err(Error::CorruptionError(
                    "Trie root is not a Node256 or root patch".into(),
                ));
            }
        }
        let root_bytes = bytes
            .get(hash_start..hash_start + TRIEHASH_ENCODED_SIZE)
            .ok_or_else(|| Error::CorruptionError("Truncated trie root hash".into()))?;
        Ok(Self {
            parent_hash: T::from_bytes(parent_bytes.try_into().expect("checked parent width")),
            root_hash: TrieHash(root_bytes.try_into().expect("checked hash width")),
        })
    }
}

#[cfg(test)]
mod format_tests {
    use super::*;
    use crate::chainstate::stacks::index::node::{TrieNode, TrieNode256, TrieNodeType};
    use stacks_common::types::chainstate::StacksBlockId;

    /// Old reserved block IDs are never interpreted as layout versions.
    #[test]
    fn legacy_reserved_bytes_remain_opaque() {
        let mut bytes = [0; READER_PREFIX_LEN];
        bytes[RESERVED_FIELD_OFFSET..ROOT_NODE_OFFSET].copy_from_slice(&TYPE_FIRST_TAG);
        bytes[ROOT_NODE_OFFSET..].fill(9);
        let header =
            BlobHeader::<StacksBlockId>::parse_format(NodeRecordFormat::Legacy, &bytes).unwrap();
        assert_eq!(header.root_hash, TrieHash([9; 32]));
        assert!(
            BlobHeader::<StacksBlockId>::parse_format(NodeRecordFormat::TypeFirstV1, &bytes)
                .is_err()
        );
    }

    /// Versioned headers retain the root's node offset while shifting its stored hash by one byte.
    #[test]
    fn versioned_root_prefix_is_checked() {
        let format = NodeRecordFormat::TypeFirstV1;
        let parent = StacksBlockId([2; 32]);
        let mut bytes = Vec::new();
        format.write_trie_header(&mut bytes, &parent).unwrap();
        assert_eq!(bytes.len(), ROOT_NODE_OFFSET);
        format
            .write_node(
                &mut bytes,
                &TrieNodeType::Node256(Box::new(TrieNode256::empty())),
                TrieHash([3; 32]),
                true,
            )
            .unwrap();
        let header =
            BlobHeader::<StacksBlockId>::parse_format(format, &bytes[..format.reader_prefix_len()])
                .unwrap();
        assert_eq!(header.parent_hash, parent);
        assert_eq!(header.root_hash, TrieHash([3; 32]));
        bytes[RESERVED_FIELD_OFFSET + 3] = 2;
        assert!(BlobHeader::<StacksBlockId>::parse_format(format, &bytes).is_err());
    }
}
