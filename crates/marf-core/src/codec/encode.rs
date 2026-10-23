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

//! Encoded sizes and emission for nodes and patches.
use super::pointer::{ptr_compressed_len, ptr_plain_len, put_ptr_compressed, put_ptr_plain};
use super::{COMPRESSED, Patch, SPARSE_MARKER, sparse_bitmap_len};
use crate::node::{Branch, Kind, Node, Ptr, id};

/// Whether the compressed slot list should use the sparse form, and its size.
fn compressed_slots_mapped(branch: &Branch, map: &dyn Fn(&Ptr) -> Ptr) -> (bool, usize) {
    let occupied: usize = branch
        .children
        .iter()
        .map(|p| ptr_compressed_len(&map(p)))
        .sum();
    let empties = branch.kind.capacity() - branch.children.len();
    let dense = occupied + empties * ptr_compressed_len(&Ptr::default());
    let sparse = 1 + sparse_bitmap_len(branch.kind) + occupied;
    if sparse < dense {
        (true, sparse)
    } else {
        (false, dense)
    }
}

/// Encoded body length of `node`, without the preceding hash.
pub fn node_len(node: &Node, compress: bool) -> usize {
    node_len_mapped(node, compress, &|p| *p)
}

/// [`node_len`] with every child pointer passed through `map` first.
pub fn node_len_mapped(node: &Node, compress: bool, map: &dyn Fn(&Ptr) -> Ptr) -> usize {
    match node {
        Node::Leaf(l) => 1 + 1 + l.path.len() + 40,
        Node::Branch(b) => {
            let slots = if compress {
                compressed_slots_mapped(b, map).1
            } else {
                let occupied: usize = b.children.iter().map(|p| ptr_plain_len(&map(p))).sum();
                occupied + (b.kind.capacity() - b.children.len()) * ptr_plain_len(&Ptr::default())
            };
            let indexes = if b.kind == Kind::Node48 { 256 } else { 0 };
            1 + slots + indexes + 1 + b.path.len()
        }
    }
}

/// Encoded body length of `patch`.
pub fn patch_len(patch: &Patch) -> usize {
    patch_len_mapped(patch, &|p| *p)
}

/// [`patch_len`] with every pointer passed through `map` first.
pub fn patch_len_mapped(patch: &Patch, map: &dyn Fn(&Ptr) -> Ptr) -> usize {
    1 + ptr_compressed_len(&map(&patch.base))
        + 1
        + patch
            .diff
            .iter()
            .map(|p| ptr_compressed_len(&map(p)))
            .sum::<usize>()
}

/// Append the body of `node` to `out`.
pub fn encode_node(node: &Node, compress: bool, out: &mut Vec<u8>) {
    encode_node_mapped(node, compress, &|p| *p, out)
}

/// [`encode_node`] with every child pointer passed through `map` first.
pub fn encode_node_mapped(
    node: &Node,
    compress: bool,
    map: &dyn Fn(&Ptr) -> Ptr,
    out: &mut Vec<u8>,
) {
    match node {
        Node::Leaf(l) => {
            out.push(id::LEAF);
            out.push(l.path.len() as u8);
            out.extend_from_slice(l.path.as_slice());
            out.extend_from_slice(&l.value.0);
        }
        Node::Branch(b) => {
            let cap = b.kind.capacity();
            if compress {
                out.push(b.kind.id() | COMPRESSED);
                let (sparse, _) = compressed_slots_mapped(b, map);
                if sparse {
                    out.push(SPARSE_MARKER);
                    let mut bitmap = [0u8; 32];
                    for (i, slot) in b.slots().enumerate() {
                        if slot.is_some() {
                            bitmap[i / 8] |= 1 << (i % 8);
                        }
                    }
                    out.extend_from_slice(&bitmap[..sparse_bitmap_len(b.kind)]);
                    for c in &b.children {
                        put_ptr_compressed(&map(c), out);
                    }
                } else {
                    for slot in b.slots() {
                        put_ptr_compressed(&slot.map(map).unwrap_or_default(), out);
                    }
                }
            } else {
                out.push(b.kind.id());
                for slot in b.slots() {
                    put_ptr_plain(&slot.map(map).unwrap_or_default(), out);
                }
            }
            if b.kind == Kind::Node48 {
                let mut indexes = [0xffu8; 256];
                for (i, c) in b.children.iter().enumerate() {
                    indexes[c.chr as usize] = i as u8;
                }
                out.extend_from_slice(&indexes);
            }
            debug_assert!(cap >= b.children.len());
            out.push(b.path.len() as u8);
            out.extend_from_slice(b.path.as_slice());
        }
    }
}

/// Append the body of `patch` to `out`. Panics if the diff is empty or has
/// more than 256 entries (neither can arise from a well-formed node).
pub fn encode_patch(patch: &Patch, out: &mut Vec<u8>) {
    encode_patch_mapped(patch, &|p| *p, out)
}

pub fn encode_patch_mapped(patch: &Patch, map: &dyn Fn(&Ptr) -> Ptr, out: &mut Vec<u8>) {
    assert!(!patch.diff.is_empty() && patch.diff.len() <= 256);
    out.push(id::PATCH);
    put_ptr_compressed(&patch.base, out);
    out.push((patch.diff.len() - 1) as u8);
    for p in &patch.diff {
        put_ptr_compressed(&map(p), out);
    }
}
