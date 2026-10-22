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

//! Node and patch encoding regression tests.
use super::*;
use crate::MARFValue;
use crate::node::{Branch, Leaf, Path};

fn roundtrip(node: &Node, compress: bool) {
    let mut out = vec![];
    encode_node(node, compress, &mut out);
    assert_eq!(
        out.len(),
        node_len(node, compress),
        "length mismatch for {node:?}"
    );
    let (decoded, used) = decode(&out, node.kind_id()).expect("decode");
    assert_eq!(used, out.len());
    assert_eq!(decoded, Decoded::Node(node.clone()));
}

fn branch(kind: Kind, n: usize) -> Branch {
    let mut b = Branch::new(kind, Path::from_slice(&[1, 2, 3]).unwrap());
    for i in 0..n {
        let mut p = Ptr::new(
            if i % 2 == 0 { id::LEAF } else { id::NODE16 },
            (i * 7) as u8,
            1000 + i as u64,
        );
        if i % 3 == 0 {
            p.id |= BACKPTR;
            p.back_block = 5 + i as u32;
        }
        assert!(b.insert(p));
    }
    b
}

#[test]
fn roundtrip_all_kinds() {
    let leaf = Node::Leaf(Leaf {
        path: Path::from_slice(&[9; 29]).unwrap(),
        value: MARFValue([7u8; 40]),
    });
    roundtrip(&leaf, false);
    roundtrip(&leaf, true);
    for kind in [Kind::Node4, Kind::Node16, Kind::Node48, Kind::Node256] {
        for n in [0usize, 1, 2, 4, 16, 36] {
            if n > kind.capacity() {
                continue;
            }
            let node = Node::Branch(branch(kind, n));
            roundtrip(&node, false);
            roundtrip(&node, true);
        }
    }
    // wide offsets and squash annotations
    let mut b = branch(Kind::Node16, 3);
    b.children[1].ptr = u64::from(u32::MAX) + 10;
    b.children[2].back_block = 77; // inline annotation
    let node = Node::Branch(b);
    roundtrip(&node, false);
    roundtrip(&node, true);
}

#[test]
fn patch_roundtrip() {
    let mut base = Ptr::new(id::NODE4 | BACKPTR, 3, 44);
    base.back_block = 2;
    let patch = Patch {
        base,
        diff: vec![
            Ptr::new(id::LEAF, 1, 5),
            Ptr::new(id::NODE4 | BACKPTR, 2, 6),
        ],
    };
    let mut out = vec![];
    encode_patch(&patch, &mut out);
    assert_eq!(out.len(), patch_len(&patch));
    let (decoded, used) = decode(&out, id::NODE4).unwrap();
    assert_eq!(used, out.len());
    assert_eq!(decoded, Decoded::Patch(patch));
}

#[test]
fn rejects_wrong_type() {
    let node = Node::Branch(branch(Kind::Node4, 2));
    let mut out = vec![];
    encode_node(&node, false, &mut out);
    assert!(decode(&out, id::NODE16).is_err());
    assert!(decode(&out[..5], id::NODE4).is_err());
}
