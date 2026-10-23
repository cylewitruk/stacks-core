//! The in-memory trie under construction for one block.
//!
//! Nodes live in a flat arena indexed by `u32`; an inline child pointer's
//! `ptr` field is an arena index until [`Trie::serialize`] assigns byte
//! offsets. Writes never touch persistent storage: a node that lives in an
//! ancestor block is copied into the arena on first touch (copy-on-write) and
//! its own children become back-pointers into that ancestor block.
//!
//! Hashes are computed once, post-order, by [`Trie::seal`].

use stacks_common::types::chainstate::TrieHash;

use crate::codec::{self, Decoded, Patch, HASH_LEN};
use crate::hash::{branch_hash, leaf_hash, ChildHash, HashScratch};
use crate::node::{backptr_children, id, Branch, Kind, Leaf, Node, Path, Ptr, BACKPTR};
use crate::{corrupt, Error, MARFValue, MarfTrieId};

/// Fixed blob header: `parent block id (32) || reserved u32 LE (0)`.
pub const HEADER_LEN: usize = 36;
/// Byte offset of the root node (and therefore of the root hash) in a blob.
pub const ROOT_OFFSET: u64 = HEADER_LEN as u64;

/// Where a node copied into this trie came from, for delta encoding at flush.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Origin {
    /// Storage id of the block the node was read from.
    pub block: u32,
    /// Back-pointer form of the pointer that was followed to read it.
    pub ptr: Ptr,
    /// Number of patches that were stacked on the base node when it was read.
    pub depth: u32,
}

#[derive(Clone, Debug)]
pub struct Slot {
    pub node: Node,
    pub hash: TrieHash,
    pub origin: Option<Origin>,
}

#[derive(Clone)]
pub struct Trie<T> {
    pub block: T,
    pub parent: T,
    slots: Vec<Slot>,
    sealed: bool,
}

/// A node read from an ancestor block, with the patch depth of its location.
pub struct Fetched {
    pub node: Node,
    pub depth: u32,
}

fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

impl<T: MarfTrieId> Trie<T> {
    /// A trie whose root is `root` (a `Node256`), copied from `origin` if any.
    pub fn new(block: T, parent: T, root: Branch, origin: Option<Origin>) -> Trie<T> {
        debug_assert_eq!(root.kind, Kind::Node256);
        Trie {
            block,
            parent,
            slots: vec![Slot {
                node: Node::Branch(root),
                hash: TrieHash::ZERO,
                origin,
            }],
            sealed: false,
        }
    }

