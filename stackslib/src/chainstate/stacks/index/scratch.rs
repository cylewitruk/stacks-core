// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020-2026 Stacks Open Internet Foundation
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

use crate::chainstate::stacks::index::node::{
    ParkedNodeHandle, TrieLeafRef, TrieNode, TrieNode16, TrieNode256, TrieNode4, TrieNode48,
    TrieNodeID, TrieNodePatch, TrieNodeRef, TrieNodeTransientMeta, TrieNodeType, TriePtr,
};
use crate::chainstate::stacks::index::{
    Error, NodeDecodeScratch, NodeParking, NodePatching, PatchChainEntry, TrieLeaf,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CurrentSlot {
    Node4,
    Node16,
    Node48,
    Node256,
    Leaf,
    Patch,
    Owned,
}

#[derive(Debug, Default)]
pub struct MarfReadState {
    node4: Option<TrieNode4>,
    node16: Option<TrieNode16>,
    node48: Option<TrieNode48>,
    node256: Option<TrieNode256>,
    leaf: Option<TrieLeaf>,
    patch: Option<TrieNodePatch>,
    node_bytes: Vec<u8>,
    /// Reusable buffer for patch chain accumulation. Taken by `take_patch_chain_buf` and restored
    /// by `restore_patch_chain_buf` to avoid per-read allocation in the patch-chasing loop.
    patch_chain_buf: Vec<PatchChainEntry>,
    owned: Option<TrieNodeType>,
    parked: Vec<TrieNodeType>,
    current_slot: Option<CurrentSlot>,
}

impl MarfReadState {
    pub fn new() -> Self {
        Self::default()
    }

    fn empty_patch() -> TrieNodePatch {
        TrieNodePatch {
            ptr: TriePtr::default(),
            ptr_diff: Vec::new(),
        }
    }

    fn take_node_bytes(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.node_bytes)
    }

    fn take_patch_chain_buf(&mut self) -> Vec<PatchChainEntry> {
        let mut buf = std::mem::take(&mut self.patch_chain_buf);
        buf.clear();
        buf
    }

    fn restore_patch_chain_buf(&mut self, mut buf: Vec<PatchChainEntry>) {
        let current_patch_capacity = self.patch.as_ref().map_or(0, |p| p.ptr_diff.capacity());
        let reusable_patch_index = buf
            .iter()
            .enumerate()
            .max_by_key(|(_, entry)| entry.patch.ptr_diff.capacity())
            .and_then(|(index, entry)| {
                (entry.patch.ptr_diff.capacity() > current_patch_capacity).then_some(index)
            });

        if let Some(index) = reusable_patch_index {
            let mut entry = buf.swap_remove(index);
            entry.patch.ptr = TriePtr::default();
            entry.patch.ptr_diff.clear();
            self.patch = Some(entry.patch);
        }

        buf.clear();
        self.patch_chain_buf = buf;
    }

    fn restore_node_bytes(&mut self, node_bytes: Vec<u8>) {
        self.node_bytes = node_bytes;
    }

    fn clear_current_node(&mut self) {
        self.current_slot = None;
        self.owned = None;
    }

    fn clear_parked_nodes(&mut self) {
        self.parked.clear();
    }

    fn get_parked_ref(&self, parked_handle: ParkedNodeHandle) -> TrieNodeRef<'_> {
        let node = self
            .parked
            .get(parked_handle.slot())
            .expect("BUG: decode scratch missing parked node slot");
        TrieNodeRef::from(node)
    }

    fn park_owned_node(&mut self, node: TrieNodeType) -> ParkedNodeHandle {
        self.parked.push(node);
        ParkedNodeHandle::new(self.parked.len() - 1)
    }

    fn park_current_node(&mut self) -> Result<ParkedNodeHandle, Error> {
        let node = match self.current_slot.take().ok_or_else(|| {
            Error::CorruptionError("decode scratch has no current node to park".to_string())
        })? {
            CurrentSlot::Node4 => TrieNodeType::Node4(
                self.node4
                    .take()
                    .expect("BUG: decode scratch lost node4 before parking"),
            ),
            CurrentSlot::Node16 => TrieNodeType::Node16(
                self.node16
                    .take()
                    .expect("BUG: decode scratch lost node16 before parking"),
            ),
            CurrentSlot::Node48 => TrieNodeType::Node48(Box::new(
                self.node48
                    .take()
                    .expect("BUG: decode scratch lost node48 before parking"),
            )),
            CurrentSlot::Node256 => TrieNodeType::Node256(Box::new(
                self.node256
                    .take()
                    .expect("BUG: decode scratch lost node256 before parking"),
            )),
            CurrentSlot::Leaf => TrieNodeType::Leaf(
                self.leaf
                    .take()
                    .expect("BUG: decode scratch lost leaf before parking"),
            ),
            CurrentSlot::Owned => self
                .owned
                .take()
                .expect("BUG: decode scratch lost owned node before parking"),
            CurrentSlot::Patch => {
                return Err(Error::CorruptionError(
                    "Cannot park patch nodes in decode scratch".to_string(),
                ));
            }
        };

        Ok(self.park_owned_node(node))
    }

    fn get_ref(&self) -> TrieNodeRef<'_> {
        match self
            .current_slot
            .expect("BUG: decode scratch has no current node")
        {
            CurrentSlot::Node4 => {
                let n = self.node4.as_ref().unwrap();
                TrieNodeRef::Node4 {
                    path: n.path.as_slice(),
                    ptrs: &n.ptrs,
                }
            }
            CurrentSlot::Node16 => {
                let n = self.node16.as_ref().unwrap();
                TrieNodeRef::Node16 {
                    path: n.path.as_slice(),
                    ptrs: &n.ptrs,
                }
            }
            CurrentSlot::Node48 => {
                let n = self.node48.as_ref().unwrap();
                TrieNodeRef::Node48 {
                    path: n.path.as_slice(),
                    indexes: n.indexes(),
                    ptrs: &n.ptrs,
                }
            }
            CurrentSlot::Node256 => {
                let n = self.node256.as_ref().unwrap();
                TrieNodeRef::Node256 {
                    path: n.path.as_slice(),
                    ptrs: &n.ptrs,
                }
            }
            CurrentSlot::Leaf => {
                let n = self.leaf.as_ref().unwrap();
                TrieNodeRef::Leaf(TrieLeafRef {
                    path: n.path.as_slice(),
                    data: n.data.as_ref(),
                    extent: n.extent,
                    inline: n.inline.as_ref(),
                })
            }
            CurrentSlot::Owned => {
                let n = self.owned.as_ref().unwrap();
                TrieNodeRef::from(n)
            }
            CurrentSlot::Patch => {
                unreachable!("BUG: patch nodes are never stored in decode scratch")
            }
        }
    }

    fn transient_meta(&self) -> Option<TrieNodeTransientMeta> {
        match self.current_slot? {
            CurrentSlot::Node4 => {
                let n = self.node4.as_ref().expect("BUG: decode scratch lost node4");
                Some(n.meta)
            }
            CurrentSlot::Node16 => {
                let n = self
                    .node16
                    .as_ref()
                    .expect("BUG: decode scratch lost node16");
                Some(n.meta)
            }
            CurrentSlot::Node48 => {
                let n = self
                    .node48
                    .as_ref()
                    .expect("BUG: decode scratch lost node48");
                Some(n.meta)
            }
            CurrentSlot::Node256 => {
                let n = self
                    .node256
                    .as_ref()
                    .expect("BUG: decode scratch lost node256");
                Some(n.meta)
            }
            CurrentSlot::Leaf | CurrentSlot::Patch => None,
            CurrentSlot::Owned => self.owned.as_ref().map(TrieNodeTransientMeta::from_node),
        }
    }

    fn store(&mut self, node: TrieNodeType) -> TrieNodeRef<'_> {
        self.current_slot = Some(CurrentSlot::Owned);
        self.owned = Some(node);
        TrieNodeRef::from(self.owned.as_ref().expect("BUG: decode scratch lost node"))
    }

    fn patch(&self) -> &TrieNodePatch {
        self.patch
            .as_ref()
            .expect("BUG: decode scratch lost patch node")
    }

    fn take_patch(&mut self) -> TrieNodePatch {
        self.patch
            .take()
            .expect("BUG: decode scratch lost patch node")
    }

    fn apply_patches_in_place(
        &mut self,
        patches: &[PatchChainEntry],
        cur_block_id: u32,
    ) -> Result<(), Error> {
        match self
            .current_slot
            .expect("BUG: decode scratch has no current node")
        {
            CurrentSlot::Node4 => {
                let node = self.node4.as_mut().expect("BUG: decode scratch lost node4");
                Self::apply_patches_to_node(node, patches, cur_block_id)
            }
            CurrentSlot::Node16 => {
                let node = self
                    .node16
                    .as_mut()
                    .expect("BUG: decode scratch lost node16");
                Self::apply_patches_to_node(node, patches, cur_block_id)
            }
            CurrentSlot::Node48 => {
                let node = self
                    .node48
                    .as_mut()
                    .expect("BUG: decode scratch lost node48");
                Self::apply_patches_to_node(node, patches, cur_block_id)
            }
            CurrentSlot::Node256 => {
                let node = self
                    .node256
                    .as_mut()
                    .expect("BUG: decode scratch lost node256");
                Self::apply_patches_to_node(node, patches, cur_block_id)
            }
            CurrentSlot::Owned => {
                match self.owned.as_mut().expect("decode scratch lost owned node") {
                    TrieNodeType::Node4(node) => {
                        Self::apply_patches_to_node(node, patches, cur_block_id)
                    }
                    TrieNodeType::Node16(node) => {
                        Self::apply_patches_to_node(node, patches, cur_block_id)
                    }
                    TrieNodeType::Node48(node) => {
                        Self::apply_patches_to_node(node.as_mut(), patches, cur_block_id)
                    }
                    TrieNodeType::Node256(node) => {
                        Self::apply_patches_to_node(node.as_mut(), patches, cur_block_id)
                    }
                    TrieNodeType::Leaf(_) => {
                        Err(Error::CorruptionError("Cannot patch a leaf".into()))
                    }
                }
            }
            CurrentSlot::Leaf | CurrentSlot::Patch => Err(Error::CorruptionError(
                "Cannot apply patches to non-intermediate decode scratch node".to_string(),
            )),
        }
    }

    fn apply_patches_to_node<N: TrieNode + std::fmt::Debug>(
        node: &mut N,
        patches: &[PatchChainEntry],
        cur_block_id: u32,
    ) -> Result<(), Error> {
        for entry in patches.iter() {
            if !entry.patch.apply_to(node, entry.block_id, cur_block_id) {
                return Err(Error::CorruptionError(
                    "Failed to apply patches to node".to_string(),
                ));
            }
        }
        let meta = node
            .meta_mut()
            .expect("BUG: branch decode scratch node missing transient metadata");
        meta.patch_depth += patches.len();
        meta.last_patch_source = patches
            .last()
            .map(|entry| (entry.block_id, entry.ptr))
            .or(meta.last_patch_source);
        Ok(())
    }

    fn decode_node4_from_slice(&mut self, bytes: &[u8]) -> Result<usize, Error> {
        self.clear_current_node();
        let node = self.node4.get_or_insert_with(TrieNode4::empty);
        let consumed = node.load_from_slice(bytes)?;
        self.current_slot = Some(CurrentSlot::Node4);
        Ok(consumed)
    }

    fn decode_node16_from_slice(&mut self, bytes: &[u8]) -> Result<usize, Error> {
        self.clear_current_node();
        let node = self.node16.get_or_insert_with(TrieNode16::empty);
        let consumed = node.load_from_slice(bytes)?;
        self.current_slot = Some(CurrentSlot::Node16);
        Ok(consumed)
    }

    fn decode_node48_from_slice(&mut self, bytes: &[u8]) -> Result<usize, Error> {
        self.clear_current_node();
        let node = self.node48.get_or_insert_with(TrieNode48::empty);
        let consumed = node.load_from_slice(bytes)?;
        self.current_slot = Some(CurrentSlot::Node48);
        Ok(consumed)
    }

    fn decode_node256_from_slice(&mut self, bytes: &[u8]) -> Result<usize, Error> {
        self.clear_current_node();
        let node = self.node256.get_or_insert_with(TrieNode256::empty);
        let consumed = node.load_from_slice(bytes)?;
        self.current_slot = Some(CurrentSlot::Node256);
        Ok(consumed)
    }

    fn decode_leaf_from_slice(&mut self, bytes: &[u8]) -> Result<usize, Error> {
        self.clear_current_node();
        let node = self.leaf.get_or_insert_with(TrieLeaf::empty);
        let consumed = node.load_from_slice(bytes)?;
        self.current_slot = Some(CurrentSlot::Leaf);
        Ok(consumed)
    }

    fn decode_patch_from_slice(&mut self, bytes: &[u8]) -> Result<usize, Error> {
        self.clear_current_node();
        let node = self.patch.get_or_insert_with(Self::empty_patch);
        let consumed = node.load_from_slice(bytes)?;
        self.current_slot = Some(CurrentSlot::Patch);
        Ok(consumed)
    }

    fn decode_node_from_slice(&mut self, id: TrieNodeID, bytes: &[u8]) -> Result<usize, Error> {
        match id {
            TrieNodeID::Node4 => self.decode_node4_from_slice(bytes),
            TrieNodeID::Node16 => self.decode_node16_from_slice(bytes),
            TrieNodeID::Node48 => self.decode_node48_from_slice(bytes),
            TrieNodeID::Node256 => self.decode_node256_from_slice(bytes),
            TrieNodeID::Leaf | TrieNodeID::ValueLeaf => self.decode_leaf_from_slice(bytes),
            _ => Err(Error::CorruptionError(format!(
                "Cannot decode node type {id:?} from slice"
            ))),
        }
    }
}

