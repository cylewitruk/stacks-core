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

//! MARF tests related to [`TrieNodePatch`] node type.

use std::fs;
use std::io::{Cursor, Read as _};

use sha2::{Digest as _, Sha256};

use super::*;
use crate::chainstate::stacks::index::{ClarityMarfTrieId as _, trie_sql};
use crate::codec::{Error as codec_error, StacksMessageCodec as _};

#[test]
fn trie_node_patch_try_from_nodetype_returns_none_when_no_diffs() {
    let node = TrieNodeType::Node4(TrieNode4::new(&[1]));

    let old_node_ptr = TriePtr::default();
    let old_node = &node;
    let new_node = &node;
    let result = TrieNodePatch::try_from_nodetype(old_node_ptr, old_node, new_node);

    assert!(
        result.is_none(),
        "None because the computed patch has no diffs"
    );
}

#[test]
fn trie_node_patch_try_from_patch_returns_none_when_no_diffs() {
    let old_patch_ptr = TriePtr::new(TrieNodeID::Node4 as u8, 0, 0);
    let old_patch = TrieNodePatch {
        ptr: old_patch_ptr.clone(),
        ptr_diff: vec![],
    };
    let new_node = TrieNodeType::Node4(TrieNode4::new(&[1]));
    let result = TrieNodePatch::try_from_patch(old_patch_ptr, &old_patch, &new_node);

    assert!(
        result.is_none(),
        "None because the computed patch has no diffs"
    );
}

#[test]
fn trie_node_patch_serialize_ok() {
    let patch_node = TrieNodePatch {
        ptr: TriePtr::new(1, 10, 0),
        ptr_diff: vec![TriePtr::new(1, 20, 0).clone(); 1],
    };

    let mut buffer = Cursor::new(Vec::new());
    patch_node
        .consensus_serialize(&mut buffer)
        .expect("serialization should be ok");

    // To fit in 1 byte, diff count is serialized 0-based (where 0 => 1 and 255 => 256)
    let diff_count = 0u8;
    assert_eq!(
        vec![6, 65, 10, 0, 0, 0, 0, diff_count, 65, 20, 0, 0, 0, 0],
        buffer.into_inner(),
    );
}

#[test]
fn trie_node_patch_serialize_fails_with_ptr_diffs_len_0() {
    let patch_node = TrieNodePatch {
        ptr: TriePtr::default(),
        ptr_diff: vec![],
    };

    let mut buffer = Cursor::new(Vec::new());
    let error = patch_node
        .consensus_serialize(&mut buffer)
        .expect_err("serialization should fail");

    assert!(
        matches!(&error, codec_error::SerializeError(msg) if msg.contains("len 0")),
        "instead got: {error}"
    );
}

#[test]
fn trie_node_patch_serialize_ok_with_ptr_diffs_len_256() {
    let patch_node = TrieNodePatch {
        ptr: TriePtr::default(),
        ptr_diff: vec![TriePtr::default(); 256],
    };

    let mut buffer = Cursor::new(Vec::new());
    let result = patch_node.consensus_serialize(&mut buffer);
    assert!(
        result.is_ok(),
        "Got Error: {}",
        result.unwrap_err().to_string()
    );
}

#[test]
fn trie_node_patch_serialize_fails_with_ptr_diffs_len_257() {
    let patch_node = TrieNodePatch {
        ptr: TriePtr::default(),
        ptr_diff: vec![TriePtr::default(); 257],
    };

    let mut buffer = Cursor::new(Vec::new());
    let error = patch_node
        .consensus_serialize(&mut buffer)
        .expect_err("serialization should fail");

    assert!(
        matches!(&error, codec_error::SerializeError(msg) if msg.contains("len 257")),
        "instead got: {error}"
    );
}

#[test]
fn trie_node_patch_deserialize_ok_with_ptr_diffs_len_1() {
    // To fit in 1 byte, diff count is serialized 0-based (where 0 => 1 and 255 => 256)
    let diff_count = 0u8;
    let mut buffer = Cursor::new(vec![6, 65, 10, 0, 0, 0, 0, diff_count, 65, 20, 0, 0, 0, 0]);

    let patch_node =
        TrieNodePatch::consensus_deserialize(&mut buffer).expect("deserialization should be ok");

    let expected = TrieNodePatch {
        ptr: TriePtr::new(1, 10, 0),
        ptr_diff: vec![TriePtr::new(1, 20, 0); 1],
    };
    assert_eq!(expected, patch_node);
}

#[test]
fn trie_node_patch_u64_ptr_roundtrip_ok() {
    let patch_node = TrieNodePatch {
        ptr: TriePtr::new(1, 10, u64::from(u32::MAX) + 7),
        ptr_diff: vec![TriePtr::new(1, 20, u64::from(u32::MAX) + 11)],
    };

    let mut buffer = Cursor::new(Vec::new());
    patch_node
        .consensus_serialize(&mut buffer)
        .expect("u64 ptr serialization should be ok");

    let decoded = TrieNodePatch::consensus_deserialize(&mut Cursor::new(buffer.into_inner()))
        .expect("u64 ptr deserialization should be ok");
    assert_eq!(patch_node, decoded);
}