    /// A trie over an existing node list whose inline pointers are arena
    /// indices and whose first node is the `Node256` root.
    pub fn from_nodes(block: T, parent: T, nodes: Vec<Node>) -> Result<Trie<T>, Error> {
        match nodes.first() {
            Some(Node::Branch(b)) if b.kind == Kind::Node256 => {}
            _ => return Err(corrupt!("first node is not a Node256 root")),
        }
        Ok(Trie {
            block,
            parent,
            slots: nodes
                .into_iter()
                .map(|node| Slot {
                    node,
                    hash: TrieHash::ZERO,
                    origin: None,
                })
                .collect(),
            sealed: false,
        })
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    pub fn is_sealed(&self) -> bool {
        self.sealed
    }

    pub fn node(&self, idx: u32) -> Result<&Node, Error> {
        self.slots
            .get(idx as usize)
            .map(|s| &s.node)
            .ok_or_else(|| corrupt!("arena index {idx} out of range ({})", self.slots.len()))
    }

    /// The hash stored for node `idx` (zero until sealed).
    pub fn hash(&self, idx: u32) -> Result<TrieHash, Error> {
        self.slots
            .get(idx as usize)
            .map(|s| s.hash)
            .ok_or_else(|| corrupt!("arena index {idx} out of range ({})", self.slots.len()))
    }

    pub fn root_hash(&self) -> TrieHash {
        self.slots[0].hash
    }

    pub fn set_root_hash(&mut self, hash: TrieHash) {
        self.slots[0].hash = hash;
    }

    fn push(&mut self, node: Node, origin: Option<Origin>) -> u32 {
        self.slots.push(Slot {
            node,
            hash: TrieHash::ZERO,
            origin,
        });
        (self.slots.len() - 1) as u32
    }

    /// Insert `value` at `key`, copying ancestor nodes forward as needed.
    /// `fetch` reads the node a back-pointer refers to.
    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn insert(
        &mut self,
        key: &[u8; 32],
        value: MARFValue,
        mut fetch: impl FnMut(&Ptr) -> Result<Fetched, Error>,
    ) -> Result<(), Error> {
        if self.sealed {
            return Err(Error::ReadOnlyError);
        }
        let mut idx = 0u32;
        let mut depth = 0usize;
        loop {
            let node = &self.slots[idx as usize].node;
            let node_path = node.path().as_slice();
            let common = common_prefix(node_path, &key[depth..]);
            if common < node_path.len() {
                return self.split(idx, depth, common, key, value);
            }
            depth += common;
            let chr = match &self.slots[idx as usize].node {
                Node::Leaf(_) => {
                    if depth != key.len() {
                        return Err(corrupt!("leaf path does not reach the end of the key"));
                    }
                    if let Node::Leaf(leaf) = &mut self.slots[idx as usize].node {
                        leaf.value = value;
                    }
                    return Ok(());
                }
                Node::Branch(_) => {
                    let Some(chr) = key.get(depth) else {
                        return Err(corrupt!("key exhausted at a branch node"));
                    };
                    *chr
                }
            };
            let child = self.slots[idx as usize]
                .node
                .as_branch()
                .and_then(|b| b.child(chr))
                .copied();
            match child {
                None => return self.attach_leaf(idx, chr, &key[depth + 1..], value),
                Some(p) if p.is_back() => {
                    let fetched = fetch(&p)?;
                    let mut node = fetched.node;
                    if node.kind_id() != p.kind_id() {
                        return Err(corrupt!(
                            "back-pointer {p:?} led to a node of type {:#x}",
                            node.kind_id()
                        ));
                    }
                    if let Node::Branch(b) = &mut node {
                        backptr_children(&mut b.children, p.back_block);
                    }
                    let new_idx = self.push(
                        node,
                        Some(Origin {
                            block: p.back_block,
                            ptr: p,
                            depth: fetched.depth,
                        }),
                    );
                    let parent = self.slots[idx as usize]
                        .node
                        .as_branch_mut()
                        .expect("branch");
                    parent.replace(Ptr::new(p.kind_id(), chr, u64::from(new_idx)));
                    idx = new_idx;
                    depth += 1;
                }
                Some(p) => {
                    idx = u32::try_from(p.ptr).map_err(|_| Error::OverflowError)?;
                    depth += 1;
                }
            }
        }
    }

    /// Add a leaf under the branch at `idx`, promoting the branch if full.
    fn attach_leaf(
        &mut self,
        idx: u32,
        chr: u8,
        rest: &[u8],
        value: MARFValue,
    ) -> Result<(), Error> {
        let leaf = Leaf {
            path: Path::from_slice(rest).ok_or(Error::OverflowError)?,
            value,
        };
        let leaf_idx = self.push(Node::Leaf(leaf), None);
        let ptr = Ptr::new(id::LEAF, chr, u64::from(leaf_idx));
        let branch = self.slots[idx as usize]
            .node
            .as_branch_mut()
            .expect("branch");
        if !branch.insert(ptr) {
            let mut promoted = branch
                .promoted()
                .ok_or_else(|| corrupt!("could not insert into a full Node256"))?;
            assert!(promoted.insert(ptr));
            *branch = promoted;
        }
        Ok(())
    }

    /// The node at `idx` diverges from `key` after `common` bytes of its
    /// path: replace it with a small branch holding the shortened node and a
    /// new leaf.
    fn split(
        &mut self,
        idx: u32,
        depth: usize,
        common: usize,
        key: &[u8; 32],
        value: MARFValue,
    ) -> Result<(), Error> {
        let slot = &mut self.slots[idx as usize];
        let old_path = *slot.node.path();
        let old_chr = old_path.as_slice()[common];
        let shared = Path::from_slice(&old_path.as_slice()[..common]).expect("<= 32");
        let old_rest = Path::from_slice(&old_path.as_slice()[common + 1..]).expect("<= 32");
        let old_is_leaf = slot.node.is_leaf();

        // Move the old node to a new arena slot (it keeps its origin).
        let kind = if idx == 0 { Kind::Node256 } else { Kind::Node4 };
        let mut parent = Node::Branch(Branch::new(kind, shared));
        std::mem::swap(&mut slot.node, &mut parent);
        let mut old_node = parent;
        old_node.set_path(old_rest);
        let old_kind = old_node.kind_id();
        let old_origin = slot.origin.take();
        let old_idx = self.push(old_node, old_origin);
        let old_ptr = Ptr::new(old_kind, old_chr, u64::from(old_idx));

        let leaf_chr = key[depth + common];
        let leaf = Leaf {
            path: Path::from_slice(&key[depth + common + 1..]).expect("<= 32"),
            value,
        };
        let leaf_idx = self.push(Node::Leaf(leaf), None);
        let leaf_ptr = Ptr::new(id::LEAF, leaf_chr, u64::from(leaf_idx));

        // Slot order is consensus-relevant and follows the original
        // implementation: splitting a leaf puts the old leaf first, splitting
        // a branch puts the new leaf first.
        let branch = self.slots[idx as usize]
            .node
            .as_branch_mut()
            .expect("branch");
        if old_is_leaf {
            assert!(branch.insert(old_ptr));
            assert!(branch.insert(leaf_ptr));
        } else {
            assert!(branch.insert(leaf_ptr));
            assert!(branch.insert(old_ptr));
        }
        Ok(())
    }

    /// Compute every node hash post-order and return the root node's hash
    /// (before the ancestor skip-list is mixed in). `block_id` maps a
    /// back-pointer's block id to that block's 32-byte identifier.
    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn seal(
        &mut self,
        mut block_id: impl FnMut(u32) -> Result<[u8; 32], Error>,
    ) -> Result<TrieHash, Error> {
        let mut scratch = HashScratch::default();
        let root = self.hash_subtree(0, &mut block_id, &mut scratch)?;
        self.sealed = true;
        Ok(root)
    }

    #[cfg_attr(
        feature = "profiler",
        stacks_profiler::profile(sample_rate = 8, unsampled = "count_only")
    )]
    fn hash_subtree(
        &mut self,
        idx: u32,
        block_id: &mut impl FnMut(u32) -> Result<[u8; 32], Error>,
        scratch: &mut HashScratch,
    ) -> Result<TrieHash, Error> {
        let n_children = match &self.slots[idx as usize].node {
            Node::Leaf(leaf) => {
                let hash = leaf_hash(leaf);
                self.slots[idx as usize].hash = hash;
                return Ok(hash);
            }
            Node::Branch(b) => b.children.len(),
        };
        // Hash inline children first (post-order), and normalize their pointer
        // ids to the children's actual kinds: a child may have been promoted
        // since the pointer was written.
        for i in 0..n_children {
            let p = self.slots[idx as usize].node.as_branch().expect("branch").children[i];
            if p.is_back() {
                continue;
            }
            let child_idx = u32::try_from(p.ptr).map_err(|_| Error::OverflowError)?;
            self.hash_subtree(child_idx, block_id, scratch)?;
            let kind = self.slots[child_idx as usize].node.kind_id();
            self.slots[idx as usize].node.as_branch_mut().expect("branch").children[i].id = kind;
        }
        let slots = &self.slots;
        let branch = slots[idx as usize].node.as_branch().expect("branch");
        let hash = branch_hash::<Error>(
            branch,
            |p| {
                if p.is_back() {
                    Ok(ChildHash::back(block_id(p.back_block)?))
                } else {
                    Ok(ChildHash::inline(slots[p.ptr as usize].hash))
                }
            },
            scratch,
        )?;
        self.slots[idx as usize].hash = hash;
        Ok(hash)
    }

    /// Serialize into the legacy blob layout. The root is written at
    /// [`ROOT_OFFSET`], followed by the remaining nodes in pre-order.
    ///
    /// With `compress`, `patcher` may replace a copied-forward node by a
    /// delta against its origin.
    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn serialize(
        &self,
        compress: bool,
        patcher: impl FnMut(&Branch, &Origin, &TrieHash) -> Result<Option<Patch>, Error>,
    ) -> Result<Vec<u8>, Error> {
        self.serialize_at(0, compress, patcher)
    }

    /// [`Trie::serialize`] with every offset shifted by `base`: the blob is
    /// laid out as if it started `base` bytes into a larger file. Used to test
    /// pointer-width selection without a multi-gigabyte trie; production
    /// blobs always use `base == 0`.
    pub(crate) fn serialize_at(
        &self,
        base: u64,
        compress: bool,
        mut patcher: impl FnMut(&Branch, &Origin, &TrieHash) -> Result<Option<Patch>, Error>,
    ) -> Result<Vec<u8>, Error> {
        // Pre-order walk over reachable nodes.
        let mut order: Vec<u32> = Vec::with_capacity(self.slots.len());
        let mut stack = vec![0u32];
        while let Some(idx) = stack.pop() {
            order.push(idx);
            if let Node::Branch(b) = &self.slots[idx as usize].node {
                for p in b.children.iter().rev() {
                    if !p.is_back() {
                        stack.push(u32::try_from(p.ptr).map_err(|_| Error::OverflowError)?);
                    }
                }
            }
        }

        // Decide each node's representation.
        enum Repr {
            Node,
            Patch(Patch),
        }
        let mut reprs: Vec<Repr> = Vec::with_capacity(order.len());
        for &idx in &order {
            let slot = &self.slots[idx as usize];
            let repr = match (&slot.node, slot.origin.as_ref()) {
                (Node::Branch(b), Some(origin))
                    if compress && origin.depth < codec::MAX_PATCH_DEPTH =>
                {
                    match patcher(b, origin, &slot.hash)? {
                        Some(patch) => Repr::Patch(patch),
                        None => Repr::Node,
                    }
                }
                _ => Repr::Node,
            };
            reprs.push(repr);
        }

        // Assign offsets. A pointer's encoded width depends on the offset it
        // holds, and offsets depend on earlier records' widths, so iterate to
        // a fixpoint: widths only ever grow, and so do offsets.
        let mut offsets = vec![0u64; self.slots.len()];
        let mut iterations = 0;
        loop {
            let mut next = vec![0u64; self.slots.len()];
            let mut pos = base + ROOT_OFFSET;
            // inline pointers hold arena indices; map them to the offsets
            // assigned so far (0 while unknown, which sizes as narrow)
            let lenient = |p: &Ptr| -> Ptr {
                if p.is_empty() || p.is_back() {
                    *p
                } else {
                    Ptr {
                        ptr: usize::try_from(p.ptr).ok().and_then(|i| offsets.get(i).copied()).unwrap_or(0),
                        ..*p
                    }
                }
            };
            for (&idx, repr) in order.iter().zip(&reprs) {
                next[idx as usize] = pos;
                let body_len = match repr {
                    Repr::Patch(p) => codec::patch_len_mapped(p, &lenient),
                    Repr::Node => codec::node_len_mapped(&self.slots[idx as usize].node, compress, &lenient),
                };
                pos += (HASH_LEN + body_len) as u64;
            }
            let stable = next == offsets;
            offsets = next;
            if stable {
                break;
            }
            iterations += 1;
            if iterations > 64 {
                return Err(corrupt!("trie layout did not converge"));
            }
        }

        let total = offsets.iter().copied().max().unwrap_or(base) as usize - base as usize
            + codec::MAX_NODE_LEN;
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(self.parent.as_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        // every reachable inline child now has an offset
        for &idx in &order {
            if let Node::Branch(b) = &self.slots[idx as usize].node {
                for p in &b.children {
                    if !p.is_back() && offsets.get(p.ptr as usize).copied().unwrap_or(0) == 0 {
                        return Err(corrupt!("inline child index {} has no offset", p.ptr));
                    }
                }
            }
        }
        let resolved = |p: &Ptr| -> Ptr {
            if p.is_empty() || p.is_back() {
                *p
            } else {
                Ptr {
                    ptr: offsets[p.ptr as usize],
                    ..*p
                }
            }
        };
        for (&idx, repr) in order.iter().zip(&reprs) {
            debug_assert_eq!(out.len() as u64 + base, offsets[idx as usize]);
            let slot = &self.slots[idx as usize];
            out.extend_from_slice(slot.hash.as_bytes());
            match repr {
                Repr::Patch(p) => codec::encode_patch_mapped(p, &resolved, &mut out),
                Repr::Node => codec::encode_node_mapped(&slot.node, compress, &resolved, &mut out),
            }
        }
        Ok(out)
    }

    /// Reload a trie from a blob written by [`Trie::serialize`] (or by the
    /// original implementation) so it can be extended further. Used for
    /// unconfirmed tries, which are never compressed.
    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn load(bytes: &[u8], block: T) -> Result<Trie<T>, Error> {
        if bytes.len() < HEADER_LEN + HASH_LEN {
            return Err(corrupt!("trie blob too short ({} bytes)", bytes.len()));
        }
        let mut parent = [0u8; 32];
        parent.copy_from_slice(&bytes[..32]);
        let parent = T::from_bytes(parent);

        let read = |offset: u64, expected: u8| -> Result<(Node, TrieHash), Error> {
            let start = usize::try_from(offset).map_err(|_| Error::OverflowError)?;
            let rec = bytes
                .get(start..)
                .ok_or_else(|| corrupt!("node offset {offset} beyond blob end"))?;
            if rec.len() < HASH_LEN {
                return Err(corrupt!("node at {offset} truncated"));
            }
            let hash = TrieHash::from_bytes(&rec[..HASH_LEN]).expect("32 bytes");
            match codec::decode(&rec[HASH_LEN..], expected)? {
                (Decoded::Node(node), _) => Ok((node, hash)),
                (Decoded::Patch(_), _) => Err(corrupt!("patch node in a reloadable trie")),
            }
        };

        let (root, root_hash) = read(ROOT_OFFSET, id::NODE256)?;
        let Node::Branch(root) = root else {
            return Err(corrupt!("root is not a branch"));
        };
        if root.kind != Kind::Node256 {
            return Err(corrupt!("root is not a Node256"));
        }
        let mut trie = Trie::new(block, parent, root, None);
        trie.slots[0].hash = root_hash;
        // Breadth-first: rewrite each inline pointer to the arena index its
        // child will occupy, then read the child.
        let mut queue = std::collections::VecDeque::new();
        let mut next = 1u32;
        let relink =
            |node: &mut Node, queue: &mut std::collections::VecDeque<Ptr>, next: &mut u32| {
                if let Node::Branch(b) = node {
                    for p in b.children.iter_mut() {
                        if !p.is_back() {
                            queue.push_back(*p);
                            p.ptr = u64::from(*next);
                            *next += 1;
                        }
                    }
                }
            };
        relink(&mut trie.slots[0].node, &mut queue, &mut next);
        while let Some(p) = queue.pop_front() {
            let (mut node, hash) = read(p.ptr, p.kind_id())?;
            relink(&mut node, &mut queue, &mut next);
            let idx = trie.push(node, None);
            trie.slots[idx as usize].hash = hash;
        }
        trie.sealed = false;
        Ok(trie)
    }
}

