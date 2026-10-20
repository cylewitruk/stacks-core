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

use stacks_common::types::chainstate::TrieHash;

use crate::node::{backptr_children, id, Branch, Kind, Leaf, Node, Path, Ptr, BACKPTR};
use crate::{corrupt, Error, MARFValue};

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

// ---------------------------------------------------------------------------
// pointers

fn ptr_plain_len(ptr: &Ptr) -> usize {
    1 + 1 + if wide(ptr.ptr) { 8 } else { 4 } + 4
}

fn ptr_compressed_len(ptr: &Ptr) -> usize {
    let bb = ptr.is_back() || ptr.back_block != 0;
    1 + 1 + if wide(ptr.ptr) { 8 } else { 4 } + if bb { 4 } else { 0 }
}

fn put_ptr_plain(ptr: &Ptr, out: &mut Vec<u8>) {
    let w = wide(ptr.ptr);
    out.push(ptr.id | if w { WIDE_PTR } else { 0 });
    out.push(ptr.chr);
    if w {
        out.extend_from_slice(&ptr.ptr.to_be_bytes());
    } else {
        out.extend_from_slice(&(ptr.ptr as u32).to_be_bytes());
    }
    out.extend_from_slice(&ptr.back_block.to_be_bytes());
}

fn put_ptr_compressed(ptr: &Ptr, out: &mut Vec<u8>) {
    let w = wide(ptr.ptr);
    let annotated = !ptr.is_back() && ptr.back_block != 0;
    out.push(
        ptr.id
            | COMPRESSED
            | if w { WIDE_PTR } else { 0 }
            | if annotated { INLINE_ANNOTATION } else { 0 },
    );
    out.push(ptr.chr);
    if w {
        out.extend_from_slice(&ptr.ptr.to_be_bytes());
    } else {
        out.extend_from_slice(&(ptr.ptr as u32).to_be_bytes());
    }
    if ptr.is_back() || annotated {
        out.extend_from_slice(&ptr.back_block.to_be_bytes());
    }
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

impl<'a> Reader<'a> {
    #[inline]
    fn take(&mut self, n: usize) -> Result<&'a [u8], Bad> {
        let end = self.pos.checked_add(n).ok_or(Bad::Truncated)?;
        let s = self.buf.get(self.pos..end).ok_or(Bad::Truncated)?;
        self.pos = end;
        Ok(s)
    }

    #[inline]
    fn u8(&mut self) -> Result<u8, Bad> {
        Ok(self.take(1)?[0])
    }

    #[inline]
    fn u32(&mut self) -> Result<u32, Bad> {
        Ok(u32::from_be_bytes(
            self.take(4)?.try_into().expect("4 bytes"),
        ))
    }

    #[inline]
    fn offset(&mut self, id: u8) -> Result<u64, Bad> {
        if id & WIDE_PTR != 0 {
            Ok(u64::from_be_bytes(
                self.take(8)?.try_into().expect("8 bytes"),
            ))
        } else {
            Ok(u64::from(self.u32()?))
        }
    }

    fn ptr_plain(&mut self) -> Result<Ptr, Bad> {
        let raw = self.u8()?;
        let chr = self.u8()?;
        let ptr = self.offset(raw)?;
        let back_block = self.u32()?;
        Ok(Ptr {
            id: raw & !WIDE_PTR,
            chr,
            ptr,
            back_block,
        })
    }

    fn ptr_compressed(&mut self) -> Result<Ptr, Bad> {
        let raw = self.u8()?;
        let chr = self.u8()?;
        let ptr = self.offset(raw)?;
        let back_block = if compressed_has_block(raw) {
            self.u32()?
        } else {
            0
        };
        Ok(Ptr {
            id: raw & !(COMPRESSED | WIDE_PTR | INLINE_ANNOTATION),
            chr,
            ptr,
            back_block,
        })
    }

    fn path_bytes(&mut self) -> Result<&'a [u8], Bad> {
        let len = self.u8()? as usize;
        if len > 32 {
            return Err(Bad::PathTooLong);
        }
        self.take(len)
    }
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

// ---------------------------------------------------------------------------
// borrowed views

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
    Leaf { value: LeafView<'a>, consumed: usize },
    /// A branch whose path matched; `child` is the slot for the key byte
    /// after the path (`None` if empty). `consumed` counts the path bytes
    /// plus that byte.
    Child { child: Option<Ptr>, consumed: usize },
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
                Kind::from_id(tid).ok_or_else(|| corrupt!("bad node type id {:#x}", self.body[0]))?,
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

/// A borrowed leaf.
#[derive(Clone, Copy)]
pub struct LeafView<'a> {
    pub path: &'a [u8],
    pub value: &'a [u8],
    end: usize,
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

impl<'a> BranchView<'a> {
    pub fn kind(&self) -> Kind {
        self.kind
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
                            bad = Some(corrupt!(
                                "{kind:?}: occupied slot {i} after an empty slot"
                            ));
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
                        return Err(corrupt!("Node48: corrupt index array: index points to empty node"));
                    }
                }
            }
        }
        out.path = Path::from_slice(trailer.path).expect("<= 32");
        Ok(trailer.end)
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