#[test]
fn trie_node_patch_apply_node4_preserves_inline_payload_pointer_identity() {
    let mut old_node = TrieNode4::new(&[]);
    let mut inline_with_payload = TriePtr::new(TrieNodeID::Node16 as u8, 0x10, 1234);
    inline_with_payload.back_block = 55;
    assert!(old_node.insert(&inline_with_payload));

    let patch = TrieNodePatch {
        ptr: TriePtr::new_backptr(TrieNodeID::Node4 as u8, 0x00, 1, 7),
        ptr_diff: vec![TriePtr::new(TrieNodeID::Node16 as u8, 0x20, 2345)],
    };

    let mut patched = old_node;
    assert!(
        patch.apply_to(&mut patched, 8, 99),
        "patch application should succeed"
    );
    let patched_ptr = patched
        .walk(0x10)
        .expect("inline child with payload should still exist");
    assert!(is_backptr(patched_ptr.id()));
    assert_eq!(patched_ptr.back_block(), 55);
}

#[test]
fn trie_node_patch_u64_ptr_serialize_fails_with_ptr_diffs_len_0() {
    let patch_node = TrieNodePatch {
        ptr: TriePtr::new(TrieNodeID::Node4 as u8, 1, 77),
        ptr_diff: vec![],
    };
    let mut buffer = Cursor::new(Vec::new());
    let error = patch_node
        .consensus_serialize(&mut buffer)
        .expect_err("u64 ptr serialization should fail");
    assert!(
        matches!(&error, codec_error::SerializeError(msg) if msg.contains("len 0")),
        "instead got: {error}"
    );
}

#[test]
fn trie_node_patch_u64_ptr_serialize_fails_with_ptr_diffs_len_257() {
    let patch_node = TrieNodePatch {
        ptr: TriePtr::new(TrieNodeID::Node4 as u8, 1, 77),
        ptr_diff: vec![TriePtr::new(TrieNodeID::Node4 as u8, 2, 88); 257],
    };
    let mut buffer = Cursor::new(Vec::new());
    let error = patch_node
        .consensus_serialize(&mut buffer)
        .expect_err("u64 ptr serialization should fail");
    assert!(
        matches!(&error, codec_error::SerializeError(msg) if msg.contains("len 257")),
        "instead got: {error}"
    );
}

#[test]
fn trie_node_patch_u64_ptr_deserialize_fails_on_truncated_payload() {
    let patch_node = TrieNodePatch {
        ptr: TriePtr::new_backptr(TrieNodeID::Node4 as u8, 0x01, u64::from(u32::MAX) + 2, 5),
        ptr_diff: vec![
            TriePtr::new(TrieNodeID::Node16 as u8, 0x02, u64::from(u32::MAX) + 3),
            TriePtr::new_backptr(TrieNodeID::Node16 as u8, 0x03, u64::from(u32::MAX) + 4, 7),
        ],
    };

    let mut buffer = Cursor::new(Vec::new());
    patch_node
        .consensus_serialize(&mut buffer)
        .expect("u64 ptr serialization should be ok");
    let mut payload = buffer.into_inner();
    payload.pop();

    let error = TrieNodePatch::consensus_deserialize(&mut Cursor::new(payload))
        .expect_err("u64 ptr deserialization should fail on truncated payload");
    assert!(
        error.to_string().contains("fill whole buffer"),
        "instead got: {error}"
    );
}

#[test]
fn trie_node_patch_u64_ptr_deserialize_fails_on_malformed_payload() {
    let patch_node = TrieNodePatch {
        ptr: TriePtr::new(TrieNodeID::Node4 as u8, 1, 2),
        ptr_diff: vec![TriePtr::new(TrieNodeID::Node16 as u8, 2, 3)],
    };
    let mut buffer = Cursor::new(Vec::new());
    patch_node
        .consensus_serialize(&mut buffer)
        .expect("u64 ptr serialization should be ok");
    let mut payload = buffer.into_inner();

    // Corrupt the patch node marker.
    payload[0] = TrieNodeID::Leaf as u8;

    let error = TrieNodePatch::consensus_deserialize(&mut Cursor::new(payload))
        .expect_err("u64 ptr deserialization should fail on malformed payload");
    assert!(
        error
            .to_string()
            .contains("Did not read a TrieNodeID::Patch"),
        "instead got: {error}"
    );
}

#[test]
fn trie_node_patch_u64_ptr_roundtrip_mixed_backptrs() {
    let patch_node = TrieNodePatch {
        ptr: TriePtr::new_backptr(TrieNodeID::Node16 as u8, 0x10, u64::from(u32::MAX) + 55, 42),
        ptr_diff: vec![
            TriePtr::new(TrieNodeID::Node4 as u8, 0x11, u64::from(u32::MAX) + 56),
            TriePtr::new_backptr(TrieNodeID::Node48 as u8, 0x12, u64::from(u32::MAX) + 57, 43),
            TriePtr::new(TrieNodeID::Node256 as u8, 0x13, 19),
        ],
    };

    let mut buffer = Cursor::new(Vec::new());
    patch_node
        .consensus_serialize(&mut buffer)
        .expect("u64 ptr mixed serialization should be ok");
    let decoded = TrieNodePatch::consensus_deserialize(&mut Cursor::new(buffer.into_inner()))
        .expect("u64 ptr mixed deserialization should be ok");
    assert_eq!(patch_node, decoded);
}