/// Make the back-pointer bit explicit on a pointer in `block`.
pub fn as_backptr(p: Ptr, block: u32) -> Ptr {
    Ptr {
        id: p.id | BACKPTR,
        back_block: block,
        ..p
    }
}

#[cfg(test)]
mod tests {
    use stacks_common::types::chainstate::StacksBlockId;

    use super::*;
    use crate::ClarityMarfTrieId;

    fn trie() -> Trie<StacksBlockId> {
        Trie::new(
            StacksBlockId([1u8; 32]),
            StacksBlockId::sentinel(),
            Branch::new(Kind::Node256, Path::EMPTY),
            None,
        )
    }

    fn no_fetch(_: &Ptr) -> Result<Fetched, Error> {
        panic!("no back-pointers expected")
    }

    fn lookup(t: &Trie<StacksBlockId>, key: &[u8; 32]) -> Option<MARFValue> {
        let mut idx = 0u32;
        let mut depth = 0;
        loop {
            let node = t.node(idx).unwrap();
            let p = node.path().as_slice();
            if &key[depth..depth + p.len()] != p {
                return None;
            }
            depth += p.len();
            match node {
                Node::Leaf(l) => return Some(l.value.clone()),
                Node::Branch(b) => {
                    let c = b.child(key[depth])?;
                    assert!(!c.is_back());
                    idx = c.ptr as u32;
                    depth += 1;
                }
            }
        }
    }

