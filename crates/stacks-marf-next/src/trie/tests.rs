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

//! Arena insertion and reload regression tests.

use stacks_common::types::chainstate::StacksBlockId;

use super::*;
use crate::node::Path;
use crate::{ClarityMarfTrieId, MARFValue};

pub fn trie() -> Trie<StacksBlockId> {
    Trie::new(
        StacksBlockId([1u8; 32]),
        StacksBlockId::sentinel(),
        Branch::new(Kind::Node256, Path::EMPTY),
        None,
    )
}

pub fn no_fetch(_: &Ptr) -> Result<Fetched, Error> {
    panic!("no back-pointers expected")
}

pub fn lookup(t: &Trie<StacksBlockId>, key: &[u8; 32]) -> Option<MARFValue> {
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
