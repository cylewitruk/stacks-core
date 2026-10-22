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

//! Borrowed record, branch and patch decoding.
use stacks_common::types::chainstate::TrieHash;

use super::pointer::ptr_compressed_len;
use super::{
    Bad, COMPRESSED, Decoded, HASH_LEN, Patch, Reader, SPARSE_MARKER, compressed_len, plain_len,
    sparse_bitmap_len,
};
use crate::node::{Branch, Kind, Leaf, Node, Path, Ptr, id};
use crate::{Error, MARFValue, corrupt};

/// How a branch body stores its slot list.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SlotEncoding {
    Plain,
    Dense,
    Sparse,
}

/// What a record is, before decoding it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RecordKind {
    Leaf,
    Branch(Kind),
    Patch,
}

/// A persisted record (`hash || body`) borrowed from storage and parsed
/// lazily: lookups read the path and select one child in a single pass over
/// the slot list, and nothing is allocated unless a caller asks for an
/// owned node.
#[derive(Clone, Copy)]
pub struct Record<'a> {
    hash: &'a [u8],
    body: &'a [u8],
}

/// The bytes after a branch's slot list: the Node48 index array (if any)
/// and the path, plus where the record ends.
struct Trailer<'a> {
    indexes: Option<&'a [u8]>,
    path: &'a [u8],
    end: usize,
}

/// One step of a key walk through a record (see [`Record::descend`]).
pub enum Descent<'a> {
    /// The record's path is not a prefix of the key at this depth.
    Mismatch,
    /// A leaf whose path matched; `consumed` is its path length.
    Leaf {
        value: LeafView<'a>,
        consumed: usize,
    },
    /// A branch whose path matched; `child` is the slot for the key byte
    /// after the path (`None` if empty). `consumed` counts the path bytes
    /// plus that byte.
    Child { child: Option<Ptr>, consumed: usize },
}
/// A borrowed leaf.
#[derive(Clone, Copy)]
pub struct LeafView<'a> {
    pub path: &'a [u8],
    pub value: &'a [u8],
    end: usize,
}
/// Byte position of every occupied slot's pointer, indexed by child
/// character (0 = no such child; position 0 is the type byte, never a slot).
struct SlotIndex([u16; 256]);

impl SlotIndex {
    const EMPTY: SlotIndex = SlotIndex([0; 256]);
}

/// A borrowed branch. Slot access scans the encoded slot list; nothing is
/// allocated.
#[derive(Clone, Copy)]
pub struct BranchView<'a> {
    kind: Kind,
    encoding: SlotEncoding,
    body: &'a [u8],
}
/// A borrowed patch: its base pointer and the (not yet decoded) diff list.
#[derive(Clone, Copy)]
pub struct PatchView<'a> {
    pub base: Ptr,
    n: usize,
    /// The body from the first diff pointer on.
    diff: &'a [u8],
}

impl<'a> Record<'a> {
    /// Parse the record at the start of `bytes`, which a pointer of type
    /// `expected_id` refers to. A patch is accepted in place of any branch.
    pub fn parse(bytes: &'a [u8], expected_id: u8) -> Result<Record<'a>, Error> {
        if bytes.len() < HASH_LEN + 1 {
            return Err(corrupt!("record truncated ({} bytes)", bytes.len()));
        }
        let (hash, body) = bytes.split_at(HASH_LEN);
        Self::check_type(body[0], expected_id)?;
        Ok(Record { hash, body })
    }

    /// Parse a body without its hash (for in-memory blobs and tests).
    pub fn parse_body(body: &'a [u8], expected_id: u8) -> Result<Record<'a>, Error> {
        if body.is_empty() {
            return Err(corrupt!("empty node body"));
        }
        Self::check_type(body[0], expected_id)?;
        Ok(Record {
            hash: &[0u8; HASH_LEN],
            body,
        })
    }

    fn check_type(raw: u8, expected_id: u8) -> Result<(), Error> {
        let tid = raw & 0x0f;
        let expected = expected_id & 0x0f;
        if tid == id::PATCH && expected != id::LEAF && expected != id::EMPTY {
            return Ok(());
        }
        if tid != expected {
            return Err(corrupt!("expected node type {expected:#x}, found {raw:#x}"));
        }
        Ok(())
    }

    pub fn hash(&self) -> TrieHash {
        TrieHash::from_bytes(self.hash).expect("32 bytes")
    }

    #[inline]
    fn type_id(&self) -> u8 {
        self.body[0] & 0x0f
    }

    pub fn kind(&self) -> Result<RecordKind, Error> {
        Ok(match self.type_id() {
            id::LEAF => RecordKind::Leaf,
            id::PATCH => RecordKind::Patch,
            tid => RecordKind::Branch(
                Kind::from_id(tid)
                    .ok_or_else(|| corrupt!("bad node type id {:#x}", self.body[0]))?,
            ),
        })
    }

    pub fn is_leaf(&self) -> bool {
        self.type_id() == id::LEAF
    }

    pub fn is_patch(&self) -> bool {
        self.type_id() == id::PATCH
    }

    pub fn leaf(&self) -> Result<LeafView<'a>, Error> {
        if !self.is_leaf() {
            return Err(corrupt!("record is not a leaf"));
        }
        Ok(LeafView::parse(self.body)?)
    }

    pub fn branch(&self) -> Result<BranchView<'a>, Error> {
        let RecordKind::Branch(kind) = self.kind()? else {
            return Err(corrupt!("record is not a branch"));
        };
        let encoding = if self.body[0] & COMPRESSED != 0 {
            if self.body.get(1) == Some(&SPARSE_MARKER) {
                SlotEncoding::Sparse
            } else {
                SlotEncoding::Dense
            }
        } else {
            SlotEncoding::Plain
        };
        Ok(BranchView {
            kind,
            encoding,
            body: self.body,
        })
    }