    #[test]
    fn insert_lookup_roundtrip_and_reload() {
        let mut t = trie();
        let mut keys = vec![];
        for i in 0..300u32 {
            let k = TrieHash::from_data(&i.to_be_bytes()).0;
            let v = MARFValue::from(i);
            t.insert(&k, v.clone(), no_fetch).unwrap();
            keys.push((k, v));
        }
        // overwrite
        let (k0, _) = keys[0].clone();
        t.insert(&k0, MARFValue::from(999u32), no_fetch).unwrap();
        keys[0].1 = MARFValue::from(999u32);
        for (k, v) in &keys {
            assert_eq!(lookup(&t, k).as_ref(), Some(v));
        }
        let root = t.seal(|_| unreachable!()).unwrap();
        t.set_root_hash(root);
        let bytes = t.serialize(false, |_, _, _| unreachable!()).unwrap();
        let t2 = Trie::load(&bytes, StacksBlockId([1u8; 32])).unwrap();
        assert_eq!(t2.len(), t.len());
        for (k, v) in &keys {
            assert_eq!(lookup(&t2, k).as_ref(), Some(v));
        }
        assert_eq!(t2.root_hash(), root);
        // reloaded trie hashes to the same root
        let mut t3 = t2.clone();
        assert_eq!(t3.seal(|_| unreachable!()).unwrap(), root);
        // compressed serialization decodes to the same nodes
        let cbytes = t.serialize(true, |_, _, _| Ok(None)).unwrap();
        assert!(cbytes.len() < bytes.len());
        let t4 = Trie::load(&cbytes, StacksBlockId([1u8; 32])).unwrap();
        for (k, v) in &keys {
            assert_eq!(lookup(&t4, k).as_ref(), Some(v));
        }
    }

