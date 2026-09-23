// Copyright (C) 2026 Stacks Open Internet Foundation
// SPDX-License-Identifier: GPL-3.0-or-later

//! Physical node envelopes, independent of logical MARF commitments.

use std::io::{self, Read, Write};
use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension};
use stacks_common::codec::StacksMessageCodec;
use stacks_common::types::chainstate::{TrieHash, TRIEHASH_ENCODED_SIZE};

use super::bits;
use super::inline_value::{self, InlineValue};
use super::mapped_node;
use super::node::{
    clear_ctrl_bits, logical_node_id, TrieNode, TrieNode16, TrieNode256, TrieNode4, TrieNode48,
    TrieNodeID, TrieNodePatch, TrieNodeType,
};
use super::packed_branch::{self, BranchView};
use super::trie_sql::SQL_MARF_TYPE_FIRST_SCHEMA_VERSION;
use super::{Error, NodeDecodeScratch, TrieLeaf, ValueExtent, ValueExtentResolver};

/// Disk layout selected explicitly by the database format metadata.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NodeRecordFormat {
    /// Historical hash followed by the node marker and payload.
    #[default]
    Legacy,
    /// Marker first; locator leaves omit both logical value and leaf hash.
    TypeFirstV1,
    /// Type-first branches and extents with compact, hashless raw leaves.
    TypeFirstV2,
    /// Compact raw leaves plus owner-backed inline value records.
    TypeFirstV3,
    /// Inline leaves with directly addressed packed branch-pointer columns.
    TypeFirstV4,
}

/// Physical record layout and shared source for explicitly requested leaf commitments.
#[derive(Clone, Default)]
pub struct RecordContext {
    /// Database-selected physical record layout.
    pub format: NodeRecordFormat,
    /// Immutable value source; unused by ordinary locator lookups.
    pub value_resolver: Option<Arc<dyn ValueExtentResolver>>,
}

impl RecordContext {
    /// Probe a positioned record, reading its payload only when a leaf hash must be computed.
    pub fn read_probe<R: Read>(&self, reader: &mut R) -> Result<(TrieNodeID, TrieHash), Error> {
        let mut bytes = [0u8; 1 + 33 + inline_value::LENGTH_BYTES + inline_value::MAX_BYTES];
        if self.format == NodeRecordFormat::Legacy {
            reader.read_exact(&mut bytes[..33])?;
            let record = self.format.parse(&bytes[..33])?;
            return Ok((record.logical_type(), self.hash(record)?));
        }
        reader.read_exact(&mut bytes[..1])?;
        let physical = self.format.physical_id(bytes[0])?;
        let count = match physical {
            TrieNodeID::InlineLeaf => {
                reader.read_exact(&mut bytes[1..2])?;
                if bytes[1] > 32 {
                    return Err(Error::CorruptionError(
                        "Invalid inline leaf path length".into(),
                    ));
                }
                let lengths = 2 + usize::from(bytes[1]);
                reader.read_exact(&mut bytes[2..lengths + 2])?;
                let count =
                    lengths + 2 + usize::from(bytes[lengths]) + usize::from(bytes[lengths + 1]);
                reader.read_exact(&mut bytes[lengths + 2..count])?;
                count
            }
            TrieNodeID::ValueLeaf | TrieNodeID::RawLeaf => {
                reader.read_exact(&mut bytes[1..2])?;
                let payload_len = if physical == TrieNodeID::RawLeaf {
                    super::raw_leaf::payload_len(bytes[1])?
                } else {
                    if bytes[1] > 32 {
                        return Err(Error::CorruptionError("Invalid leaf path length".into()));
                    }
                    1 + usize::from(bytes[1]) + ValueExtent::ENCODED_SIZE
                };
                let count = 1 + payload_len;
                reader.read_exact(&mut bytes[2..count])?;
                count
            }
            _ => {
                reader.read_exact(&mut bytes[1..33])?;
                33
            }
        };
        let record = self.format.parse(&bytes[..count])?;
        Ok((record.logical_type(), self.hash(record)?))
    }

