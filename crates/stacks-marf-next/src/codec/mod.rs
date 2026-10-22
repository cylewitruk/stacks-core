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

//! The legacy on-disk node encoding.
//!
//! This is the only module that knows how nodes are laid out in a trie blob.
//! Every persisted node is `hash(32) || body`; the body's first byte is the
//! node type id with control bits:
//!
//! * `0x40` the child pointer list is compressed (dense or sparse);
//! * `0x80` on a child pointer: a back-pointer into another block;
//! * `0x20` on a child pointer: its offset is 8 bytes rather than 4;
//! * `0x10` on a compressed inline child pointer: a `back_block` annotation
//!   follows (squash metadata).
//!
//! Bodies:
//!
//! ```text
//! leaf    : 0x01 || path_len || path || value[40]
//! branch  : id || slots || [Node48: 256 index bytes] || path_len || path
//! patch   : 0x06 || base_ptr(compressed) || (n-1) || diff_ptr(compressed)*n
//! ```
//!
//! Plain slots are `cap` fixed pointers (`id chr ptr[4|8] back_block[4]`).
//! Compressed slots are either dense (`cap` compressed pointers) or sparse
//! (`0xff || bitmap || occupied compressed pointers`). A compressed pointer is
//! `id chr ptr[4|8]` plus `back_block[4]` only when it is a back-pointer or
//! carries the `0x10` annotation.
//!
//! A patch is a delta against a node in an older block (its `base`). Reading
//! a patched node means following `base` pointers until a real node is found
//! and re-applying the deltas on the way back ([`apply_patch`]).

pub use self::encode::{
    encode_node, encode_node_mapped, encode_patch, encode_patch_mapped, node_len, node_len_mapped,
    patch_len, patch_len_mapped,
};
pub use self::patch::{apply_patch, make_patch};
pub use self::record::{BranchView, Descent, LeafView, PatchView, Record, RecordKind, decode};
use crate::node::{BACKPTR, Kind, Node, Ptr, id};
use crate::{Error, corrupt};

const COMPRESSED: u8 = 0x40;
const WIDE_PTR: u8 = 0x20;
const INLINE_ANNOTATION: u8 = 0x10;
const SPARSE_MARKER: u8 = 0xff;

/// Maximum number of patches that may be stacked on one base node.
pub const MAX_PATCH_DEPTH: u32 = 4;

/// Size of the hash that precedes every node body.
pub const HASH_LEN: usize = 32;

/// Upper bound on `hash || body` for any node or patch, for sizing reads.
/// Node256 plain: 32 + 1 + 256*14 + 33 = 3650; patch: 32 + 1 + 14 + 1 + 256*14 = 3632.
pub const MAX_NODE_LEN: usize = 3650;

/// Upper bound on `hash || body` for a record whose body starts with
/// `type_byte` (plain pointers of 14 bytes, a 32-byte path, Node48 index
/// array); `MAX_NODE_LEN` for an unknown type. Lets a storage layer that
/// must copy records (SQLite blobs) read a leaf as 106 bytes rather than
/// the Node256 bound.
pub fn max_record_len(type_byte: u8) -> usize {
    let body = match type_byte & 0x0f {
        id::LEAF => 1 + 1 + 32 + 40,
        id::NODE4 => 1 + 4 * 14 + 1 + 32,
        id::NODE16 => 1 + 16 * 14 + 1 + 32,
        id::NODE48 => 1 + 48 * 14 + 256 + 1 + 32,
        id::NODE256 => 1 + 256 * 14 + 1 + 32,
        id::PATCH => 1 + 14 + 1 + 256 * 14,
        _ => return MAX_NODE_LEN,
    };
    HASH_LEN + body
}

/// A delta against a node in an older block.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Patch {
    /// Back-pointer to the node being patched (`back_block` names its block).
    pub base: Ptr,
    /// Children to insert or replace, in no particular order.
    pub diff: Vec<Ptr>,
}

/// What a node slot on disk decodes to.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Decoded {
    Node(Node),
    Patch(Patch),
}

fn sparse_bitmap_len(kind: Kind) -> usize {
    match kind {
        Kind::Node4 => 1,
        Kind::Node16 => 2,
        Kind::Node48 => 6,
        Kind::Node256 => 32,
    }
}

fn wide(ptr: u64) -> bool {
    ptr > u64::from(u32::MAX)
}
/// Why a record body could not be read. Kept tiny so the hot decoding loops
/// return it by value; converted to [`Error`] at the public boundary.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Bad {
    Truncated,
    PathTooLong,
}

impl From<Bad> for Error {
    fn from(b: Bad) -> Error {
        match b {
            Bad::Truncated => corrupt!("node body truncated"),
            Bad::PathTooLong => corrupt!("node path is longer than 32 bytes"),
        }
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}
/// Encoded size of a plain pointer whose first byte is `raw`.
#[inline]
fn plain_len(raw: u8) -> usize {
    2 + if raw & WIDE_PTR != 0 { 8 } else { 4 } + 4
}

#[inline]
fn compressed_has_block(raw: u8) -> bool {
    raw & (BACKPTR | INLINE_ANNOTATION) != 0
}

/// Encoded size of a compressed pointer whose first byte is `raw`.
#[inline]
fn compressed_len(raw: u8) -> usize {
    2 + if raw & WIDE_PTR != 0 { 8 } else { 4 } + if compressed_has_block(raw) { 4 } else { 0 }
}

mod encode;
mod patch;
mod pointer;
mod record;
#[cfg(test)]
mod tests;
