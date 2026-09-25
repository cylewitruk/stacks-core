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

/// This module defines the methods for reading and inserting into a Trie
#[cfg(feature = "marf-read-bench-counters")]
use crate::chainstate::stacks::index::read_bench;

use std::ops::Deref;
use std::sync::Arc;

use rusqlite::Connection;
use sha2::Digest;
use stacks_common::types::chainstate::{TrieHash, TRIEHASH_ENCODED_SIZE};
use stacks_common::util::macros::is_trace;

use crate::chainstate::stacks::index::marf::MarfReadCtx;
use crate::chainstate::stacks::index::node::{
    clear_backptr, is_backptr, CursorNodeHandle, TrieCursor, TrieNode, TrieNode16, TrieNode256,
    TrieNode4, TrieNode48, TrieNodeID, TrieNodeType, TriePtr,
};
use crate::chainstate::stacks::index::scratch::MarfReadState;
use crate::chainstate::stacks::index::storage::{TrieHashCalculationMode, TrieStorageConnection};
use crate::chainstate::stacks::index::{
    bits, Error, MarfTrieId, NodeParking, NodePatching, NodePath, ReadNodeBacking, ReadTrieNode,
    TrieHasher, TrieLeaf, TrieReadStorage,
};

/// We don't actually instantiate a Trie, but we still need to pass a type parameter for the
/// storage implementation.
pub struct Trie {}

/// Fetch children hashes and compute the node's hash
fn get_nodetype_hash<T: MarfTrieId, Db: Deref<Target = Connection>>(
    storage: &mut TrieStorageConnection<T, Db>,
    node: &TrieNodeType,
) -> Result<TrieHash, Error> {
    if storage.hash_calculation_mode == TrieHashCalculationMode::Deferred {
        trace!(
            "get_nodetype_hash (deferred): hash {:?} = {:?} + ::children::",
            &TrieHash([0u8; 32]),
            node
        );
        return Ok(TrieHash([0u8; 32]));
    }

    let mut hasher = TrieHasher::new();

    node.write_consensus_bytes(storage, &mut hasher)
        .expect("IO Failure pushing to hasher.");

    storage.write_children_hashes(node, &mut hasher)?;

    let mut res = [0u8; 32];
    res.copy_from_slice(hasher.finalize().as_slice());

    let ret = TrieHash(res);

    trace!(
        "get_nodetype_hash: hash {:?} = {:?} + ::children::",
        &ret,
        node
    );
    Ok(ret)
}

impl Trie {
    /// Resolve a trie pointer to the block-local pointer it ultimately references.
    ///
    /// If `ptr` is a back-pointer, this opens the referenced block and returns the corresponding
    /// non-backptr `TriePtr`. If `ptr` is already local to the currently-open block, it is
    /// returned unchanged.
    pub fn resolve_backptr<T: MarfTrieId, R: TrieReadStorage<T> + ?Sized>(
        storage: &mut R,
        ptr: &TriePtr,
    ) -> Result<TriePtr, Error> {
        if !is_backptr(ptr.id()) {
            if ptr.id() == (TrieNodeID::Empty as u8) {
                return Err(Error::CorruptionError("ptr is empty".to_string()));
            }
            return Ok(*ptr);
        }

        #[cfg(feature = "marf-read-bench-counters")]
        read_bench::update(|c| c.backpointer_follows += 1);

        let back_block_hash = storage
            .get_block_from_local_id(ptr.back_block())
            .inspect_err(|_e| {
                test_debug!("Failed to get block from local ID {}", ptr.back_block());
            })?;

        storage
            .open_block_known_id(&back_block_hash, ptr.back_block())
            .inspect_err(|_e| {
                test_debug!(
                    "Failed to open block {} with id {}: {_e:?}",
                    &back_block_hash,
                    ptr.back_block(),
                );
            })?;

        let backptr = ptr.from_backptr();
        Ok(backptr)
    }