    /// Resolve a missing logical value only for leaves whose physical representation needs it.
    pub fn resolve_leaf(&self, leaf: &mut TrieLeaf) -> Result<(), Error> {
        if leaf.data.is_none() {
            let resolver = self.value_resolver.as_ref().ok_or_else(|| {
                Error::CorruptionError("No value source for hashless leaf".into())
            })?;
            leaf.data = Some(if let Some(value) = &leaf.inline {
                resolver.inline_commitment(value)?
            } else {
                let extent = leaf
                    .extent
                    .ok_or_else(|| Error::CorruptionError("Hashless leaf has no value".into()))?;
                resolver.commitment(extent)?
            });
        }
        Ok(())
    }

    /// Return a stored branch hash or compute a compact leaf's logical hash on demand.
    pub fn hash(&self, record: NodeRecord<'_>) -> Result<TrieHash, Error> {
        if let Some(hash) = record.hash {
            return Ok(hash);
        }
        let (node, _) = record.decode_node(TrieNodeID::Leaf as u8)?;
        let TrieNodeType::Leaf(mut leaf) = node else {
            return Err(Error::CorruptionError("Hashless non-leaf record".into()));
        };
        self.resolve_leaf(&mut leaf)?;
        Ok(bits::get_leaf_hash(&leaf))
    }
}

/// Borrowed record fields; the payload excludes the marker and any stored hash.
#[derive(Clone, Copy, Debug)]
pub struct NodeRecord<'a> {
    /// Layout defining the node payload encoding.
    pub format: NodeRecordFormat,
    /// Physical marker including encoding flags.
    pub marker: u8,
    /// Stored hash, absent for locator-only leaves.
    pub hash: Option<TrieHash>,
    /// Payload backed directly by the supplied file bytes.
    pub payload: &'a [u8],
    /// Number of envelope bytes preceding the payload.
    pub prefix_len: usize,
}

impl NodeRecordFormat {
    /// Whether this codec uses a marker-first envelope and mapped branch directories.
    pub const fn is_type_first(self) -> bool {
        !matches!(self, Self::Legacy)
    }

    /// Explicit physical format version; zero denotes historical records.
    pub const fn version(self) -> u8 {
        match self {
            Self::Legacy => 0,
            Self::TypeFirstV1 => 1,
            Self::TypeFirstV2 => 2,
            Self::TypeFirstV3 => 3,
            Self::TypeFirstV4 => 4,
        }
    }

    /// Validate a physical marker before interpreting its envelope.
    fn physical_id(self, marker: u8) -> Result<TrieNodeID, Error> {
        let physical = TrieNodeID::from_u8(clear_ctrl_bits(marker))
            .ok_or_else(|| Error::CorruptionError("Unknown physical node type".into()))?;
        if physical == TrieNodeID::Empty || (self.is_type_first() && marker != physical as u8) {
            return Err(Error::CorruptionError(
                "Invalid physical node marker".into(),
            ));
        }
        if physical == TrieNodeID::ValueLeaf && (!self.is_type_first() || marker != physical as u8)
        {
            return Err(Error::CorruptionError(
                "Unsupported locator-leaf encoding".into(),
            ));
        }
        if physical == TrieNodeID::RawLeaf
            && !matches!(
                self,
                Self::TypeFirstV2 | Self::TypeFirstV3 | Self::TypeFirstV4
            )
        {
            return Err(Error::CorruptionError(
                "Unsupported compact raw-leaf encoding".into(),
            ));
        }
        if physical == TrieNodeID::InlineLeaf
            && !matches!(self, Self::TypeFirstV3 | Self::TypeFirstV4)
        {
            return Err(Error::CorruptionError(
                "Unsupported inline-leaf encoding".into(),
            ));
        }
        Ok(physical)
    }

    /// Load explicit layout metadata; databases without it retain the legacy format.
    pub fn from_database(db: &Connection) -> Result<Self, Error> {
        let present: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='marf_record_format')", [], |row| row.get(0))?;
        if !present {
            return Ok(Self::Legacy);
        }
        let version: i64 = db.query_row(
            "SELECT version FROM marf_record_format WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        let schema: u64 =
            db.query_row("SELECT version FROM schema_version", [], |row| row.get(0))?;
        if schema != SQL_MARF_TYPE_FIRST_SCHEMA_VERSION {
            return Err(Error::CorruptionError(
                "Type-first metadata requires schema 4".into(),
            ));
        }
        match version {
            1 => Ok(Self::TypeFirstV1),
            2 => Ok(Self::TypeFirstV2),
            3 => Ok(Self::TypeFirstV3),
            4 => Ok(Self::TypeFirstV4),
            _ => Err(Error::CorruptionError(
                "Unsupported MARF record format version".into(),
            )),
        }
    }