/// [`TrieNodePatch::make_ptr_diff`] in the following scenario:
///
/// ## Input
/// - `old_ptrs` is empty
/// - `new_ptrs` contains a single empty pointer
///
/// ## Expected behavior
/// - No differences are produced
#[test]
fn trie_node_patch_make_ptr_diff_case1() {
    let old_node_ptr = TriePtr::new_backptr(TrieNodeID::Patch as u8, 0x00, 0, 1);
    let old_ptrs = [];
    let new_ptrs = [TriePtr::new(TrieNodeID::Empty as u8, 0x00, 0)];

    let diff = TrieNodePatch::make_ptr_diff_for_test(&old_node_ptr, &old_ptrs, &new_ptrs);
    assert_eq!(0, diff.len());
}

/// [`TrieNodePatch::make_ptr_diff`] in the following scenario:
///
/// ## Input
/// - `old_ptrs` is empty
/// - `new_ptrs` contains:
///   - one normal (non-backpointer) node
///   - one backpointer node
///
/// ## Expected behavior
/// - Both pointers are reported as differences
#[test]
fn trie_node_patch_make_ptr_diff_case2() {
    let old_node_ptr = TriePtr::new_backptr(TrieNodeID::Patch as u8, 0x00, 0, 1);
    let old_ptrs = [];
    let new_ptrs = [
        TriePtr::new(TrieNodeID::Node4 as u8, 0x00, 0),
        TriePtr::new_backptr(TrieNodeID::Node4 as u8, 0x01, 0, 1),
    ];

    let diff = TrieNodePatch::make_ptr_diff_for_test(&old_node_ptr, &old_ptrs, &new_ptrs);
    assert_eq!(2, diff.len());
    assert_eq!(TriePtr::new(TrieNodeID::Node4 as u8, 0x00, 0), diff[0]);
    assert_eq!(
        TriePtr::new_backptr(TrieNodeID::Node4 as u8, 0x01, 0, 1),
        diff[1]
    );
}

/// [`TrieNodePatch::make_ptr_diff`] in the following scenario:
///
/// ## Input
/// - `old_ptr` is **not** a backpointer
/// - `new_ptr` **is** a backpointer
/// - `new_ptr.back_block` matches `old_node_ptr.back_block`
/// - After normalization, `new_ptr` equals `old_ptr`
///
/// ## Expected behavior
/// - No differences are produced
#[test]
fn trie_node_patch_make_ptr_diff_case3() {
    let old_node_ptr = TriePtr::new_backptr(TrieNodeID::Patch as u8, 0x00, 0, 1);
    let old_ptrs = [TriePtr::new(TrieNodeID::Node4 as u8, 0x00, 0)];
    let new_ptrs = [TriePtr::new_backptr(TrieNodeID::Node4 as u8, 0x00, 0, 1)];

    let diff = TrieNodePatch::make_ptr_diff_for_test(&old_node_ptr, &old_ptrs, &new_ptrs);
    assert_eq!(0, diff.len());
}

/// [`TrieNodePatch::make_ptr_diff`] in the following scenario:
///
/// ## Input
/// - `old_ptr` is **not** a backpointer
/// - `new_ptr` **is** a backpointer
/// - `new_ptr.back_block` matches `old_node_ptr.back_block`
/// - After normalization, `new_ptr` does **not** equal `old_ptr`
///
/// ## Expected behavior
/// - The new pointer is reported as a difference
#[test]
fn trie_node_patch_make_ptr_diff_case4() {
    let old_node_ptr = TriePtr::new_backptr(TrieNodeID::Patch as u8, 0x00, 0, 1);
    let old_ptrs = [TriePtr::new(TrieNodeID::Node4 as u8, 0x00, 0)];
    let new_ptrs = [TriePtr::new_backptr(TrieNodeID::Node4 as u8, 0x00, 100, 1)];

    let diff = TrieNodePatch::make_ptr_diff_for_test(&old_node_ptr, &old_ptrs, &new_ptrs);
    assert_eq!(1, diff.len());
    assert_eq!(
        TriePtr::new_backptr(TrieNodeID::Node4 as u8, 0x00, 100, 1),
        diff[0]
    );
}

/// [`TrieNodePatch::make_ptr_diff`] in the following scenario:
///
/// ## Input
/// - `old_ptr` is **not** a backpointer
/// - `new_ptr` **is** a backpointer
/// - `new_ptr.back_block` does **not** match `old_node_ptr.back_block`
/// - `new_ptr` does **not** equal `old_ptr`
///
/// ## Expected behavior
/// - The new pointer is reported as a difference
#[test]
fn trie_node_patch_make_ptr_diff_case5() {
    let old_node_ptr = TriePtr::new_backptr(TrieNodeID::Patch as u8, 0x00, 0, 1);
    let old_ptrs = [TriePtr::new(TrieNodeID::Node4 as u8, 0x00, 0)];
    let new_ptrs = [TriePtr::new_backptr(
        TrieNodeID::Node4 as u8,
        0x00,
        100,
        100,
    )];

    let diff = TrieNodePatch::make_ptr_diff_for_test(&old_node_ptr, &old_ptrs, &new_ptrs);
    assert_eq!(1, diff.len());
    assert_eq!(
        TriePtr::new_backptr(TrieNodeID::Node4 as u8, 0x00, 100, 100),
        diff[0]
    );
}

