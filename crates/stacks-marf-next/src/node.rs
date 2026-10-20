//! The logical trie node model.
//!
//! A trie node is either a [`Leaf`] (compressed path + 40-byte value) or a
//! [`Branch`] of one of four arities. A branch stores only its occupied child
//! slots, in slot order; the arity ([`Kind`]) says how many slots exist, which
//! matters because node hashes commit to every slot, empty or not.
//!
//! Slot order is an invariant inherited from the original implementation:
//! `Node4/16/48` fill slots in insertion order and never vacate them (the MARF
//! has no deletes), so the occupied slots are always a prefix of the slot
//! array; `Node256` slots are indexed by path character, so its children are
//! kept sorted by `chr`.

use std::fmt;

use stacks_common::util::hash::to_hex;

use crate::MARFValue;

/// Raw node type identifiers as stored on disk and hashed in consensus.
pub mod id {
    pub const EMPTY: u8 = 0;
    pub const LEAF: u8 = 1;
    pub const NODE4: u8 = 2;
    pub const NODE16: u8 = 3;
    pub const NODE48: u8 = 4;
    pub const NODE256: u8 = 5;
    pub const PATCH: u8 = 6;
}

/// Pointer flag: the child lives in an earlier block's trie.
pub const BACKPTR: u8 = 0x80;

/// Branch arity.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Kind {
    Node4,
    Node16,
    Node48,
    Node256,
}

impl Kind {
    pub fn from_id(id: u8) -> Option<Kind> {
        match id & 0x0f {
            id::NODE4 => Some(Kind::Node4),
            id::NODE16 => Some(Kind::Node16),
            id::NODE48 => Some(Kind::Node48),
            id::NODE256 => Some(Kind::Node256),
            _ => None,
        }
    }

    pub fn id(self) -> u8 {
        match self {
            Kind::Node4 => id::NODE4,
            Kind::Node16 => id::NODE16,
            Kind::Node48 => id::NODE48,
            Kind::Node256 => id::NODE256,
        }
    }

    /// Number of child slots.
    pub fn capacity(self) -> usize {
        match self {
            Kind::Node4 => 4,
            Kind::Node16 => 16,
            Kind::Node48 => 48,
            Kind::Node256 => 256,
        }
    }

    /// The next larger arity, if any.
    pub fn promoted(self) -> Option<Kind> {
        match self {
            Kind::Node4 => Some(Kind::Node16),
            Kind::Node16 => Some(Kind::Node48),
            Kind::Node48 => Some(Kind::Node256),
            Kind::Node256 => None,
        }
    }
}

/// A child pointer.
///
/// `id` is the child's node type id (`id::LEAF` .. `id::NODE256`) with the
/// [`BACKPTR`] bit set when the child lives in another block. `ptr` locates
/// the child: an arena index while the trie is in memory, a byte offset
/// within the owning blob once persisted. `back_block` is the owning block's
/// storage id for back-pointers; for inline pointers it is normally zero, and
/// a non-zero value is a squash annotation recording the child's original
/// block, which must be preserved verbatim.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, Default)]
pub struct Ptr {
    pub id: u8,
    pub chr: u8,
    pub ptr: u64,
    pub back_block: u32,
}

impl Ptr {
    pub fn new(id: u8, chr: u8, ptr: u64) -> Ptr {
        Ptr {
            id,
            chr,
            ptr,
            back_block: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.id == id::EMPTY
    }

    pub fn is_back(&self) -> bool {
        self.id & BACKPTR != 0
    }

    /// The child's node type id without the back-pointer bit.
    pub fn kind_id(&self) -> u8 {
        self.id & !BACKPTR
    }

    pub fn is_leaf(&self) -> bool {
        self.kind_id() == id::LEAF
    }

    /// This pointer as an inline pointer into the block it refers to.
    pub fn without_back(&self) -> Ptr {
        Ptr {
            id: self.kind_id(),
            chr: self.chr,
            ptr: self.ptr,
            back_block: 0,
        }
    }
}

/// Turn every inline child into a back-pointer into `block`, keeping
/// existing back-pointers and squash annotations (non-zero `back_block`).
pub fn backptr_children(children: &mut [Ptr], block: u32) {
    for p in children.iter_mut() {
        if p.is_empty() || p.is_back() {
            continue;
        }
        if p.back_block == 0 {
            p.back_block = block;
        }
        p.id |= BACKPTR;
    }
}

/// A compressed path segment: up to 32 bytes stored inline.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Path {
    len: u8,
    data: [u8; 32],
}

