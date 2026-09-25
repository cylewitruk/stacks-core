//! Physical-only pointer packing; existing raw, inline and extent leaves are unchanged.
use std::path::Path;

use crate::relocation::BlobRelocation;
use blockstack_lib::chainstate::stacks::index::node::TrieNodeID;
use blockstack_lib::chainstate::stacks::index::packed_branch::PackedBranch;
use blockstack_lib::chainstate::stacks::index::record::NodeRecordFormat;
use blockstack_lib::chainstate::stacks::index::{Error, TrieLeaf};

/// Source layout admitted by this converter version.
pub const SOURCE_FORMAT: NodeRecordFormat = NodeRecordFormat::TypeFirstV3;
/// Published destination layout.
pub const DESTINATION_FORMAT: NodeRecordFormat = NodeRecordFormat::TypeFirstV4;
/// Bind exact planning, inherited leaves and finalized ancestor lookup semantics.
pub const BINDING: &str = "packed-pointers-v4-exact-ancestors-plan-1";

/// Physical record counts, not unique or live value counts.
#[derive(Default)]
pub struct Counts {
    /// Existing extent references.
    pub extents: u64,
    /// Existing inline references, unchanged by conversion.
    pub inlined: u64,
    /// Existing inline record plus descriptor bytes.
    pub inline_bytes: u64,
    /// Counts of Node4/16/48/256 records.
    pub branches: [u64; 4],
    /// Branch counts selecting target widths 2/3/4/8 bytes.
    pub targets: [u64; 4],
    /// Branch counts selecting origin widths 1/2/3/4 bytes, including empty origin columns.
    pub origins: [u64; 4],
    /// Patch records retaining their existing pointer codec.
    pub patches: u64,
}

impl Counts {
    /// Inspect actual rewritten records after exact size validation.
    pub fn inspect(bytes: &[u8], plan: &BlobRelocation) -> Result<Self, Error> {
        let mut result = Self::default();
        for (_, offset) in &plan.offsets {
            let record = DESTINATION_FORMAT.parse(&bytes[*offset as usize..])?;
            match record.logical_type() {
                TrieNodeID::Patch => result.patches += 1,
                TrieNodeID::Leaf => {
                    let (node, _) = record.decode_node(TrieNodeID::Leaf as u8)?;
                    if let blockstack_lib::chainstate::stacks::index::node::TrieNodeType::Leaf(
                        leaf,
                    ) = node
                    {
                        result.extents += u64::from(leaf.extent.is_some());
                        if let Some(value) = &leaf.inline {
                            result.inlined += 1;
                            result.inline_bytes +=
                                (value.record().len() + value.descriptor().len()) as u64;
                        }
                    }
                }
                id => {
                    let branch = PackedBranch::parse(id, record.payload)?;
                    let widths = record.payload[1 + branch.path().len()];
                    result.branches[id as usize - TrieNodeID::Node4 as usize] += 1;
                    result.targets[usize::from(widths & 3)] += 1;
                    result.origins[usize::from((widths >> 2) & 3)] += 1;
                }
            }
        }
        Ok(result)
    }
}

/// No value-file mapping is needed for pointer packing.
pub struct Codec;
impl Codec {
    /// Keep the adapter interface without opening or reading a values file.
    pub fn open(_: &Path) -> crate::migration::Result<Self> {
        Ok(Self)
    }
    /// Preserve the complete existing leaf representation.
    pub fn transform(&self, _: &mut TrieLeaf, _: &mut Counts) -> Result<(), Error> {
        Ok(())
    }
}
