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

//! Parallel source-leaf preparation with dependency-ordered final relocation.

use std::collections::{HashMap, HashSet};

use blockstack_lib::chainstate::stacks::index::node::{
    TrieNodeID, TrieNodeType, TriePtr, is_backptr,
};
use blockstack_lib::chainstate::stacks::index::record::NodeRecordFormat;
use blockstack_lib::chainstate::stacks::index::{Error, MARFValue, TrieLeaf};

use crate::value_lookup::{Reference, ValueLookup};

/// Values resolved once for a bounded source trie, before ordered pointer relocation.
pub struct PreparedLeaves {
    /// Owned final leaf representations; full keys distinguish raw mappings.
    values: HashMap<MARFValue, Reference>,
}

impl PreparedLeaves {
    /// Walk reachable source records only; ancestors are handled by the ordered publisher.
    pub fn read(bytes: &[u8], lookup: Option<&ValueLookup>) -> Result<Self, String> {
        let Some(lookup) = lookup else {
            return Ok(Self {
                values: HashMap::new(),
            });
        };
        NodeRecordFormat::Legacy
            .validate_trie_header(bytes)
            .map_err(|e| e.to_string())?;
        let mut pending = vec![36u64];
        let mut visited = HashSet::new();
        let mut values = HashMap::new();
        while let Some(offset) = pending.pop() {
            if !visited.insert(offset) {
                continue;
            }
            let offset = usize::try_from(offset).map_err(|e| e.to_string())?;
            let input = bytes.get(offset..).ok_or("source record outside trie")?;
            let record = NodeRecordFormat::Legacy
                .parse(input)
                .map_err(|e| e.to_string())?;
            let mut add = |ptr: &TriePtr| {
                if !ptr.is_empty() && !is_backptr(ptr.id()) {
                    pending.push(ptr.ptr());
                }
            };
            if record.logical_type() == TrieNodeID::Patch {
                let (patch, _) = record.decode_patch().map_err(|e| e.to_string())?;
                add(&patch.ptr);
                for ptr in &patch.ptr_diff {
                    add(ptr);
                }
            } else {
                let (node, _) = record
                    .decode_node(record.logical_type() as u8)
                    .map_err(|e| e.to_string())?;
                if let TrieNodeType::Leaf(leaf) = &node {
                    let key = leaf.value().map_err(|e| e.to_string())?;
                    if !values.contains_key(key) {
                        values.insert(key.clone(), lookup.get(key).map_err(|e| e.to_string())?);
                    }
                } else {
                    for ptr in node.ptrs() {
                        add(ptr);
                    }
                }
            }
        }
        Ok(Self { values })
    }

    /// Apply the exact pre-resolved representation; a missed leaf is a traversal bug.
    pub fn transform(&self, leaf: &mut TrieLeaf) -> Result<(), Error> {
        self.values
            .get(leaf.value()?)
            .ok_or_else(|| {
                Error::CorruptionError("source leaf missing from parallel preparation".into())
            })?
            .apply(leaf);
        Ok(())
    }
}