/// [`TrieNodePatch::make_ptr_diff`] in the following scenario:
///
/// ## Input
/// - `old_ptr` **is** a backpointer
/// - `new_ptr` **is** a backpointer
/// - `new_ptr` equals `old_ptr`
///
/// ## Expected behavior
/// - No differences are produced
#[test]
fn trie_node_patch_make_ptr_diff_case6() {
    let old_node_ptr = TriePtr::new_backptr(TrieNodeID::Patch as u8, 0x00, 0, 1);
    let old_ptrs = [TriePtr::new_backptr(TrieNodeID::Node4 as u8, 0x00, 0x00, 2)];
    let new_ptrs = [TriePtr::new_backptr(TrieNodeID::Node4 as u8, 0x00, 0x00, 2)];

    let diff = TrieNodePatch::make_ptr_diff_for_test(&old_node_ptr, &old_ptrs, &new_ptrs);
    assert_eq!(0, diff.len());
}

/// [`TrieNodePatch::make_ptr_diff`] in the following scenario:
///
/// ## Input
/// - `old_ptr` is **not** a backpointer
/// - `new_ptr` is **not** a backpointer
/// - `new_ptr` equals `old_ptr`
///
/// ## Expected behavior
/// - The pointer is reported as a difference
#[test]
fn trie_node_patch_make_ptr_diff_case7() {
    let old_node_ptr = TriePtr::new_backptr(TrieNodeID::Patch as u8, 0x00, 0, 1);
    let old_ptrs = [TriePtr::new(TrieNodeID::Node4 as u8, 0x00, 0x00)];
    let new_ptrs = [TriePtr::new(TrieNodeID::Node4 as u8, 0x00, 0x00)];

    let diff = TrieNodePatch::make_ptr_diff_for_test(&old_node_ptr, &old_ptrs, &new_ptrs);
    assert_eq!(1, diff.len());
    assert_eq!(TriePtr::new(TrieNodeID::Node4 as u8, 0x00, 0x00), diff[0]);
}

/// [`TrieNodePatch::make_ptr_diff`] in the following scenario:
///
/// ## Input
/// - `old_ptrs` contains a non-empty pointer
/// - `new_ptrs` contains a single empty pointer
///
/// ## Expected behavior
/// - No differences are produced
///
/// ## Note
/// In real scenarios, a Trie node with only empty pointers won't exist,
/// as nodes are created only when at least one child is present.
/// This test exists purely to exercise `make_ptr_diff` with such an input,
/// ensuring all code paths are covered and behavior is well-defined.
#[test]
fn trie_node_patch_make_ptr_diff_case8() {
    let old_node_ptr = TriePtr::new_backptr(TrieNodeID::Patch as u8, 0x00, 0, 1);
    let old_ptrs = [TriePtr::new(TrieNodeID::Node4 as u8, 0x00, 0)];
    let new_ptrs = [TriePtr::new(TrieNodeID::Empty as u8, 0x00, 0)];

    let diff = TrieNodePatch::make_ptr_diff_for_test(&old_node_ptr, &old_ptrs, &new_ptrs);
    assert_eq!(0, diff.len());
}

/// [`TrieNodePatch::make_ptr_diff`] in the following scenario:
///
/// ## Input
/// - `old_ptr` **is** a backpointer
/// - `new_ptr` is **not** a backpointer
/// - Both pointers refer to the same logical node
///
/// ## Expected behavior
/// - The new pointer is reported as a difference
#[test]
fn trie_node_patch_make_ptr_diff_case9() {
    let old_node_ptr = TriePtr::new_backptr(TrieNodeID::Patch as u8, 0x00, 0, 1);
    let old_ptrs = [TriePtr::new_backptr(TrieNodeID::Node4 as u8, 0x00, 42, 2)];
    let new_ptrs = [TriePtr::new(TrieNodeID::Node4 as u8, 0x00, 42)];

    let diff = TrieNodePatch::make_ptr_diff_for_test(&old_node_ptr, &old_ptrs, &new_ptrs);
    assert_eq!(1, diff.len());
    assert_eq!(TriePtr::new(TrieNodeID::Node4 as u8, 0x00, 42), diff[0]);
}

/// [`TrieNodePatch::make_ptr_diff`] in the following scenario:
///
/// ## Input
/// - `old_ptr` **is** a backpointer
/// - `new_ptr` is **not** a backpointer
/// - `new_ptr` does **not** equal `old_ptr`
///
/// ## Expected behavior
/// - The new pointer is reported as a difference
#[test]
fn trie_node_patch_make_ptr_diff_case10() {
    let old_node_ptr = TriePtr::new_backptr(TrieNodeID::Patch as u8, 0x00, 0, 1);
    let old_ptrs = [TriePtr::new_backptr(TrieNodeID::Node4 as u8, 0x00, 10, 2)];
    let new_ptrs = [TriePtr::new(TrieNodeID::Node4 as u8, 0x00, 99)];

    let diff = TrieNodePatch::make_ptr_diff_for_test(&old_node_ptr, &old_ptrs, &new_ptrs);
    assert_eq!(1, diff.len());
    assert_eq!(TriePtr::new(TrieNodeID::Node4 as u8, 0x00, 99), diff[0]);
}

