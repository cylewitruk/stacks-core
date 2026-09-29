//! V4-to-V4.1 adapter for the shared offline migration pipeline.

use std::path::Path;

use blockstack_lib::chainstate::stacks::index::Error;
use blockstack_lib::chainstate::stacks::index::node::{TrieNodeID, TrieNodeType};
use blockstack_lib::chainstate::stacks::index::packed_branch::BranchView;
use blockstack_lib::chainstate::stacks::index::record::NodeRecordFormat;

use crate::relocation::{BlobRelocation, RelocationPlan, rewrite_plan_v41};

/// Required source layout.
pub const SOURCE_FORMAT: NodeRecordFormat = NodeRecordFormat::TypeFirstV4;
/// Published destination layout, rejected by adopted V4 readers.
pub const DESTINATION_FORMAT: NodeRecordFormat = NodeRecordFormat::TypeFirstV41;

/// Bind the selected physical codec to the source and relocation plan.
pub fn binding() -> String {
    "v41-a-direct-exact-ancestors-plan-1".to_owned()
}

/// Physical counts after exact relocated-size validation.
#[derive(Default)]
pub struct Counts {
    /// Existing extent references.
    pub extents: u64,
    /// Existing inline references.
    pub inlined: u64,
    /// Existing inline payload and descriptor bytes.
    pub inline_bytes: u64,
    /// Node4/16/48/256 record counts.
    pub branches: [u64; 4],
    /// Legacy V4 target-width buckets, not used in V4.1.
    pub targets: [u64; 4],
    /// Legacy V4 origin-width buckets, not used in V4.1.
    pub origins: [u64; 4],
    /// Retained physical patch records.
    pub patches: u64,
}

impl Counts {
    /// Inspect all exact records in one rewritten trie.
    pub fn inspect(bytes: &[u8], plan: &BlobRelocation) -> Result<Self, Error> {
        let mut result = Self::default();
        for (index, (_, offset)) in plan.offsets.iter().enumerate() {
            let record = DESTINATION_FORMAT.parse(&bytes[*offset as usize..])?;
            let end = plan
                .offsets
                .get(index + 1)
                .map_or(plan.length, |pair| pair.1);
            match record.logical_type() {
                TrieNodeID::Patch => result.patches += 1,
                TrieNodeID::Leaf => {
                    let (node, length) = record.decode_node(TrieNodeID::Leaf as u8)?;
                    if *offset + length as u64 != end {
                        return Err(Error::CorruptionError(
                            "V4.1 leaf extent differs from plan".into(),
                        ));
                    }
                    if let TrieNodeType::Leaf(leaf) = node {
                        result.extents += u64::from(leaf.extent.is_some());
                        if let Some(value) = &leaf.inline {
                            result.inlined += 1;
                            result.inline_bytes +=
                                (value.record().len() + value.descriptor().len()) as u64;
                        }
                    }
                }
                id => {
                    let branch = BranchView::parse(DESTINATION_FORMAT, id, record.payload)?;
                    if *offset + 33 + branch.byte_len() as u64 != end {
                        return Err(Error::CorruptionError(
                            "V4.1 branch extent differs from plan".into(),
                        ));
                    }
                    result.branches[id as usize - TrieNodeID::Node4 as usize] += 1;
                }
            }
        }
        Ok(result)
    }
}

/// Physical-only Node256 metadata conversion.
pub struct Codec;

impl Codec {
    /// Create the physical-only codec adapter.
    pub fn open(_: &Path) -> crate::migration::Result<Self> {
        Ok(Self)
    }
}

/// Plan one source trie with exact compact branch sizes and ancestor offsets.
pub fn plan_blob(
    bytes: &[u8],
    resolve: impl FnMut(u32, u64) -> Result<u64, Error>,
) -> Result<BlobRelocation, Error> {
    BlobRelocation::plan_packed_v41(bytes, SOURCE_FORMAT, resolve)
}

/// Rewrite through either owned or mmap-backed relocation metadata.
pub fn rewrite_blob(
    plan: &impl RelocationPlan,
    bytes: &[u8],
    resolve: impl FnMut(u32, u64) -> Result<u64, Error>,
    _codec: &Codec,
) -> Result<Vec<u8>, Error> {
    rewrite_plan_v41(plan, bytes, resolve)
}
