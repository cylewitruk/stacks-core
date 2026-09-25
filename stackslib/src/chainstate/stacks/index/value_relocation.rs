//! Physical trie relocation for offline direct-value migration. Logical hashes are unchanged.

use std::collections::BTreeMap;
use std::io::{Cursor, Write};

use stacks_common::types::chainstate::{StacksBlockId, TrieHash};

use super::bits;
use super::blob_layout::ROOT_NODE_OFFSET;
use super::node::{is_backptr, logical_node_id, TrieNodePatch, TrieNodeType, TriePtr};
use super::packed_branch;
use super::record::NodeRecordFormat;
use super::scratch::MarfReadState;
use super::{Error, MARFValue, ReadTrieItemKind, TrieLeaf, ValueExtent};

/// Deterministic physical offsets for one trie blob, persisted by the migration coordinator.
#[derive(Debug, Clone)]
pub struct BlobRelocation {
    /// Ordered pairs of original and destination offsets, including the root at byte 36.
    pub offsets: Vec<(u64, u64)>,
    /// Total reserved destination bytes.
    pub length: u64,
}

/// One decoded physical record, preserving patches instead of materializing ancestors.
enum Record {
    /// Full trie node.
    Node(TrieNodeType),
    /// Incremental node patch.
    Patch(TrieNodePatch),
}