impl Path {
    pub const EMPTY: Path = Path {
        len: 0,
        data: [0u8; 32],
    };

    /// `None` if `s` is longer than 32 bytes.
    pub fn from_slice(s: &[u8]) -> Option<Path> {
        if s.len() > 32 {
            return None;
        }
        let mut data = [0u8; 32];
        data[..s.len()].copy_from_slice(s);
        Some(Path {
            len: s.len() as u8,
            data,
        })
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.data[..self.len as usize]
    }

    pub fn len(&self) -> usize {
        self.len as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl fmt::Debug for Path {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&to_hex(self.as_slice()))
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct Leaf {
    pub path: Path,
    pub value: MARFValue,
}

impl fmt::Debug for Leaf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Leaf(path={:?} value={})",
            self.path,
            self.value.to_hex()
        )
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct Branch {
    pub kind: Kind,
    pub path: Path,
    /// Occupied child slots in slot order (see module docs).
    pub children: Vec<Ptr>,
}

impl fmt::Debug for Branch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}(path={:?} children=[", self.kind, self.path)?;
        for (i, c) in self.children.iter().enumerate() {
            if i > 0 {
                f.write_str(",")?;
            }
            write!(
                f,
                "id({})chr({:02x})ptr({})bblk({})",
                c.id, c.chr, c.ptr, c.back_block
            )?;
        }
        f.write_str("])")
    }
}

impl Branch {
    pub fn new(kind: Kind, path: Path) -> Branch {
        Branch {
            kind,
            path,
            children: Vec::with_capacity(kind.capacity().min(16)),
        }
    }

    /// Build from a full slot array (empty slots have `id == EMPTY`),
    /// verifying the slot-order invariant.
    pub fn from_slots(kind: Kind, path: Path, slots: &[Ptr]) -> Result<Branch, crate::Error> {
        if slots.len() != kind.capacity() {
            return Err(crate::corrupt!(
                "{kind:?}: expected {} slots, got {}",
                kind.capacity(),
                slots.len()
            ));
        }
        let mut children = Vec::with_capacity(slots.iter().filter(|p| !p.is_empty()).count());
        let mut seen_empty = false;
        for (i, p) in slots.iter().enumerate() {
            if p.is_empty() {
                seen_empty = true;
                continue;
            }
            match kind {
                Kind::Node256 => {
                    if p.chr as usize != i {
                        return Err(crate::corrupt!("Node256: slot {i} holds chr {:02x}", p.chr));
                    }
                }
                _ => {
                    if seen_empty {
                        return Err(crate::corrupt!(
                            "{kind:?}: occupied slot {i} after an empty slot"
                        ));
                    }
                }
            }
            children.push(*p);
        }
        Ok(Branch {
            kind,
            path,
            children,
        })
    }

    pub fn child(&self, chr: u8) -> Option<&Ptr> {
        match self.kind {
            // sorted by chr
            Kind::Node256 => self.children.binary_search_by_key(&chr, |p| p.chr).ok().map(|i| &self.children[i]),
            _ => self.children.iter().find(|p| p.chr == chr),
        }
    }

    pub fn child_mut(&mut self, chr: u8) -> Option<&mut Ptr> {
        match self.kind {
            Kind::Node256 => match self.children.binary_search_by_key(&chr, |p| p.chr) {
                Ok(i) => Some(&mut self.children[i]),
                Err(_) => None,
            },
            _ => self.children.iter_mut().find(|p| p.chr == chr),
        }
    }

    pub fn is_full(&self) -> bool {
        self.children.len() >= self.kind.capacity()
    }

    /// Insert or replace the child at `ptr.chr`. Returns `false` if the
    /// character is new and the node is full.
    pub fn insert(&mut self, ptr: Ptr) -> bool {
        debug_assert!(!ptr.is_empty());
        if let Some(slot) = self.child_mut(ptr.chr) {
            *slot = ptr;
            return true;
        }
        if self.is_full() {
            return false;
        }
        match self.kind {
            Kind::Node256 => {
                let pos = self.children.partition_point(|p| p.chr < ptr.chr);
                self.children.insert(pos, ptr);
            }
            _ => self.children.push(ptr),
        }
        true
    }