/// [`TrieNodePatch::make_ptr_diff`] in the following scenario:
///
/// ## Input
/// - `old_ptr` **is** a backpointer
/// - `new_ptr` **is** a backpointer
/// - `new_ptr.back_block` matches `old_node_ptr.back_block`
/// - `new_ptr` does **not** equal `old_ptr`
///
/// ## Expected behavior
/// - The new pointer is reported as a difference
#[test]
fn trie_node_patch_make_ptr_diff_case11() {
    let old_node_ptr = TriePtr::new_backptr(TrieNodeID::Patch as u8, 0x00, 0, 1);
    let old_ptrs = [TriePtr::new_backptr(TrieNodeID::Node4 as u8, 0x00, 10, 1)];
    let new_ptrs = [TriePtr::new_backptr(TrieNodeID::Node4 as u8, 0x00, 20, 1)];

    let diff = TrieNodePatch::make_ptr_diff_for_test(&old_node_ptr, &old_ptrs, &new_ptrs);
    assert_eq!(1, diff.len());
    assert_eq!(
        TriePtr::new_backptr(TrieNodeID::Node4 as u8, 0x00, 20, 1),
        diff[0]
    );
}

/// [`TrieNodePatch::make_ptr_diff`] in the following scenario:
///
/// ## Input
/// - `old_ptrs` contains multiple pointers with the same `chr`
/// - The last pointer with that `chr` overwrites the previous one
/// - `new_ptr` matches the last `old_ptr`
///
/// ## Expected behavior
/// - No differences are produced
///
/// ## Note
/// In real scenarios, a Trie node has at most one pointer per `chr` value.
/// This test exists purely to exercise `make_ptr_diff` with such an input,
/// ensuring all code paths are covered and behavior is well-defined.
#[test]
fn trie_node_patch_make_ptr_diff_case12() {
    let old_node_ptr = TriePtr::new_backptr(TrieNodeID::Patch as u8, 0x00, 0, 1);
    let old_ptrs = [
        TriePtr::new(TrieNodeID::Node4 as u8, 0x01, 10),
        TriePtr::new(TrieNodeID::Node4 as u8, 0x01, 20),
    ];
    let new_ptrs = [TriePtr::new(TrieNodeID::Node4 as u8, 0x01, 20)];

    let diff = TrieNodePatch::make_ptr_diff_for_test(&old_node_ptr, &old_ptrs, &new_ptrs);
    assert_eq!(1, diff.len());
    assert_eq!(TriePtr::new(TrieNodeID::Node4 as u8, 0x01, 20), diff[0]);
}

/// Aggregated test of [`TrieNodePatch::make_ptr_diff`] combining all singular scenarios.
///
/// ## Input
/// - `old_ptrs` contains a mix of:
///   - non-backpointers
///   - backpointers
///   - duplicate `chr` entries (last one wins)
/// - `new_ptrs` contains a mix of:
///   - empty pointers
///   - normalized backpointers
///   - mismatching backpointers
///   - matching and non-matching non-backpointers
///
/// ## Expected behavior
/// - Only pointers that semantically differ from their corresponding old pointers
///   are included in the diff
#[test]
fn trie_node_patch_make_ptr_diff_all_in_one() {
    let old_node_ptr = TriePtr::new_backptr(TrieNodeID::Patch as u8, 0x00, 0, 1);

    let old_ptrs = [
        // Case 3 / 4 / 5
        TriePtr::new(TrieNodeID::Node4 as u8, 0x00, 0),
        // Case 6 / 9 / 10
        TriePtr::new_backptr(TrieNodeID::Node4 as u8, 0x01, 10, 2),
        // Case 7
        TriePtr::new(TrieNodeID::Node4 as u8, 0x02, 20),
        // Case 12: duplicate chr, first (overwritten)
        TriePtr::new(TrieNodeID::Node4 as u8, 0x03, 30),
        // Case 12: duplicate chr, second (effective)
        TriePtr::new(TrieNodeID::Node4 as u8, 0x03, 40),
    ];

    let new_ptrs = [
        // Case 1 / 8: empty pointer (ignored)
        TriePtr::new(TrieNodeID::Empty as u8, 0xFF, 0),
        // Case 3: normalized backptr equals old_ptr (no diff)
        TriePtr::new_backptr(TrieNodeID::Node4 as u8, 0x00, 0, 1),
        // Case 4: normalized backptr != old_ptr (diff)
        TriePtr::new_backptr(TrieNodeID::Node4 as u8, 0x00, 100, 1),
        // Case 9: old backptr, new non-backptr, same target (diff)
        TriePtr::new(TrieNodeID::Node4 as u8, 0x01, 10),
        // Case 10: old backptr, new non-backptr, different target (diff)
        TriePtr::new(TrieNodeID::Node4 as u8, 0x01, 99),
        // Case 7: both non-backptr equal (diff)
        TriePtr::new(TrieNodeID::Node4 as u8, 0x02, 20),
        // Case 11: both backptr, unequal, same back_block (diff)
        TriePtr::new_backptr(TrieNodeID::Node4 as u8, 0x02, 200, 1),
        // Case 12: duplicate chr, matches last old_ptr (diff)
        TriePtr::new(TrieNodeID::Node4 as u8, 0x03, 40),
        // Case 2: new_ptr with no corresponding old_ptr (diff)
        TriePtr::new_backptr(TrieNodeID::Node4 as u8, 0x04, 0, 1),
    ];

    let diff = TrieNodePatch::make_ptr_diff_for_test(&old_node_ptr, &old_ptrs, &new_ptrs);

    let expected = vec![
        // Case 4
        TriePtr::new_backptr(TrieNodeID::Node4 as u8, 0x00, 100, 1),
        // Case 9
        TriePtr::new(TrieNodeID::Node4 as u8, 0x01, 10),
        // Case 10
        TriePtr::new(TrieNodeID::Node4 as u8, 0x01, 99),
        // Case 7
        TriePtr::new(TrieNodeID::Node4 as u8, 0x02, 20),
        // Case 11
        TriePtr::new_backptr(TrieNodeID::Node4 as u8, 0x02, 200, 1),
        // Case 12
        TriePtr::new(TrieNodeID::Node4 as u8, 0x03, 40),
        // Case 2
        TriePtr::new_backptr(TrieNodeID::Node4 as u8, 0x04, 0, 1),
    ];

    assert_eq!(diff, expected);
}