impl Record {
    /// Visit every physical reference, including patch bases.
    fn map_pointers(
        &mut self,
        mut visit: impl FnMut(&mut TriePtr) -> Result<(), Error>,
    ) -> Result<(), Error> {
        match self {
            Self::Node(node) if !node.is_leaf() => {
                for ptr in node.ptrs_mut() {
                    visit(ptr)?;
                }
            }
            Self::Patch(patch) => {
                visit(&mut patch.ptr)?;
                for ptr in &mut patch.ptr_diff {
                    visit(ptr)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Exact V4 record size after target relocation, without serializing payload bytes.
    fn packed_size(
        &self,
        mut resolve: impl FnMut(&TriePtr) -> Result<u64, Error>,
    ) -> Result<u64, Error> {
        Ok(match self {
            Self::Node(node) if node.is_leaf() => {
                NodeRecordFormat::TypeFirstV4.node_len(node, true)
            }
            Self::Node(node) => 33 + packed_branch::payload_len_with_targets(node, resolve)?,
            Self::Patch(patch) => {
                let mut size = patch.size() + 32;
                for ptr in std::iter::once(&patch.ptr).chain(patch.ptr_diff.iter()) {
                    let mut mapped = *ptr;
                    mapped.ptr = resolve(ptr)?;
                    size = size - ptr.compressed_size() + mapped.compressed_size();
                }
                size
            }
        } as u64)
    }

    /// Encode a compressed physical record after its unchanged logical hash.
    fn encode(
        &self,
        hash: impl Into<Option<TrieHash>>,
        format: NodeRecordFormat,
    ) -> Result<Vec<u8>, Error> {
        let hash = match hash.into() {
            Some(hash) => hash,
            None if matches!(self, Self::Node(TrieNodeType::Leaf(leaf)) if format.is_type_first() && (leaf.extent.is_some() || matches!(format, NodeRecordFormat::TypeFirstV2 | NodeRecordFormat::TypeFirstV3 | NodeRecordFormat::TypeFirstV4))) => {
                TrieHash([0; 32])
            }
            None => return Err(corrupt("destination requires an unavailable node hash")),
        };
        let mut bytes = Vec::new();
        match self {
            Self::Node(node) => format.write_node(&mut bytes, node, hash, true)?,
            Self::Patch(patch) => format.write_patch(&mut bytes, patch, hash)?,
        }
        Ok(bytes)
    }
}

/// Decode exactly one source record and advance to the next physical record.
fn read_record(
    input: &mut Cursor<&[u8]>,
    scratch: &mut MarfReadState,
    format: NodeRecordFormat,
) -> Result<(Record, Option<TrieHash>), Error> {
    let offset = usize::try_from(input.position()).map_err(|_| Error::OverflowError)?;
    let id = *input
        .get_ref()
        .get(
            offset
                .checked_add(if format.is_type_first() { 0 } else { 32 })
                .ok_or(Error::OverflowError)?,
        )
        .ok_or_else(|| corrupt("truncated trie record"))?
        & 0x0f;
    let item =
        bits::read_trie_item_at_head_ref_format(input, logical_node_id(id), format, scratch)?;
    let hash = item.hash;
    let record = match item.kind {
        ReadTrieItemKind::Node(node) => Record::Node(node.into_owned_node()?.0),
        ReadTrieItemKind::Patch(patch) => Record::Patch(patch.clone()),
    };
    Ok((record, hash))
}

impl BlobRelocation {
    /// Plan compact value leaves and branch directories with worst-case pointer widths.
    /// Padding makes offsets independent of ancestor traversal order and pointer widening.
    pub fn plan(
        bytes: &[u8],
        mut has_extent: impl FnMut(&MARFValue) -> Result<bool, Error>,
    ) -> Result<Self, Error> {
        Self::plan_with_pointer_width(
            bytes,
            NodeRecordFormat::Legacy,
            NodeRecordFormat::TypeFirstV1,
            |leaf| {
                leaf.extent = has_extent(leaf.value()?)?.then_some(ValueExtent {
                    store_id: [0; 16],
                    offset: 0,
                    length: 0,
                });
                Ok(())
            },
            u64::MAX,
        )
    }

    /// Plan exact four-byte offsets; all referenced trie plans must pass the same bound.
    pub fn plan_narrow(
        bytes: &[u8],
        mut has_extent: impl FnMut(&MARFValue) -> Result<bool, Error>,
    ) -> Result<Self, Error> {
        Self::plan_format(
            bytes,
            NodeRecordFormat::Legacy,
            NodeRecordFormat::TypeFirstV1,
            |leaf| {
                leaf.extent = has_extent(leaf.value()?)?.then_some(ValueExtent {
                    store_id: [0; 16],
                    offset: 0,
                    length: 0,
                });
                Ok(())
            },
        )
    }

    /// Plan a codec upgrade with four-byte pointers and a deterministic leaf transformation.
    pub fn plan_format(
        bytes: &[u8],
        source: NodeRecordFormat,
        destination: NodeRecordFormat,
        transform: impl FnMut(&mut TrieLeaf) -> Result<(), Error>,
    ) -> Result<Self, Error> {
        if destination.version() < source.version() {
            return Err(corrupt("trie codec downgrade is unsupported"));
        }
        if destination == NodeRecordFormat::TypeFirstV4 {
            return Err(corrupt(
                "packed relocation requires resolved ancestor plans",
            ));
        }
        source.validate_trie_header(bytes)?;
        let plan = Self::plan_with_pointer_width(
            bytes,
            source,
            destination,
            transform,
            u64::from(u32::MAX),
        )?;
        if plan.length > u64::from(u32::MAX) {
            return Err(corrupt("trie exceeds narrow relocation limit"));
        }
        Ok(plan)
    }

    /// Plan exact packed records using finalized ancestor offsets and monotone local refinement.
    /// Ancestor plans must precede this trie; the callback also handles patch base references.
    pub fn plan_packed(
        bytes: &[u8],
        source: NodeRecordFormat,
        mut resolve_backptr: impl FnMut(u32, u64) -> Result<u64, Error>,
    ) -> Result<Self, Error> {
        source.validate_trie_header(bytes)?;
        if bytes.len() <= ROOT_NODE_OFFSET {
            return Err(corrupt("missing trie root"));
        }
        let mut input = Cursor::new(bytes);
        let mut scratch = MarfReadState::new();
        let mut records = BTreeMap::new();
        let mut pending = vec![ROOT_NODE_OFFSET as u64];
        while let Some(old) = pending.pop() {
            if records.contains_key(&old) {
                continue;
            }
            input.set_position(old);
            let (mut record, _) = read_record(&mut input, &mut scratch, source)?;
            let source_end = input.position();
            record.map_pointers(|ptr| {
                if !ptr.is_empty() {
                    if is_backptr(ptr.id()) {
                        ptr.ptr = resolve_backptr(ptr.back_block(), ptr.ptr())?;
                    } else {
                        pending.push(ptr.ptr());
                    }
                }
                Ok(())
            })?;
            // Leaves are unchanged by pointer packing; no extent contents are needed.
            records.insert(old, (source_end, record));
        }
        let mut previous_end = ROOT_NODE_OFFSET as u64;
        for (old, (end, _)) in &records {
            if *old < previous_end || *end > bytes.len() as u64 {
                return Err(corrupt("overlapping or out-of-bounds trie records"));
            }
            previous_end = *end;
        }
        let calculate = |previous: Option<&Self>| -> Result<Self, Error> {
            let mut offsets = Vec::with_capacity(records.len());
            let mut length = ROOT_NODE_OFFSET as u64;
            for (old, (_, record)) in &records {
                offsets.push((*old, length));
                let size = record.packed_size(|ptr| {
                    if ptr.is_empty() || is_backptr(ptr.id()) {
                        return Ok(ptr.ptr());
                    }
                    previous.map_or(Ok(u64::MAX), |plan| plan.resolve(ptr.ptr()))
                })?;
                length = length.checked_add(size).ok_or(Error::OverflowError)?;
            }
            Ok(Self { offsets, length })
        };
        let mut previous = calculate(None)?;
        loop {
            let next = calculate(Some(&previous))?;
            if next.length > previous.length
                || next
                    .offsets
                    .iter()
                    .zip(&previous.offsets)
                    .any(|(new, old)| new.1 > old.1)
            {
                return Err(corrupt("packed relocation widths increased"));
            }
            if next.offsets == previous.offsets && next.length == previous.length {
                return Ok(next);
            }
            previous = next;
        }
    }

    /// Inventory records with a chosen physical pointer-width reservation.
    fn plan_with_pointer_width(
        bytes: &[u8],
        source: NodeRecordFormat,
        destination: NodeRecordFormat,
        mut transform: impl FnMut(&mut TrieLeaf) -> Result<(), Error>,
        widest_pointer: u64,
    ) -> Result<Self, Error> {
        if bytes.len() <= ROOT_NODE_OFFSET {
            return Err(corrupt("missing trie root"));
        }
        let mut input = Cursor::new(bytes);
        input.set_position(ROOT_NODE_OFFSET as u64);
        let mut scratch = MarfReadState::new();
        let mut records = BTreeMap::new();
        let mut pending = vec![ROOT_NODE_OFFSET as u64];
        while let Some(old) = pending.pop() {
            if records.contains_key(&old) {
                continue;
            }
            input.set_position(old);
            let (mut record, hash) = read_record(&mut input, &mut scratch, source)?;
            let source_end = input.position();
            record.map_pointers(|ptr| {
                if !ptr.is_empty() {
                    if !is_backptr(ptr.id()) {
                        pending.push(ptr.ptr());
                    }
                    ptr.ptr = widest_pointer;
                }
                Ok(())
            })?;
            if let Record::Node(TrieNodeType::Leaf(leaf)) = &mut record {
                transform(leaf)?;
            }
            records.insert(
                old,
                (source_end, record.encode(hash, destination)?.len() as u64),
            );
        }
        let mut offsets = Vec::with_capacity(records.len());
        let mut length = ROOT_NODE_OFFSET as u64;
        let mut previous_end = ROOT_NODE_OFFSET as u64;
        for (old, (end, size)) in records {
            if old < previous_end || end > bytes.len() as u64 {
                return Err(corrupt("overlapping or out-of-bounds trie records"));
            }
            offsets.push((old, length));
            length = length.checked_add(size).ok_or(Error::OverflowError)?;
            previous_end = end;
        }
        Ok(Self { offsets, length })
    }

    /// Resolve an original node offset, rejecting pointers into record interiors.
    pub fn resolve(&self, offset: u64) -> Result<u64, Error> {
        self.offsets
            .binary_search_by_key(&offset, |entry| entry.0)
            .map(|index| self.offsets[index].1)
            .map_err(|_| corrupt("pointer does not identify a source record"))
    }

    /// Rewrite all records while preserving hashes, patch depth and block identities.
    /// `resolve_backptr` maps offsets in other blobs; `locate_value` supplies content locators.
    pub fn rewrite(
        &self,
        bytes: &[u8],
        resolve_backptr: impl FnMut(u32, u64) -> Result<u64, Error>,
        mut locate_value: impl FnMut(&MARFValue) -> Result<Option<ValueExtent>, Error>,
    ) -> Result<Vec<u8>, Error> {
        self.rewrite_format(
            bytes,
            NodeRecordFormat::Legacy,
            NodeRecordFormat::TypeFirstV1,
            resolve_backptr,
            |leaf| {
                leaf.extent = locate_value(leaf.value()?)?;
                Ok(())
            },
        )
    }

    /// Apply a matching format plan, retaining hashes and physical backpointer identities.
    pub fn rewrite_format(
        &self,
        bytes: &[u8],
        source: NodeRecordFormat,
        destination: NodeRecordFormat,
        resolve_backptr: impl FnMut(u32, u64) -> Result<u64, Error>,
        transform: impl FnMut(&mut TrieLeaf) -> Result<(), Error>,
    ) -> Result<Vec<u8>, Error> {
        rewrite_plan_format(self, bytes, source, destination, resolve_backptr, transform)
    }
}

/// Immutable offset map independent of its owned or mmap-backed representation.
pub trait RelocationPlan {
    /// Number of physical records.
    fn len(&self) -> usize;
    /// Whether the plan contains no records.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Encoded destination trie length.
    fn length(&self) -> u64;
    /// Original and relocated offsets at a validated record index.
    fn pair(&self, index: usize) -> (u64, u64);
    /// Resolve only exact physical record boundaries.
    fn resolve(&self, offset: u64) -> Result<u64, Error> {
        let mut low = 0;
        let mut high = self.len();
        while low < high {
            let mid = low + (high - low) / 2;
            let pair = self.pair(mid);
            match pair.0.cmp(&offset) {
                std::cmp::Ordering::Less => low = mid + 1,
                std::cmp::Ordering::Greater => high = mid,
                std::cmp::Ordering::Equal => return Ok(pair.1),
            }
        }
        Err(corrupt("pointer does not identify a source record"))
    }
}

impl RelocationPlan for BlobRelocation {
    fn len(&self) -> usize {
        self.offsets.len()
    }
    fn length(&self) -> u64 {
        self.length
    }
    fn pair(&self, index: usize) -> (u64, u64) {
        self.offsets[index]
    }
}

/// Rewrite through either an owned plan or an immutable mmap-backed relocation index.
pub fn rewrite_plan_format(
    plan: &impl RelocationPlan,
    bytes: &[u8],
    source: NodeRecordFormat,
    destination: NodeRecordFormat,
    mut resolve_backptr: impl FnMut(u32, u64) -> Result<u64, Error>,
    mut transform: impl FnMut(&mut TrieLeaf) -> Result<(), Error>,
) -> Result<Vec<u8>, Error> {
    if destination.version() < source.version() {
        return Err(corrupt("trie codec downgrade is unsupported"));
    }
    source.validate_trie_header(bytes)?;
    let size = usize::try_from(plan.length()).map_err(|_| Error::OverflowError)?;
    let mut output = vec![0; size];
    output
        .get_mut(..ROOT_NODE_OFFSET)
        .ok_or_else(|| corrupt("invalid relocation length"))?
        .copy_from_slice(
            bytes
                .get(..ROOT_NODE_OFFSET)
                .ok_or_else(|| corrupt("truncated blob header"))?,
        );
    // Preserve the parent identity but replace the historical reserved field with the version.
    let parent = bytes
        .get(..32)
        .ok_or_else(|| corrupt("truncated parent identity"))?;
    let mut header = Vec::with_capacity(ROOT_NODE_OFFSET);
    destination.write_trie_header(
        &mut header,
        &StacksBlockId(parent.try_into().expect("parent width")),
    )?;
    output[..ROOT_NODE_OFFSET].copy_from_slice(&header);
    let mut input = Cursor::new(bytes);
    input.set_position(ROOT_NODE_OFFSET as u64);
    let mut scratch = MarfReadState::new();
    for index in 0..plan.len() {
        let (old, new) = plan.pair(index);
        input.set_position(old);
        let (mut record, hash) = read_record(&mut input, &mut scratch, source)?;
        record.map_pointers(|ptr| {
            if !ptr.is_empty() {
                ptr.ptr = if is_backptr(ptr.id()) {
                    resolve_backptr(ptr.back_block(), ptr.ptr())?
                } else {
                    plan.resolve(ptr.ptr())?
                };
            }
            Ok(())
        })?;
        if let Record::Node(TrieNodeType::Leaf(leaf)) = &mut record {
            transform(leaf)?;
        }
        let encoded = record.encode(hash, destination)?;
        let end = new
            .checked_add(encoded.len() as u64)
            .ok_or(Error::OverflowError)?;
        let limit = if index + 1 < plan.len() {
            plan.pair(index + 1).1
        } else {
            plan.length()
        };
        if end > limit || (destination == NodeRecordFormat::TypeFirstV4 && end != limit) {
            return Err(corrupt("relocated record does not match reserved space"));
        }
        output
            .get_mut(new as usize..end as usize)
            .ok_or_else(|| corrupt("invalid relocation range"))?
            .write_all(&encoded)?;
    }
    Ok(output)
}

/// Describe a rejected offline physical layout.
fn corrupt(message: &str) -> Error {
    Error::CorruptionError(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chainstate::stacks::index::node::{TrieNode4, TrieNodeID};
    use crate::chainstate::stacks::index::TrieLeaf;

    /// Decode the new output format without requiring compact leaves to store hashes.
    fn read_output(
        input: &mut Cursor<&[u8]>,
        scratch: &mut MarfReadState,
    ) -> (Record, Option<TrieHash>) {
        let id = input.get_ref()[input.position() as usize];
        let item = bits::read_trie_item_at_head_ref_format(
            input,
            super::super::node::logical_node_id(id),
            NodeRecordFormat::TypeFirstV1,
            scratch,
        )
        .unwrap();
        let hash = item.hash;
        let record = match item.kind {
            ReadTrieItemKind::Node(node) => Record::Node(node.into_owned_node().unwrap().0),
            ReadTrieItemKind::Patch(patch) => Record::Patch(patch.clone()),
        };
        (record, hash)
    }

    /// Relocation retains leaf commitments and rewrites local, historical and patch references.
    #[test]
    fn relocation_preserves_hashes_and_patch_references() {
        let leaf = TrieLeaf::from_value(&[2, 3], MARFValue::from(7));
        let hash = bits::get_leaf_hash(&leaf);
        let leaf_record = Record::Node(TrieNodeType::Leaf(leaf));
        let root = Record::Node(TrieNodeType::Node4(TrieNode4::new(&[])));
        let root_size = root
            .encode(TrieHash([1; 32]), NodeRecordFormat::Legacy)
            .unwrap()
            .len();
        let leaf_offset = ROOT_NODE_OFFSET as u64 + root_size as u64;
        let mut root = root;
        if let Record::Node(node) = &mut root {
            node.ptrs_mut()[0] = TriePtr::new(TrieNodeID::Leaf as u8, 1, leaf_offset);
            node.ptrs_mut()[1] = TriePtr::new_backptr(TrieNodeID::Leaf as u8, 2, 99, 4);
        }
        // Pointer widths alter sparse encoding; derive the final original leaf location.
        let root_size = root
            .encode(TrieHash([1; 32]), NodeRecordFormat::Legacy)
            .unwrap()
            .len();
        let leaf_offset = ROOT_NODE_OFFSET as u64 + root_size as u64;
        if let Record::Node(node) = &mut root {
            node.ptrs_mut()[0].ptr = leaf_offset;
        }
        let patch = Record::Patch(TrieNodePatch {
            ptr: TriePtr::new_backptr(TrieNodeID::Node4 as u8, 0, 36, 3),
            ptr_diff: vec![TriePtr::new(TrieNodeID::Leaf as u8, 1, leaf_offset)],
        });
        let mut source = vec![0; ROOT_NODE_OFFSET];
        let patch_offset = leaf_offset
            + leaf_record
                .encode(hash, NodeRecordFormat::Legacy)
                .unwrap()
                .len() as u64;
        // Replace an existing backpointer so the encoded root length stays unchanged.
        if let Record::Node(node) = &mut root {
            node.ptrs_mut()[1] = TriePtr::new_backptr(TrieNodeID::Leaf as u8, 2, 99, 4);
            node.ptrs_mut()[2] = TriePtr::new(TrieNodeID::Node4 as u8, 3, patch_offset);
        }
        // The added child changes sparse root size; rebuild offsets before serialization.
        let delta = root
            .encode(TrieHash([1; 32]), NodeRecordFormat::Legacy)
            .unwrap()
            .len() as u64
            - root_size as u64;
        if let Record::Node(node) = &mut root {
            node.ptrs_mut()[0].ptr += delta;
            node.ptrs_mut()[2].ptr += delta;
        }
        let leaf_offset = leaf_offset + delta;
        let mut patch = patch;
        if let Record::Patch(patch) = &mut patch {
            patch.ptr_diff[0].ptr = leaf_offset;
        }
        source.extend(
            root.encode(TrieHash([1; 32]), NodeRecordFormat::Legacy)
                .unwrap(),
        );
        source.extend(leaf_record.encode(hash, NodeRecordFormat::Legacy).unwrap());
        source.extend(
            patch
                .encode(TrieHash([2; 32]), NodeRecordFormat::Legacy)
                .unwrap(),
        );
        let plan = BlobRelocation::plan(&source, |_| Ok(true)).unwrap();
        let extent = ValueExtent {
            store_id: [5; 16],
            offset: 48,
            length: 100,
        };
        let output = plan
            .rewrite(&source, |_, ptr| Ok(ptr + 1000), |_| Ok(Some(extent)))
            .unwrap();
        let mut input = Cursor::new(output.as_slice());
        let mut scratch = MarfReadState::new();
        input.set_position(plan.offsets[0].1);
        let (Record::Node(root), _) = read_output(&mut input, &mut scratch) else {
            panic!()
        };
        assert_eq!(root.ptrs()[0].ptr(), plan.resolve(leaf_offset).unwrap());
        assert_eq!(root.ptrs()[1].ptr(), 1099);
        input.set_position(plan.offsets[1].1);
        let (Record::Node(TrieNodeType::Leaf(leaf)), stored_hash) =
            read_output(&mut input, &mut scratch)
        else {
            panic!()
        };
        assert_eq!(leaf.extent, Some(extent));
        assert_eq!(stored_hash, None);
        assert_eq!(leaf.data, None);
        let mut resolved = leaf;
        resolved.data = Some(MARFValue::from(7));
        assert_eq!(bits::get_leaf_hash(&resolved), hash);
        input.set_position(plan.offsets[2].1);
        let (Record::Patch(patch), _) = read_output(&mut input, &mut scratch) else {
            panic!()
        };
        assert_eq!(patch.ptr.ptr(), 1036);
        assert_eq!(patch.ptr_diff[0].ptr(), plan.resolve(leaf_offset).unwrap());
        assert!(plan.resolve(leaf_offset + 1).is_err());

        // A second physical rewrite accepts both raw values and unresolved v1 locators.
        for external_value in [false, true] {
            let initial = BlobRelocation::plan_narrow(&source, |_| Ok(external_value)).unwrap();
            let v1 = initial
                .rewrite(
                    &source,
                    |_, ptr| Ok(ptr),
                    |_| Ok(external_value.then_some(extent)),
                )
                .unwrap();
            let next = BlobRelocation::plan_format(
                &v1,
                NodeRecordFormat::TypeFirstV1,
                NodeRecordFormat::TypeFirstV2,
                |_| Ok(()),
            )
            .unwrap();
            let v2 = next
                .rewrite_format(
                    &v1,
                    NodeRecordFormat::TypeFirstV1,
                    NodeRecordFormat::TypeFirstV2,
                    |_, ptr| Ok(ptr),
                    |_| Ok(()),
                )
                .unwrap();
            assert!(NodeRecordFormat::TypeFirstV2
                .validate_trie_header(&v2)
                .is_ok());
            assert!(NodeRecordFormat::TypeFirstV1
                .validate_trie_header(&v2)
                .is_err());
            let offset = next.resolve(initial.resolve(leaf_offset).unwrap()).unwrap();
            let record = NodeRecordFormat::TypeFirstV2
                .parse(&v2[offset as usize..])
                .unwrap();
            let (TrieNodeType::Leaf(leaf), _) = record.decode_node(TrieNodeID::Leaf as u8).unwrap()
            else {
                panic!()
            };
            assert_eq!(leaf.extent, external_value.then_some(extent));
            if !external_value {
                assert_eq!(leaf.value().unwrap(), &MARFValue::from(7));
                assert_eq!(bits::get_leaf_hash(&leaf), hash);
                assert!(v2.len() < v1.len());
            }
            if external_value {
                use crate::chainstate::stacks::index::inline_value::InlineValue;
                let inline = InlineValue::from_parts(&[1, 2, 3], &[4, 5]).unwrap();
                let transform = |leaf: &mut TrieLeaf| {
                    if leaf.extent.is_some() {
                        leaf.extent = None;
                        leaf.inline = Some(inline.clone());
                    }
                    Ok(())
                };
                let next = BlobRelocation::plan_format(
                    &v2,
                    NodeRecordFormat::TypeFirstV2,
                    NodeRecordFormat::TypeFirstV3,
                    transform,
                )
                .unwrap();
                let v3 = next
                    .rewrite_format(
                        &v2,
                        NodeRecordFormat::TypeFirstV2,
                        NodeRecordFormat::TypeFirstV3,
                        |_, ptr| Ok(ptr),
                        transform,
                    )
                    .unwrap();
                let pos = next.resolve(offset).unwrap() as usize;
                let record = NodeRecordFormat::TypeFirstV3.parse(&v3[pos..]).unwrap();
                let (TrieNodeType::Leaf(leaf), _) =
                    record.decode_node(TrieNodeID::Leaf as u8).unwrap()
                else {
                    panic!("expected inline")
                };
                assert_eq!(leaf.inline, Some(inline));
                assert_eq!(leaf.extent, None);
                assert_eq!(leaf.data, None);
                assert!(v3.len() < v2.len());
                assert!(BlobRelocation::plan_format(
                    &v3,
                    NodeRecordFormat::TypeFirstV3,
                    NodeRecordFormat::TypeFirstV2,
                    |_| Ok(())
                )
                .is_err());
            }
            assert!(BlobRelocation::plan_format(
                &v2,
                NodeRecordFormat::TypeFirstV2,
                NodeRecordFormat::TypeFirstV1,
                |_| Ok(())
            )
            .is_err());
        }
    }
}

#[cfg(test)]
mod packed_tests {
    use super::*;
    use crate::chainstate::stacks::index::node::{TrieNode, TrieNode256, TrieNodeID};

    /// Compact all records exactly, including large historical patch-base pointers.
    #[test]
    fn packed_relocation_resolves_ancestors_and_removes_all_record_gaps() {
        let source_format = NodeRecordFormat::TypeFirstV3;
        let destination = NodeRecordFormat::TypeFirstV4;
        let mut root = TrieNode256::empty();
        root.ptrs[0] = TriePtr::new(TrieNodeID::Leaf as u8, 0, 1000);
        root.ptrs[0].back_block = 42; // Inline squash annotation must not invoke ancestor lookup.
        root.ptrs[1] = TriePtr::new(TrieNodeID::Node4 as u8, 1, 2000);
        root.ptrs[2] = TriePtr::new_backptr(TrieNodeID::Leaf as u8, 2, u64::MAX, 9);
        let leaf = TrieLeaf::from_value(&[], MARFValue::from(7));
        let patch = TrieNodePatch {
            ptr: TriePtr::new_backptr(TrieNodeID::Node4 as u8, 0, u64::MAX, 9),
            ptr_diff: vec![
                TriePtr::new(TrieNodeID::Leaf as u8, 4, 1000),
                TriePtr::new_backptr(TrieNodeID::Leaf as u8, 5, u64::MAX, 10),
            ],
        };
        let mut source = vec![];
        source_format
            .write_trie_header(&mut source, &StacksBlockId([2; 32]))
            .unwrap();
        source_format
            .write_node(
                &mut source,
                &TrieNodeType::Node256(Box::new(root)),
                TrieHash([3; 32]),
                true,
            )
            .unwrap();
        assert!(source.len() < 1000);
        source.resize(1000, 0);
        source_format
            .write_node(
                &mut source,
                &TrieNodeType::Leaf(leaf),
                TrieHash([4; 32]),
                true,
            )
            .unwrap();
        source.resize(2000, 0);
        source_format
            .write_patch(&mut source, &patch, TrieHash([5; 32]))
            .unwrap();
        for target in [
            65535,
            65536,
            16777215,
            16777216,
            u64::from(u32::MAX),
            u64::from(u32::MAX) + 1,
        ] {
            let resolve = |block: u32, offset: u64| {
                assert_eq!(offset, u64::MAX);
                assert!([9, 10].contains(&block));
                Ok(target)
            };
            let plan = BlobRelocation::plan_packed(&source, source_format, resolve).unwrap();
            let bytes = plan
                .rewrite_format(&source, source_format, destination, resolve, |_| Ok(()))
                .unwrap();
            assert_eq!(bytes.len() as u64, plan.length);
            assert!(bytes.len() < source.len());
            let mut cursor = Cursor::new(bytes.as_slice());
            let mut scratch = MarfReadState::new();
            for (index, (_, offset)) in plan.offsets.iter().enumerate() {
                cursor.set_position(*offset);
                let (record, hash) = read_record(&mut cursor, &mut scratch, destination).unwrap();
                let end = plan.offsets.get(index + 1).map_or(plan.length, |p| p.1);
                assert_eq!(
                    cursor.position(),
                    end,
                    "record contains reservation padding"
                );
                match record {
                    Record::Node(TrieNodeType::Node256(root)) => {
                        assert_eq!(root.ptrs[0].ptr, plan.resolve(1000).unwrap());
                        assert_eq!(root.ptrs[0].back_block, 42);
                        assert_eq!(root.ptrs[1].ptr, plan.resolve(2000).unwrap());
                        assert_eq!(root.ptrs[2].ptr, target);
                        assert_eq!(hash, Some(TrieHash([3; 32])));
                    }
                    Record::Patch(patch) => {
                        assert_eq!(patch.ptr.ptr, target);
                        assert_eq!(patch.ptr_diff[0].ptr, plan.resolve(1000).unwrap());
                        assert_eq!(patch.ptr_diff[1].ptr, target);
                        assert_eq!(hash, Some(TrieHash([5; 32])));
                    }
                    Record::Node(TrieNodeType::Leaf(leaf)) => {
                        assert_eq!(leaf.value().unwrap(), &MARFValue::from(7))
                    }
                    _ => panic!("unexpected relocated node"),
                }
            }
            assert!(plan.resolve(1001).is_err());
            let mut padded = plan.clone();
            padded.length += 1;
            assert!(padded
                .rewrite_format(&source, source_format, destination, resolve, |_| Ok(()))
                .is_err());
        }
        assert!(
            BlobRelocation::plan_packed(&source, source_format, |_, _| Err(corrupt(
                "missing ancestor"
            )))
            .is_err()
        );
        assert!(
            BlobRelocation::plan_format(&source, source_format, destination, |_| Ok(())).is_err()
        );
    }
}
