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

//! Trie layout, serialization and reload.
use stacks_common::types::chainstate::TrieHash;

use super::{HEADER_LEN, Origin, ROOT_OFFSET, Trie};
use crate::codec::{Decoded, HASH_LEN, Patch};
use crate::node::{Branch, Kind, Node, Ptr, id};
use crate::{Error, MarfTrieId, codec, corrupt};

impl<T: MarfTrieId> Trie<T> {
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
    fn serialize_at(
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
                        ptr: usize::try_from(p.ptr)
                            .ok()
                            .and_then(|i| offsets.get(i).copied())
                            .unwrap_or(0),
                        ..*p
                    }
                }
            };
            for (&idx, repr) in order.iter().zip(&reprs) {
                next[idx as usize] = pos;
                let body_len = match repr {
                    Repr::Patch(p) => codec::patch_len_mapped(p, &lenient),
                    Repr::Node => {
                        codec::node_len_mapped(&self.slots[idx as usize].node, compress, &lenient)
                    }
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

#[cfg(test)]
mod tests {
    use stacks_common::types::chainstate::TrieHash;

    use super::super::ROOT_OFFSET;
    use crate::MARFValue;
    use crate::codec::{self, Decoded, HASH_LEN};
    use crate::node::{Node, id};
    use crate::trie::tests::{no_fetch, trie};
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