    /// Read the root node of the currently open trie.
    ///
    /// Probe the stored root header first so compatibility checks do not create an escaping borrow,
    /// and then perform a single real read of the root node.
    pub fn read_root<'a, T: MarfTrieId, R: TrieReadStorage<T> + ?Sized>(
        storage: &'a mut R,
        decode_scratch: &'a mut impl NodePatching,
    ) -> Result<ReadTrieNode<'a>, Error> {
        let root_ptr = storage.root_trieptr();
        let (stored_id, _root_hash) = storage.read_node_type_id(&root_ptr)?;
        if stored_id != TrieNodeID::Node256 && stored_id != TrieNodeID::Patch {
            return Err(Error::CorruptionError(format!(
                "Root ptr {:?} does not reference a node256 or patch root (found {:?})",
                root_ptr, stored_id
            )));
        }

        storage.read_node_with_state(&root_ptr, decode_scratch)
    }

    /// Follow a back-pointer back to a trie node in a previous trie.
    ///
    /// If the ptr is a back-pointer, then shunt to the block that contains the target node, read
    /// it, and update the cursor to record that we followed the back-pointer.
    ///
    /// If the ptr is not a back-pointer, read the node from this trie.
    /// s must point to this trie's block, not the block pointed at by the ptr.
    ///
    /// Either way, return the node view and the ptr to the node in the block in which it was
    /// found (it will _not_ be a back-pointer).
    pub fn walk_backptr<'a, T: MarfTrieId, R: TrieReadStorage<T>>(
        storage: &'a mut R,
        ptr: &TriePtr,
        cursor: &mut TrieCursor<T>,
        decode_scratch: &'a mut impl NodePatching,
    ) -> Result<(ReadTrieNode<'a>, TriePtr), Error> {
        let followed_backptr = is_backptr(ptr.id());
        let resolved_ptr = Trie::resolve_backptr(storage, ptr)?;
        let current_block = storage.get_cur_block();
        let read = storage.read_node_with_state(&resolved_ptr, decode_scratch)?;
        if followed_backptr {
            cursor.repair_backptr_step_backptr_deferred(&resolved_ptr, current_block);
        }
        Ok((read, resolved_ptr))
    }

    /// Read a node's children's hashes as a vector of TrieHashes.
    /// This only works for intermediate nodes and leafs (the latter of which have no children).
    ///
    /// See: TrieStorageConnection::write_children_hashes for more information on the hash contents.
    pub fn get_children_hashes<T: MarfTrieId, R: TrieReadStorage<T>>(
        storage: &mut R,
        node: &TrieNodeType,
    ) -> Result<Vec<TrieHash>, Error> {
        Trie::get_children_hashes_by_ptrs(storage, node.ptrs())
    }

    pub fn get_children_hashes_by_ptrs<T: MarfTrieId, R: TrieReadStorage<T> + ?Sized>(
        storage: &mut R,
        ptrs: &[TriePtr],
    ) -> Result<Vec<TrieHash>, Error> {
        let mut buffer = Vec::with_capacity(ptrs.len() * TRIEHASH_ENCODED_SIZE);
        storage.write_children_hashes_by_ptrs(ptrs, &mut buffer)?;
        assert_eq!(buffer.len() % TRIEHASH_ENCODED_SIZE, 0);

        let trie_hashes: Vec<_> = buffer
            .chunks_exact(TRIEHASH_ENCODED_SIZE)
            .map(|x| {
                TrieHash::from_bytes(x).expect("Failed to re-encode TrieHash from byte buffer")
            })
            .collect();

        Ok(trie_hashes)
    }

    /// Given an existing leaf, replace it with the new leaf.
    /// c must point to the node to replace.
    fn replace_leaf<T: MarfTrieId, Db: Deref<Target = Connection>>(
        storage: &mut TrieStorageConnection<T, Db>,
        cursor: &mut TrieCursor<T>,
        cur_leaf_path: &[u8],
        value: &mut TrieLeaf,
    ) -> Result<TriePtr, Error> {
        let leaf_ptr = cursor.ptr();
        if leaf_ptr.id() != TrieNodeID::Leaf as u8 {
            return Err(Error::CorruptionError(format!("Not a leaf: {leaf_ptr:?}")));
        }

        value.path = NodePath::from_slice(cur_leaf_path)
            .ok_or_else(|| Error::CorruptionError("Node path exceeds 32 bytes".into()))?;

        let leaf_hash = bits::get_leaf_hash(value);

        storage.write_node(leaf_ptr.try_ptr_into_u32()?, value, leaf_hash)?;

        trace!("replace_leaf: wrote {value:?} at {leaf_ptr:?}");
        Ok(cursor.ptr())
    }

    /// Append a leaf to the trie, and return the TriePtr to it.
    /// Do lazy expansion -- have the leaf store the trailing path to it.
    /// Return the TriePtr to the newly-written leaf
    fn append_leaf<T: MarfTrieId, Db: Deref<Target = Connection>>(
        storage: &mut TrieStorageConnection<T, Db>,
        cursor: &mut TrieCursor<T>,
        value: &mut TrieLeaf,
    ) -> Result<TriePtr, Error> {
        assert!(cursor.chr().is_some());

        let ptr = storage.last_ptr()?;
        let chr = cursor.chr().unwrap();

        value.path =
            NodePath::from_slice(cursor.path.as_bytes().get(cursor.index..).ok_or_else(|| {
                Error::CorruptionError("Cursor path shorter than cursor index".into())
            })?)
            .ok_or_else(|| Error::CorruptionError("Node path exceeds 32 bytes".into()))?;

        let leaf_hash = bits::get_leaf_hash(value);
        let leaf_ptr = TriePtr::new(TrieNodeID::Leaf as u8, chr, ptr.into());
        storage.write_node(ptr, value, leaf_hash)?;

        trace!("append_leaf: append {:?} at {:?}", value, &leaf_ptr);
        Ok(leaf_ptr)
    }

    /// Given a leaf and a cursor that is _not_ EOP, and a new leaf, create a node4 with the two
    /// leaves as its children and return its pointer.
    ///
    /// f must point to the start of cur_leaf.
    ///
    /// before:
    ///
    /// leaf[path=aabbccddeeff00112233]=123456
    ///
    /// insert (aabbccddeeff99887766, 98765)
    ///
    /// after:
    ///                          [00]leaf[path=112233]=123456
    ///                         /
    /// node4[path=aabbccddeeff]
    ///                         \
    ///                          [99]leaf[887766]=98765
    ///
    fn promote_leaf_to_node4<T: MarfTrieId, Db: Deref<Target = Connection>>(
        storage: &mut TrieStorageConnection<T, Db>,
        cursor: &mut TrieCursor<T>,
        cur_leaf_data: &mut TrieLeaf,
        new_leaf_data: &mut TrieLeaf,
    ) -> Result<TriePtr, Error> {
        // can only work if we're not at the end of the path, and the current node has a path
        assert!(!cursor.eop());
        assert!(!cur_leaf_data.path.is_empty());

        // switch from lazy expansion to path compression --
        // * the current and new leaves will have unique suffixes
        // * the node4 will have their shared prefix
        let cur_leaf_ptr = cursor.ptr();

        let node4_path = NodePath::from_slice(
            cur_leaf_data
                .path
                .get(..cursor.ntell())
                .ok_or_else(|| Error::CorruptionError("Node4 leaf path too short".into()))?,
        )
        .ok_or_else(|| Error::CorruptionError("Node path exceeds 32 bytes".into()))?;
        let node4_chr = cur_leaf_ptr.chr();

        let cur_leaf_chr = *cur_leaf_data
            .path
            .get(cursor.ntell())
            .ok_or_else(|| Error::CorruptionError("Current leaf path too short".into()))?;
        let cur_leaf_path_start = if cursor.ntell() >= cur_leaf_data.path.len() {
            cursor.ntell()
        } else {
            cursor.ntell() + 1
        };

        let cur_leaf_path = NodePath::from_slice(
            cur_leaf_data
                .path
                .get(cur_leaf_path_start..)
                .ok_or_else(|| Error::CorruptionError("Current leaf path too short".into()))?,
        )
        .ok_or_else(|| Error::CorruptionError("Node path exceeds 32 bytes".into()))?;

        // update current leaf (path changed) and save it
        let cur_leaf_disk_ptr = cur_leaf_ptr.ptr();
        let cur_leaf_new_ptr =
            TriePtr::new(TrieNodeID::Leaf as u8, cur_leaf_chr, cur_leaf_disk_ptr);

        assert!(cur_leaf_path.len() <= cur_leaf_data.path.len());
        let _sav_cur_leaf_data = cur_leaf_data.clone();
        cur_leaf_data.path = cur_leaf_path;
        storage.resolve_leaf_value(cur_leaf_data)?;
        let cur_leaf_hash = bits::get_leaf_hash(cur_leaf_data);

        // NOTE: this is safe since the current leaf's byte representation has gotten shorter
        storage.write_node(
            cur_leaf_ptr.try_ptr_into_u32()?,
            cur_leaf_data,
            cur_leaf_hash,
        )?;

        // append the new leaf at the end of the trie.
        let new_leaf_array_ptr = storage.last_ptr()?;
        let new_leaf_chr = cursor.path[cursor.tell()]; // NOTE: this is safe because !cursor.eop()
        new_leaf_data.path = NodePath::from_slice(
            &cursor.path[std::cmp::min(cursor.tell() + 1, cursor.path.len())..],
        )
        .ok_or_else(|| Error::CorruptionError("Node path exceeds 32 bytes".into()))?;
        let new_leaf_hash = bits::get_leaf_hash(new_leaf_data);

        // put new leaf at the end of this Trie
        let new_leaf_ptr = TriePtr::new(
            TrieNodeID::Leaf as u8,
            new_leaf_chr,
            new_leaf_array_ptr.into(),
        );

        storage.write_node(new_leaf_array_ptr, new_leaf_data, new_leaf_hash)?;

        // append the Node4 that points to both of them, and put it after the new leaf
        let mut node4_data = TrieNode4::new(&node4_path);

        assert!(node4_data.insert(&cur_leaf_new_ptr));
        assert!(node4_data.insert(&new_leaf_ptr));

        let node4_hash = bits::get_node_hash(
            &node4_data,
            &[
                cur_leaf_hash,
                new_leaf_hash,
                TrieHash::EMPTY,
                TrieHash::EMPTY,
            ],
            storage,
        );

        let node4 = TrieNodeType::Node4(node4_data);

        // append the node4 to the end of the trie
        let node4_array_ptr = storage.last_ptr()?;

        let ret = TriePtr::new(TrieNodeID::Node4 as u8, node4_chr, node4_array_ptr.into());
        storage.write_nodetype(node4_array_ptr, &node4, node4_hash)?;

        // update cursor to point to this node4 as the last-node-visited, and set the newly-created
        // ptr as the last ptr traversed (so the cursor still points to this leaf, but accurately
        // reflects the path taken to it).
        cursor.repair_retarget(&node4, &ret, &storage.get_cur_block());

        trace!(
            "Promoted {:?} to {:?}, {:?}, {:?}, new ptr = {:?}",
            _sav_cur_leaf_data,
            cur_leaf_data,
            &node4,
            new_leaf_data,
            &ret
        );
        Ok(ret)
    }

    #[cfg(test)]
    pub fn test_promote_leaf_to_node4<T: MarfTrieId, Db: Deref<Target = Connection>>(
        storage: &mut TrieStorageConnection<T, Db>,
        cursor: &mut TrieCursor<T>,
        cur_leaf_data: &mut TrieLeaf,
        new_leaf_data: &mut TrieLeaf,
    ) -> Result<TriePtr, Error> {
        Trie::promote_leaf_to_node4(storage, cursor, cur_leaf_data, new_leaf_data)
    }

    fn node_has_space(chr: u8, children: &[TriePtr]) -> bool {
        for child in children.iter().rev() {
            if child.is_empty() || child.chr() == chr {
                return true;
            }
        }
        return false;
    }

    /// Try to insert a leaf node into the given node, if there's space to do so and if the leaf
    /// belongs as a child of this node.
    /// If so, then save the leaf and its hash, update the node's ptrs and hash, and return the
    /// node's ptr and the node's new hash so we can update the trie.
    /// Return None if there's no space, or if the leaf doesn't share its full path prefix with the
    /// given node.
    ///
    /// ```text
    /// before:
    ///                          [00]nodeY[path=112233] ...
    ///                         /
    /// nodeX[path=aabbccddeeff]
    ///
    /// insert (aabbccddeeff99887766, 123456)
    ///
    ///                          [00]nodeY[path=112233] ...
    ///                         /
    /// nodeX[path=aabbccddeeff]
    ///                         \
    ///                          [99]leaf[path=887766]=123456
    /// ```
    fn try_attach_leaf<T: MarfTrieId, Db: Deref<Target = Connection>>(
        storage: &mut TrieStorageConnection<T, Db>,
        cursor: &mut TrieCursor<T>,
        leaf: &mut TrieLeaf,
        node: &mut TrieNodeType,
    ) -> Result<Option<TriePtr>, Error> {
        // can only do this if we're at the end of the node's path
        if !cursor.eonp(node) {
            // nope
            return Ok(None);
        }
        assert!(cursor.chr().is_some());
        assert!(!node.is_leaf());

        let has_space = Trie::node_has_space(cursor.chr().unwrap(), node.ptrs());
        if !has_space {
            // nope!
            return Ok(None);
        }

        // write leaf and update parent
        let leaf_ptr = Trie::append_leaf(storage, cursor, leaf)?;
        let inserted = node.insert(&leaf_ptr);

        assert!(inserted);

        let new_node_hash = get_nodetype_hash(storage, node)?;

        storage.write_nodetype(cursor.ptr().try_ptr_into_u32()?, node, new_node_hash)?;

        Ok(Some(cursor.ptr()))
    }

    /// Given a node and a leaf, attach the leaf.  Promote the intermediate node if necessary.
    /// Does the same thing as try_attach_leaf, but the node might get expanaded.  In this case, the
    /// new node will be appended and the old node will be leaked in the storage implementation
    /// (leakage isn't a concern in practice, because the "leak" will happen inside the TrieRAM
    /// storage implementation, which will be garbage-collected and dumped to disk once we finish
    /// all the block's inserts and call the TrieRAM's containing TrieStorageConnection instance's
    /// flush() method).
    fn insert_leaf<T: MarfTrieId, Db: Deref<Target = Connection>>(
        storage: &mut TrieStorageConnection<T, Db>,
        cursor: &mut TrieCursor<T>,
        leaf: &mut TrieLeaf,
        node: &mut TrieNodeType,
    ) -> Result<TriePtr, Error> {
        // can only do this if we're at the end of the node's path
        assert!(cursor.eonp(node));

        if let Some(ptr) = Trie::try_attach_leaf(storage, cursor, leaf, node)? {
            return Ok(ptr);
        }

        // not enough space -- need to promote node
        let mut new_node = match node {
            TrieNodeType::Leaf(_) => panic!("Cannot insert into a leaf"),
            TrieNodeType::Node256(_) => panic!("Somehow could not insert into a Node256"),
            TrieNodeType::Node4(ref data) => TrieNodeType::Node16(TrieNode16::from_node4(data)),
            TrieNodeType::Node16(ref data) => {
                TrieNodeType::Node48(Box::new(TrieNode48::from_node16(data)))
            }
            TrieNodeType::Node48(ref data) => {
                TrieNodeType::Node256(Box::new(TrieNode256::from_node48(data.as_ref())))
            }
        };

        let node_ptr = cursor.ptr();
        let leaf_ptr = Trie::append_leaf(storage, cursor, leaf)?;
        let inserted = new_node.insert(&leaf_ptr);
        assert!(inserted);

        let new_node_hash = get_nodetype_hash(storage, &new_node)?;

        // append this leaf to the Trie
        let new_node_array_ptr = storage.last_ptr()?;

        let ret = TriePtr::new(new_node.id(), node_ptr.chr(), new_node_array_ptr.into());
        storage.write_nodetype(new_node_array_ptr, &new_node, new_node_hash)?;

        // update the cursor so its path of nodes and ptrs accurately reflects that we would have
        // visited this leaf on its path.
        cursor.repair_retarget(&new_node, &ret, &storage.get_cur_block());
        Ok(ret)
    }

    #[cfg(test)]
    pub fn test_insert_leaf<T: MarfTrieId, Db: Deref<Target = Connection>>(
        storage: &mut TrieStorageConnection<T, Db>,
        cursor: &mut TrieCursor<T>,
        leaf: &mut TrieLeaf,
        node: &mut TrieNodeType,
    ) -> Result<TriePtr, Error> {
        Trie::insert_leaf(storage, cursor, leaf, node)
    }

    /// Given a node and a leaf to insert, break apart the node's compressed path into the shared
    /// prefix and the node- and leaf-specific segments, and add a Node4 at the break with the
    /// leaf.  Updates the given node and leaf, and returns the node4's ptr and hash.
    ///
    /// ```text
    /// before:
    ///                                        [00]nodeY[path=112233]...
    ///                                       /
    /// (parent)----[aa]nodeX[path=bbccddeeff]
    ///                                       \
    ///                                        [99]nodeZ[path=887766]...
    ///
    /// insert (aabbccffccbbaa, 123456)
    ///
    /// after:
    ///
    ///                                  [ff]leaf[path=ccbbaa]=123456
    ///                                 /
    /// (parent)----[aa]node4[path=bbcc]---[dd]nodeX[path=eeff]---[00]nodeY[path=112233]...
    ///                                                        \
    ///                                                         [99]nodeZ[path=887766]...
    ///
    /// ```
    /// (if nodeX was the root, then there is no parent, and the resulting node will be a node256
    /// instead of a node4).
    ///
    fn splice_leaf<T: MarfTrieId, Db: Deref<Target = Connection>>(
        storage: &mut TrieStorageConnection<T, Db>,
        cursor: &mut TrieCursor<T>,
        leaf: &mut TrieLeaf,
        node: &mut TrieNodeType,
    ) -> Result<TriePtr, Error> {
        assert!(!cursor.eop());
        assert!(!cursor.eonp(node));
        assert!(cursor.chr().is_some());
        assert!(!node.is_leaf());

        let node_path = node.path_bytes();
        let shared_path_prefix = NodePath::from_slice(
            node_path
                .get(0..cursor.ntell())
                .ok_or_else(|| Error::CorruptionError("Node path too short".into()))?,
        )
        .ok_or_else(|| Error::CorruptionError("Node path exceeds 32 bytes".into()))?;
        let leaf_path = NodePath::from_slice(&cursor.path[cursor.tell() + 1..])
            .ok_or_else(|| Error::CorruptionError("Node path exceeds 32 bytes".into()))?;
        let new_cur_node_path = NodePath::from_slice(
            node_path
                .get(cursor.ntell() + 1..)
                .ok_or_else(|| Error::CorruptionError("Node path too short".into()))?,
        )
        .ok_or_else(|| Error::CorruptionError("Node path exceeds 32 bytes".into()))?;
        let new_cur_node_chr = *node_path
            .get(cursor.ntell()) // chr for node-X post-update
            .ok_or_else(|| Error::CorruptionError("Node path too short".into()))?;

        // store leaf
        leaf.path = leaf_path;
        let leaf_chr = cursor.path[cursor.tell()];
        let leaf_disk_ptr = storage.last_ptr()?;
        let leaf_hash = bits::get_leaf_hash(leaf);
        let leaf_ptr = TriePtr::new(TrieNodeID::Leaf as u8, leaf_chr, leaf_disk_ptr.into());
        storage.write_node(leaf_disk_ptr, leaf, leaf_hash)?;

        // update current node (node-X) and make a new path and ptr for it
        let cur_node_cur_ptr = cursor.ptr();
        let new_cur_node_array_ptr = storage.last_ptr()?;
        let new_cur_node_ptr = TriePtr::new(
            cur_node_cur_ptr.id(),
            new_cur_node_chr,
            new_cur_node_array_ptr.into(),
        );

        node.set_path(new_cur_node_path);

        let new_cur_node_hash = get_nodetype_hash(storage, node)?;

        let mut new_node4 = TrieNode4::new(&shared_path_prefix);
        new_node4.insert(&leaf_ptr);
        new_node4.insert(&new_cur_node_ptr);

        let new_node_hash = bits::get_node_hash(
            &new_node4,
            &[
                leaf_hash,
                new_cur_node_hash,
                TrieHash::EMPTY,
                TrieHash::EMPTY,
            ],
            storage,
        );

        let (new_node_id, new_node) = if cursor.node_ptrs.len() == 1 {
            // we just split the compressed path in the root node,
            // so make sure the root node _stays_ as a node256.
            // Note that the hash we write here doesn't matter -- it'll get overwritten in the
            // subsequent call to update_root_hash()
            (
                TrieNodeID::Node256,
                TrieNode256::from_node4(&new_node4).as_trie_node_type(),
            )
        } else {
            (TrieNodeID::Node4, TrieNodeType::Node4(new_node4))
        };

        // store node4 where node-X used to be
        storage.write_nodetype(
            cur_node_cur_ptr.try_ptr_into_u32()?,
            &new_node,
            new_node_hash,
        )?;

        // store node-X at the end
        storage.write_nodetype(new_cur_node_array_ptr, node, new_cur_node_hash)?;

        let ret = TriePtr::new(
            new_node_id as u8,
            cur_node_cur_ptr.chr(),
            cur_node_cur_ptr.ptr(),
        );
        cursor.repair_retarget(&new_node, &ret, &storage.get_cur_block());

        trace!("splice_leaf: node-X' at {ret:?}");
        Ok(ret)
    }

    /// Read the node at the cursor's current position, resolving deferred (parked or
    /// persisted) handles into a borrowable `ReadTrieNode`.
    ///
    /// # Transient metadata
    ///
    /// For **parked** cursor nodes the returned `ReadTrieNode` has **no `transient_meta`**.
    /// Calling `into_owned_node()` on it will therefore produce a `TrieNodeType` with
    /// zeroed `cowptr` / `patch_depth` / `last_patch_source`.
    ///
    /// This is safe because current callers (`add_value`, `splice_leaf`, etc.)
    /// only use the resulting owned node for **new-slot** TrieRAM mutations — never
    /// to overwrite the original COW-backed slot whose metadata matters for
    /// `dump_compressed_consume`.
    ///
    /// **Do not** use the returned node for in-place overwrite or flush logic without
    /// first ensuring transient metadata is preserved.
    fn read_deferred_cursor_node<'a, T: MarfTrieId, Db: Deref<Target = Connection>>(
        storage: &'a mut TrieStorageConnection<T, Db>,
        cursor: &TrieCursor<T>,
        decode_scratch: &'a mut (impl NodePatching + NodeParking),
    ) -> Result<ReadTrieNode<'a>, Error> {
        let node_handle = cursor.node_handle().ok_or_else(|| {
            Error::CorruptionError("Cursor is uninitialized or missing survival handle".to_string())
        })?;

        let (node_ptr, node_block) = match node_handle {
            CursorNodeHandle::Parked(parked_handle) => {
                return Ok(ReadTrieNode::from_state_borrowed(
                    decode_scratch.get_parked_ref(*parked_handle),
                    None,
                ));
            }
            CursorNodeHandle::Persisted { ptr, block_hash } => (*ptr, block_hash.clone()),
        };

        if storage.get_cur_block() == node_block {
            return storage.read_node_with_state(&node_ptr, decode_scratch);
        }

        let (cur_block, cur_block_id) = storage.get_cur_block_and_id();
        storage.open_block(&node_block)?;
        let read = storage.read_node_with_state(&node_ptr, decode_scratch)?;
        let ReadTrieNode {
            hash,
            patch_depth,
            backing,
            transient_meta,
            ..
        } = read;

        enum DeferredNodeAction {
            ParkCurrent,
            ParkOwned(TrieNodeType),
        }

        let action = match backing {
            ReadNodeBacking::VolatileDecoded(_) => DeferredNodeAction::ParkCurrent,
            ReadNodeBacking::PersistedDecoded(node) => {
                DeferredNodeAction::ParkOwned(node.to_owned_node())
            }
            ReadNodeBacking::PersistedBytes(node) => {
                let node = node.decode_node()?;
                DeferredNodeAction::ParkOwned(node)
            }
            ReadNodeBacking::Owned(node) => DeferredNodeAction::ParkOwned(node),
        };

        let parked_handle = match action {
            DeferredNodeAction::ParkCurrent => decode_scratch.park_current_node()?,
            DeferredNodeAction::ParkOwned(node) => decode_scratch.park_owned_node(node),
        };

        storage.open_block_maybe_id(&cur_block, cur_block_id)?;
        let mut result =
            ReadTrieNode::from_state_borrowed(decode_scratch.get_parked_ref(parked_handle), hash)
                .with_patch_depth(patch_depth);
        if let Some(meta) = transient_meta {
            result = result.with_transient_meta(meta);
        }
        Ok(result)
    }

    /// Add a new value to the Trie at the location pointed at by the cursor.
    /// Returns a ptr to be inserted into the last node visited by the cursor.
    pub fn add_value<T: MarfTrieId, Db: Deref<Target = Connection>>(
        storage: &mut TrieStorageConnection<T, Db>,
        cursor: &mut TrieCursor<T>,
        value: &mut TrieLeaf,
        decode_scratch: &mut (impl NodePatching + NodeParking),
    ) -> Result<TriePtr, Error> {
        enum MutationTarget {
            ReplaceLeaf(NodePath),
            MutateNode(TrieNodeType),
        }

        let mutation_target = {
            let current_node_read = match cursor.node() {
                Some(node) => ReadTrieNode::from_owned(node, None),
                None => Trie::read_deferred_cursor_node(storage, cursor, decode_scratch)?,
            };

            if cursor.eop() {
                if let Some(cur_leaf) = current_node_read.as_leaf()? {
                    MutationTarget::ReplaceLeaf(NodePath::from_slice(cur_leaf.path).ok_or_else(
                        || Error::CorruptionError("Node path exceeds 32 bytes".into()),
                    )?)
                } else {
                    MutationTarget::MutateNode(current_node_read.into_owned_node()?.0)
                }
            } else {
                MutationTarget::MutateNode(current_node_read.into_owned_node()?.0)
            }
        };

        if cursor.eop() {
            match mutation_target {
                MutationTarget::ReplaceLeaf(cur_leaf_path) => {
                    Trie::replace_leaf(storage, cursor, &cur_leaf_path, value)
                }
                MutationTarget::MutateNode(mut node) => {
                    Trie::insert_leaf(storage, cursor, value, &mut node)
                }
            }
        } else {
            // didn't reach the end of the path, so we're on an intermediate node
            // or we're somewhere in the path of a leaf.
            // Either tack the leaf on (possibly promoting the node), or splice the leaf in.
            let mut node = match mutation_target {
                MutationTarget::ReplaceLeaf(_) => {
                    return Err(Error::CorruptionError(
                        "Leaf replacement target encountered before end of path".to_string(),
                    ));
                }
                MutationTarget::MutateNode(node) => node,
            };
            if cursor.eonp(&node) {
                trace!(
                    "eop = {}, eonp = {}, c = {cursor:?}, node = {node:?}",
                    cursor.eop(),
                    cursor.eonp(&node),
                );
                Trie::insert_leaf(storage, cursor, value, &mut node)
            } else {
                match node {
                    TrieNodeType::Leaf(ref mut data) => {
                        Trie::promote_leaf_to_node4(storage, cursor, data, value)
                    }
                    _ => Trie::splice_leaf(storage, cursor, value, &mut node),
                }
            }
        }
    }

    /// Perform the reads, lookups, etc. for computing the ancestor byte vector.
    /// This method _does not_ restore the previously open block on failure, the caller will do that.
    fn inner_get_trie_ancestor_hashes_bytes<T: MarfTrieId, R: TrieReadStorage<T> + ?Sized>(
        storage: &mut R,
        cursor_opt: &mut Option<TrieCursor<T>>,
        decode_scratch: &mut impl NodePatching,
    ) -> Result<Vec<TrieHash>, Error> {
        let mut nested_cursor = None;
        let mut read_ctx =
            MarfReadCtx::new(storage, cursor_opt, &mut nested_cursor, decode_scratch);
        let cur_block_header = read_ctx.storage().get_cur_block();
        // definitely enough space for the foreseeable future
        //    ancestor depth _cannot_ exceed 32 -- 2^32 > max size of u32
        //    (which is how we are identifying blocks).
        let mut hash_buf = Vec::with_capacity(33);

        // here is where some mind-bending things begin to happen.
        //   we want to find the block at a given _height_. but how to do so?
        //   use the data stored already in the MARF.
        let cur_block_height = read_ctx
            .get_block_height_miner_tip(&cur_block_header, &cur_block_header)
            .map_err(|e| match e {
                Error::NotFoundError => Error::CorruptionError(format!(
                    "Could not obtain block height for block {}: not found",
                    &cur_block_header
                )),
                x => x,
            })?
            .ok_or_else(|| {
                Error::CorruptionError(format!(
                    "Could not obtain block height for block {}: got None",
                    &cur_block_header
                ))
            })?;

        let mut log_depth = 0;
        while log_depth < 32 && (1u32 << log_depth) <= cur_block_height {
            let ancestor_height = cur_block_height - (1u32 << log_depth);
            if let Some(root) = read_ctx.storage().indexed_ancestor_root(
                &cur_block_header,
                cur_block_height,
                ancestor_height,
            )? {
                hash_buf.push(root);
                log_depth += 1;
                continue;
            }
            #[cfg(feature = "commit-residency-diagnostics")]
            let resolve_span = stacks_profiler::diagnostic_span!("Seal: Resolve ancestor identity");
            let prev_block_header = read_ctx
                .get_block_at_height_with_current_height(
                    ancestor_height,
                    &cur_block_header,
                    cur_block_height,
                )?
                .ok_or_else(|| {
                    Error::CorruptionError(format!(
                        "Could not obtain block hash at block height {}",
                        ancestor_height
                    ))
                })?;

            #[cfg(feature = "commit-residency-diagnostics")]
            drop(resolve_span);
            #[cfg(feature = "commit-residency-diagnostics")]
            let _root_span = stacks_profiler::diagnostic_span!("Seal: Retrieve ancestor root");
            let ancestor_hash = if read_ctx
                .storage()
                .squash_height()
                .is_some_and(|h| ancestor_height <= h)
            {
                read_ctx
                    .storage()
                    .squashed_block_root_hash_by_height(ancestor_height)?
                    .ok_or_else(|| {
                        Error::CorruptionError(format!(
                            "Could not obtain squashed root hash at height {ancestor_height}"
                        ))
                    })?
            } else {
                read_ctx.storage().open_block(&prev_block_header)?;
                let root_ptr = read_ctx.storage().root_trieptr();
                read_ctx.storage().read_node_hash(&root_ptr)?
            };

            trace!(
                "Include root hash {} from block {} in ancestor #{}",
                ancestor_hash,
                prev_block_header,
                1u32 << log_depth
            );

            hash_buf.push(ancestor_hash);

            log_depth += 1;
        }

        Ok(hash_buf)
    }

    /// Calculate the byte vector of the ancestor root hashes of this trie.
    /// `storage` must point to the block that contains the trie's root.
    pub fn get_trie_ancestor_hashes_bytes<T: MarfTrieId, R: TrieReadStorage<T> + ?Sized>(
        storage: &mut R,
        cursor_opt: &mut Option<TrieCursor<T>>,
        decode_scratch: &mut impl NodePatching,
    ) -> Result<Arc<[TrieHash]>, Error> {
        let (cur_block_header, cur_block_id) = storage.get_cur_block_and_id();
        if let Some(cached_ancestor_hashes_bytes) =
            storage.check_cached_ancestor_hashes_bytes(&cur_block_header)
        {
            Ok(cached_ancestor_hashes_bytes)
        } else {
            let result =
                Trie::inner_get_trie_ancestor_hashes_bytes(storage, cursor_opt, decode_scratch);
            let result = result.map(Arc::<[TrieHash]>::from);
            if let Ok(ref result) = result {
                storage.set_cached_ancestor_hashes_bytes(&cur_block_header, Arc::clone(result));
            }

            // restore
            storage.open_block_maybe_id(&cur_block_header, cur_block_id)?;
            result
        }
    }

    /// Calculate the bytes of the ancestor root hashes of this trie, plus the current trie's root.
    /// Return the resulting sequence of hashes a a single byte buffer.
    pub fn get_trie_root_ancestor_hashes_bytes<T: MarfTrieId, Db: Deref<Target = Connection>>(
        storage: &mut TrieStorageConnection<T, Db>,
        children_root_hash: &TrieHash,
    ) -> Result<Vec<TrieHash>, Error> {
        trace!(
            "Calculate Trie hash from root node digest {:?}",
            children_root_hash
        );
        let mut cursor = None;
        let mut decode_scratch = MarfReadState::new();
        let ancestor_hashes =
            Trie::get_trie_ancestor_hashes_bytes(storage, &mut cursor, &mut decode_scratch)?;
        let mut ancestor_bytes = Vec::with_capacity(ancestor_hashes.len() + 1);
        ancestor_bytes.insert(0, *children_root_hash);
        ancestor_bytes.extend_from_slice(&ancestor_hashes);

        trace!(
            "Trie ancestor bytes for root hash calculation: {:?}",
            &ancestor_bytes
        );

        Ok(ancestor_bytes)
    }

    /// Calculate the root hash of the trie (i.e. the hash for the root node) by including both the
    /// digest of this Trie, as well as a geometric sequence of prior Trie root hashes as far back
    /// as we can go.
    pub fn get_trie_root_hash<T: MarfTrieId, Db: Deref<Target = Connection>>(
        storage: &mut TrieStorageConnection<T, Db>,
        children_root_hash: &TrieHash,
        cursor_opt: &mut Option<TrieCursor<T>>,
        decode_scratch: &mut impl NodePatching,
    ) -> Result<TrieHash, Error> {
        let ancestor_hashes =
            Trie::get_trie_ancestor_hashes_bytes(storage, cursor_opt, decode_scratch)?;
        if ancestor_hashes.is_empty() {
            Ok(*children_root_hash)
        } else {
            let mut hashes = Vec::with_capacity(ancestor_hashes.len() + 1);
            hashes.push(*children_root_hash);
            hashes.extend_from_slice(&ancestor_hashes);
            Ok(TrieHash::from_data_array(&hashes))
        }
    }

    /// Unwind a [`TrieCursor`] to update the Merkle root of the trie.
    ///
    /// The root hashes of each trie form a Merkle skip-list -- the hash of trie `i` is calculated
    /// from the hash of its children, plus the hash tries `i-1`, `i-2`, `i-4`, `i-8`, ..., `i-2**j`, ...
    ///
    /// This is required for Merkle proofs to work (specifically, the shunt proofs).
    fn recalculate_root_hash<T: MarfTrieId, Db: Deref<Target = Connection>>(
        storage: &mut TrieStorageConnection<T, Db>,
        cursor: &TrieCursor<T>,
        update_skiplist: bool,
        ancestor_cursor: &mut Option<TrieCursor<T>>,
        decode_scratch: &mut impl NodePatching,
    ) -> Result<(), Error> {
        assert!(!cursor.node_ptrs.is_empty());

        let mut ptrs = cursor.node_ptrs.clone();
        trace!("update_root_hash: ptrs = {ptrs:?}");
        let mut child_ptr = ptrs.pop().unwrap();

        if ptrs.is_empty() {
            // Root node was already updated by trie operations, but it will have the wrong hash.
            // We need to "fix" the root node so it mixes in its ancestor hashes.
            trace!("Fix up root node so it mixes in its ancestor hashes");

            if child_ptr != storage.root_trieptr() {
                return Err(Error::CorruptionError(
                    "Only ptr is not the root".to_string(),
                ));
            }

            // O(1) swap from TrieRAM - avoids the heap allocation of into_owned_node().
            let child_index = child_ptr.try_ptr_into_u32()?;
            let (node, cur_hash) = storage.take_ram_node(child_index)?;

            if !node.is_node256() {
                storage.restore_ram_node(child_index, node, cur_hash)?;
                return Err(Error::CorruptionError(
                    "Only ptr was not a node256".to_string(),
                ));
            }

            let my_hash = match get_nodetype_hash(storage, &node) {
                Ok(my_hash) => my_hash,
                Err(e) => {
                    storage.restore_ram_node(child_index, node, cur_hash)?;
                    return Err(e);
                }
            };

            // Restore before ancestor hash lookup (which traverses the trie).
            storage.restore_ram_node(child_index, node, my_hash)?;

            let h = if update_skiplist {
                trace!("Update root skiplist");
                Trie::get_trie_root_hash(storage, &my_hash, ancestor_cursor, decode_scratch)?
            } else {
                trace!("Not updating root skiplist");
                my_hash
            };

            if cfg!(test) && is_trace() {
                let node_hash = my_hash;
                let node_debug = format!("root@{child_ptr:?}");
                let _ = Trie::get_trie_root_ancestor_hashes_bytes(storage, &node_hash)
                    .map(|_hs| {
                        storage.clear_cached_ancestor_hashes_bytes();
                        trace!("update_root_hash: Updated {node_debug} with {child_ptr:?} from {cur_hash} to {node_hash} + {:?} = {h} (fixed root)", &_hs.get(1..));
                    });
            }

            debug!("Next root hash is {h} (update_skiplist={update_skiplist})");

            // Update the final hash if it differs from my_hash (skiplist case).
            if h != my_hash {
                let (node, _) = storage.take_ram_node(child_index)?;
                storage.restore_ram_node(child_index, node, h)?;
            }
        } else {
            while let Some(ptr) = ptrs.pop() {
                if is_backptr(ptr.id()) {
                    // This node was not altered, but instead queued to the cursor as part of walking a
                    // backptr skiplist. Do nothing.
                    continue;
                }

                // O(1) swap from TrieRAM - avoids the heap allocation of into_owned_node().
                let ptr_index = ptr.try_ptr_into_u32()?;
                let (mut node, cur_hash) = storage.take_ram_node(ptr_index)?;
                assert!(!node.is_leaf());

                // This child_ptr MUST be in the node.
                let updated = node.replace(&child_ptr);
                if !updated {
                    trace!("FAILED TO UPDATE {node:?} WITH {child_ptr:?}: {cursor:?}");
                    storage.restore_ram_node(ptr_index, node, cur_hash)?;
                    return Err(Error::CorruptionError(
                        "Failed to update parent node during root hash recalculation".to_string(),
                    ));
                }

                let is_root_node256 = node.is_node256() && ptr == storage.root_trieptr();
                let content_hash = match get_nodetype_hash(storage, &node) {
                    Ok(content_hash) => content_hash,
                    Err(e) => {
                        storage.restore_ram_node(ptr_index, node, cur_hash)?;
                        return Err(e);
                    }
                };

                if !is_root_node256 {
                    trace!(
                        "update_root_hash: Updated node with {child_ptr:?} from {cur_hash:?} to {content_hash:?}"
                    );
                    storage.restore_ram_node(ptr_index, node, content_hash)?;
                } else {
                    // Root Node256: restore with temp hash so ancestor traversal works,
                    // then compute final hash and fix up.
                    storage.restore_ram_node(ptr_index, node, TrieHash([0; 32]))?;

                    let h = if update_skiplist {
                        Trie::get_trie_root_hash(
                            storage,
                            &content_hash,
                            ancestor_cursor,
                            decode_scratch,
                        )?
                    } else {
                        content_hash
                    };

                    if cfg!(test) && is_trace() {
                        let _ = Trie::get_trie_root_ancestor_hashes_bytes(storage, &content_hash)
                                    .map(|_hs| {
                                        storage.clear_cached_ancestor_hashes_bytes();
                                        trace!("update_root_hash: Updated node with {child_ptr:?} from {cur_hash:?} to {content_hash:?} + {:?} = {h:?}", &_hs.get(1..));
                                    });
                    }

                    debug!("Next root hash is {h} (update_skiplist={update_skiplist})");

                    let (node, _) = storage.take_ram_node(ptr_index)?;
                    storage.restore_ram_node(ptr_index, node, h)?;
                }

                child_ptr = ptr;
                child_ptr.id = clear_backptr(child_ptr.id);
            }
        }

        // Must be at the root
        assert_eq!(child_ptr, storage.root_trieptr());
        Ok(())
    }

    pub fn update_root_hash<T: MarfTrieId, Db: Deref<Target = Connection>>(
        storage: &mut TrieStorageConnection<T, Db>,
        cursor: &TrieCursor<T>,
        decode_scratch: &mut impl NodePatching,
    ) -> Result<(), Error> {
        let mut ancestor_cursor = None;
        Trie::recalculate_root_hash(
            storage,
            cursor,
            storage.hash_calculation_mode != TrieHashCalculationMode::Deferred,
            &mut ancestor_cursor,
            decode_scratch,
        )
    }

    pub fn update_root_node_hash<T: MarfTrieId, Db: Deref<Target = Connection>>(
        storage: &mut TrieStorageConnection<T, Db>,
        cursor: &TrieCursor<T>,
        decode_scratch: &mut impl NodePatching,
    ) -> Result<(), Error> {
        let mut ancestor_cursor = None;
        Trie::recalculate_root_hash(storage, cursor, false, &mut ancestor_cursor, decode_scratch)
    }
}

#[cfg(test)]
pub mod testing {
    use super::*;

    impl Trie {
        pub fn test_try_attach_leaf<T: MarfTrieId, Db: Deref<Target = Connection>>(
            storage: &mut TrieStorageConnection<T, Db>,
            cursor: &mut TrieCursor<T>,
            leaf: &mut TrieLeaf,
            node: &mut TrieNodeType,
        ) -> Result<Option<TriePtr>, Error> {
            Trie::try_attach_leaf(storage, cursor, leaf, node)
        }

        pub fn test_splice_leaf<T: MarfTrieId, Db: Deref<Target = Connection>>(
            storage: &mut TrieStorageConnection<T, Db>,
            cursor: &mut TrieCursor<T>,
            leaf: &mut TrieLeaf,
            node: &mut TrieNodeType,
        ) -> Result<TriePtr, Error> {
            Trie::splice_leaf(storage, cursor, leaf, node)
        }
    }
}