/// Verifies that compression produces meaningfully smaller `.blobs` files when the workload
/// exercises deep patch chains (the same keys updated across many blocks).
///
/// This more closely mimics production genesis sync where contract state keys are updated
/// repeatedly, causing the same trie paths to be COW'd across consecutive blocks and building patch
/// chains up to `MAX_PATCH_DEPTH`.
///
/// NOTE: this test is written to be portable across branches to compare root hashes, and pins this
/// branch's blob-size baseline for its widened pointer / inline back-block byte layout. It
/// explicitly uses disk-backed storage to be able to use e.g. `marf-inspect` on the resulting
/// MARFs.
#[rstest]
#[case::immediate(&opts::OPTS_IMM_EXT, &opts::OPTS_IMM_EXT_COMP)]
#[case::deferred(&opts::OPTS_DEF_EXT, &opts::OPTS_DEF_EXT_COMP)]
fn test_marf_compression_reduces_blob_size(
    #[case] uncompressed_opts: &MARFOpenOpts,
    #[case] compressed_opts: &MARFOpenOpts,
) {
    // Minimum compression savings (%) required for the test to pass.
    const MIN_SAVINGS_PCT: f64 = 10.0;

    // 128 keys updated across 16 blocks — forces deep patch chains.
    let num_keys: usize = 128;
    let num_blocks: usize = 16;

    // Per-block blob lengths and root hashes locked in for this branch's
    // widened pointer / inline back-block byte layout.
    // Any on-disk format regression or change to the compression algorithm will break these.
    const EXPECTED_BLOB_TOTAL_UNC: u64 = 301_285;
    const EXPECTED_BLOB_TOTAL_COM: u64 = 263_404;
    #[rustfmt::skip]
    const EXPECTED_UNC_BLOB_LENS: [u64; 16] = [
        18_327, 18_613, 18_691, 18_764, 18_764, 18_837, 18_837, 18_837,
        18_842, 18_914, 18_914, 18_988, 18_988, 18_988, 18_988, 18_993,
    ];
    #[rustfmt::skip]
    const EXPECTED_COM_BLOB_LENS: [u64; 16] = [
        15_912, 16_325, 16_379, 16_428, 16_436, 16_380, 16_497, 16_487,
        16_492, 16_530, 16_522, 16_591, 16_589, 16_589, 16_590, 16_657,
    ];
    #[rustfmt::skip]
    const EXPECTED_ROOT_HASHES: [&str; 16] = [
        "cef6bde81f56058ceb5185d91e5bd7d98f424edc08f907372009312661603d80",
        "bd099b4d99365277d728547923ff67e384992b283594890043fe2db431fa2077",
        "62e3106a96d012cb5d148c83478224a3a223885cf2df310da4769482578ece96",
        "b2b2a29fd28379913dcde906f3e595dde3f2a39c6116d75dbeecac361aefa291",
        "ed8119d27e83a78fb85e2e5cf553b497b99211e2aa6ad2bfdc0be1f45eb50dd6",
        "f69f5dcf9f6568279942b096eedc99ee50d0250b238333d99742facc32077a60",
        "bcc5cde9edf9d52667aafa575e5d42eaa41ee1633f2b2b6ab62c54ff5283c8d1",
        "3bed100f94c49782ad0d06a55bec126eccc77219338cc65ba49432c3e7baa84b",
        "c9767ee160c767b7eaea7d0a4a7c71cf71b4a24f0a25ea730970fa4a5e4037b0",
        "fef22adcddb070568740bbadad520b09dd0389dc41c9bb2642ca0846c1db914e",
        "537bf9f84d0547dddfea8d949536e3ff0e76a05ca70882bd47d39c6bc08eebc0",
        "9354c77de97481042040fa7878bdfe4ca756485670aec250c18ecbfee08cc183",
        "4d1d60d6040f042aa9438c6a67eee1f01fd2290e2b3eeb94815445320253c52d",
        "ed4d570f682350d647ea3caeb0126ee21eb3f42e66ce5e38ad210d569c4a84de",
        "2ea3a3e2c49434640d852602e192ccd7246aa39369bef67555fd9365eeee7122",
        "3680f624a9bfbf0b036ea098d8bc16676919200fe636c92636c581bb04fa7c9a",
    ];

    // Pre-generate stable keys.
    let keys: Vec<String> = {
        let mut data = vec![0u8; 32];
        (0..num_keys)
            .map(|_| {
                let path_bytes = Sha512Trunc256Sum::from_data(&data).as_bytes().to_vec();
                data.copy_from_slice(&path_bytes[0..32]);
                to_hex(&path_bytes)
            })
            .collect()
    };

    let run_marf = |test_name: &str, marf_opts: &MARFOpenOpts| -> (u64, Vec<String>, Vec<u64>) {
        let hash_str = match marf_opts.hash_calculation_mode {
            TrieHashCalculationMode::Immediate => "imm",
            TrieHashCalculationMode::Deferred => "def",
            TrieHashCalculationMode::All => "all",
        };
        let compress_str = if marf_opts.compress { "com" } else { "unc" };
        let test_dir = format!("/tmp/stacks-marf-tests/{test_name}-noop-{hash_str}-{compress_str}");
        if fs::metadata(&test_dir).is_ok() {
            fs::remove_dir_all(&test_dir).unwrap();
        }
        fs::create_dir_all(&test_dir).unwrap();
        let test_file = format!("{test_dir}/marf.sqlite");

        let f = TrieFileStorage::open(&test_file, marf_opts.clone()).unwrap();
        let mut marf = MARF::from_storage(f);
        let mut last_block = BlockHeaderHash::sentinel();
        let mut block_headers = vec![];

        for blk in 0..num_blocks {
            let mut block_hash_bytes = [0u8; 32];
            block_hash_bytes[0..8].copy_from_slice(&(blk as u64).to_be_bytes());
            let block_header = BlockHeaderHash(block_hash_bytes);

            marf.begin(&last_block, &block_header).unwrap();
            for (i, key) in keys.iter().enumerate() {
                let mut value = [0u8; 40];
                value[0..8].copy_from_slice(&(blk as u64).to_be_bytes());
                value[8..16].copy_from_slice(&(i as u64).to_be_bytes());
                let leaf = TrieLeaf::from_value(&[], MARFValue(value));
                marf.insert_raw(TrieHash::from_key(key), leaf).unwrap();
            }
            marf.commit().unwrap();
            block_headers.push(block_header.clone());
            last_block = block_header;
        }

        // Collect per-block diagnostics: root hash + blob segment SHA256.
        let blob_path = format!("{test_dir}/marf.sqlite.blobs");
        let mut blob_file = fs::File::open(&blob_path)
            .unwrap_or_else(|_| panic!("blob file should exist at {blob_path}"));
        let blob_total = blob_file.metadata().unwrap().len();

        let db_path = format!("{test_dir}/marf.sqlite");
        let db = rusqlite::Connection::open_with_flags(
            &db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();

        let mut diagnostics = vec![];
        let mut per_block_lens = Vec::with_capacity(num_blocks);
        for (blk, bhh) in block_headers.iter().enumerate() {
            let root_hash = marf.get_root_hash_at(bhh).unwrap();

            // Read this block's blob segment and hash it.
            let (offset, length) =
                trie_sql::get_external_trie_offset_length(&db, (blk + 1) as u32).unwrap_or((0, 0));

            per_block_lens.push(length);

            let blob_sha = if length > 0 {
                use std::io::{Seek, SeekFrom};
                blob_file.seek(SeekFrom::Start(offset)).unwrap();
                let mut buf = vec![0u8; length as usize];
                blob_file.read_exact(&mut buf).unwrap();
                let hash = Sha256::digest(&buf);
                format!("{:x}", hash)
            } else {
                "n/a".to_string()
            };

            diagnostics.push(format!(
                "  block {blk:2}: root={} blob_offset={offset} blob_len={length} blob_sha256={blob_sha:.16}",
                root_hash.to_hex(),
            ));
        }

        eprintln!("=== {test_name} ({hash_str}/{compress_str}) total_blob={blob_total} ===");
        for d in &diagnostics {
            eprintln!("{d}");
        }

        let root_hashes: Vec<String> = block_headers
            .iter()
            .map(|bhh| marf.get_root_hash_at(bhh).unwrap().to_hex())
            .collect();

        (blob_total, root_hashes, per_block_lens)
    };

    let (uncompressed_size, unc_roots, unc_lens) =
        run_marf("compression_overlap_unc", uncompressed_opts);
    let (compressed_size, com_roots, com_lens) =
        run_marf("compression_overlap_com", compressed_opts);

    // Root hashes must match between compressed and uncompressed on the same branch.
    assert_eq!(
        unc_roots, com_roots,
        "Root hash mismatch between compressed and uncompressed runs"
    );

    // Lock in exact per-block blob sizes and root hashes against this branch's baseline.
    // Changes to the on-disk encoding, patch algorithm, or hash computation will fail here.
    assert_eq!(
        uncompressed_size, EXPECTED_BLOB_TOTAL_UNC,
        "uncompressed total blob size changed"
    );
    assert_eq!(
        compressed_size, EXPECTED_BLOB_TOTAL_COM,
        "compressed total blob size changed"
    );
    assert_eq!(
        unc_lens, EXPECTED_UNC_BLOB_LENS,
        "uncompressed per-block blob sizes changed"
    );
    assert_eq!(
        com_lens, EXPECTED_COM_BLOB_LENS,
        "compressed per-block blob sizes changed"
    );
    let expected_roots: Vec<String> = EXPECTED_ROOT_HASHES.iter().map(|s| s.to_string()).collect();
    assert_eq!(unc_roots, expected_roots, "root hashes changed");

    let savings_pct = 100.0 * (1.0 - (compressed_size as f64 / uncompressed_size as f64));
    eprintln!(
        "Blob sizes (overlapping keys, {num_blocks} blocks × {num_keys} keys): \
         uncompressed={uncompressed_size}, compressed={compressed_size}, savings={savings_pct:.1}%"
    );
    assert!(
        savings_pct >= MIN_SAVINGS_PCT,
        "compression savings {savings_pct:.1}% below minimum {MIN_SAVINGS_PCT}%: \
         compressed={compressed_size}, uncompressed={uncompressed_size}"
    );
}

/// Regression test: `make_node_patch` must restore `cur_block` after opening the ancestor block.
///
/// Without the fix, `make_node_patch` left `cur_block` pointing to an ancestor trie on its success
/// path. During `dump_compressed_consume` (where `uncommitted_writes` has been `.take()`'d), the
/// stale `cur_block` caused subsequent operations to read from the wrong trie or panic.
///
/// This test runs the compressed flush with overlapping COW'd keys and verifies that all values are
/// readable afterward — which requires `cur_block` to be properly restored during flush.
#[test]
fn make_node_patch_restores_cur_block_during_compressed_flush() {
    let test_dir = "/tmp/stacks-marf-tests/make_node_patch_cur_block_restore";
    if fs::metadata(test_dir).is_ok() {
        fs::remove_dir_all(test_dir).unwrap();
    }
    fs::create_dir_all(test_dir).unwrap();
    let test_file = format!("{test_dir}/marf.sqlite");

    let opts = MARFOpenOpts::new(TrieHashCalculationMode::Immediate, true).with_compression(true);
    let f = TrieFileStorage::open(&test_file, opts).unwrap();
    let mut marf = MARF::from_storage(f);

    let num_blocks = 5;
    let num_keys = 20;

    // Pre-generate stable keys (same keys used across blocks → COW nodes).
    let keys: Vec<String> = {
        let mut data = vec![0u8; 32];
        (0..num_keys)
            .map(|_| {
                let path_bytes = Sha512Trunc256Sum::from_data(&data).as_bytes().to_vec();
                data.copy_from_slice(&path_bytes[0..32]);
                to_hex(&path_bytes)
            })
            .collect()
    };

    let mut last_block = BlockHeaderHash::sentinel();
    let mut block_headers = vec![];

    for blk in 0..num_blocks {
        let mut block_hash_bytes = [0u8; 32];
        block_hash_bytes[0..8].copy_from_slice(&(blk as u64).to_be_bytes());
        let block_header = BlockHeaderHash(block_hash_bytes);

        marf.begin(&last_block, &block_header).unwrap();
        for (i, key) in keys.iter().enumerate() {
            let mut value = [0u8; 40];
            value[0..8].copy_from_slice(&(blk as u64).to_be_bytes());
            value[8..16].copy_from_slice(&(i as u64).to_be_bytes());
            let leaf = TrieLeaf::from_value(&[], MARFValue(value));
            marf.insert_raw(TrieHash::from_key(key), leaf).unwrap();
        }
        // commit() triggers the compressed flush, which exercises make_node_patch.
        // Before the fix, this could leave cur_block pointing to an ancestor.
        marf.commit().unwrap();
        block_headers.push(block_header.clone());
        last_block = block_header;
    }

    // Verify that all values from the latest block are readable.
    // If make_node_patch corrupted cur_block during flush, subsequent reads would fail
    // with NotFoundError or return wrong data.
    for (i, key) in keys.iter().enumerate() {
        let value = marf
            .get_by_key(&last_block, key)
            .unwrap_or_else(|e| panic!("Failed to read key {i} from block {}: {e:?}", last_block));
        let value = value.unwrap_or_else(|| panic!("Key {i} not found in block {}", last_block));

        let expected_blk = (num_blocks - 1) as u64;
        let got_blk = u64::from_be_bytes(value.0[0..8].try_into().unwrap());
        assert_eq!(got_blk, expected_blk, "Key {i}: wrong block in value");
    }

    // Also verify reads from earlier blocks to exercise cross-block backpointer resolution.
    for (blk, bhh) in block_headers.iter().enumerate() {
        for (i, key) in keys.iter().enumerate() {
            let value = marf
                .get_by_key(bhh, key)
                .unwrap_or_else(|e| panic!("Failed to read key {i} from block {blk}: {e:?}"));
            let value = value.unwrap_or_else(|| panic!("Key {i} not found in block {blk}"));

            let got_blk = u64::from_be_bytes(value.0[0..8].try_into().unwrap());
            assert_eq!(
                got_blk, blk as u64,
                "Key {i} at block {blk}: wrong block in value"
            );
        }
    }
}