    pub fn patch(&self) -> Result<PatchView<'a>, Error> {
        if !self.is_patch() {
            return Err(corrupt!("record is not a patch"));
        }
        let mut r = Reader {
            buf: self.body,
            pos: 1,
        };
        let base = r.ptr_compressed()?;
        let n = r.u8()? as usize + 1;
        Ok(PatchView {
            base,
            n,
            diff: &self.body[r.pos..],
        })
    }

    /// Walk one step of `key` from `depth` through this record (which must
    /// not be a patch): compare the record's path with the key and, for a
    /// branch, select the child for the next key byte, in one pass.
    pub fn descend(&self, key: &[u8], depth: usize) -> Result<Descent<'a>, Error> {
        if self.is_leaf() {
            let leaf = LeafView::parse(self.body)?;
            return Ok(if key[depth..].starts_with(leaf.path) {
                Descent::Leaf {
                    value: leaf,
                    consumed: leaf.path.len(),
                }
            } else {
                Descent::Mismatch
            });
        }
        let view = self.branch()?;
        let mut slots = SlotIndex::EMPTY;
        let trailer = view.skip_scan(&mut slots)?;
        let rest = &key[depth..];
        if !rest.starts_with(trailer.path) {
            return Ok(Descent::Mismatch);
        }
        let Some(&chr) = rest.get(trailer.path.len()) else {
            return Err(corrupt!("Path reached a non-leaf"));
        };
        let child = view.child_at(&slots, chr)?;
        Ok(Descent::Child {
            child,
            consumed: trailer.path.len() + 1,
        })
    }

    /// Fully decode the record into an owned value; also returns the body length.
    pub fn decode(&self) -> Result<(Decoded, usize), Error> {
        match self.kind()? {
            RecordKind::Leaf => {
                let l = self.leaf()?;
                Ok((Decoded::Node(Node::Leaf(l.to_leaf())), l.end))
            }
            RecordKind::Branch(_) => {
                let view = self.branch()?;
                let mut branch = Branch::new(view.kind(), Path::EMPTY);
                let end = view.fill_with_end(&mut branch)?;
                Ok((Decoded::Node(Node::Branch(branch)), end))
            }
            RecordKind::Patch => {
                let p = self.patch()?;
                let (patch, used) = p.to_patch_with_len()?;
                Ok((Decoded::Patch(patch), p.start() + used))
            }
        }
    }
}
impl<'a> LeafView<'a> {
    fn parse(body: &'a [u8]) -> Result<LeafView<'a>, Bad> {
        let mut r = Reader { buf: body, pos: 1 };
        let path = r.path_bytes()?;
        let value = r.take(40)?;
        Ok(LeafView {
            path,
            value,
            end: r.pos,
        })
    }

    /// View an in-memory leaf.
    pub fn borrow(leaf: &'a Leaf) -> LeafView<'a> {
        LeafView {
            path: leaf.path.as_slice(),
            value: &leaf.value.0,
            end: 0,
        }
    }

    pub fn value(&self) -> MARFValue {
        let mut v = [0u8; 40];
        v.copy_from_slice(self.value);
        MARFValue(v)
    }

    pub fn to_leaf(&self) -> Leaf {
        Leaf {
            path: Path::from_slice(self.path).expect("<= 32"),
            value: self.value(),
        }
    }
}
/// Decode the node body at the start of `body` (the bytes after the hash).
/// `expected_id` is the node type the referring pointer claims (back-pointer
/// bit ignored); a patch is accepted in place of any branch type.
/// Returns the decoded value and the number of body bytes consumed.
pub fn decode(body: &[u8], expected_id: u8) -> Result<(Decoded, usize), Error> {
    Record::parse_body(body, expected_id)?.decode()
}

impl<'a> BranchView<'a> {
    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// The compressed path.
    pub fn path(&self) -> Result<&'a [u8], Error> {
        let mut slots = SlotIndex::EMPTY;
        Ok(self.skip_scan(&mut slots)?.path)
    }

    /// The child at `chr`, if any.
    pub fn child(&self, chr: u8) -> Result<Option<Ptr>, Error> {
        let mut slots = SlotIndex::EMPTY;
        self.skip_scan(&mut slots)?;
        self.child_at(&slots, chr)
    }

    /// Visit every occupied slot in slot order; returns the path.
    pub fn for_each_slot(&self, mut visit: impl FnMut(usize, Ptr)) -> Result<&'a [u8], Error> {
        Ok(self
            .scan(|i, p| {
                visit(i, p);
                true
            })?
            .expect("full scan")
            .path)
    }
    /// Decode into `out`, reusing its allocation.
    pub fn fill(&self, out: &mut Branch) -> Result<(), Error> {
        self.fill_with_end(out).map(|_| ())
    }