    /// Publish the layout after all existing records have been converted, or in an empty store.
    pub fn publish(self, db: &Connection) -> Result<(), Error> {
        if !self.is_type_first() {
            return Err(Error::CorruptionError(
                "Record format downgrade is unsupported".into(),
            ));
        }
        db.execute_batch("CREATE TABLE IF NOT EXISTS marf_record_format (singleton INTEGER PRIMARY KEY CHECK(singleton=1), version INTEGER NOT NULL)")?;
        let previous: Option<i64> = db
            .query_row(
                "SELECT version FROM marf_record_format WHERE singleton=1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if previous.is_some_and(|version| version > i64::from(self.version())) {
            return Err(Error::CorruptionError(
                "Record format downgrade is unsupported".into(),
            ));
        }

        db.execute(
            "INSERT OR REPLACE INTO marf_record_format VALUES(1, ?1)",
            [self.version()],
        )?;
        db.execute(
            "UPDATE schema_version SET version=?1",
            [SQL_MARF_TYPE_FIRST_SCHEMA_VERSION],
        )?;
        db.execute(
            "UPDATE migrated_version SET version=?1",
            [SQL_MARF_TYPE_FIRST_SCHEMA_VERSION],
        )?;
        Ok(())
    }

    /// Split a physical envelope without allocating or decoding its payload.
    pub fn parse(self, bytes: &[u8]) -> Result<NodeRecord<'_>, Error> {
        let marker_offset = if self == Self::Legacy {
            TRIEHASH_ENCODED_SIZE
        } else {
            0
        };
        let marker = *bytes
            .get(marker_offset)
            .ok_or_else(|| Error::CorruptionError("Truncated node marker".into()))?;
        let physical = self.physical_id(marker)?;
        let hashless = matches!(
            physical,
            TrieNodeID::ValueLeaf | TrieNodeID::RawLeaf | TrieNodeID::InlineLeaf
        );
        let (hash, prefix_len) = if hashless {
            (None, 1)
        } else {
            let start = usize::from(self.is_type_first());
            let hash_bytes = bytes
                .get(start..start + TRIEHASH_ENCODED_SIZE)
                .ok_or_else(|| Error::CorruptionError("Truncated stored node hash".into()))?;
            (
                Some(TrieHash(
                    hash_bytes.try_into().expect("checked hash length"),
                )),
                1 + TRIEHASH_ENCODED_SIZE,
            )
        };
        let payload = bytes
            .get(prefix_len..)
            .ok_or_else(|| Error::CorruptionError("Truncated node envelope".into()))?;
        Ok(NodeRecord {
            format: self,
            marker,
            hash,
            payload,
            prefix_len,
        })
    }

    /// Bound a node read before its physical marker and occupancy are known.
    pub fn max_record_len(self, expected_id: u8) -> Result<usize, Error> {
        let id = TrieNodeID::from_u8(clear_ctrl_bits(expected_id))
            .ok_or_else(|| Error::CorruptionError("Unknown expected node type".into()))?;
        if self.is_type_first()
            && matches!(
                id,
                TrieNodeID::Node4 | TrieNodeID::Node16 | TrieNodeID::Node48 | TrieNodeID::Node256
            )
        {
            return Ok(1
                + TRIEHASH_ENCODED_SIZE
                + if self == Self::TypeFirstV4 {
                    packed_branch::max_payload_len(id)?
                } else {
                    mapped_node::max_payload_len(id)?
                });
        }
        if matches!(self, Self::TypeFirstV3 | Self::TypeFirstV4)
            && clear_ctrl_bits(logical_node_id(expected_id)) == TrieNodeID::Leaf as u8
        {
            return Ok(1 + 33 + inline_value::LENGTH_BYTES + inline_value::MAX_BYTES);
        }
        bits::get_read_node_max_byte_len(expected_id)
    }

    /// Encoded length including the physical envelope.
    pub fn node_len(self, node: &TrieNodeType, compressed: bool) -> usize {
        if self.is_type_first() {
            if let TrieNodeType::Leaf(leaf) = node {
                if let Some(inline) = &leaf.inline {
                    return 1 + bits::get_path_byte_len(&leaf.path) + inline.encoded_len();
                }
                if leaf.extent.is_some() {
                    return 1 + bits::get_path_byte_len(&leaf.path) + ValueExtent::ENCODED_SIZE;
                }
                if matches!(
                    self,
                    Self::TypeFirstV2 | Self::TypeFirstV3 | Self::TypeFirstV4
                ) {
                    return 2
                        + leaf.path.len()
                        + leaf.data.as_ref().map_or(40, super::raw_leaf::value_width);
                }
            }
        }
        if self.is_type_first() && !node.is_leaf() {
            return 1
                + TRIEHASH_ENCODED_SIZE
                + if self == Self::TypeFirstV4 {
                    packed_branch::payload_len(node)
                } else {
                    mapped_node::payload_len(node)
                };
        }
        TRIEHASH_ENCODED_SIZE
            + if compressed {
                node.byte_len_compressed()
            } else {
                node.byte_len()
            }
    }

    /// Write a node, retaining its logical hash only when the physical layout stores it.
    pub fn write_node<W: Write>(
        self,
        writer: &mut W,
        node: &TrieNodeType,
        hash: TrieHash,
        compressed: bool,
    ) -> Result<(), Error> {
        if let TrieNodeType::Leaf(leaf) = node {
            if let Some(inline) = &leaf.inline {
                if !matches!(self, Self::TypeFirstV3 | Self::TypeFirstV4) || leaf.extent.is_some() {
                    return Err(Error::CorruptionError(
                        "Invalid inline leaf format or conflicting locator".into(),
                    ));
                }
                writer.write_all(&[TrieNodeID::InlineLeaf as u8])?;
                bits::write_path_to_bytes(&leaf.path, writer)?;
                return Ok(inline.write_to(writer)?);
            }
        }
        if self.is_type_first() {
            if let TrieNodeType::Leaf(leaf) = node {
                if let Some(extent) = leaf.extent {
                    writer.write_all(&[TrieNodeID::ValueLeaf as u8])?;
                    bits::write_path_to_bytes(&leaf.path, writer)?;
                    return Ok(extent.write_to(writer)?);
                }
                if matches!(
                    self,
                    Self::TypeFirstV2 | Self::TypeFirstV3 | Self::TypeFirstV4
                ) {
                    writer.write_all(&[TrieNodeID::RawLeaf as u8])?;
                    return super::raw_leaf::write(leaf, writer);
                }
            }
            if !node.is_leaf() {
                writer.write_all(&[node.id()])?;
                writer.write_all(hash.as_ref())?;
                return if self == Self::TypeFirstV4 {
                    packed_branch::write_payload(writer, node)
                } else {
                    mapped_node::write_payload(writer, node)
                };
            }
            let mut writer = HashAfterMarker {
                writer,
                hash: Some(hash),
            };
            if compressed {
                node.write_bytes_compressed(&mut writer)
            } else {
                node.write_bytes(&mut writer)
            }
        } else {
            writer.write_all(hash.as_ref())?;
            if compressed {
                node.write_bytes_compressed(writer)
            } else {
                node.write_bytes(writer)
            }
        }
    }

    /// Write a branch patch with the selected physical envelope.
    pub fn write_patch<W: Write>(
        self,
        writer: &mut W,
        patch: &TrieNodePatch,
        hash: TrieHash,
    ) -> Result<(), Error> {
        if self.is_type_first() {
            patch
                .consensus_serialize(&mut HashAfterMarker {
                    writer,
                    hash: Some(hash),
                })
                .map_err(|error| Error::CorruptionError(error.to_string()))
        } else {
            writer.write_all(hash.as_ref())?;
            patch
                .consensus_serialize(writer)
                .map_err(|error| Error::CorruptionError(error.to_string()))
        }
    }
}

