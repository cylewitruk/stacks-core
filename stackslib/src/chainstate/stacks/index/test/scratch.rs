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

use crate::chainstate::stacks::index::node::{
    TrieNode, TrieNode16, TrieNode256, TrieNode4, TrieNode48, TrieNodeID, TrieNodePatch,
    TrieNodeType, TriePtr,
};
use crate::chainstate::stacks::index::scratch::MarfReadState;
use crate::chainstate::stacks::index::{
    MARFValue, NodeDecodeScratch, NodePatching, PatchChainEntry, ReadTrieNode, TrieLeaf,
};
use crate::codec::StacksMessageCodec;

fn make_test_nodes() -> Vec<TrieNodeType> {
    let mut node4 = TrieNode4::new(&[0x01, 0x02]);
    assert!(node4.insert(&TriePtr::new(TrieNodeID::Leaf as u8, 0x11, 7)));

    let mut node16 = TrieNode16::new(&[0x03, 0x04, 0x05]);
    assert!(node16.insert(&TriePtr::new(TrieNodeID::Node4 as u8, 0x22, 9)));

    let mut node48 = TrieNode48::new(&[0x06, 0x07, 0x08, 0x09]);
    assert!(node48.insert(&TriePtr::new(TrieNodeID::Node16 as u8, 0x33, 11)));

    let mut node256 = TrieNode256::new(&[0x0a, 0x0b, 0x0c, 0x0d, 0x0e]);
    assert!(node256.insert(&TriePtr::new(TrieNodeID::Node48 as u8, 0x44, 13)));

    let leaf = TrieLeaf::from_value(&[0x0f, 0x10], MARFValue::from(77u32));

    vec![
        TrieNodeType::Node4(node4),
        TrieNodeType::Node16(node16),
        TrieNodeType::Node48(Box::new(node48)),
        TrieNodeType::Node256(Box::new(node256)),
        TrieNodeType::Leaf(leaf),
    ]
}

fn decode_into_scratch<'a>(
    scratch: &'a mut MarfReadState,
    node: &TrieNodeType,
) -> crate::chainstate::stacks::index::node::TrieNodeRef<'a> {
    let mut bytes = Vec::new();
    match node {
        TrieNodeType::Node4(node) => node.write_bytes(&mut bytes).unwrap(),
        TrieNodeType::Node16(node) => node.write_bytes(&mut bytes).unwrap(),
        TrieNodeType::Node48(node) => node.write_bytes(&mut bytes).unwrap(),
        TrieNodeType::Node256(node) => node.write_bytes(&mut bytes).unwrap(),
        TrieNodeType::Leaf(node) => node.write_bytes(&mut bytes).unwrap(),
    }
    let consumed = scratch.decode_node_from_slice(TrieNodeID::from_u8(node.id()).unwrap(), &bytes);
    assert_eq!(consumed.unwrap(), bytes.len());
    scratch.get_ref()
}

fn owned_from_scratch(scratch: &MarfReadState) -> TrieNodeType {
    let mut read = ReadTrieNode::from_state_borrowed(scratch.get_ref(), None);
    if let Some(meta) = scratch.transient_meta() {
        read = read.with_transient_meta(meta);
    }
    read.into_owned_node().unwrap().0
}

#[test]
fn trie_node_decode_scratch_decode_and_get_ref_per_variant() {
    let mut scratch = MarfReadState::new();

    for node in make_test_nodes() {
        let node_ref = decode_into_scratch(&mut scratch, &node);
        assert_eq!(node_ref.to_owned_node(), node);

        let get_ref = scratch.get_ref();
        assert_eq!(get_ref.to_owned_node(), node);
    }
}

#[test]
fn trie_node_decode_scratch_overwrite_tracks_latest_node() {
    let mut scratch = MarfReadState::new();

    let node4 = TrieNodeType::Node4(TrieNode4::new(&[0x01]));
    let node16 = TrieNodeType::Node16(TrieNode16::new(&[0x02, 0x03]));
    let leaf = TrieNodeType::Leaf(TrieLeaf::from_value(
        &[0x04, 0x05, 0x06],
        MARFValue::from(9u32),
    ));

    decode_into_scratch(&mut scratch, &node4);
    assert_eq!(scratch.get_ref().to_owned_node(), node4);

    decode_into_scratch(&mut scratch, &node16);
    assert_eq!(scratch.get_ref().to_owned_node(), node16);

    decode_into_scratch(&mut scratch, &leaf);
    assert_eq!(scratch.get_ref().to_owned_node(), leaf);
}