    /// Walk a blob laid out at virtual offset `base`, checking that every
    /// pointer lands on a record of the claimed type, and return the leaves.
    fn walk_blob(bytes: &[u8], base: u64) -> Vec<(Vec<u8>, MARFValue)> {
        let mut leaves = vec![];
        let mut stack = vec![(ROOT_OFFSET + base, id::NODE256, vec![])];
        while let Some((off, kind, prefix)) = stack.pop() {
            let at = (off - base) as usize;
            let (decoded, _) =
                codec::decode(&bytes[at + HASH_LEN..], kind).expect("decode at pointer");
            let Decoded::Node(node) = decoded else {
                panic!("patch")
            };
            let mut path = prefix;
            path.extend_from_slice(node.path().as_slice());
            match node {
                Node::Leaf(l) => leaves.push((path, l.value)),
                Node::Branch(b) => {
                    for c in &b.children {
                        assert!(!c.is_back());
                        let mut p = path.clone();
                        p.push(c.chr);
                        stack.push((c.ptr, c.kind_id(), p));
                    }
                }
            }
        }
        leaves.sort();
        leaves
    }

    #[test]
    fn layout_agrees_with_emission_across_the_u32_boundary() {
        let mut t = trie();
        let mut expected = vec![];
        for i in 0..200u32 {
            let k = TrieHash::from_data(&i.to_le_bytes()).0;
            t.insert(&k, MARFValue::from(i), no_fetch).unwrap();
            expected.push((k.to_vec(), MARFValue::from(i)));
        }
        expected.sort();
        let root = t.seal(|_| unreachable!()).unwrap();
        t.set_root_hash(root);
        let plain = t.serialize(false, |_, _, _| Ok(None)).unwrap();
        for compress in [false, true] {
            // Place the boundary inside the blob so pointers are mixed: some
            // records sit below 4 GiB and keep 4-byte offsets, later ones need 8.
            let reference = t.serialize(compress, |_, _, _| Ok(None)).unwrap();
            let mut lengthened = false;
            for cut in [
                plain.len() as u64 / 3,
                plain.len() as u64 / 2,
                plain.len() as u64 - 100,
            ] {
                let base = u64::from(u32::MAX) - cut;
                let bytes = t.serialize_at(base, compress, |_, _, _| Ok(None)).unwrap();
                assert!(bytes.len() >= reference.len());
                lengthened |= bytes.len() > reference.len();
                assert_eq!(
                    walk_blob(&bytes, base),
                    expected,
                    "compress={compress} cut={cut}"
                );
            }
            assert!(lengthened, "some pointers must have widened");
        }
        // and the ordinary layout is unchanged
        assert_eq!(walk_blob(&plain, 0), expected);
    }
}