/// A borrowed patch: its base pointer and the (not yet decoded) diff list.
#[derive(Clone, Copy)]
pub struct PatchView<'a> {
    pub base: Ptr,
    n: usize,
    /// The body from the first diff pointer on.
    diff: &'a [u8],
}

impl PatchView<'_> {
    /// Body offset of the first diff pointer.
    fn start(&self) -> usize {
        1 + ptr_compressed_len(&self.base) + 1
    }

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

    pub fn to_patch(self) -> Result<Patch, Error> {
        Ok(self.to_patch_with_len()?.0)
    }

    /// Apply this patch (see [`apply_patch`]).
    pub fn apply(&self, base: &mut Branch, patch_block: u32, cur_block: u32) -> Result<(), Error> {
        let mut fits = true;
        prepare_patch_base(base, self.base.back_block);
        self.for_each_diff(|d| fits &= base.insert(d))?;
        finish_patch(base, fits, patch_block, cur_block)
    }
}

fn prepare_patch_base(base: &mut Branch, base_block: u32) {
    backptr_children(&mut base.children, base_block);
}

fn finish_patch(base: &mut Branch, fits: bool, patch_block: u32, cur_block: u32) -> Result<(), Error> {
    if !fits {
        return Err(corrupt!("patch does not fit its base node"));
    }
    backptr_children(&mut base.children, patch_block);
    for c in base.children.iter_mut() {
        if c.is_back() && c.back_block == cur_block {
            c.id &= !BACKPTR;
            c.back_block = 0;
        }
    }
    Ok(())
}

/// Apply a patch with base pointer `patch_base` and diff pointers `diff`,
/// read from `patch_block`, onto `base` (the node as resolved so far),
/// producing the node as seen from `cur_block`.
///
/// Mirrors the original `apply_node*`: before the patch the base's inline
/// children become back-pointers into the patch's base block; after it the
/// patch's inline diffs become back-pointers into the patch's own block; and
/// pointers that ended up pointing into `cur_block` become inline again.
pub fn apply_patch(
    base: &mut Branch,
    patch_base: Ptr,
    diff: &[Ptr],
    patch_block: u32,
    cur_block: u32,
) -> Result<(), Error> {
    prepare_patch_base(base, patch_base.back_block);
    let mut fits = true;
    for d in diff {
        fits &= base.insert(*d);
    }
    finish_patch(base, fits, patch_block, cur_block)
}

// ---------------------------------------------------------------------------
// decoding

/// Decode the node body at the start of `body` (the bytes after the hash).
/// `expected_id` is the node type the referring pointer claims (back-pointer
/// bit ignored); a patch is accepted in place of any branch type.
/// Returns the decoded value and the number of body bytes consumed.
pub fn decode(body: &[u8], expected_id: u8) -> Result<(Decoded, usize), Error> {
    Record::parse_body(body, expected_id)?.decode()
}

// ---------------------------------------------------------------------------
// encoding

/// Whether the compressed slot list should use the sparse form, and its size.
fn compressed_slots_mapped(branch: &Branch, map: &dyn Fn(&Ptr) -> Ptr) -> (bool, usize) {
    let occupied: usize = branch.children.iter().map(|p| ptr_compressed_len(&map(p))).sum();
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
        + patch.diff.iter().map(|p| ptr_compressed_len(&map(p))).sum::<usize>()
}

/// Append the body of `node` to `out`.
pub fn encode_node(node: &Node, compress: bool, out: &mut Vec<u8>) {
    encode_node_mapped(node, compress, &|p| *p, out)
}

/// [`encode_node`] with every child pointer passed through `map` first.
pub fn encode_node_mapped(node: &Node, compress: bool, map: &dyn Fn(&Ptr) -> Ptr, out: &mut Vec<u8>) {
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

// ---------------------------------------------------------------------------
// patches

/// Compute the patch that turns `old` (the node at `base`, as read from its
/// own block) into `new` (the same node as rewritten in the current trie),
/// or `None` if no patch applies (different type or path, or no change).
///
/// `new` must be fully back-pointer-ified relative to committed blocks; its
/// inline children refer to the current trie and always appear in the diff.
pub fn make_patch(base: Ptr, old_children: &[Ptr], new: &Branch) -> Option<Patch> {
    debug_assert!(base.is_back());
    let mut diff = Vec::new();
    for np in &new.children {
        match old_children.iter().find(|op| op.chr == np.chr) {
            None => diff.push(*np),
            Some(op) => {
                let same = if !op.is_back() && np.is_back() && np.back_block == base.back_block {
                    // `np` may be the back-pointer-ified copy of `op`.
                    *op == np.without_back()
                } else {
                    op == np && np.is_back()
                };
                if !same {
                    diff.push(*np);
                }
            }
        }
    }
    if diff.is_empty() {
        None
    } else {
        Some(Patch { base, diff })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