impl NodeRecord<'_> {
    /// Logical node identity used in parent pointers; patches retain their physical identity.
    pub fn logical_type(&self) -> TrieNodeID {
        TrieNodeID::from_u8(clear_ctrl_bits(logical_node_id(self.marker)))
            .expect("record marker validated on construction")
    }

    /// Decode a node payload with no intermediate copy of its serialized bytes.
    pub fn decode_node(&self, expected_id: u8) -> Result<(TrieNodeType, usize), Error> {
        let expected = TrieNodeID::from_u8(clear_ctrl_bits(logical_node_id(expected_id)))
            .ok_or_else(|| Error::CorruptionError("Unknown expected node type".into()))?;
        if TrieNodeID::from_u8(clear_ctrl_bits(self.marker)).is_none() {
            return Err(Error::CorruptionError(
                "Unknown physical node marker".into(),
            ));
        }
        if self.logical_type() == TrieNodeID::Patch {
            return Err(Error::Patch(self.decode_patch()?.0));
        }
        if self.logical_type() != expected {
            return Err(Error::CorruptionError(
                "Physical node disagrees with parent pointer".into(),
            ));
        }
        if self.format.is_type_first() && expected != TrieNodeID::Leaf {
            let view = BranchView::parse(self.format, expected, self.payload)?;
            return Ok((view.to_owned_node()?, self.prefix_len + view.byte_len()));
        }
        macro_rules! decode {
            ($ty:ty, $variant:ident) => {{
                let mut node = <$ty>::empty();
                let consumed = node.load_from_parts(self.marker, self.payload)?;
                (TrieNodeType::$variant(node), consumed)
            }};
            (boxed $ty:ty, $variant:ident) => {{
                let mut node = Box::new(<$ty>::empty());
                let consumed = node.load_from_parts(self.marker, self.payload)?;
                (TrieNodeType::$variant(node), consumed)
            }};
        }
        let (node, consumed) = match expected {
            TrieNodeID::Node4 => decode!(TrieNode4, Node4),
            TrieNodeID::Node16 => decode!(TrieNode16, Node16),
            TrieNodeID::Node48 => decode!(boxed TrieNode48, Node48),
            TrieNodeID::Node256 => decode!(boxed TrieNode256, Node256),
            TrieNodeID::Leaf => decode!(TrieLeaf, Leaf),
            _ => return Err(Error::CorruptionError("Invalid logical node type".into())),
        };
        Ok((node, self.prefix_len + consumed))
    }

    /// Decode into reusable slots, returning the full physical record length consumed.
    pub fn decode_into_scratch(
        &self,
        expected_id: u8,
        scratch: &mut impl NodeDecodeScratch,
    ) -> Result<usize, Error> {
        let logical = TrieNodeID::from_u8(clear_ctrl_bits(logical_node_id(self.marker)))
            .ok_or_else(|| Error::CorruptionError("Unknown physical node marker".into()))?;
        let consumed = if logical == TrieNodeID::Patch {
            scratch.decode_patch_from_parts(self.marker, self.payload)?
        } else {
            if logical as u8 != clear_ctrl_bits(logical_node_id(expected_id)) {
                return Err(Error::CorruptionError(
                    "Physical node disagrees with parent pointer".into(),
                ));
            }
            if self.format.is_type_first() && logical != TrieNodeID::Leaf {
                let view = BranchView::parse(self.format, logical, self.payload)?;
                scratch.store(view.to_owned_node()?);
                view.byte_len()
            } else {
                scratch.decode_node_from_parts(logical, self.marker, self.payload)?
            }
        };
        Ok(self.prefix_len + consumed)
    }

    /// Decode a patch without copying bytes around its stored hash.
    pub fn decode_patch(&self) -> Result<(TrieNodePatch, usize), Error> {
        let mut patch = TrieNodePatch {
            ptr: Default::default(),
            ptr_diff: Vec::new(),
        };
        let consumed = patch.load_from_parts(self.marker, self.payload)?;
        Ok((patch, self.prefix_len + consumed))
    }
}