    /// Decode into an owned branch.
    pub fn to_branch(&self) -> Result<Branch, Error> {
        let mut out = Branch::new(self.kind, Path::EMPTY);
        self.fill(&mut out)?;
        Ok(out)
    }
}

impl<'a> BranchView<'a> {
    /// Scan the slot list in slot order, calling `visit(slot, ptr)` for every
    /// occupied slot while it returns `true`. Returns the trailer when the
    /// whole list was scanned, `None` if `visit` stopped early.
    fn scan(&self, mut visit: impl FnMut(usize, Ptr) -> bool) -> Result<Option<Trailer<'a>>, Bad> {
        let mut r = Reader {
            buf: self.body,
            pos: 1,
        };
        let cap = self.kind.capacity();
        match self.encoding {
            SlotEncoding::Plain => {
                for i in 0..cap {
                    let p = r.ptr_plain()?;
                    if !p.is_empty() && !visit(i, p) {
                        return Ok(None);
                    }
                }
            }
            SlotEncoding::Dense => {
                for i in 0..cap {
                    let p = r.ptr_compressed()?;
                    if !p.is_empty() && !visit(i, p) {
                        return Ok(None);
                    }
                }
            }
            SlotEncoding::Sparse => {
                r.pos += 1;
                let bitmap = r.take(sparse_bitmap_len(self.kind))?;
                for i in 0..cap {
                    if bitmap[i / 8] & (1 << (i % 8)) != 0 {
                        let p = r.ptr_compressed()?;
                        if !visit(i, p) {
                            return Ok(None);
                        }
                    }
                }
            }
        }
        let indexes = if self.kind == Kind::Node48 {
            Some(r.take(256)?)
        } else {
            None
        };
        let path = r.path_bytes()?;
        Ok(Some(Trailer {
            indexes,
            path,
            end: r.pos,
        }))
    }

    /// Read the pointer at `pos`.
    #[inline]
    fn ptr_at(&self, pos: usize) -> Result<Ptr, Bad> {
        let mut r = Reader {
            buf: self.body,
            pos,
        };
        match self.encoding {
            SlotEncoding::Plain => r.ptr_plain(),
            _ => r.ptr_compressed(),
        }
    }

    /// Pass over the slot list reading only each pointer's first two bytes
    /// (type flags and character), recording where every occupied slot's
    /// pointer starts; returns the trailer (path and Node48 indexes).
    fn skip_scan(&self, slots: &mut SlotIndex) -> Result<Trailer<'a>, Bad> {
        let body = self.body;
        let cap = self.kind.capacity();
        let mut pos = 1usize;
        let mut record = |pos: usize| -> Result<usize, Bad> {
            let raw = *body.get(pos).ok_or(Bad::Truncated)?;
            let chr = *body.get(pos + 1).ok_or(Bad::Truncated)?;
            if raw & 0x0f != id::EMPTY {
                slots.0[chr as usize] = pos as u16;
            }
            Ok(if self.encoding == SlotEncoding::Plain {
                plain_len(raw)
            } else {
                compressed_len(raw)
            })
        };
        match self.encoding {
            SlotEncoding::Plain | SlotEncoding::Dense => {
                for _ in 0..cap {
                    pos += record(pos)?;
                }
            }
            SlotEncoding::Sparse => {
                pos += 1;
                let bl = sparse_bitmap_len(self.kind);
                let bitmap = body.get(pos..pos + bl).ok_or(Bad::Truncated)?;
                pos += bl;
                for i in 0..cap {
                    if bitmap[i / 8] & (1 << (i % 8)) != 0 {
                        pos += record(pos)?;
                    }
                }
            }
        }
        let mut r = Reader { buf: body, pos };
        let indexes = if self.kind == Kind::Node48 {
            Some(r.take(256)?)
        } else {
            None
        };
        let path = r.path_bytes()?;
        Ok(Trailer {
            indexes,
            path,
            end: r.pos,
        })
    }

    /// The child at `chr` given a completed [`skip_scan`](Self::skip_scan).
    fn child_at(&self, slots: &SlotIndex, chr: u8) -> Result<Option<Ptr>, Error> {
        let pos = slots.0[chr as usize];
        if pos == 0 {
            return Ok(None);
        }
        let p = self.ptr_at(pos as usize)?;
        if p.chr != chr {
            return Err(corrupt!(
                "{:?}: slot at {pos} holds chr {:02x}, expected {chr:02x}",
                self.kind,
                p.chr
            ));
        }
        Ok(Some(p))
    }
    /// Decode into `out`, reusing its allocation; returns the body length.
    fn fill_with_end(&self, out: &mut Branch) -> Result<usize, Error> {
        out.kind = self.kind;
        out.children.clear();
        let kind = self.kind;
        let mut bad: Option<Error> = None;
        let trailer = self
            .scan(|i, p| {
                if bad.is_some() {
                    return true;
                }
                match kind {
                    Kind::Node256 => {
                        if p.chr as usize != i {
                            bad = Some(corrupt!("Node256: slot {i} holds chr {:02x}", p.chr));
                        }
                    }
                    _ => {
                        if i != out.children.len() {
                            bad = Some(corrupt!("{kind:?}: occupied slot {i} after an empty slot"));
                        }
                    }
                }
                out.children.push(p);
                true
            })?
            .expect("full scan");
        if let Some(e) = bad {
            return Err(e);
        }
        if let Some(indexes) = trailer.indexes {
            for (i, p) in out.children.iter().enumerate() {
                if indexes[p.chr as usize] != i as u8 {
                    return Err(corrupt!("Node48: corrupt index array: invalid index value"));
                }
            }
            for (chr, ix) in indexes.iter().enumerate() {
                let ix = *ix as i8;
                if ix >= 0 {
                    let ok = out
                        .children
                        .get(ix as usize)
                        .map(|p| p.chr as usize == chr)
                        .unwrap_or(false);
                    if !ok {
                        return Err(corrupt!(
                            "Node48: corrupt index array: index points to empty node"
                        ));
                    }
                }
            }
        }
        out.path = Path::from_slice(trailer.path).expect("<= 32");
        Ok(trailer.end)
    }
}

impl PatchView<'_> {
    /// Body offset of the first diff pointer.
    fn start(&self) -> usize {
        1 + ptr_compressed_len(&self.base) + 1
    }
    /// Decode the diff and report how many encoded bytes it consumed.
    fn to_patch_with_len(self) -> Result<(Patch, usize), Error> {
        let mut diff = Vec::with_capacity(self.n);
        let used = self.for_each_diff(|p| diff.push(p))?;
        Ok((
            Patch {
                base: self.base,
                diff,
            },
            used,
        ))
    }
}

impl PatchView<'_> {
    /// Visit the diff pointers in order; returns the number of diff bytes.
    pub fn for_each_diff(&self, mut visit: impl FnMut(Ptr)) -> Result<usize, Error> {
        let mut r = Reader {
            buf: self.diff,
            pos: 0,
        };
        for _ in 0..self.n {
            visit(r.ptr_compressed()?);
        }
        Ok(r.pos)
    }
    pub fn to_patch(self) -> Result<Patch, Error> {
        Ok(self.to_patch_with_len()?.0)
    }
}