impl NodeDecodeScratch for MarfReadState {
    fn take_node_bytes(&mut self) -> Vec<u8> {
        MarfReadState::take_node_bytes(self)
    }

    fn restore_node_bytes(&mut self, bytes: Vec<u8>) {
        MarfReadState::restore_node_bytes(self, bytes)
    }

    fn decode_node_from_parts(
        &mut self,
        id: TrieNodeID,
        marker: u8,
        payload: &[u8],
    ) -> Result<usize, Error> {
        self.clear_current_node();
        macro_rules! decode {
            ($slot:ident, $ty:ty, $tag:ident) => {{
                let node = self.$slot.get_or_insert_with(<$ty>::empty);
                let consumed = node.load_from_parts(marker, payload)?;
                self.current_slot = Some(CurrentSlot::$tag);
                Ok(consumed)
            }};
        }
        match id {
            TrieNodeID::Node4 => decode!(node4, TrieNode4, Node4),
            TrieNodeID::Node16 => decode!(node16, TrieNode16, Node16),
            TrieNodeID::Node48 => decode!(node48, TrieNode48, Node48),
            TrieNodeID::Node256 => decode!(node256, TrieNode256, Node256),
            TrieNodeID::Leaf | TrieNodeID::ValueLeaf => decode!(leaf, TrieLeaf, Leaf),
            _ => Err(Error::CorruptionError("Invalid scratch node type".into())),
        }
    }