#[test]
fn trie_node_decode_scratch_to_owned_preserves_patch_metadata() {
    let mut scratch = MarfReadState::new();

    let mut node4 = TrieNode4::new(&[0x01, 0x02]);
    assert!(node4.insert(&TriePtr::new(TrieNodeID::Leaf as u8, 0x11, 7)));
    node4.meta.patch_depth = 1;
    node4.meta.last_patch_source = Some((5, TriePtr::new(TrieNodeID::Node4 as u8, 0x22, 9)));

    let node = TrieNodeType::Node4(node4);
    scratch.store(node.clone());

    assert_eq!(owned_from_scratch(&scratch), node);
    assert_ne!(scratch.get_ref().to_owned_node(), node);
}

#[test]
fn trie_node_decode_scratch_apply_patches_in_place_matches_owned_path() {
    let mut scratch = MarfReadState::new();

    let mut node4 = TrieNode4::new(&[0x01, 0x02]);
    assert!(node4.insert(&TriePtr::new(TrieNodeID::Leaf as u8, 0x11, 7)));

    let patches = vec![
        PatchChainEntry {
            block_id: 5,
            ptr: TriePtr::new_backptr(TrieNodeID::Node4 as u8, 0x00, 9, 4),
            patch: TrieNodePatch {
                ptr: TriePtr::new_backptr(TrieNodeID::Node4 as u8, 0x00, 9, 4),
                ptr_diff: vec![TriePtr::new(TrieNodeID::Leaf as u8, 0x22, 11)],
            },
        },
        PatchChainEntry {
            block_id: 6,
            ptr: TriePtr::new_backptr(TrieNodeID::Node4 as u8, 0x00, 9, 5),
            patch: TrieNodePatch {
                ptr: TriePtr::new_backptr(TrieNodeID::Node4 as u8, 0x00, 9, 5),
                ptr_diff: vec![TriePtr::new(TrieNodeID::Leaf as u8, 0x33, 12)],
            },
        },
    ];

    // Build expected result by applying patches in-place to a clone
    let mut expected_node4 = node4.clone();
    for entry in patches.iter() {
        assert!(entry.patch.apply_to(&mut expected_node4, entry.block_id, 6));
    }
    expected_node4.meta.patch_depth += patches.len();
    expected_node4.meta.last_patch_source = patches.last().map(|entry| (entry.block_id, entry.ptr));
    let expected = TrieNodeType::Node4(expected_node4);

    decode_into_scratch(&mut scratch, &TrieNodeType::Node4(node4));
    scratch
        .apply_patches_in_place(&patches, 6)
        .expect("in-place patch application should succeed");

    assert_eq!(owned_from_scratch(&scratch), expected);

    let recovered_capacity = patches
        .iter()
        .map(|entry| entry.patch.ptr_diff.capacity())
        .max()
        .unwrap();
    scratch.restore_patch_chain_buf(patches);
    assert!(scratch.patch().ptr_diff.capacity() >= recovered_capacity);
}

#[test]
fn trie_node_decode_scratch_decode_patch_from_slice_reuses_storage() {
    let mut scratch = MarfReadState::new();
    let patch_a = TrieNodePatch {
        ptr: TriePtr::new_backptr(TrieNodeID::Node16 as u8, 0x10, 0x1111, 0x2222),
        ptr_diff: vec![
            TriePtr::new_backptr(TrieNodeID::Leaf as u8, 0x20, 0x3333, 0x4444),
            TriePtr::new(TrieNodeID::Node4 as u8, 0x21, 0x5555),
            TriePtr::new(TrieNodeID::Node16 as u8, 0x22, 0x6666),
        ],
    };
    let patch_b = TrieNodePatch {
        ptr: TriePtr::new_backptr(TrieNodeID::Node4 as u8, 0x30, 0x7777, 0x8888),
        ptr_diff: vec![TriePtr::new(TrieNodeID::Leaf as u8, 0x31, 0x9999)],
    };

    let mut patch_a_bytes = Vec::new();
    patch_a.consensus_serialize(&mut patch_a_bytes).unwrap();
    scratch.decode_patch_from_slice(&patch_a_bytes).unwrap();
    let first_capacity = scratch.patch().ptr_diff.capacity();
    assert_eq!(scratch.patch(), &patch_a);

    let mut patch_b_bytes = Vec::new();
    patch_b.consensus_serialize(&mut patch_b_bytes).unwrap();
    scratch.decode_patch_from_slice(&patch_b_bytes).unwrap();

    assert_eq!(scratch.patch(), &patch_b);
    assert!(scratch.patch().ptr_diff.capacity() >= first_capacity);
}