/// Inserts a hash after the first marker byte without buffering the node payload.
struct HashAfterMarker<'a, W> {
    /// Output stream.
    writer: &'a mut W,
    /// Hash awaiting the first marker byte.
    hash: Option<TrieHash>,
}

impl<W: Write> Write for HashAfterMarker<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        if let Some(hash) = self.hash.take() {
            self.writer.write_all(&bytes[..1])?;
            self.writer.write_all(hash.as_ref())?;
            self.writer.write_all(&bytes[1..])?;
        } else {
            self.writer.write_all(bytes)?;
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use super::*;
    use crate::chainstate::stacks::index::node::TriePtr;
    use crate::chainstate::stacks::index::MARFValue;

    /// Branch payloads and logical encodings survive both envelope layouts and compression modes.
    #[test]
    fn branches_preserve_payloads_and_hashes() {
        let mut n4 = TrieNode4::empty();
        let mut n16 = TrieNode16::empty();
        let mut n48 = TrieNode48::empty();
        let mut n256 = TrieNode256::empty();
        for chr in 0..4 {
            let ptr = TriePtr::new_backptr(TrieNodeID::Leaf as u8, chr, 900 + u64::from(chr), 12);
            n4.insert(&ptr);
            n16.insert(&ptr);
            n48.insert(&ptr);
            n256.insert(&ptr);
        }
        let nodes = [
            TrieNodeType::Node4(n4),
            TrieNodeType::Node16(n16),
            TrieNodeType::Node48(Box::new(n48)),
            TrieNodeType::Node256(Box::new(n256)),
        ];
        for node in nodes {
            for compressed in [false, true] {
                for format in [NodeRecordFormat::Legacy, NodeRecordFormat::TypeFirstV1] {
                    let mut bytes = Vec::new();
                    let hash = TrieHash([0xab; 32]);
                    format
                        .write_node(&mut bytes, &node, hash, compressed)
                        .unwrap();
                    assert_eq!(bytes.len(), format.node_len(&node, compressed));
                    let record = format.parse(&bytes).unwrap();
                    assert_eq!(record.hash, Some(hash));
                    let (decoded, consumed) = record.decode_node(node.id()).unwrap();
                    assert_eq!(consumed, bytes.len());
                    assert_eq!(decoded, node);
                    assert_eq!(record.payload.as_ptr(), bytes[record.prefix_len..].as_ptr());
                    if format.is_type_first() {
                        assert_eq!(clear_ctrl_bits(bytes[0]), node.id());
                        assert_eq!(&bytes[1..33], hash.as_ref());
                    }
                }
            }
        }
    }

    /// Compact leaves carry no commitment or stored hash and retain their logical parent ID.
    #[test]
    fn locator_leaf_is_72_bytes_smaller_and_unresolved() {
        let mut leaf = TrieLeaf::from_value(&[2, 3, 4], MARFValue([0xab; 40]));
        let extent = ValueExtent {
            store_id: [5; 16],
            offset: 8192,
            length: 300,
        };
        leaf.extent = Some(extent);
        let logical_hash = bits::get_leaf_hash(&leaf);
        let node = TrieNodeType::Leaf(leaf.clone());
        let mut legacy = Vec::new();
        NodeRecordFormat::Legacy
            .write_node(&mut legacy, &node, logical_hash, false)
            .unwrap();
        let mut compact = Vec::new();
        NodeRecordFormat::TypeFirstV1
            .write_node(&mut compact, &node, logical_hash, false)
            .unwrap();
        assert_eq!(legacy.len() - compact.len(), 72);
        assert_eq!(
            compact.len(),
            1 + 1 + leaf.path.len() + ValueExtent::ENCODED_SIZE
        );
        assert_eq!(compact[0], TrieNodeID::ValueLeaf as u8);
        let record = NodeRecordFormat::TypeFirstV1.parse(&compact).unwrap();
        assert_eq!(record.hash, None);
        assert_eq!(record.logical_type(), TrieNodeID::Leaf);
        let (decoded, consumed) = record.decode_node(TrieNodeID::Leaf as u8).unwrap();
        assert_eq!(consumed, compact.len());
        let TrieNodeType::Leaf(mut decoded) = decoded else {
            panic!("expected leaf")
        };
        assert_eq!(decoded.data, None);
        assert_eq!(decoded.extent, Some(extent));
        assert!(decoded.value().is_err());
        decoded.data = leaf.data;
        assert_eq!(bits::get_leaf_hash(&decoded), logical_hash);
        assert!(record.decode_node(TrieNodeID::Node4 as u8).is_err());
        for end in 0..compact.len() {
            let result = NodeRecordFormat::TypeFirstV1
                .parse(&compact[..end])
                .and_then(|r| r.decode_node(TrieNodeID::Leaf as u8));
            assert!(result.is_err(), "accepted truncated locator at {end}");
        }
    }

    /// Plain system leaves retain their inline value and hash in type-first databases.
    #[test]
    fn inline_leaf_preserves_value() {
        let node = TrieNodeType::Leaf(TrieLeaf::from_value(&[], MARFValue([6; 40])));
        let mut bytes = Vec::new();
        NodeRecordFormat::TypeFirstV1
            .write_node(&mut bytes, &node, TrieHash([8; 32]), true)
            .unwrap();
        let record = NodeRecordFormat::TypeFirstV1.parse(&bytes).unwrap();
        assert_eq!(record.marker, TrieNodeID::Leaf as u8);
        assert_eq!(record.hash, Some(TrieHash([8; 32])));
        assert_eq!(record.decode_node(node.id()).unwrap().0, node);
    }

    /// Patch envelopes preserve all local and historical pointer fields.
    #[test]
    fn patches_preserve_pointers() {
        let patch = TrieNodePatch {
            ptr: TriePtr::new_backptr(TrieNodeID::Node256 as u8, 10, 1000, 9),
            ptr_diff: vec![TriePtr::new(TrieNodeID::Leaf as u8, 20, 2000)],
        };
        for format in [NodeRecordFormat::Legacy, NodeRecordFormat::TypeFirstV1] {
            let mut bytes = Vec::new();
            format
                .write_patch(&mut bytes, &patch, TrieHash([3; 32]))
                .unwrap();
            let record = format.parse(&bytes).unwrap();
            let (decoded, consumed) = record.decode_patch().unwrap();
            assert_eq!(decoded, patch);
            assert_eq!(consumed, bytes.len());
            assert_matches!(
                record.decode_node(TrieNodeID::Node256 as u8),
                Err(Error::Patch(_))
            );
        }
    }

    /// Buffered and borrowed reads expose an unresolved locator without requiring a hash.
    #[test]
    fn compact_leaf_reads_match_across_backings() {
        use crate::chainstate::stacks::index::scratch::MarfReadState;
        use crate::chainstate::stacks::index::{BorrowedNodeBytes, ReadTrieItemKind, ReadTrieNode};
        use std::io::{Cursor, Seek};

        let extent = ValueExtent {
            store_id: [4; 16],
            offset: 100,
            length: 150,
        };
        let mut leaf = TrieLeaf::from_value(&[3, 9], MARFValue([1; 40]));
        leaf.extent = Some(extent);
        let mut bytes = vec![0; 11];
        NodeRecordFormat::TypeFirstV1
            .write_node(
                &mut bytes,
                &TrieNodeType::Leaf(leaf),
                TrieHash([8; 32]),
                true,
            )
            .unwrap();
        let end = bytes.len() as u64;
        let record = NodeRecordFormat::TypeFirstV1.parse(&bytes[11..]).unwrap();
        let borrowed =
            ReadTrieNode::from_stable_bytes(BorrowedNodeBytes::from_record(record), record.hash);
        assert!(borrowed.is_leaf().unwrap());
        assert_eq!(borrowed.hash, None);
        assert_eq!(borrowed.as_leaf().unwrap().unwrap().data, None);
        assert_eq!(borrowed.as_leaf().unwrap().unwrap().extent, Some(extent));

        let mut input = Cursor::new(bytes);
        input.set_position(11);
        let mut scratch = MarfReadState::new();
        let read = bits::read_trie_item_at_head_ref_format(
            &mut input,
            TrieNodeID::Leaf as u8,
            NodeRecordFormat::TypeFirstV1,
            &mut scratch,
        )
        .unwrap();
        assert_eq!(read.hash, None);
        let ReadTrieItemKind::Node(node) = read.kind else {
            panic!("expected node")
        };
        assert_eq!(node.as_leaf().unwrap().unwrap().extent, Some(extent));
        assert_eq!(input.stream_position().unwrap(), end);
    }

    /// Unknown markers and unassigned locator flags fail before payload decoding.
    #[test]
    fn invalid_envelopes_fail_closed() {
        for marker in [0, 15, 0x17, 0x87] {
            assert!(NodeRecordFormat::TypeFirstV1.parse(&[marker; 100]).is_err());
        }
        let mut bytes = [0; 100];
        bytes[32] = TrieNodeID::ValueLeaf as u8;
        assert!(NodeRecordFormat::Legacy.parse(&bytes).is_err());
    }
}