    /// Replace the child at `ptr.chr`; `false` if there is none.
    pub fn replace(&mut self, ptr: Ptr) -> bool {
        match self.child_mut(ptr.chr) {
            Some(slot) => {
                *slot = ptr;
                true
            }
            None => false,
        }
    }

    /// Visit every slot in slot order; `None` for empty slots.
    pub fn slots(&self) -> Slots<'_> {
        Slots {
            branch: self,
            slot: 0,
            next_child: 0,
        }
    }

    /// The same node with the next larger arity (`None` for `Node256`).
    pub fn promoted(&self) -> Option<Branch> {
        let kind = self.kind.promoted()?;
        let mut children = self.children.clone();
        if kind == Kind::Node256 {
            children.sort_by_key(|p| p.chr);
        }
        Some(Branch {
            kind,
            path: self.path,
            children,
        })
    }
}

/// Iterator over a branch's slots (see [`Branch::slots`]).
pub struct Slots<'a> {
    branch: &'a Branch,
    slot: usize,
    next_child: usize,
}

impl<'a> Iterator for Slots<'a> {
    type Item = Option<&'a Ptr>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.slot >= self.branch.kind.capacity() {
            return None;
        }
        let slot = self.slot;
        self.slot += 1;
        match self.branch.children.get(self.next_child) {
            Some(child) if self.branch.kind != Kind::Node256 || child.chr as usize == slot => {
                self.next_child += 1;
                Some(Some(child))
            }
            _ => Some(None),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.branch.kind.capacity() - self.slot;
        (n, Some(n))
    }
}

impl ExactSizeIterator for Slots<'_> {}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Node {
    Leaf(Leaf),
    Branch(Branch),
}

impl Node {
    pub fn path(&self) -> &Path {
        match self {
            Node::Leaf(l) => &l.path,
            Node::Branch(b) => &b.path,
        }
    }

    pub fn set_path(&mut self, path: Path) {
        match self {
            Node::Leaf(l) => l.path = path,
            Node::Branch(b) => b.path = path,
        }
    }

    pub fn kind_id(&self) -> u8 {
        match self {
            Node::Leaf(_) => id::LEAF,
            Node::Branch(b) => b.kind.id(),
        }
    }

    pub fn is_leaf(&self) -> bool {
        matches!(self, Node::Leaf(_))
    }

    pub fn as_branch(&self) -> Option<&Branch> {
        match self {
            Node::Branch(b) => Some(b),
            Node::Leaf(_) => None,
        }
    }

    pub fn as_branch_mut(&mut self) -> Option<&mut Branch> {
        match self {
            Node::Branch(b) => Some(b),
            Node::Leaf(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_iterate_in_slot_order() {
        let mut b = Branch::new(Kind::Node4, Path::EMPTY);
        assert!(b.insert(Ptr::new(id::LEAF, 9, 1)));
        assert!(b.insert(Ptr::new(id::LEAF, 3, 2)));
        let slots: Vec<Option<u8>> = b.slots().map(|s| s.map(|p| p.chr)).collect();
        assert_eq!(slots, vec![Some(9), Some(3), None, None]);

        let mut b = Branch::new(Kind::Node256, Path::EMPTY);
        assert!(b.insert(Ptr::new(id::LEAF, 9, 1)));
        assert!(b.insert(Ptr::new(id::LEAF, 3, 2)));
        let slots: Vec<Option<u8>> = b.slots().map(|s| s.map(|p| p.chr)).collect();
        assert_eq!(slots.len(), 256);
        assert_eq!(slots[3], Some(3));
        assert_eq!(slots[9], Some(9));
        assert_eq!(slots.iter().filter(|s| s.is_some()).count(), 2);
    }

    #[test]
    fn insert_replaces_and_fills() {
        let mut b = Branch::new(Kind::Node4, Path::EMPTY);
        for c in 0..4u8 {
            assert!(b.insert(Ptr::new(id::LEAF, c, c as u64)));
        }
        assert!(!b.insert(Ptr::new(id::LEAF, 7, 7)));
        assert!(b.insert(Ptr::new(id::NODE4, 2, 99)));
        assert_eq!(b.child(2).unwrap().ptr, 99);
        let p = b.promoted().unwrap();
        assert_eq!(p.kind, Kind::Node16);
        assert_eq!(p.children, b.children);
    }
}