    fn decode_patch_from_parts(&mut self, marker: u8, payload: &[u8]) -> Result<usize, Error> {
        self.clear_current_node();
        let patch = self.patch.get_or_insert_with(Self::empty_patch);
        let consumed = patch.load_from_parts(marker, payload)?;
        self.current_slot = Some(CurrentSlot::Patch);
        Ok(consumed)
    }

    fn decode_node_from_slice(&mut self, id: TrieNodeID, bytes: &[u8]) -> Result<usize, Error> {
        MarfReadState::decode_node_from_slice(self, id, bytes)
    }

    fn decode_patch_from_slice(&mut self, bytes: &[u8]) -> Result<usize, Error> {
        MarfReadState::decode_patch_from_slice(self, bytes)
    }

    fn get_ref(&self) -> TrieNodeRef<'_> {
        MarfReadState::get_ref(self)
    }

    fn transient_meta(&self) -> Option<TrieNodeTransientMeta> {
        MarfReadState::transient_meta(self)
    }

    fn patch(&self) -> &TrieNodePatch {
        MarfReadState::patch(self)
    }

    fn take_patch(&mut self) -> TrieNodePatch {
        MarfReadState::take_patch(self)
    }

    fn take_patch_chain_buf(&mut self) -> Vec<PatchChainEntry> {
        MarfReadState::take_patch_chain_buf(self)
    }

    fn restore_patch_chain_buf(&mut self, buf: Vec<PatchChainEntry>) {
        MarfReadState::restore_patch_chain_buf(self, buf)
    }

    fn store(&mut self, node: TrieNodeType) -> TrieNodeRef<'_> {
        MarfReadState::store(self, node)
    }

    fn clear_current_node(&mut self) {
        MarfReadState::clear_current_node(self)
    }
}

impl NodeParking for MarfReadState {
    fn park_current_node(&mut self) -> Result<ParkedNodeHandle, Error> {
        MarfReadState::park_current_node(self)
    }

    fn park_owned_node(&mut self, node: TrieNodeType) -> ParkedNodeHandle {
        MarfReadState::park_owned_node(self, node)
    }

    fn get_parked_ref(&self, handle: ParkedNodeHandle) -> TrieNodeRef<'_> {
        MarfReadState::get_parked_ref(self, handle)
    }

    fn clear_parked_nodes(&mut self) {
        MarfReadState::clear_parked_nodes(self)
    }
}

impl NodePatching for MarfReadState {
    fn apply_patches_in_place(
        &mut self,
        patches: &[PatchChainEntry],
        cur_block_id: u32,
    ) -> Result<(), Error> {
        MarfReadState::apply_patches_in_place(self, patches, cur_block_id)
    }
}