#[cfg(test)]
mod inline_tests {
    use std::io::{Cursor, Seek};

    use super::*;
    use crate::chainstate::stacks::index::MARFValue;

    /// Test resolver isolates physical framing from the Clarity adapter.
    struct Resolver;
    impl ValueExtentResolver for Resolver {
        fn commitment(&self, _extent: ValueExtent) -> Result<MARFValue, Error> {
            Err(Error::CorruptionError("unexpected extent".into()))
        }
        fn inline_commitment(&self, value: &InlineValue) -> Result<MARFValue, Error> {
            Ok(MARFValue::from_value(&format!(
                "{:?}:{:?}",
                value.record(),
                value.descriptor()
            )))
        }
    }

    /// Inline records preserve logical hashes, framing and scratch reuse at every path length.
    #[test]
    fn inline_codec_paths_hashes_bounds_and_reuse() {
        let format = NodeRecordFormat::TypeFirstV3;
        let context = RecordContext {
            format,
            value_resolver: Some(Arc::new(Resolver)),
        };
        for path_len in 0..=32 {
            for (record_len, descriptor_len) in [(0, 0), (1, 29), (30, 0), (255, 255)] {
                let inline =
                    InlineValue::from_parts(&vec![7; record_len], &vec![9; descriptor_len])
                        .unwrap();
                let data = Resolver.inline_commitment(&inline).unwrap();
                let mut leaf = TrieLeaf::from_value(&vec![3; path_len], data.clone());
                leaf.inline = Some(inline.clone());
                let hash = bits::get_leaf_hash(&leaf);
                let node = TrieNodeType::Leaf(leaf);
                let mut bytes = Vec::new();
                format.write_node(&mut bytes, &node, hash, true).unwrap();
                assert_eq!(bytes.len(), format.node_len(&node, true));
                assert!(bytes.len() <= format.max_record_len(TrieNodeID::Leaf as u8).unwrap());
                let record = format.parse(&bytes).unwrap();
                assert_eq!(record.hash, None);
                assert_eq!(context.hash(record).unwrap(), hash);
                let (TrieNodeType::Leaf(mut decoded), used) =
                    record.decode_node(TrieNodeID::Leaf as u8).unwrap()
                else {
                    panic!("leaf expected")
                };
                assert_eq!(used, bytes.len());
                assert_eq!(decoded.inline, Some(inline));
                assert_eq!(decoded.data, None);
                assert_eq!(decoded.extent, None);
                context.resolve_leaf(&mut decoded).unwrap();
                assert_eq!(decoded.data, Some(data));
                let mut cursor = Cursor::new(&bytes);
                assert_eq!(
                    context.read_probe(&mut cursor).unwrap(),
                    (TrieNodeID::Leaf, hash)
                );
                assert_eq!(cursor.stream_position().unwrap() as usize, bytes.len());
                for end in 0..bytes.len() {
                    assert!(format
                        .parse(&bytes[..end])
                        .and_then(|record| record.decode_node(1))
                        .is_err());
                }
                for old in [
                    NodeRecordFormat::Legacy,
                    NodeRecordFormat::TypeFirstV1,
                    NodeRecordFormat::TypeFirstV2,
                ] {
                    assert!(old.write_node(&mut Vec::new(), &node, hash, true).is_err());
                    if old.is_type_first() {
                        assert!(old.parse(&bytes).is_err());
                    }
                }
                let raw = TrieLeaf::from_value(&[], MARFValue([0; 40]));
                let mut raw_bytes = Vec::new();
                format
                    .write_node(&mut raw_bytes, &TrieNodeType::Leaf(raw), hash, true)
                    .unwrap();
                decoded
                    .load_from_parts(raw_bytes[0], &raw_bytes[1..])
                    .unwrap();
                assert!(decoded.inline.is_none());
                assert_eq!(decoded.data, Some(MARFValue([0; 40])));
            }
        }
    }
}
