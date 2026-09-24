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

#[cfg(feature = "marf-read-bench-counters")]
use crate::chainstate::stacks::index::read_bench;

use std::collections::{HashMap, VecDeque};
use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::ops::Deref;
use std::path::Path;
use std::sync::Arc;
use std::{fmt, fs, io, mem};

use rusqlite::{Connection, OpenFlags, Transaction};
use sha2::Digest;
use stacks_common::codec::StacksMessageCodec;
use stacks_common::types::chainstate::{TrieHash, TRIEHASH_ENCODED_SIZE};
use stacks_common::util::hash::to_hex;

use crate::chainstate::stacks::index::bits::{
    is_inline_child_ptr, reserved_root_size, resolve_inline_child_offsets,
};
use crate::chainstate::stacks::index::blob_layout::{self, BlobHeader};
use crate::chainstate::stacks::index::cache::*;
use crate::chainstate::stacks::index::direct_hash_index::{DirectHashGuard, DirectHashIndex};
use crate::chainstate::stacks::index::file::{MappedTrieItem, TrieFile, TrieFileNodeHashReader};
use crate::chainstate::stacks::index::marf::MARFOpenOpts;
use crate::chainstate::stacks::index::node::{
    is_backptr, set_backptr, TrieCowPtr, TrieNode, TrieNodeID, TrieNodePatch, TrieNodeRef,
    TrieNodeTransientMeta, TrieNodeType, TriePtr,
};
use crate::chainstate::stacks::index::packed_branch;
use crate::chainstate::stacks::index::record::{NodeRecordFormat, RecordContext};
use crate::chainstate::stacks::index::result_cache::{
    ResultCache, ResultCacheControl, ResultCacheGuard, DEFAULT_RESULT_CACHE_CAPACITY,
};
use crate::chainstate::stacks::index::scratch::MarfReadState;
use crate::chainstate::stacks::index::trie::Trie;
use crate::chainstate::stacks::index::{
    bits, trie_sql, BlockMap, ClarityMarfTrieId, Error, MARFValue, MarfDataEntry, MarfTrieId,
    NodePatching, NodePath, PatchChainEntry, ReadTrieItem, ReadTrieItemKind, ReadTrieNode,
    TrieHasher, TrieLeaf, TrieReadStorage, ValueExtentResolver, MAX_PATCH_DEPTH,
};
use crate::util_lib::db::{
    sql_pragma, sqlite_open, tx_begin_immediate, Error as db_error, SQLITE_MARF_PAGE_SIZE,
    SQLITE_MMAP_SIZE,
};

/// A trait for reading the hash of a node into a given Write impl, given the pointer to a node in
/// a trie.
pub trait NodeHashReader {
    fn read_node_hash<W: Write>(&mut self, ptr: &TriePtr, w: &mut W) -> Result<(), Error>;
}

impl<T: MarfTrieId> BlockMap for TrieFileStorage<T> {
    type TrieId = T;

    fn get_block_hash(&self, id: u32) -> Result<T, Error> {
        DirectHashIndex::block_hash(self.data.direct_hash_index.as_ref(), &self.db, id)
    }

    fn get_block_hash_caching(&mut self, id: u32) -> Result<&T, Error> {
        self.cache.get_block_hash_caching(id, |id| {
            DirectHashIndex::block_hash(self.data.direct_hash_index.as_ref(), &self.db, id)
        })
    }

    fn is_block_hash_cached(&self, id: u32) -> bool {
        self.cache.ref_block_hash(id).is_some()
    }

    fn get_block_id(&self, block_hash: &T) -> Result<u32, Error> {
        trie_sql::get_block_identifier(&self.db, block_hash)
    }

    fn get_block_id_caching(&mut self, block_hash: &T) -> Result<u32, Error> {
        get_block_id_caching_impl(self.data.unconfirmed, &mut self.cache, &self.db, block_hash)
    }
}

impl<T: MarfTrieId, Db: Deref<Target = Connection>> BlockMap for TrieStorageConnection<'_, T, Db> {
    type TrieId = T;

    fn get_block_hash(&self, id: u32) -> Result<T, Error> {
        DirectHashIndex::block_hash(self.data.direct_hash_index.as_ref(), &self.db, id)
    }

    fn get_block_hash_caching<'a>(&'a mut self, id: u32) -> Result<&'a T, Error> {
        self.cache.get_block_hash_caching(id, |id| {
            DirectHashIndex::block_hash(self.data.direct_hash_index.as_ref(), &self.db, id)
        })
    }

    fn is_block_hash_cached(&self, id: u32) -> bool {
        self.cache.ref_block_hash(id).is_some()
    }

    fn get_block_id(&self, block_hash: &T) -> Result<u32, Error> {
        trie_sql::get_block_identifier(&self.db, block_hash)
    }

    fn get_block_id_caching(&mut self, block_hash: &T) -> Result<u32, Error> {
        get_block_id_caching_impl(self.data.unconfirmed, self.cache, &self.db, block_hash)
    }
}

impl<T: MarfTrieId> BlockMap for TrieSqlHashMapCursor<'_, T> {
    type TrieId = T;

    fn get_block_hash(&self, id: u32) -> Result<T, Error> {
        DirectHashIndex::block_hash(self.direct_hash_index, self.db, id)
    }

    fn get_block_hash_caching(&mut self, id: u32) -> Result<&T, Error> {
        self.cache.get_block_hash_caching(id, |id| {
            DirectHashIndex::block_hash(self.direct_hash_index, self.db, id)
        })
    }

    fn is_block_hash_cached(&self, id: u32) -> bool {
        self.cache.ref_block_hash(id).is_some()
    }

    fn get_block_id(&self, block_hash: &T) -> Result<u32, Error> {
        trie_sql::get_block_identifier(self.db, block_hash)
    }

    fn get_block_id_caching(&mut self, block_hash: &T) -> Result<u32, Error> {
        get_block_id_caching_impl(self.unconfirmed, self.cache, self.db, block_hash)
    }
}

impl<T: MarfTrieId> BlockMap for ReopenedTrieStorageConnection<'_, T> {
    type TrieId = T;

    fn get_block_hash(&self, id: u32) -> Result<T, Error> {
        DirectHashIndex::block_hash(self.data.direct_hash_index.as_ref(), self.db, id)
    }

    fn get_block_hash_caching(&mut self, id: u32) -> Result<&T, Error> {
        self.cache.get_block_hash_caching(id, |id| {
            DirectHashIndex::block_hash(self.data.direct_hash_index.as_ref(), self.db, id)
        })
    }

    fn is_block_hash_cached(&self, id: u32) -> bool {
        self.cache.ref_block_hash(id).is_some()
    }

    fn get_block_id(&self, block_hash: &T) -> Result<u32, Error> {
        trie_sql::get_block_identifier(self.db, block_hash)
    }

    fn get_block_id_caching(&mut self, block_hash: &T) -> Result<u32, Error> {
        get_block_id_caching_impl(self.unconfirmed(), &mut self.cache, self.db, block_hash)
    }
}

enum FlushOptions<'a, T: MarfTrieId> {
    CurrentHeader,
    NewHeader(&'a T),
    MinedTable(&'a T),
    UnconfirmedTable,
}

impl<T: MarfTrieId> fmt::Display for FlushOptions<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            FlushOptions::CurrentHeader => write!(f, "self"),
            FlushOptions::MinedTable(bhh) => write!(f, "{}.mined", bhh),
            FlushOptions::NewHeader(bhh) => write!(f, "{}", bhh),
            FlushOptions::UnconfirmedTable => write!(f, "self.unconfirmed"),
        }
    }
}

/// Uncommitted storage state to be flushed
#[derive(Clone)]
pub enum UncommittedState<T: MarfTrieId> {
    /// read-write
    RW(TrieRAM<T>),
    /// read-only, sealed, with root hash
    Sealed(TrieRAM<T>, TrieHash),
}

impl<T: MarfTrieId> UncommittedState<T> {
    /// Clear the contents
    pub fn format(&mut self) -> Result<(), Error> {
        match self {
            UncommittedState::RW(ref mut trie_ram) => trie_ram.format(),
            _ => {
                panic!("FATAL: cannot format a sealed TrieRAM");
            }
        }
    }

    /// Get a hint as to how big the uncommitted state is
    pub fn size_hint(&self) -> usize {
        match self {
            UncommittedState::RW(ref trie_ram) => trie_ram.size_hint(),
            UncommittedState::Sealed(ref trie_ram, _) => trie_ram.size_hint(),
        }
    }

    /// Get an immutable reference to the inner TrieRAM
    pub fn trie_ram_ref(&self) -> &TrieRAM<T> {
        match self {
            UncommittedState::RW(ref trie_ram) => trie_ram,
            UncommittedState::Sealed(ref trie_ram, ..) => trie_ram,
        }
    }

    /// Get a mutable reference to the inner TrieRAM
    pub fn trie_ram_mut(&mut self) -> &mut TrieRAM<T> {
        match self {
            UncommittedState::RW(ref mut trie_ram) => trie_ram,
            UncommittedState::Sealed(ref mut trie_ram, ..) => trie_ram,
        }
    }

    /// Read a node's hash
    pub fn read_node_hash(&self, ptr: &TriePtr) -> Result<TrieHash, Error> {
        self.trie_ram_ref().read_node_hash(ptr)
    }

    /// Read a node's hash and the node itself by reference.
    pub fn read_node(&mut self, ptr: &TriePtr) -> Result<ReadTrieNode<'_>, Error> {
        self.trie_ram_mut().read_node(ptr)
    }

    /// Write a node and its hash to a particular slot in the TrieRAM.
    /// Panics if the UncommittedState is sealed already.
    pub fn write_nodetype(
        &mut self,
        node_array_ptr: u32,
        node: &TrieNodeType,
        hash: TrieHash,
    ) -> Result<(), Error> {
        match self {
            UncommittedState::RW(ref mut trie_ram) => {
                trie_ram.write_nodetype(node_array_ptr, node, hash)
            }
            UncommittedState::Sealed(..) => {
                panic!("FATAL: tried to write to a sealed TrieRAM");
            }
        }
    }

    /// Take a node+hash out of the TrieRAM, leaving a placeholder.
    /// Panics if the UncommittedState is sealed.
    pub fn take_node(&mut self, ptr: u32) -> Result<(TrieNodeType, TrieHash), Error> {
        match self {
            UncommittedState::RW(ref mut trie_ram) => trie_ram.take_node(ptr),
            UncommittedState::Sealed(..) => {
                panic!("FATAL: tried to take from a sealed TrieRAM");
            }
        }
    }

    /// Restore a node+hash into a TrieRAM slot.
    /// Panics if the UncommittedState is sealed.
    pub fn restore_node(
        &mut self,
        ptr: u32,
        node: TrieNodeType,
        hash: TrieHash,
    ) -> Result<(), Error> {
        match self {
            UncommittedState::RW(ref mut trie_ram) => trie_ram.restore_node(ptr, node, hash),
            UncommittedState::Sealed(..) => {
                panic!("FATAL: tried to restore to a sealed TrieRAM");
            }
        }
    }

    /// Write a node hash to a particular slot in the TrieRAM.
    /// Panics of the UncommittedState is sealed already.
    pub fn write_node_hash(&mut self, node_array_ptr: u32, hash: TrieHash) -> Result<(), Error> {
        match self {
            UncommittedState::RW(ref mut trie_ram) => {
                trie_ram.write_node_hash(node_array_ptr, hash)
            }
            UncommittedState::Sealed(..) => {
                panic!("FATAL: tried to write to a sealed TrieRAM");
            }
        }
    }

    /// Get the last pointer (i.e. last slot) of the TrieRAM
    pub fn last_ptr(&mut self) -> Result<u32, Error> {
        self.trie_ram_mut().last_ptr()
    }

    /// Seal the TrieRAM.  Calculate its root hash and prevent any subsequent writes from
    /// succeeding.
    fn seal(
        self,
        storage_tx: &mut TrieStorageTransaction<T>,
    ) -> Result<UncommittedState<T>, Error> {
        match self {
            UncommittedState::RW(mut trie_ram) => {
                let root_hash = trie_ram.inner_seal(storage_tx)?;
                Ok(UncommittedState::Sealed(trie_ram, root_hash))
            }
            _ => {
                panic!("FATAL: tried to re-seal a sealed TrieRAM");
            }
        }
    }

    /// Dump the TrieRAM to the given writeable `f`.  If the TrieRAM is not sealed yet, then seal
    /// it first and then dump it.
    fn dump<F: Write + Seek>(
        self,
        storage_tx: &mut TrieStorageTransaction<T>,
        f: &mut F,
        bhh: &T,
    ) -> Result<(), Error> {
        if self.trie_ram_ref().block_header != *bhh {
            error!("Failed to dump {:?}: not the current block", bhh);
            return Err(Error::NotFoundError);
        }

        match self {
            UncommittedState::RW(mut trie_ram) => {
                // seal it first, then dump it
                debug!("Seal and dump trie for {}", bhh);
                trie_ram.inner_seal_dump(storage_tx)?;
                trie_ram.dump_consume(f, storage_tx.data.record_context.format)?;
                Ok(())
            }
            UncommittedState::Sealed(trie_ram, _rh) => {
                // already sealed
                debug!(
                    "Dump already-sealed trie for {} (root hash was {})",
                    bhh, _rh
                );
                trie_ram.dump_consume(f, storage_tx.data.record_context.format)?;
                Ok(())
            }
        }
    }

    /// Dump the TrieRAM to the given writeable `f`.  If the TrieRAM is not sealed yet, then seal
    /// it first and then dump it.  The nodes in the trie will be compressed before writing.
    fn dump_compressed<F: Write + Seek>(
        self,
        storage_tx: &mut TrieStorageTransaction<T>,
        f: &mut F,
        bhh: &T,
    ) -> Result<(), Error> {
        if self.trie_ram_ref().block_header != *bhh {
            error!("Failed to dump {:?}: not the current block", bhh);
            return Err(Error::NotFoundError);
        }

        match self {
            UncommittedState::RW(mut trie_ram) => {
                // seal it first, then dump it
                debug!("Seal and dump trie for {}", bhh);
                trie_ram.inner_seal_dump(storage_tx)?;
                trie_ram.dump_compressed_consume(storage_tx, f)?;
                Ok(())
            }
            UncommittedState::Sealed(trie_ram, _rh) => {
                // already sealed
                debug!(
                    "Dump already-sealed trie for {} (root hash was {})",
                    bhh, _rh
                );
                trie_ram.dump_compressed_consume(storage_tx, f)?;
                Ok(())
            }
        }
    }

    #[cfg(test)]
    pub fn print_to_stderr(&self) {
        self.trie_ram_ref().print_to_stderr()
    }
}

/// In-RAM trie storage.
/// Used by TrieFileStorage to buffer the next trie being built.
///
/// Pointers in `TrieRAM` are index-based, not disk-offset-based:
/// `TriePtr::ptr()` is treated as an in-memory node index into `data`, and
/// traversal/indexing paths are intentionally bounded to `u32`.
/// Large `u64` byte offsets are only materialized when serializing this trie
/// to persistent storage (see `dump_consume`/`dump_compressed_consume`).
#[derive(Clone)]
pub struct TrieRAM<T: MarfTrieId> {
    /// Lookup results scoped to the lifetime of this mutable trie.
    result_cache: ResultCache,
    data: Vec<(TrieNodeType, TrieHash)>,
    block_header: T,
    readonly: bool,

    /// Number of node writes, used to estimate buffer capacity.
    write_count: u64,

    total_bytes: usize,

    /// does this TrieRAM represent data temporarily moved out of another TrieRAM?
    is_moved: bool,

    parent: T,
    /// False after raw or reserved-key writes that may change logical ancestry mappings.
    ancestry_safe: bool,
}

pub enum DumpPtr {
    Normal(u32),
    Patch(u32, [u8; 32], TrieNodePatch),
}

impl DumpPtr {
    pub fn ptr(&self) -> u32 {
        match self {
            Self::Normal(ptr) => *ptr,
            Self::Patch(ptr, ..) => *ptr,
        }
    }

    pub fn hash_bytes(&self) -> Option<&[u8; 32]> {
        match self {
            Self::Normal(..) => None,
            Self::Patch(_, bytes, _) => Some(bytes),
        }
    }

    pub fn patch(&self) -> Option<&TrieNodePatch> {
        match self {
            Self::Normal(..) => None,
            Self::Patch(_, _, patch) => Some(patch),
        }
    }

    pub fn hash_and_patch(&self) -> Option<(&[u8; 32], &TrieNodePatch)> {
        match self {
            Self::Normal(..) => None,
            Self::Patch(_, hash_bytes, patch) => Some((hash_bytes, patch)),
        }
    }

    pub fn patch_mut(&mut self) -> Option<&mut TrieNodePatch> {
        match self {
            Self::Normal(..) => None,
            Self::Patch(_, _, patch) => Some(patch),
        }
    }
}

/// Compute a V4 root reservation by monotone refinement in actual write order.
fn packed_root_reservation<'a>(
    data: &[(TrieNodeType, TrieHash)],
    count: usize,
    entry: impl Fn(usize) -> (u32, Option<&'a TrieNodePatch>),
) -> Result<u64, Error> {
    let size = |index: usize,
                resolve: &mut dyn FnMut(&TriePtr) -> Result<u64, Error>|
     -> Result<u64, Error> {
        let (id, patch) = entry(index);
        if let Some(patch) = patch {
            let mut length = TRIEHASH_ENCODED_SIZE + patch.size();
            for ptr in &patch.ptr_diff {
                let mut mapped = *ptr;
                mapped.ptr = resolve(ptr)?;
                length = length - ptr.compressed_size() + mapped.compressed_size();
            }
            return Ok(length as u64);
        }
        let (node, _) = data
            .get(id as usize)
            .ok_or_else(|| Error::CorruptionError("Invalid packed dump node".into()))?;
        Ok(if node.is_leaf() {
            NodeRecordFormat::TypeFirstV4.node_len(node, true)
        } else {
            1 + TRIEHASH_ENCODED_SIZE + packed_branch::payload_len_with_targets(node, resolve)?
        } as u64)
    };
    let mut reserved = size(0, &mut |ptr| {
        Ok(if is_inline_child_ptr(ptr) {
            u64::MAX
        } else {
            ptr.ptr
        })
    })?;
    let mut offsets = vec![0u64; data.len()];
    loop {
        offsets.fill(0);
        let mut cursor = (blob_layout::ROOT_NODE_OFFSET as u64)
            .checked_add(reserved)
            .ok_or(Error::OverflowError)?;
        for index in (1..count).rev() {
            let length = size(index, &mut |ptr| resolve_dump_target(ptr, &offsets))?;
            let (id, _) = entry(index);
            *offsets.get_mut(id as usize).ok_or(Error::OverflowError)? = cursor;
            cursor = cursor.checked_add(length).ok_or(Error::OverflowError)?;
        }
        let next = size(0, &mut |ptr| resolve_dump_target(ptr, &offsets))?;
        if next == reserved {
            return Ok(reserved);
        }
        if next > reserved {
            return Err(Error::CorruptionError(
                "Packed root reservation increased".into(),
            ));
        }
        reserved = next;
    }
}

/// Resolve only already placed inline children during size planning.
fn resolve_dump_target(ptr: &TriePtr, offsets: &[u64]) -> Result<u64, Error> {
    if !is_inline_child_ptr(ptr) {
        return Ok(ptr.ptr);
    }
    offsets
        .get(ptr.try_ptr_into_usize()?)
        .copied()
        .filter(|offset| *offset != 0)
        .ok_or_else(|| Error::CorruptionError("Unplaced child in packed dump plan".into()))
}

/// Trie in RAM without the serialization overhead
impl<T: MarfTrieId> TrieRAM<T> {
    pub fn new(block_header: &T, capacity_hint: usize, parent: &T) -> TrieRAM<T> {
        TrieRAM {
            result_cache: ResultCache::default(),
            data: Vec::with_capacity(capacity_hint),
            block_header: block_header.clone(),
            readonly: false,

            write_count: 0,

            total_bytes: 0,

            is_moved: false,

            parent: parent.clone(),
            ancestry_safe: true,
        }
    }

    /// Inner method to instantiate a TrieRAM from existing Trie data.
    fn from_data(block_header: T, data: Vec<(TrieNodeType, TrieHash)>, parent: T) -> TrieRAM<T> {
        TrieRAM {
            result_cache: ResultCache::default(),
            data,
            block_header,
            readonly: false,

            write_count: 0,

            total_bytes: 0,

            is_moved: false,

            parent,
            ancestry_safe: false,
        }
    }

    /// Instantiate a `TrieRAM` from this `TrieRAM`'s `data` and `block_header`.  This TrieRAM will
    /// have its data set to an empty list.  The new TrieRAM will have its `is_moved` field set to
    /// `true`.
    /// The purpose of this method is to temporarily "re-instate" a `TrieRAM` into a
    /// `TrieFileStorage` while it is being flushed, so that all of the `TrieFileStorage` methods
    /// will continue to work on it.
    ///
    /// The result cache follows the data, retaining its shared invalidation signal.
    /// Do not call directly; instead, use `with_reinstated_data()`.
    fn move_to(&mut self) -> TrieRAM<T> {
        TrieRAM {
            result_cache: mem::take(&mut self.result_cache),
            data: mem::take(&mut self.data),
            block_header: self.block_header.clone(),
            readonly: self.readonly,

            write_count: self.write_count,

            total_bytes: self.total_bytes,

            is_moved: true,

            parent: self.parent.clone(),
            ancestry_safe: self.ancestry_safe,
        }
    }

    /// Restore the data and result cache from a temporarily reinstated `TrieRAM`.
    /// The given `TrieRAM` *must* have been created with a prior call to `self.move_to()`.
    ///
    /// Do not call directly; instead use `with_reinstated_data()`.
    fn replace_from(&mut self, other: TrieRAM<T>) {
        assert!(!self.is_moved);
        assert!(other.is_moved);
        assert_eq!(self.block_header, other.block_header);
        self.ancestry_safe = other.ancestry_safe;
        self.result_cache = other.result_cache;
        self.data = other.data;
    }

    /// Temporarily re-instate this TrieRAM's data as the `uncommitted_writes` field in a given storage
    /// connection, run the closure `f` with it, and then restore the original `uncommitted_writes` data.
    /// This method does not compose -- calling `with_reinstated_data` within the given closure `f`
    /// will lead to a runtime panic.
    ///
    /// The purpose of this method is to calculate the trie root hash from a trie that is in the
    /// process of being flushed.
    fn with_reinstated_data<F, R>(&mut self, storage: &mut TrieStorageTransaction<T>, f: F) -> R
    where
        F: FnOnce(&mut TrieRAM<T>, &mut TrieStorageTransaction<T>) -> R,
    {
        // do NOT call this function within another instance of this function.  Only tears and
        // misery would result.
        assert!(
            !self.is_moved,
            "FATAL: tried to move a TrieRAM after it had been moved"
        );

        let old_uncommitted_writes = storage.data.uncommitted_writes.take();

        let moved_trie_ram = self.move_to();
        storage.data.uncommitted_writes = Some((
            self.block_header.clone(),
            UncommittedState::RW(moved_trie_ram),
        ));

        let result = f(self, storage);

        // restore
        let (_, moved_extended) = storage
            .data
            .uncommitted_writes
            .take()
            .expect("FATAL: unable to retake moved TrieRAM");

        match moved_extended {
            UncommittedState::RW(trie_ram) => {
                self.replace_from(trie_ram);
            }
            _ => {
                unreachable!()
            }
        };

        storage.data.uncommitted_writes = old_uncommitted_writes;
        result
    }

    /// Calculate the MARF root hash from a trie root hash.
    ///
    /// This hashes the trie root hash with a geometric series of prior trie hashes.
    fn calculate_marf_root_hash(
        &mut self,
        storage: &mut TrieStorageTransaction<T>,
        root_hash: &TrieHash,
    ) -> TrieHash {
        let (cur_block_hash, cur_block_id) = storage.get_cur_block_and_id();

        storage.data.set_block(self.block_header.clone(), None);

        let mut cursor = None;
        let mut decode_scratch = MarfReadState::new();
        let marf_root_hash =
            Trie::get_trie_root_hash(storage, root_hash, &mut cursor, &mut decode_scratch)
                .expect("FATAL: unable to calculate MARF root hash from moved TrieRAM");

        test_debug!(
            "cur_block_hash = {}, cur_block_id = {:?}, self.block_header = {}, have last extended? {}, root_hash: {}, trie_root_hash = {}",
            &cur_block_hash,
            &cur_block_id,
            &self.block_header,
            storage.data.uncommitted_writes.is_some(),
            root_hash,
            &marf_root_hash
        );

        storage.data.set_block(cur_block_hash, cur_block_id);

        marf_root_hash
    }

    /// Calculate and store the MARF root hash, as well as any necessary intermediate nodes.
    ///
    /// This should only be used when in deferred hashing mode.
    fn inner_seal_marf(
        &mut self,
        storage_tx: &mut TrieStorageTransaction<T>,
    ) -> Result<TrieHash, Error> {
        // find trie root hash
        debug!("Calculate trie root hash");
        #[cfg(feature = "commit-residency-diagnostics")]
        let _nodes = stacks_profiler::diagnostic_span!("Seal: Node hashes");
        let root_trie_hash = self.calculate_node_hashes(storage_tx, 0)?;
        #[cfg(feature = "commit-residency-diagnostics")]
        drop(_nodes);
        #[cfg(feature = "commit-residency-diagnostics")]
        let _ancestors = stacks_profiler::diagnostic_span!("Seal: Ancestor hashes");

        // find marf root hash -- the hash of the trie root node hash, and the hashes of the
        // geometric series of ancestor tries.  Because the trie is already in the process of
        // being flushed, we have to temporarily reinstate its data into `storage_tx` so we can
        // use it to walk down the various MARF paths needed to query ancestor tries.
        let marf_root_hash = self.with_reinstated_data(storage_tx, |moved_trieram, storage| {
            debug!("Calculate marf root hash");
            moved_trieram.calculate_marf_root_hash(storage, &root_trie_hash)
        });

        if TrieHashCalculationMode::All == storage_tx.hash_calculation_mode {
            // If we are doing both eager and deferred hashing (i.e. via a test), then verify
            // that we get the same marf hash either way.
            let (_, expected_root_hash) = self.get_nodetype(0)?;
            assert_eq!(expected_root_hash, &marf_root_hash);
        }

        // need to store this hash too, since we deferred calculation
        self.write_node_hash(0, marf_root_hash)?;
        Ok(marf_root_hash)
    }

    /// Get the trie root hash of the trie ram, and update all nodes' root hashes if we're in
    /// deferred hash mode.  Returns the resulting MARF root.  This is part of the seal operation.
    fn inner_seal(
        &mut self,
        storage_tx: &mut TrieStorageTransaction<T>,
    ) -> Result<TrieHash, Error> {
        if TrieHashCalculationMode::Deferred == storage_tx.hash_calculation_mode
            || TrieHashCalculationMode::All == storage_tx.hash_calculation_mode
        {
            self.inner_seal_marf(storage_tx)
        } else {
            // already available
            let marf_root_hash =
                TrieRAM::read_node_hash(self, &TriePtr::new(TrieNodeID::Node256 as u8, 0, 0))?;

            Ok(marf_root_hash)
        }
    }

    #[cfg(test)]
    pub fn test_inner_seal(
        &mut self,
        storage_tx: &mut TrieStorageTransaction<T>,
    ) -> Result<TrieHash, Error> {
        self.inner_seal(storage_tx)
    }

    /// Seal a trie ram while in the process of dumping it.  If the storage's hash calculation mode
    /// is Deferred, then this updates all the node hashes as well and stores the new node hash.
    /// Otherwise, this is a no-op.
    /// This part of the seal operation.
    fn inner_seal_dump(&mut self, storage_tx: &mut TrieStorageTransaction<T>) -> Result<(), Error> {
        if TrieHashCalculationMode::Deferred == storage_tx.hash_calculation_mode
            || TrieHashCalculationMode::All == storage_tx.hash_calculation_mode
        {
            let marf_root_hash = self.inner_seal_marf(storage_tx)?;
            debug!("Deferred root hash calculation is {}", &marf_root_hash);
        }
        Ok(())
    }

    /// Hash children before their parent without cloning or removing trie nodes.
    /// Deferred mode stores computed branch hashes; eager verification preserves them.
    fn calculate_node_hashes(
        &mut self,
        storage_tx: &mut TrieStorageTransaction<T>,
        node_ptr: u32,
    ) -> Result<TrieHash, Error> {
        let (node, node_hash) = self.get_nodetype(node_ptr)?;
        if node.is_leaf() {
            return Ok(*node_hash);
        }
        match node.max_ptrs() {
            4 => self.calculate_branch_hashes::<4>(storage_tx, node_ptr),
            16 => self.calculate_branch_hashes::<16>(storage_tx, node_ptr),
            48 => self.calculate_branch_hashes::<48>(storage_tx, node_ptr),
            256 => self.calculate_branch_hashes::<256>(storage_tx, node_ptr),
            _ => unreachable!("Invalid branch capacity"),
        }
    }

    /// Keep child commitments in stack scratch sized to this branch's actual fanout.
    #[inline(never)]
    fn calculate_branch_hashes<const N: usize>(
        &mut self,
        storage_tx: &mut TrieStorageTransaction<T>,
        node_ptr: u32,
    ) -> Result<TrieHash, Error> {
        let child_count = N;
        let mut child_hashes = [TrieHash::EMPTY; N];
        for (slot, hash) in child_hashes.iter_mut().enumerate().take(child_count) {
            // Copy only this pointer, releasing the node borrow before recursion.
            let ptr = self.get_nodetype(node_ptr)?.0.ptrs()[slot];
            if ptr.is_empty() {
                continue;
            }
            if is_backptr(ptr.id()) {
                hash.0.copy_from_slice(
                    storage_tx
                        .get_block_hash_caching(ptr.back_block())?
                        .as_bytes(),
                );
            } else {
                let child_idx = ptr.try_ptr_into_u32()?;
                *hash = self.calculate_node_hashes(storage_tx, child_idx)?;
                if storage_tx.hash_calculation_mode == TrieHashCalculationMode::Deferred
                    && ptr.id() != TrieNodeID::Leaf as u8
                {
                    self.write_node_hash(child_idx, *hash)?;
                }
            }
        }
        let node = &self.get_nodetype(node_ptr)?.0;
        let mut hasher = TrieHasher::new();
        node.write_consensus_bytes_with_child_hashes(&child_hashes[..child_count], &mut hasher)?;
        for hash in child_hashes.iter().take(child_count) {
            hasher.write_all(hash.as_bytes())?;
        }
        Ok(TrieHash(hasher.finalize().into()))
    }

    /// Walk through the buffered TrieNodes and dump them to f.
    /// This consumes this TrieRAM instance.
    ///
    /// Uses a child-before-parent DFS write order.
    ///
    /// The root node's space is reserved at the front of the
    /// blob (immediately after the header) assuming u64 pointers,
    /// and it is written last once all child offsets are known.
    pub(crate) fn dump_consume<F: Write + Seek>(
        mut self,
        f: &mut F,
        format: NodeRecordFormat,
    ) -> Result<u64, Error> {
        let header_size = blob_layout::ROOT_NODE_OFFSET as u64;
        let root_mem_ptr = TriePtr::new(TrieNodeID::Node256 as u8, 0, 0).try_ptr_into_u32()?;

        // Step 1: collect nodes in root-first DFS order.
        let write_order = {
            let mut order = Vec::with_capacity(self.data.len());
            let mut stack = vec![root_mem_ptr];
            while let Some(ptr) = stack.pop() {
                order.push(ptr);
                let (node, _) = self.get_nodetype(ptr)?;
                if !node.is_leaf() {
                    for child in node.ptrs().iter() {
                        if is_inline_child_ptr(child) {
                            stack.push(child.try_ptr_into_u32()?);
                        }
                    }
                }
            }
            order
        };
        // Split root from the remaining nodes.
        let (&root_ptr, descendants) = write_order
            .split_first()
            .ok_or_else(|| Error::CorruptionError("Empty trie in dump_consume".into()))?;
        if root_ptr != root_mem_ptr {
            return Err(Error::CorruptionError(
                "Root node missing from dump_consume write order".into(),
            ));
        }

        // Step 2: reserve space for the root at the front of the blob.
        //
        // We assume all inline child pointers need u64 encoding (worst case).
        // When some child offsets fit in u32, the root's actual size is
        // smaller than the reserved space, leaving a small dead gap (at most
        // 4 * n_inline_children bytes) between the root and the first descendant.
        let root_reserved_size = if format == NodeRecordFormat::TypeFirstV4 {
            packed_root_reservation(&self.data, write_order.len(), |index| {
                (write_order[index], None)
            })?
        } else {
            let (root_node, _) = self.get_nodetype(root_mem_ptr)?;
            reserved_root_size(format.node_len(root_node, false), root_node.ptrs())?
        };

        // Write the fixed blob header.
        f.rewind()?;
        format.write_trie_header(f, &self.parent)?;

        f.seek(SeekFrom::Start(header_size + root_reserved_size))?;

        // Map from in-memory index -> file offset where each node was written.
        let mut file_offsets = vec![0u64; self.data.len()];

        // Step 3: write all descendant nodes in child-before-parent order.
        //
        // Reverse-iterating `descendants` ensures each child's file offset is already
        // recorded by the time we write its parent.
        for &mem_ptr in descendants.iter().rev() {
            let mem_idx = mem_ptr as usize;
            *file_offsets.get_mut(mem_idx).ok_or_else(|| {
                Error::CorruptionError("Node index out of range in dump_consume".into())
            })? = f.stream_position()?;

            let entry = self.data.get_mut(mem_idx).ok_or_else(|| {
                Error::CorruptionError("Invalid node pointer in dump_consume".into())
            })?;

            if !entry.0.is_leaf() {
                resolve_inline_child_offsets(entry.0.ptrs_mut(), &file_offsets)?;
            }

            format.write_node(f, &entry.0, entry.1, false)?;
        }

        let end_offset = f.stream_position()?;

        // Step 4: write the root node into its reserved space.
        let entry = self
            .data
            .get_mut(root_mem_ptr as usize)
            .ok_or_else(|| Error::CorruptionError("Invalid root pointer in dump_consume".into()))?;

        if !entry.0.is_leaf() {
            resolve_inline_child_offsets(entry.0.ptrs_mut(), &file_offsets)?;
        }
        f.seek(SeekFrom::Start(header_size))?;
        format.write_node(f, &entry.0, entry.1, false)?;
        let root_written = f.stream_position()? - header_size;
        if root_written > root_reserved_size
            || (format == NodeRecordFormat::TypeFirstV4 && root_written != root_reserved_size)
        {
            return Err(Error::CorruptionError(
                "Root does not fit planned reservation".into(),
            ));
        }

        Ok(end_offset)
    }

    fn make_node_patch(
        storage_tx: &mut TrieStorageTransaction<T>,
        base_ptr: TrieCowPtr,
        node: &TrieNodeType,
        decode_scratch: &mut impl NodePatching,
    ) -> Result<Option<TrieNodePatch>, Error> {
        // Save block state. We use `set_block` to restore instead of `open_block` because the
        // current block may be the uncommitted trie (which has been `.take()`'d from storage during
        // `dump_compressed_consume`), making it unreachable via `open_block`.
        let (cur_block, cur_block_id) = storage_tx.get_cur_block_and_id();
        let result = (|| {
            storage_tx.open_block(&base_ptr.block_id())?;
            let read = storage_tx.read_node_with_state(base_ptr.ptr(), decode_scratch)?;
            if read.patch_depth >= MAX_PATCH_DEPTH as usize {
                return Ok(None);
            }
            if read.path_bytes()? != node.path_bytes() {
                return Ok(None);
            }

            let (old_node, _) = read.as_node_ref()?;

            trace!(
                "Make patch from old node from block {:?} to new node {:?}",
                &old_node,
                node
            );
            Ok(TrieNodePatch::try_from_noderef(
                *base_ptr.ptr(),
                old_node,
                &node,
            ))
        })();
        storage_tx.data.set_block(cur_block, cur_block_id);
        result
    }

    /// Walk through the buffered TrieNodes and dump them to f, compressing the trie.
    /// This consumes this TrieRAM instance.
    /// The trie will already have been sealed.
    ///
    /// ## Space improvements
    ///
    /// * Do not store backptr 0's if the node isn't a backptr
    /// * Store a compact representation for sparse child pointer lists
    /// * If a node was copied from another, then only store the difference in ptrs (TrieNodePatch)
    ///
    /// Uses a child-before-parent DFS write order.
    ///
    /// Returns Ok(len) to report number of bytes written
    /// Returns Err(..) if we fail to write
    pub(crate) fn dump_compressed_consume<F: Write + Seek>(
        mut self,
        storage_tx: &mut TrieStorageTransaction<T>,
        f: &mut F,
    ) -> Result<u64, Error> {
        let header_size = blob_layout::ROOT_NODE_OFFSET as u64;
        let format = storage_tx.data.record_context.format;
        let max_patch_depth = MAX_PATCH_DEPTH as usize;
        let root_mem_ptr = TriePtr::new(TrieNodeID::Node256 as u8, 0, 0).try_ptr_into_u32()?;

        // Step 1: collect nodes in root-first DFS order, computing patch
        // payloads along the way.
        let mut write_order = Vec::with_capacity(self.data.len());
        let mut decode_scratch = MarfReadState::new();
        {
            let mut stack = vec![root_mem_ptr];
            while let Some(pointer) = stack.pop() {
                let (node, node_hash) = self.get_nodetype(pointer)?;

                // If possible, store a patch instead of the whole node.
                let mut patch_node_opt = if !node.is_leaf() && node.patch_depth() < max_patch_depth
                {
                    if let Some((last_patch_block_id, last_patch_ptr)) = node.last_patch_source() {
                        let block_hash = storage_tx.get_block_hash_caching(last_patch_block_id)?;
                        let mut patch_ptr = TriePtr::new(
                            set_backptr(TrieNodeID::Patch as u8),
                            last_patch_ptr.chr(),
                            last_patch_ptr.ptr(),
                        );
                        patch_ptr.back_block = last_patch_block_id;
                        let base_ptr = TrieCowPtr::new(block_hash.clone(), patch_ptr);
                        let patch_node_opt =
                            Self::make_node_patch(storage_tx, base_ptr, node, &mut decode_scratch)?;
                        if let Some(patch_node) = patch_node_opt {
                            trace!("Create amendment patch for node at {base_ptr:?}: {node:?}");
                            Some((node_hash.to_bytes(), patch_node))
                        } else {
                            None
                        }
                    } else if let Some(cowptr) = node.get_cow_ptr() {
                        let patch_node_opt =
                            Self::make_node_patch(storage_tx, *cowptr, node, &mut decode_scratch)?;
                        if let Some(patch_node) = patch_node_opt {
                            trace!("Create COW patch for node at {cowptr:?}: {node:?}");
                            Some((node_hash.to_bytes(), patch_node))
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                };

                debug_assert!({
                    if let Some((_, patch_node)) = patch_node_opt.as_ref() {
                        let node_inline = node
                            .ptrs()
                            .iter()
                            .filter(|p| is_inline_child_ptr(p))
                            .map(|p| p.chr());
                        let diff_inline = patch_node
                            .ptr_diff
                            .iter()
                            .filter(|p| is_inline_child_ptr(p))
                            .map(|p| p.chr());
                        node_inline.eq(diff_inline)
                    } else {
                        true
                    }
                });

                // Push children onto the DFS stack.
                if !node.is_leaf() {
                    for child in node.ptrs().iter() {
                        if is_inline_child_ptr(child) {
                            let idx = child.try_ptr_into_u32()?;
                            stack.push(idx);
                        }
                    }
                }
                if let Some((hash_bytes, patch)) = patch_node_opt.take() {
                    write_order.push(DumpPtr::Patch(pointer, hash_bytes, patch));
                } else {
                    write_order.push(DumpPtr::Normal(pointer));
                }
            }
        }
        let packed_reservation = if format == NodeRecordFormat::TypeFirstV4 {
            Some(packed_root_reservation(
                &self.data,
                write_order.len(),
                |index| (write_order[index].ptr(), write_order[index].patch()),
            )?)
        } else {
            None
        };

        // Split root (index 0) from the remaining nodes.
        let (root_dp, descendants) = write_order.split_first_mut().ok_or_else(|| {
            Error::CorruptionError("Empty trie in dump_compressed_consume".into())
        })?;
        if root_dp.ptr() != root_mem_ptr {
            return Err(Error::CorruptionError(
                "Root node missing from dump_compressed_consume write order".into(),
            ));
        }

        // Step 2: reserve space for the root at the front of the blob.
        //
        // We assume all inline child pointers need u64 encoding (worst case).
        // When some child offsets fit in u32, the root's actual size is
        // smaller than the reserved space, leaving a small dead gap (at most
        // 4 * n_inline_children bytes) between the root and the first descendant.
        let root_reserved_size = if let Some(reserved) = packed_reservation {
            reserved
        } else {
            if let Some(patch) = root_dp.patch() {
                reserved_root_size(TRIEHASH_ENCODED_SIZE + patch.size(), &patch.ptr_diff)?
            } else {
                let (root_node, _) = self.get_nodetype(root_mem_ptr)?;
                reserved_root_size(format.node_len(root_node, true), root_node.ptrs())?
            }
        };

        // Write the fixed blob header.
        f.rewind()?;
        format.write_trie_header(f, &self.parent)?;

        // Seek past the reserved root space.
        f.seek(SeekFrom::Start(header_size + root_reserved_size))?;

        // Map from in-memory index -> file offset where each node was written.
        let mut file_offsets = vec![0u64; self.data.len()];

        // Helper: write a DumpPtr (patch or normal) to the file.
        fn write_dump_ptr<F: Write + Seek>(
            f: &mut F,
            dp: &DumpPtr,
            data: &[(TrieNodeType, TrieHash)],
            format: NodeRecordFormat,
        ) -> Result<(), Error> {
            if let Some((hash_bytes, patch)) = dp.hash_and_patch() {
                format.write_patch(f, patch, TrieHash(*hash_bytes))?;
            } else {
                let node = data.get(dp.ptr() as usize).ok_or_else(|| {
                    Error::CorruptionError("node pointer invalid in compressed dump".into())
                })?;
                format.write_node(f, &node.0, node.1, true)?;
            }
            Ok(())
        }

        // Step 3: write all descendant nodes in child-before-parent order.
        //
        // Reverse-iterating `descendants` ensures each child's file offset is already
        // recorded by the time we write its parent.
        for dp in descendants.iter_mut().rev() {
            let dp_idx = dp.ptr() as usize;
            *file_offsets.get_mut(dp_idx).ok_or_else(|| {
                Error::CorruptionError("Node index out of range in dump_compressed_consume".into())
            })? = f.stream_position()?;
            if let Some(patch) = dp.patch_mut() {
                resolve_inline_child_offsets(patch.ptr_diff.as_mut_slice(), &file_offsets)?;
            } else {
                let entry = self.data.get_mut(dp_idx).ok_or_else(|| {
                    Error::CorruptionError("Invalid node pointer in dump_compressed_consume".into())
                })?;
                if !entry.0.is_leaf() {
                    resolve_inline_child_offsets(entry.0.ptrs_mut(), &file_offsets)?;
                }
            }
            write_dump_ptr(f, dp, &self.data, format)?;
        }

        let end_offset = f.stream_position()?;

        // Step 4: write the root node into its reserved space.
        if let Some(patch) = root_dp.patch_mut() {
            resolve_inline_child_offsets(patch.ptr_diff.as_mut_slice(), &file_offsets)?;
        } else {
            let entry = self.data.get_mut(root_dp.ptr() as usize).ok_or_else(|| {
                Error::CorruptionError("Invalid root pointer in dump_compressed_consume".into())
            })?;
            if !entry.0.is_leaf() {
                resolve_inline_child_offsets(entry.0.ptrs_mut(), &file_offsets)?;
            }
        }
        f.seek(SeekFrom::Start(header_size))?;
        write_dump_ptr(f, root_dp, &self.data, format)?;
        let root_written = f
            .stream_position()?
            .checked_sub(header_size)
            .ok_or(Error::OverflowError)?;
        if root_written > root_reserved_size
            || (format == NodeRecordFormat::TypeFirstV4 && root_written != root_reserved_size)
        {
            return Err(Error::CorruptionError(
                "Compressed root does not fit planned reservation".into(),
            ));
        }

        Ok(end_offset)
    }

    /// Load the trie from `f`.
    ///
    /// The trie will have the same structure as the on-disk trie, but it may have nodes in a
    /// different order.
    pub fn load<F: Read + Seek>(f: &mut F, bhh: &T) -> Result<TrieRAM<T>, Error> {
        Self::load_with_context(f, bhh, &RecordContext::default())
    }

    /// Load an uncompressed mutable trie using its explicitly selected disk layout.
    pub fn load_with_context<F: Read + Seek>(
        f: &mut F,
        bhh: &T,
        context: &RecordContext,
    ) -> Result<TrieRAM<T>, Error> {
        let mut data: Vec<(TrieNodeType, TrieHash)> = vec![];
        let mut frontier = VecDeque::new();

        // read parent
        f.rewind()?;
        let mut header = [0u8; blob_layout::ROOT_NODE_OFFSET];
        f.read_exact(&mut header)?;
        context.format.validate_trie_header(&header)?;
        let parent_hash = T::from_bytes(header[..32].try_into().expect("parent width"));

        fn load_node<F: Read + Seek>(
            f: &mut F,
            ptr: &TriePtr,
            context: &RecordContext,
            scratch: &mut MarfReadState,
        ) -> Result<(TrieNodeType, TrieHash), Error> {
            f.seek(SeekFrom::Start(ptr.ptr()))?;
            let (mut node, hash) =
                bits::read_trie_item_at_head_ref_format(f, ptr.id(), context.format, scratch)?
                    .into_node()?
                    .into_owned_node()?;
            let hash = match hash {
                Some(hash) => hash,
                None => {
                    let TrieNodeType::Leaf(ref mut leaf) = node else {
                        return Err(Error::CorruptionError("Hashless non-leaf".into()));
                    };
                    context.resolve_leaf(leaf)?;
                    bits::get_leaf_hash(leaf)
                }
            };
            Ok((node, hash))
        }
        let mut decode_scratch = MarfReadState::new();

        let root_disk_ptr = blob_layout::ROOT_NODE_OFFSET as u64;

        let root_ptr = TriePtr::new(TrieNodeID::Node256 as u8, 0, root_disk_ptr);
        let (mut root_node, root_hash) = load_node(f, &root_ptr, context, &mut decode_scratch)
            .inspect_err(|e| error!("Failed to read root node info for {bhh:?}: {e:?}"))?;

        let mut next_index = 1;
        let mut decode_scratch = MarfReadState::new();

        if let TrieNodeType::Node256(ref mut data) = root_node {
            // queue children in the same order we stored them
            for ptr in data.ptrs.iter_mut() {
                if is_inline_child_ptr(ptr) {
                    frontier.push_back(*ptr);

                    // fix up ptrs
                    ptr.ptr = next_index;
                    next_index += 1;
                }
            }
        } else {
            return Err(Error::CorruptionError(
                "First TrieRAM node is not a Node256".to_string(),
            ));
        }

        data.push((root_node, root_hash));

        while !frontier.is_empty() {
            let next_ptr = frontier
                .pop_front()
                .expect("BUG: no ptr in non-empty frontier");
            let (mut next_node, next_hash) = load_node(f, &next_ptr, context, &mut decode_scratch)
                .inspect_err(|e| error!("Failed to read node at {next_ptr:?}: {e:?}"))?;

            if !next_node.is_leaf() {
                // queue children in the same order we stored them
                let ptrs: &mut [TriePtr] = match next_node {
                    TrieNodeType::Node4(ref mut data) => &mut data.ptrs,
                    TrieNodeType::Node16(ref mut data) => &mut data.ptrs,
                    TrieNodeType::Node48(ref mut data) => &mut data.ptrs,
                    TrieNodeType::Node256(ref mut data) => &mut data.ptrs,
                    _ => {
                        unreachable!();
                    }
                };

                for ptr in ptrs {
                    if is_inline_child_ptr(ptr) {
                        frontier.push_back(*ptr);

                        // fix up ptrs
                        ptr.ptr = next_index;
                        next_index += 1;
                    }
                }
            }

            data.push((next_node, next_hash));
        }

        Ok(TrieRAM::from_data((*bhh).clone(), data, parent_hash))
    }

    /// Hint as to how many entries to allocate for the inner Vec when creating a TrieRAM
    fn size_hint(&self) -> usize {
        self.write_count as usize
        // the size hint is used for a capacity guess on the data vec, which is _nodes_
        //  NOT bytes. this led to enormous over-allocations
    }

    /// Clear the TrieRAM contents
    pub fn format(&mut self) -> Result<(), Error> {
        self.result_cache.before_raw_write();
        if self.readonly {
            trace!("Read-only!");
            return Err(Error::ReadOnlyError);
        }

        self.data.clear();
        Ok(())
    }

    /// Read a node's hash from the TrieRAM.  ptr.ptr() is an array index.
    pub fn read_node_hash(&self, ptr: &TriePtr) -> Result<TrieHash, Error> {
        let idx = ptr.try_ptr_into_usize()?;
        let (_, node_trie_hash) = self.data.get(idx).ok_or_else(|| {
            error!(
                "TrieRAM: Failed to read node bytes: {} >= {}",
                ptr.ptr(),
                self.data.len()
            );
            Error::NotFoundError
        })?;

        Ok(*node_trie_hash)
    }

    /// Get an immutable reference to a node and its hash from the TrieRAM.  ptr.ptr() is an array index.
    pub fn get_nodetype(&self, ptr: u32) -> Result<&(TrieNodeType, TrieHash), Error> {
        self.data.get(ptr as usize).ok_or_else(|| {
            error!(
                "TrieRAM get_nodetype({:?}): Failed to read node: {ptr} >= {}",
                &self.block_header,
                self.data.len()
            );
            Error::NotFoundError
        })
    }

    /// Take a node+hash out of the TrieRAM at the given slot, leaving a cheap placeholder.
    ///
    /// The caller MUST call [`restore_node()`](Self::restore_node) to put a node back before the
    /// TrieRAM is read at this slot again. This is used by the hash-recalculation path to avoid
    /// cloning nodes while still allowing `&mut self` access to storage for hash computation.
    pub fn take_node(&mut self, ptr: u32) -> Result<(TrieNodeType, TrieHash), Error> {
        self.result_cache.before_raw_write();
        let data_len = self.data.len();
        let bhh = &self.block_header;
        let slot = self.data.get_mut(ptr as usize).ok_or_else(|| {
            error!("TrieRAM take_node({bhh:?}): {ptr} >= {data_len}");
            Error::NotFoundError
        })?;
        Ok(std::mem::replace(slot, Self::slot_placeholder()))
    }

    /// Restore a node+hash into a `TrieRAM` slot previously emptied by
    /// [`take_node()`](Self::take_node).
    pub fn restore_node(
        &mut self,
        ptr: u32,
        node: TrieNodeType,
        hash: TrieHash,
    ) -> Result<(), Error> {
        self.result_cache.before_raw_write();
        let data_len = self.data.len();
        let bhh = &self.block_header;
        let slot = self.data.get_mut(ptr as usize).ok_or_else(|| {
            error!("TrieRAM restore_node({bhh:?}): {ptr} >= {data_len}");
            Error::NotFoundError
        })?;
        *slot = (node, hash);
        Ok(())
    }

    /// Cheap placeholder value for a temporarily-empty `TrieRAM` slot.
    fn slot_placeholder() -> (TrieNodeType, TrieHash) {
        (
            TrieNodeType::Leaf(TrieLeaf {
                path: NodePath::default(),
                data: Some(MARFValue([0u8; 40])),
                extent: None,
                inline: None,
            }),
            TrieHash([0u8; TRIEHASH_ENCODED_SIZE]),
        )
    }

    pub fn read_node(&mut self, ptr: &TriePtr) -> Result<ReadTrieNode<'_>, Error> {
        let bhh = &self.block_header;
        trace!("TrieRAM: read_node({bhh:?}): at {ptr:?}");

        let idx = ptr.try_ptr_into_usize()?;
        if let Some((node, hash)) = self.data.get(idx) {
            Ok(
                ReadTrieNode::from_borrowed(TrieNodeRef::from(node), Some(*hash))
                    .with_transient_meta(TrieNodeTransientMeta::from_node(node)),
            )
        } else {
            error!(
                "TrieRAM read_node({bhh:?}): Failed to read node {ptr:?}: {} >= {}",
                ptr.ptr(),
                self.data.len()
            );
            Err(Error::NotFoundError)
        }
    }

    /// Store a node and its hash to the `TrieRAM` at the given slot.
    pub fn write_nodetype(
        &mut self,
        node_array_ptr: u32,
        node: &TrieNodeType,
        hash: TrieHash,
    ) -> Result<(), Error> {
        self.result_cache.before_raw_write();
        if self.readonly {
            trace!("Read-only!");
            return Err(Error::ReadOnlyError);
        }

        let bhh = &self.block_header;
        trace!("TrieRAM: write_nodetype({bhh:?}): at {node_array_ptr}: {hash:?} {node:?}");

        self.write_count += 1;
        let node_index = node_array_ptr as usize;
        if let Some(existing_node) = self.data.get_mut(node_index) {
            *existing_node = (node.clone(), hash);
            Ok(())
        } else if node_array_ptr
            == u32::try_from(self.data.len()).map_err(|_| Error::OverflowError)?
        {
            self.data.push((node.clone(), hash));
            self.total_bytes += bits::get_node_byte_len(node);
            Ok(())
        } else {
            error!("Failed to write node bytes: off the end of the buffer");
            Err(Error::NotFoundError)
        }
    }

    /// Store a node hash into the `TrieRAM` at a given node slot.
    pub fn write_node_hash(&mut self, node_array_ptr: u32, hash: TrieHash) -> Result<(), Error> {
        if self.readonly {
            trace!("Read-only!");
            return Err(Error::ReadOnlyError);
        }

        let bhh = &self.block_header;
        trace!("TrieRAM: write_node_hash({bhh:?}): at {node_array_ptr}: {hash:?}",);

        // can only set the hash of an existing node
        let node_index = node_array_ptr as usize;
        if let Some(existing_node) = self.data.get_mut(node_index) {
            existing_node.1 = hash;
            Ok(())
        } else {
            error!("Failed to write node hash bytes: off the end of the buffer");
            Err(Error::NotFoundError)
        }
    }

    /// Get the next ptr value for a node to store.
    pub fn last_ptr(&mut self) -> Result<u32, Error> {
        u32::try_from(self.data.len()).map_err(|_| Error::OverflowError)
    }

    #[cfg(test)]
    pub fn print_to_stderr(&self) {
        for dat in self.data.iter() {
            eprintln!("{}: {:?}", &dat.1, &dat.0);
        }
    }

    #[cfg(test)]
    pub fn data(&self) -> &Vec<(TrieNodeType, TrieHash)> {
        &self.data
    }
}

impl<T: MarfTrieId> NodeHashReader for TrieRAM<T> {
    fn read_node_hash<W: Write>(&mut self, ptr: &TriePtr, w: &mut W) -> Result<(), Error> {
        let idx = ptr.try_ptr_into_usize()?;
        let (_, node_trie_hash) = self.data.get(idx).ok_or_else(|| {
            error!(
                "TrieRAM: Failed to read node bytes: {} >= {}",
                ptr.ptr(),
                self.data.len()
            );
            Error::NotFoundError
        })?;
        w.write_all(node_trie_hash.as_bytes())?;
        Ok(())
    }
}

pub struct TrieSqlCursor<'a> {
    record_context: &'a RecordContext,
    db: &'a Connection,
    block_id: u32,
}

pub struct TrieSqlHashMapCursor<'a, T: MarfTrieId> {
    direct_hash_index: Option<&'a DirectHashIndex>,
    db: &'a Connection,
    cache: &'a mut BlockHashCache<T>,
    unconfirmed: bool,
}

impl NodeHashReader for TrieSqlCursor<'_> {
    fn read_node_hash<W: Write>(&mut self, ptr: &TriePtr, w: &mut W) -> Result<(), Error> {
        trie_sql::read_node_hash_bytes(self.db, w, self.block_id, ptr, self.record_context)
    }
}

/// `TrieStorageTransaction` is an alias for [`TrieStorageConnection`] specialized with
/// [`Transaction<'a>`]. Any storage methods that require a live write transaction are defined only
/// for [`TrieStorageConnection<'a, T, Transaction<'a>>`] (e.g.,
/// [`flush()`](TrieStorageConnection::flush), [`commit_tx()`](TrieStorageConnection::commit_tx)).
pub type TrieStorageTransaction<'a, T> = TrieStorageConnection<'a, T, Transaction<'a>>;

/// Hash calculation mode
#[derive(Debug, Clone, PartialEq, Copy)]
pub enum TrieHashCalculationMode {
    /// Calculate all trie node hashes as we insert leaves
    Immediate,
    /// Do not calculate trie node hashes until we dump the trie to disk
    Deferred,
    /// Calculate trie hashes both on leaf insert and on trie dump.  Used for testing.
    All,
}

///
///  TrieStorageConnection is a pointer to an open TrieFileStorage.
///  The `Db` type parameter encodes the connection state: `&'a Connection`
///  for read-only access, `Transaction<'a>` for a live write transaction.
///  Mutations on TrieStorageConnection's `data` field propagate to the
///  TrieFileStorage that created the connection.
///  This is the main interface to the storage methods, and defines most
///    of the storage functionality.
///
pub struct TrieStorageConnection<'a, T: MarfTrieId, Db: Deref<Target = Connection> = &'a Connection>
{
    /// Invalidates on implicit SQL rollback; disarmed only after successful commit.
    result_cache_rollback: Option<ResultCacheGuard>,
    /// Revokes direct hash reads when this transaction ends.
    _direct_hash_guard: Option<DirectHashGuard>,
    pub db_path: &'a str,
    db: Db,
    /// Invalidates shared resolved nodes on explicit or implicit SQL rollback.
    patch_cache_rollback: Option<SharedLruGuard<(T, u64), ResolvedPatchNode>>,
    blobs: Option<&'a mut TrieFile>,
    data: &'a mut TrieStorageTransientData<T>,
    cache: &'a mut BlockHashCache<T>,
    pub hash_calculation_mode: TrieHashCalculationMode,
    compress: bool,
    mmap: bool,

    // used in testing in order to short-circuit block-height lookups
    //   when the trie struct is tested outside of marf.rs usage
    #[cfg(test)]
    pub test_genesis_block: &'a mut Option<T>,
}

/// TrieStorageTransientData holds all the data that _isn't_ committed to the underlying SQL
/// storage. Used internally to simplify the TrieStorageConnection/TrieFileStorage interactions
pub struct TrieStorageTransientData<T: MarfTrieId> {
    /// Optional immutable historical block/root hash snapshot.
    direct_hash_index: Option<DirectHashIndex>,
    /// Capacity applied to each new mutable trie.
    result_cache_capacity: usize,
    /// Invalidates active results after failed or rolled-back storage operations.
    result_cache_control: Arc<ResultCacheControl>,
    /// Shared immutable value source for resolving locator-only leaves.
    record_context: RecordContext,
    /// This is all the nodes written but not yet committed to disk.
    pub uncommitted_writes: Option<(T, UncommittedState<T>)>,

    /// Currently-open block (may be `uncommitted_writes.unwrap().0`)
    cur_block: T,
    /// Tracks the `row_id` for `cur_block`.
    ///
    /// If `cur_block == uncommitted_writes`, this value should always be `None`.
    cur_block_id: Option<u32>,

    /// Runtime statistics on reading nodes
    read_count: u64,
    read_backptr_count: u64,
    read_node_count: u64,
    read_leaf_count: u64,

    /// Runtime statistics on writing nodes
    write_count: u64,
    write_node_count: u64,
    write_leaf_count: u64,

    /// List of ancestral trie root hashes that must be hashed with the `uncommitted_writes` root
    /// node hash to produce the [`MarfTrieId`] for the trie when it gets written to disk.
    ///
    /// This is maintained by the MARF whenever it needs to update the trie root hash after a leaf
    /// insert, so that a batch of leaf inserts into `uncommitted_writes` don't require an ancestor
    /// trie hash query more than once.
    trie_ancestor_hash_bytes_cache: Option<(T, Arc<[TrieHash]>)>,

    /// Is the trie opened read-only?
    readonly: bool,

    /// Does this trie represent unconfirmed state?
    unconfirmed: bool,

    /// row ID of a trie that represents unconfirmed state (i.e. trie state that will never become
    /// part of the MARF, but nevertheless represents a persistent scratch space).
    ///
    /// If this field is `Some(..)`, then the storage was used to (re-)open an unconfirmed trie (via
    /// `open_unconfirmed()` or `open_block()` when `self.unconfirmed` is `true`), or used to create
    /// an unconfirmed trie (via `extend_to_unconfirmed_block()`).
    unconfirmed_block_id: Option<u32>,

    /// Cached external blob file offset for `cur_block_id`.
    ///
    /// Populated when a committed block is opened, so that hot-path reads for the current block can
    /// bypass the `RefCell<HashMap>` offset cache in `TrieFile`.
    cur_block_trie_offset: Option<u64>,

    /// Decoded roots and their hashes/patch depths, keyed by immutable committed block ID.
    /// Allocated on first use so disabled caches cost nothing and MARFs stay small on the stack.
    root_node_cache: Option<Box<ArrayLru<u32, (TrieNodeType, Option<TrieHash>, usize), 4>>>,
    /// Whether committed root reads use the small LRU.
    root_node_cache_enabled: bool,
    /// Fully resolved immutable nodes shared across reopens, keyed by block hash and offset.
    resolved_patch_cache: SharedLru<(T, u64), ResolvedPatchNode>,
    /// Retains the last returned node while callers borrow it, without holding a cache lock.
    resolved_patch_node: Option<Arc<ResolvedPatchNode>>,

    /// Snapshot metadata if this MARF is squashed.
    squash_info: Option<SquashInfo>,
}

/// Fully decoded node with its stored hash and patch depth.
type ResolvedPatchNode = (TrieNodeType, Option<TrieHash>, usize);

/// Snapshot metadata cached at open time for squashed MARFs.
#[derive(Clone, Debug)]
pub struct SquashInfo {
    /// Archival MARF root hash committed to the chain at the squash boundary.
    pub archival_marf_root_hash: TrieHash,
    /// Root node hash of the squash trie. i.e. `hash(consensus_bytes(root) || children_content_hashes)`
    pub squash_root_node_hash: TrieHash,
    /// Backing MARF's own height at the squash tip - Stacks block height for
    /// the clarity/index MARFs, sortition block height for the sortition MARF.
    pub squash_height: u32,
}

// disk-backed Trie.
// Keeps the last-extended Trie in-RAM and flushes it to disk on either a call to flush() or a call
// to extend_to_block() with a different block header hash.
pub struct TrieFileStorage<T: MarfTrieId> {
    pub db_path: String,

    db: Connection,
    blobs: Option<TrieFile>,
    data: TrieStorageTransientData<T>,
    cache: BlockHashCache<T>,
    hash_calculation_mode: TrieHashCalculationMode,
    compress: bool,
    mmap: bool,

    // used in testing in order to short-circuit block-height lookups
    //   when the trie struct is tested outside of marf.rs usage
    #[cfg(test)]
    pub test_genesis_block: Option<T>,
}

/// Helper to open a MARF
fn marf_sqlite_open<P: AsRef<Path>>(
    db_path: P,
    open_flags: OpenFlags,
    foreign_keys: bool,
) -> Result<Connection, db_error> {
    let db = sqlite_open(db_path, open_flags, foreign_keys)?;
    sql_pragma(&db, "mmap_size", &SQLITE_MMAP_SIZE)?;
    sql_pragma(&db, "page_size", &SQLITE_MARF_PAGE_SIZE)?;
    Ok(db)
}

impl<T: MarfTrieId> Default for TrieStorageTransientData<T> {
    fn default() -> Self {
        Self {
            direct_hash_index: None,
            result_cache_capacity: DEFAULT_RESULT_CACHE_CAPACITY,
            result_cache_control: Arc::default(),
            record_context: RecordContext::default(),
            uncommitted_writes: None,
            cur_block: T::sentinel(),
            cur_block_id: None,
            read_count: 0,
            read_backptr_count: 0,
            read_node_count: 0,
            read_leaf_count: 0,
            write_count: 0,
            write_node_count: 0,
            write_leaf_count: 0,
            trie_ancestor_hash_bytes_cache: None,
            readonly: false,
            unconfirmed: false,
            unconfirmed_block_id: None,
            cur_block_trie_offset: None,
            squash_info: None,
            root_node_cache: None,
            root_node_cache_enabled: true,
            resolved_patch_cache: SharedLru::new(16),
            resolved_patch_node: None,
        }
    }
}

impl<T: MarfTrieId> TrieStorageTransientData<T> {
    /// Invalidate cached committed roots if this view has allocated a cache.
    fn clear_root_node_cache(&mut self) {
        if let Some(cache) = self.root_node_cache.as_mut() {
            cache.clear();
        }
    }

    /// Construct transient data targeting a specific block, with the given read/write flags.
    /// All stat counters start at zero and caches start empty.
    pub fn new(cur_block: T, cur_block_id: Option<u32>, readonly: bool, unconfirmed: bool) -> Self {
        Self {
            cur_block,
            cur_block_id,
            readonly,
            unconfirmed,
            ..Self::default()
        }
    }

    /// Target the transient data to a particular block, and optionally its block ID.
    ///
    /// Clears the cached trie offset (it will be re-populated on first read).
    fn set_block(&mut self, bhh: T, id: Option<u32>) {
        trace!("set_block({},{:?})", &bhh, &id);
        self.cur_block_id = id;
        self.cur_block = bhh;
        self.cur_block_trie_offset = None;
    }

    fn clear_block_id(&mut self) {
        self.cur_block_id = None;
    }

    pub fn set_ancestor_hashes_bytes(&mut self, bhh: &T, bytes: Arc<[TrieHash]>) {
        self.trie_ancestor_hash_bytes_cache = Some((bhh.clone(), bytes));
    }

    pub fn get_ancestor_hashes_bytes(&self, bhh: &T) -> Option<Arc<[TrieHash]>> {
        if let Some((ref cached_bhh, ref cached_bytes)) = self.trie_ancestor_hash_bytes_cache {
            if cached_bhh == bhh {
                return Some(Arc::clone(cached_bytes));
            }
        }
        None
    }

    pub fn clear_ancestor_hashes_bytes(&mut self) {
        self.trie_ancestor_hash_bytes_cache = None;
    }

    fn set_squash_info(&mut self, squash_info: Option<SquashInfo>) {
        self.result_cache_control.invalidate();
        self.clear_root_node_cache();
        self.resolved_patch_cache.clear();
        self.squash_info = squash_info;
    }
}

pub struct ReopenedTrieStorageConnection<'a, T: MarfTrieId> {
    pub db_path: &'a str,
    db: &'a Connection,
    blobs: Option<TrieFile>,
    data: TrieStorageTransientData<T>,
    cache: BlockHashCache<T>,
    pub hash_calculation_mode: TrieHashCalculationMode,
    compress: bool,
    mmap: bool,

    // used in testing in order to short-circuit block-height lookups
    //   when the trie struct is tested outside of marf.rs usage
    #[cfg(test)]
    pub test_genesis_block: Option<T>,
}

impl<'a, T: MarfTrieId> ReopenedTrieStorageConnection<'a, T> {
    pub fn db_conn(&self) -> &Connection {
        self.db
    }

    pub fn readonly(&self) -> bool {
        self.data.readonly
    }

    pub fn unconfirmed(&self) -> bool {
        self.data.unconfirmed
    }

    pub fn connection(&mut self) -> TrieStorageConnection<'_, T> {
        TrieStorageConnection {
            result_cache_rollback: None,
            _direct_hash_guard: None,
            patch_cache_rollback: None,
            db: &self.db,
            db_path: self.db_path,
            data: &mut self.data,
            blobs: self.blobs.as_mut(),
            cache: &mut self.cache,
            hash_calculation_mode: self.hash_calculation_mode,
            compress: self.compress,
            mmap: self.mmap,

            #[cfg(test)]
            test_genesis_block: &mut self.test_genesis_block,
        }
    }
}

#[cfg(test)]
impl<T: MarfTrieId> ReopenedTrieStorageConnection<'_, T> {
    pub fn read_node<'b>(
        &'b mut self,
        ptr: &TriePtr,
        scratch: &'b mut impl NodePatching,
    ) -> Result<ReadTrieNode<'b>, Error> {
        let block_id = self.data.cur_block_id.ok_or(Error::NotFoundError)?;
        read_patched_persisted_node(
            self.db,
            &self.data.record_context,
            self.blobs.as_ref(),
            self.data.unconfirmed_block_id,
            block_id,
            ptr.from_backptr(),
            self.data.cur_block_trie_offset,
            scratch,
            None,
        )
    }
}

/// Shared implementation for `TrieReadStorage::open_block`, used by both
/// `TrieStorageConnection` and `ReopenedTrieStorageConnection`.
///
/// `cache` is required because `get_block_id_caching` accesses [`BlockHashCache<T>`], which lives
/// on the storage struct alongside (not inside) `TrieStorageTransientData`.
fn open_block_impl<T: MarfTrieId>(
    data: &mut TrieStorageTransientData<T>,
    db: &Connection,
    cache: &mut BlockHashCache<T>,

    bhh: &T,
) -> Result<(), Error> {
    if *bhh == data.cur_block && data.cur_block_id.is_some() {
        if data.unconfirmed
            && data.cur_block_id == trie_sql::get_unconfirmed_block_identifier(db, bhh)?
        {
            test_debug!(
                "{} unconfirmed trie block ID is {:?}",
                bhh,
                &data.cur_block_id
            );
            data.unconfirmed_block_id = data.cur_block_id;
        }

        return Ok(());
    }

    let sentinel = T::sentinel();
    if *bhh == sentinel {
        let block_id_opt = get_block_id_caching_impl(data.unconfirmed, cache, db, bhh).ok();
        data.set_block(sentinel, block_id_opt);

        return Ok(());
    }

    if let Some((ref uncommitted_bhh, _)) = data.uncommitted_writes {
        if uncommitted_bhh == bhh {
            if data.unconfirmed
                && data.cur_block_id == trie_sql::get_unconfirmed_block_identifier(db, bhh)?
            {
                test_debug!(
                    "{} unconfirmed trie block ID is {:?}",
                    bhh,
                    &data.cur_block_id
                );
                data.unconfirmed_block_id = data.cur_block_id;
            }
            data.set_block(bhh.clone(), None);

            return Ok(());
        }
    }

    if data.unconfirmed {
        if let Some(block_id) = trie_sql::get_unconfirmed_block_identifier(db, bhh)? {
            data.set_block(bhh.clone(), Some(block_id));

            test_debug!("{} unconfirmed trie block ID is {}", bhh, block_id);
            data.unconfirmed_block_id = Some(block_id);
            return Ok(());
        }
    }

    let block_id = get_block_id_caching_impl(data.unconfirmed, cache, db, bhh).map_err(|e| {
        test_debug!("Failed to open {:?}: {:?}", bhh, e);
        e
    })?;

    data.set_block(bhh.clone(), Some(block_id));

    Ok(())
}

/// Shared implementation for `TrieReadStorage::open_block_known_id`, used by both
/// `TrieStorageConnection` and `ReopenedTrieStorageConnection`.
///
/// Panics if `bhh` matches the currently-being-built uncommitted block (programming error).
fn open_block_known_id_impl<T: MarfTrieId>(
    data: &mut TrieStorageTransientData<T>,
    bhh: &T,
    id: u32,
) -> Result<(), Error> {
    if *bhh == data.cur_block && data.cur_block_id.is_some() {
        return Ok(());
    }

    if let Some((ref uncommitted_bhh, _)) = data.uncommitted_writes {
        if uncommitted_bhh == bhh {
            panic!("BUG: passed id of a currently building block");
        }
    }

    data.set_block(bhh.clone(), Some(id));
    Ok(())
}

/// Inner implementation of `BlockMap::get_block_id_caching` shared across storage types.
/// Extracted here so that `open_block_impl` can replicate caching behavior without
/// borrowing the full storage struct.
fn get_block_id_caching_impl<T: MarfTrieId>(
    unconfirmed: bool,
    cache: &mut BlockHashCache<T>,
    db: &Connection,
    block_hash: &T,
) -> Result<u32, Error> {
    if unconfirmed {
        trie_sql::get_block_identifier(db, block_hash)
    } else if let Some(id) = cache.load_block_id(block_hash) {
        Ok(id)
    } else {
        let id = trie_sql::get_block_identifier(db, block_hash)?;
        cache.store_block_hash(id, block_hash.clone());
        Ok(id)
    }
}

/// Patch-aware per-node read shared by [`TrieStorageConnection`] and
/// [`ReopenedTrieStorageConnection`].
///
/// Inlines the dispatch from `inner_read_persisted_trie_item` (blobs vs. SQL, unconfirmed
/// guard) and runs the full patch-chasing loop. Both storage types call this from their
/// `TrieReadStorage::read_node_with_state` impls.
fn read_patched_persisted_node<'b>(
    db: &Connection,
    record_context: &RecordContext,
    blobs: Option<&TrieFile>,
    unconfirmed_block_id: Option<u32>,
    mut block_id: u32,
    mut ptr: TriePtr,
    cur_block_trie_offset: Option<u64>,
    scratch: &'b mut impl NodePatching,
    mut first_patch: Option<(TrieHash, u8, &[u8])>,
) -> Result<ReadTrieNode<'b>, Error> {
    let target_block_id = block_id;
    let mut node_hash_opt = None;
    let mut patches = scratch.take_patch_chain_buf();
    let mut trie_offset_hint = cur_block_trie_offset;

    for _ in 0..=MAX_PATCH_DEPTH {
        #[cfg(feature = "marf-read-bench-counters")]
        read_bench::update(|c| c.resolved_item_reads += 1);

        let read_result = if let Some((hash, marker, payload)) = first_patch.take() {
            scratch
                .decode_patch_from_parts(marker, payload)
                .map(|_| ReadTrieItem::from_patch(scratch.patch(), Some(hash)))
        } else if unconfirmed_block_id == Some(block_id) {
            trace!("Read persisted node from unconfirmed block id {block_id}");
            trie_sql::read_trie_item(db, block_id, &ptr, scratch, record_context.format)
        } else {
            match blobs {
                Some(blobs) => blobs.read_trie_item(db, block_id, &ptr, trie_offset_hint, scratch),
                None => {
                    trie_sql::read_trie_item(db, block_id, &ptr, scratch, record_context.format)
                }
            }
        };
        let read = match read_result {
            Ok(read) => read,
            Err(e) => {
                scratch.restore_patch_chain_buf(patches);
                return Err(e);
            }
        };
        // Clear the hint after the first iteration — subsequent reads chase into
        // different blocks via backptrs and need fresh offset lookups.
        trie_offset_hint = None;
        let ReadTrieItem { hash, kind, .. } = read;

        match kind {
            ReadTrieItemKind::Node(_) => {
                let node_hash = node_hash_opt.or(hash);
                if node_hash.is_none() && !patches.is_empty() {
                    scratch.restore_patch_chain_buf(patches);
                    return Err(Error::CorruptionError(
                        "Patch chain terminates in a hashless leaf".into(),
                    ));
                }
                if !patches.is_empty() {
                    patches.reverse();
                    if let Err(e) = scratch.apply_patches_in_place(&patches, target_block_id) {
                        scratch.restore_patch_chain_buf(patches);
                        return Err(e);
                    }
                }

                let patch_depth = patches.len();
                let transient_meta = scratch.transient_meta();
                scratch.restore_patch_chain_buf(patches);
                let mut node = ReadTrieNode::from_state_borrowed(scratch.get_ref(), node_hash)
                    .with_patch_depth(patch_depth);
                if let Some(meta) = transient_meta {
                    node = node.with_transient_meta(meta);
                }
                return Ok(node);
            }
            ReadTrieItemKind::Patch(_) => {
                #[cfg(feature = "marf-read-bench-counters")]
                read_bench::update(|c| c.patch_records += 1);

                let node_patch = scratch.take_patch();
                trace!(
                    "read_patched_persisted_node({block_id}): at {ptr:?} read patch {node_patch:?} (original hash is {hash:?})"
                );
                let new_ptr = node_patch.ptr.from_backptr();
                let new_block_id = node_patch.ptr.back_block();

                patches.push(PatchChainEntry {
                    block_id,
                    ptr,
                    patch: node_patch,
                });

                ptr = new_ptr;
                block_id = new_block_id;
                if node_hash_opt.is_none() {
                    node_hash_opt = hash;
                }
            }
        }
    }
    scratch.restore_patch_chain_buf(patches);
    Err(Error::NodeTooDeep)
}

impl<T: MarfTrieId, Db: Deref<Target = Connection>> TrieStorageConnection<'_, T, Db> {
    /// Identify a committed ancestor-index base, falling back on unsupported trie views.
    fn indexed_ancestor_base(
        &mut self,
        block: &T,
        height: u32,
        target: u32,
    ) -> Result<Option<(u32, T, u32)>, Error> {
        if self.data.unconfirmed
            || self.data.squash_info.is_some()
            || self.data.direct_hash_index.is_none()
        {
            return Ok(None);
        }
        let (base, base_height) = match &self.data.uncommitted_writes {
            Some((active, state)) if active == block => {
                let trie = state.trie_ram_ref();
                if !trie.ancestry_safe || height == 0 || target >= height {
                    return Ok(None);
                }
                (trie.parent.clone(), height - 1)
            }
            _ => (block.clone(), height),
        };
        let id = match self.get_block_id_caching(&base) {
            Ok(id) => id,
            Err(Error::NotFoundError | Error::SQLError(rusqlite::Error::QueryReturnedNoRows)) => {
                return Ok(None)
            }
            Err(error) => return Err(error),
        };
        Ok(Some((id, base, base_height)))
    }
}

impl<T: MarfTrieId, Db: Deref<Target = Connection>> TrieReadStorage<T>
    for TrieStorageConnection<'_, T, Db>
{
    fn indexed_ancestor_root(
        &mut self,
        block: &T,
        height: u32,
        target: u32,
    ) -> Result<Option<TrieHash>, Error> {
        let Some((id, base, base_height)) = self.indexed_ancestor_base(block, height, target)?
        else {
            return Ok(None);
        };
        Ok(self
            .data
            .direct_hash_index
            .as_ref()
            .and_then(|index| index.ancestor_root(&self.db, id, &base, base_height, target)))
    }

    fn indexed_ancestor(
        &mut self,
        block: &T,
        height: u32,
        target: u32,
    ) -> Result<Option<T>, Error> {
        let Some((id, base, base_height)) = self.indexed_ancestor_base(block, height, target)?
        else {
            return Ok(None);
        };
        Ok(self
            .data
            .direct_hash_index
            .as_ref()
            .and_then(|index| index.ancestor_hash(&self.db, id, &base, base_height, target)))
    }

    fn resolve_leaf_value(&mut self, leaf: &mut TrieLeaf) -> Result<(), Error> {
        self.data.record_context.resolve_leaf(leaf)
    }

    fn cached_result(&mut self, block: &T, path: &TrieHash) -> Option<Option<TrieLeaf>> {
        let (active, UncommittedState::RW(trie)) = self.data.uncommitted_writes.as_mut()? else {
            return None;
        };
        if active != block {
            return None;
        }
        trie.result_cache.get(path)
    }

    fn cache_result(&mut self, block: &T, path: TrieHash, value: Option<TrieLeaf>) {
        if let Some((active, UncommittedState::RW(trie))) = self.data.uncommitted_writes.as_mut() {
            if active == block {
                trie.result_cache.put(path, value);
            }
        }
    }

    fn read_node_with_state<'a, S: NodePatching>(
        &'a mut self,
        ptr: &TriePtr,
        state: &'a mut S,
    ) -> Result<ReadTrieNode<'a>, Error> {
        trace!("read_node({:?}): {:?}", &self.data.cur_block, ptr);

        #[cfg(feature = "marf-read-bench-counters")]
        read_bench::update(|c| c.logical_node_reads += 1);
        self.data.read_count += 1;
        if is_backptr(ptr.id()) {
            self.data.read_backptr_count += 1;
        } else if ptr.id() == TrieNodeID::Leaf as u8 {
            self.data.read_leaf_count += 1;
        } else {
            self.data.read_node_count += 1;
        }

        let clear_ptr = ptr.from_backptr();

        if self.has_open_uncommitted_trie() {
            let (_, uncommitted_trie) = self
                .data
                .uncommitted_writes
                .as_mut()
                .expect("BUG: uncommitted state disappeared while it was open");
            #[cfg(feature = "marf-read-bench-counters")]
            read_bench::update(|c| c.ram_node_reads += 1);
            return uncommitted_trie.read_node(&clear_ptr);
        }

        let Some(id) = self.data.cur_block_id else {
            debug!("Not found (no file is open)");
            return Err(Error::NotFoundError);
        };

        let trie_offset = if self.data.unconfirmed_block_id == Some(id) {
            None
        } else {
            self.current_trie_offset_hint(id)
        };

        #[cfg(feature = "marf-read-bench-counters")]
        if self.data.unconfirmed_block_id != Some(id) && clear_ptr == self.root_trieptr() {
            read_bench::update(|c| c.committed_root_requests += 1);
        }

        // Uncommitted and persisted unconfirmed roots can change in place; never cache them.
        if self.data.root_node_cache_enabled
            && self.data.unconfirmed_block_id != Some(id)
            && clear_ptr == self.root_trieptr()
        {
            let cache = self
                .data
                .root_node_cache
                .get_or_insert_with(|| Box::new(ArrayLru::new()));
            let cached = cache.contains_key(&id);
            #[cfg(feature = "marf-read-bench-counters")]
            read_bench::update(|c| {
                if cached {
                    c.root_cache_hits += 1;
                } else {
                    c.root_cache_misses += 1;
                }
            });
            if !cached {
                let read = read_patched_persisted_node(
                    &self.db,
                    &self.data.record_context,
                    self.blobs.as_deref(),
                    self.data.unconfirmed_block_id,
                    id,
                    clear_ptr,
                    trie_offset,
                    state,
                    None,
                )?;
                let patch_depth = read.patch_depth;
                let (node, hash) = read.into_owned_node()?;
                cache.put(id, (node, hash, patch_depth));
            }
            let (node, hash, patch_depth) = cache.get(&id).expect("root was found or inserted");
            return Ok(ReadTrieNode::from_borrowed(TrieNodeRef::from(node), *hash)
                .with_patch_depth(*patch_depth)
                .with_transient_meta(TrieNodeTransientMeta::from_node(node)));
        }

        let patch_cache_enabled =
            self.data.resolved_patch_cache.enabled() && self.data.unconfirmed_block_id != Some(id);
        // A row ID may be reused after rollback; use the immutable block identity.
        let patch_cache_key = (self.data.cur_block.clone(), clear_ptr.ptr());
        let (patch_cache_generation, cached) = if patch_cache_enabled {
            self.data.resolved_patch_cache.get(&patch_cache_key)
        } else {
            (0, None)
        };
        if let Some(node) = cached {
            #[cfg(feature = "marf-read-bench-counters")]
            read_bench::update(|c| c.resolved_patch_cache_hits += 1);
            self.data.resolved_patch_node = Some(node);
            let (node, hash, patch_depth) = self
                .data
                .resolved_patch_node
                .as_deref()
                .expect("retained resolved node");
            return Ok(ReadTrieNode::from_borrowed(TrieNodeRef::from(node), *hash)
                .with_patch_depth(*patch_depth)
                .with_transient_meta(TrieNodeTransientMeta::from_node(node)));
        }

        // Keep the first mapped patch body so the resolver does not probe it again.
        let mut first_patch = None;
        if self.data.unconfirmed_block_id != Some(id) {
            if let Some(ref blobs) = self.blobs {
                match blobs.read_trie_item_borrowed(&self.db, id, &clear_ptr, trie_offset)? {
                    Some(MappedTrieItem::Node(node)) => {
                        #[cfg(feature = "marf-read-bench-counters")]
                        read_bench::update(|c| c.mmap_node_returns += 1);
                        return Ok(node);
                    }
                    Some(MappedTrieItem::Patch {
                        hash,
                        marker,
                        payload,
                    }) => first_patch = Some((hash, marker, payload)),
                    None => {}
                }
            }
        }

        let read = read_patched_persisted_node(
            &self.db,
            &self.data.record_context,
            self.blobs.as_deref(),
            self.data.unconfirmed_block_id,
            id,
            clear_ptr,
            trie_offset,
            state,
            first_patch,
        )?;
        if patch_cache_enabled && read.patch_depth > 0 {
            #[cfg(feature = "marf-read-bench-counters")]
            read_bench::update(|c| c.resolved_patch_cache_misses += 1);
            let patch_depth = read.patch_depth;
            let (node, hash) = read.into_owned_node()?;
            let resolved = Arc::new((node, hash, patch_depth));
            self.data.resolved_patch_cache.put_if_current(
                patch_cache_generation,
                patch_cache_key,
                Arc::clone(&resolved),
            );
            self.data.resolved_patch_node = Some(resolved);
            let (node, hash, patch_depth) = self
                .data
                .resolved_patch_node
                .as_deref()
                .expect("retained resolved node");
            return Ok(ReadTrieNode::from_borrowed(TrieNodeRef::from(node), *hash)
                .with_patch_depth(*patch_depth)
                .with_transient_meta(TrieNodeTransientMeta::from_node(node)));
        }
        Ok(read)
    }

    fn open_block(&mut self, bhh: &T) -> Result<(), Error> {
        trace!(
            "open_block({}) (unconfirmed={:?},{}) in {}",
            bhh,
            &self.data.unconfirmed_block_id,
            self.unconfirmed(),
            self.db_path
        );
        open_block_impl(self.data, &self.db, self.cache, bhh)
    }

    fn open_block_known_id(&mut self, bhh: &T, id: u32) -> Result<(), Error> {
        trace!(
            "open_block_known_id({},{}) (unconfirmed={:?},{}) from {},{:?} in {}",
            bhh,
            id,
            &self.data.unconfirmed_block_id,
            self.unconfirmed(),
            &self.data.cur_block,
            &self.data.cur_block_id,
            self.db_path,
        );
        open_block_known_id_impl(self.data, bhh, id)
    }

    fn get_cur_block_and_id(&self) -> (T, Option<u32>) {
        (self.data.cur_block.clone(), self.data.cur_block_id)
    }

    fn root_trieptr(&self) -> TriePtr {
        TriePtr::new(TrieNodeID::Node256 as u8, 0, self.root_ptr())
    }

    fn read_node_hash(&mut self, ptr: &TriePtr) -> Result<TrieHash, Error> {
        if self.has_open_uncommitted_trie() {
            let (_, uncommitted_trie) = self
                .data
                .uncommitted_writes
                .as_mut()
                .expect("BUG: uncommitted state disappeared while it was open");
            return uncommitted_trie.read_node_hash(ptr);
        }

        match self.data.cur_block_id {
            Some(block_id) => {
                if ptr.ptr() == self.root_ptr() && !is_backptr(ptr.id()) {
                    if let Some(index) = &self.data.direct_hash_index {
                        if let Some(hash) =
                            index.root_hash(&self.db, block_id, &self.data.cur_block)?
                        {
                            return Ok(hash);
                        }
                    }
                }
                let node_hash = self.inner_read_persisted_node_hash(block_id, ptr)?;

                Ok(node_hash)
            }
            None => {
                error!("Not found (no file is open)");
                Err(Error::NotFoundError)
            }
        }
    }

    fn read_node_type_id(&mut self, ptr: &TriePtr) -> Result<(TrieNodeID, TrieHash), Error> {
        let clear_ptr = ptr.from_backptr();

        if self.has_open_uncommitted_trie() {
            let (_, uncommitted_trie) = self
                .data
                .uncommitted_writes
                .as_mut()
                .expect("BUG: uncommitted state disappeared while it was open");
            let read_node = uncommitted_trie.read_node(&clear_ptr)?;
            let node_id = read_node
                .node_type()
                .filter(|node_id| *node_id != TrieNodeID::Patch)
                .ok_or_else(|| {
                    Error::CorruptionError("Unknown trie node type in uncommitted trie".to_string())
                })?;
            let hash = read_node.hash.ok_or_else(|| {
                Error::CorruptionError("Missing node hash in uncommitted trie read".to_string())
            })?;
            return Ok((node_id, hash));
        }

        match self.data.cur_block_id {
            Some(id) => {
                if self.blobs.is_some() && self.data.unconfirmed_block_id != Some(id) {
                    let trie_offset = self.current_trie_offset_hint(id);
                    let blobs = self
                        .blobs
                        .as_ref()
                        .expect("BUG: trie blobs disappeared after presence check");
                    blobs.read_node_type_id(&self.db, id, &clear_ptr, trie_offset)
                } else {
                    trie_sql::probe_node_type(&self.db, id, &clear_ptr, &self.data.record_context)
                }
            }
            None => Err(Error::NotFoundError),
        }
    }

    fn is_squashed(&self) -> bool {
        TrieStorageConnection::is_squashed(self)
    }

    fn squash_height(&self) -> Option<u32> {
        TrieStorageConnection::squash_height(self)
    }

    fn squashed_block_root_hash_by_height(&self, height: u32) -> Result<Option<TrieHash>, Error> {
        trie_sql::read_squashed_block_root_hash_by_height(self.sqlite_conn(), height)
    }

    fn squashed_block_height(&self, block_hash: &T) -> Result<Option<u32>, Error> {
        TrieStorageConnection::squashed_block_height(self, block_hash)
    }

    fn squashed_block_hash_by_height(&self, height: u32) -> Result<Option<T>, Error> {
        trie_sql::read_squashed_block_hash_by_height::<T>(self.sqlite_conn(), height)
    }

    fn check_historical_read_allowed(&self, block_hash: &T) -> Result<(), Error> {
        TrieStorageConnection::check_historical_read_allowed(self, block_hash)
    }

    fn set_cached_ancestor_hashes_bytes(&mut self, bhh: &T, bytes: Arc<[TrieHash]>) {
        self.data.set_ancestor_hashes_bytes(bhh, bytes);
    }

    fn check_cached_ancestor_hashes_bytes(&mut self, bhh: &T) -> Option<Arc<[TrieHash]>> {
        self.data.get_ancestor_hashes_bytes(bhh)
    }

    #[cfg(test)]
    fn test_genesis_block(&self) -> Option<T> {
        self.test_genesis_block.clone()
    }

    fn write_children_hashes_by_ptrs<W: Write + ?Sized>(
        &mut self,
        ptrs: &[TriePtr],
        w: &mut W,
    ) -> Result<(), Error> {
        trace!("write_children_hashes for {:?}", ptrs);

        if let Some((ref uncommitted_bhh, ref mut uncommitted_trie)) = self.data.uncommitted_writes
        {
            if &self.data.cur_block == uncommitted_bhh {
                let mut map = TrieSqlHashMapCursor {
                    direct_hash_index: self.data.direct_hash_index.as_ref(),
                    db: &self.db,
                    cache: self.cache,
                    unconfirmed: self.data.unconfirmed,
                };
                let res = Self::inner_write_children_hashes(
                    uncommitted_trie.trie_ram_mut(),
                    &mut map,
                    ptrs,
                    w,
                );
                return res;
            }
        }

        let block_id = self.data.cur_block_id.ok_or_else(|| {
            error!("Failed to get cur block as hash reader");
            Error::NotFoundError
        })?;
        // Unconfirmed tries remain in SQLite even when confirmed tries use a blob file.
        if self.blobs.is_some() && self.data.unconfirmed_block_id != Some(block_id) {
            let trie_offset = self.current_trie_offset_hint(block_id);
            let blobs = self
                .blobs
                .as_ref()
                .expect("BUG: trie blobs disappeared after presence check");
            let mut cursor = TrieFileNodeHashReader::new(&self.db, blobs, block_id, trie_offset);
            let mut map = TrieSqlHashMapCursor {
                direct_hash_index: self.data.direct_hash_index.as_ref(),
                db: &self.db,
                cache: self.cache,
                unconfirmed: self.data.unconfirmed,
            };
            let res = Self::inner_write_children_hashes(&mut cursor, &mut map, ptrs, w);
            res
        } else {
            let mut cursor = TrieSqlCursor {
                db: &self.db,
                record_context: &self.data.record_context,
                block_id,
            };
            let mut map = TrieSqlHashMapCursor {
                direct_hash_index: self.data.direct_hash_index.as_ref(),
                db: &self.db,
                cache: self.cache,
                unconfirmed: self.data.unconfirmed,
            };
            let res = Self::inner_write_children_hashes(&mut cursor, &mut map, ptrs, w);
            res
        }
    }
}

impl<T: MarfTrieId> TrieFileStorage<T> {
    /// Detect whether this MARF was produced by a squash operation and, if
    /// so, cache the squash metadata [`SquashInfo`].
    ///
    /// The metadata is read from the `marf_squash_info` SQL table.
    fn load_squash_info(&mut self) -> Result<(), Error> {
        let squash_info = trie_sql::read_squash_info(&self.db)?.map(|sql_info| SquashInfo {
            archival_marf_root_hash: sql_info.archival_marf_root_hash,
            squash_root_node_hash: sql_info.squash_root_node_hash,
            squash_height: sql_info.squash_height,
        });

        self.data.set_squash_info(squash_info);
        Ok(())
    }

    /// Returns cached squashing metadata, if present.
    pub fn squash_info(&self) -> Option<&SquashInfo> {
        self.data.squash_info.as_ref()
    }

    pub fn connection(&mut self) -> TrieStorageConnection<'_, T> {
        TrieStorageConnection {
            result_cache_rollback: None,
            _direct_hash_guard: None,
            patch_cache_rollback: None,
            db: &self.db,
            db_path: &self.db_path,
            data: &mut self.data,
            blobs: self.blobs.as_mut(),
            cache: &mut self.cache,
            hash_calculation_mode: self.hash_calculation_mode,
            compress: self.compress,
            mmap: self.mmap,

            #[cfg(test)]
            test_genesis_block: &mut self.test_genesis_block,
        }
    }

    /// Build a read-only storage connection which can be used for reads without modifying the
    ///  calling TrieFileStorage struct (i.e., the tip pointer is only changed in the connection)
    ///  but reusing the TrieFileStorage's existing SQLite Connection (avoiding the overhead of
    ///   `reopen_readonly`).
    pub fn reopen_connection(&self) -> Result<ReopenedTrieStorageConnection<'_, T>, Error> {
        let data = TrieStorageTransientData {
            uncommitted_writes: self.data.uncommitted_writes.clone(),
            squash_info: self.data.squash_info.clone(),
            root_node_cache: self.data.root_node_cache.clone(),
            root_node_cache_enabled: self.data.root_node_cache_enabled,
            resolved_patch_cache: self.data.resolved_patch_cache.clone(),
            result_cache_capacity: self.data.result_cache_capacity,
            record_context: self.data.record_context.clone(),
            direct_hash_index: self.data.direct_hash_index.clone(),
            ..TrieStorageTransientData::new(
                self.data.cur_block.clone(),
                self.data.cur_block_id,
                true,
                self.unconfirmed(),
            )
        };
        // perf note: should we attempt to clone the cache
        let cache = BlockHashCache::new();
        let blobs = self
            .blobs
            .as_ref()
            .map(TrieFile::reopen_readonly)
            .transpose()?;
        let hash_calculation_mode = self.hash_calculation_mode;
        Ok(ReopenedTrieStorageConnection {
            db_path: &self.db_path,
            db: &self.db,
            blobs,
            data,
            cache,
            hash_calculation_mode,
            compress: self.compress,
            mmap: self.mmap,
            #[cfg(test)]
            test_genesis_block: self.test_genesis_block.clone(),
        })
    }

    pub fn transaction(&mut self) -> Result<TrieStorageTransaction<'_, T>, Error> {
        if self.readonly() {
            return Err(Error::ReadOnlyError);
        }
        let tx = tx_begin_immediate(&mut self.db)?;

        let direct_hash_guard = self
            .data
            .direct_hash_index
            .as_mut()
            .map(|index| index.begin(&tx))
            .transpose()?;
        let result_cache_rollback = Some(self.data.result_cache_control.guard());
        Ok(TrieStorageConnection {
            result_cache_rollback,
            _direct_hash_guard: direct_hash_guard,
            patch_cache_rollback: Some(self.data.resolved_patch_cache.rollback_guard()),
            db: tx,
            db_path: &self.db_path,
            data: &mut self.data,
            blobs: self.blobs.as_mut(),
            cache: &mut self.cache,
            hash_calculation_mode: self.hash_calculation_mode,
            compress: self.compress,
            mmap: self.mmap,

            #[cfg(test)]
            test_genesis_block: &mut self.test_genesis_block,
        })
    }

    pub fn sqlite_conn(&self) -> &Connection {
        &self.db
    }

    pub fn sqlite_tx(&mut self) -> Result<Transaction<'_>, db_error> {
        tx_begin_immediate(&mut self.db)
    }

    pub fn into_sqlite_conn(self) -> Connection {
        self.db
    }

    fn open_opts(
        db_path: &str,
        readonly: bool,
        unconfirmed: bool,
        marf_opts: MARFOpenOpts,
    ) -> Result<TrieFileStorage<T>, Error> {
        let mut create_flag = false;
        let open_flags = if db_path != ":memory:" {
            match fs::metadata(db_path) {
                Err(e) => {
                    if e.kind() == io::ErrorKind::NotFound {
                        // need to create
                        if !readonly {
                            create_flag = true;
                            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
                        } else {
                            return Err(Error::NotFoundError);
                        }
                    } else {
                        return Err(Error::IOError(e));
                    }
                }
                Ok(_md) => {
                    // can just open
                    if !readonly {
                        OpenFlags::SQLITE_OPEN_READ_WRITE
                    } else {
                        OpenFlags::SQLITE_OPEN_READ_ONLY
                    }
                }
            }
        } else {
            create_flag = true;
            if !readonly {
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
            } else {
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_CREATE
            }
        };

        let mut db = marf_sqlite_open(db_path, open_flags, false)?;
        let db_path = db_path.to_string();

        if create_flag {
            trie_sql::create_tables_if_needed(&mut db)?;
        }

        let mut blobs = if marf_opts.external_blobs {
            Some(TrieFile::from_db_path(&db_path, readonly, marf_opts.mmap)?)
        } else {
            None
        };

        if readonly {
            trie_sql::ensure_no_migration_necessary::<T>(&mut db)?;
        } else {
            let prev_schema_version = trie_sql::migrate_tables_if_needed::<T>(&mut db)?;
            // Only the schema-2 migration moved trie blobs to external storage.
            // Later schema migrations should not rewrite the blob.
            if prev_schema_version < trie_sql::SQL_MARF_EXTERNAL_BLOBS_SCHEMA_VERSION
                || marf_opts.force_db_migrate
            {
                if let Some(blobs) = blobs.as_mut() {
                    if TrieFile::exists(&db_path)? {
                        // migrate blobs out of the old DB
                        blobs.export_trie_blobs::<T>(&db, &db_path)?;
                    }
                }
            }
            if trie_sql::detect_partial_migration(&db)? {
                panic!(
                    "PARTIAL MIGRATION DETECTED! This is an irrecoverable error. You will need to restart your node from genesis."
                );
            }
        }

        debug!(
            "Opened TrieFileStorage {}; external blobs: {}",
            db_path,
            blobs.is_some()
        );

        let cache = BlockHashCache::new();

        let mut ret = TrieFileStorage {
            db_path,
            db,
            cache,
            blobs,
            hash_calculation_mode: marf_opts.hash_calculation_mode,
            compress: marf_opts.compress,
            mmap: marf_opts.mmap,

            data: TrieStorageTransientData {
                root_node_cache_enabled: marf_opts.root_node_cache,
                resolved_patch_cache: SharedLru::new(marf_opts.resolved_patch_cache_capacity),
                result_cache_capacity: marf_opts.result_cache_capacity,
                ..TrieStorageTransientData::new(T::sentinel(), None, readonly, unconfirmed)
            },

            // used in testing in order to short-circuit block-height lookups
            //   when the trie struct is tested outside of marf.rs usage
            #[cfg(test)]
            test_genesis_block: None,
        };

        ret.data.record_context.format = NodeRecordFormat::from_database(&ret.db)?;
        if let Some(blobs) = ret.blobs.as_mut() {
            blobs.set_record_context(ret.data.record_context.clone());
        }
        ret.load_squash_info()?;
        ret.data.direct_hash_index = DirectHashIndex::open(&ret.db, Path::new(&ret.db_path))?;
        Ok(ret)
    }

    #[cfg(test)]
    pub fn new_memory(marf_opts: MARFOpenOpts) -> Result<TrieFileStorage<T>, Error> {
        TrieFileStorage::open(":memory:", marf_opts)
    }

    pub fn open(db_path: &str, marf_opts: MARFOpenOpts) -> Result<TrieFileStorage<T>, Error> {
        TrieFileStorage::open_opts(db_path, false, false, marf_opts)
    }

    pub fn open_readonly(
        db_path: &str,
        marf_opts: MARFOpenOpts,
    ) -> Result<TrieFileStorage<T>, Error> {
        TrieFileStorage::open_opts(db_path, true, false, marf_opts)
    }

    pub fn open_unconfirmed(
        db_path: &str,
        marf_opts: MARFOpenOpts,
    ) -> Result<TrieFileStorage<T>, Error> {
        TrieFileStorage::open_opts(db_path, false, true, marf_opts)
    }

    pub fn readonly(&self) -> bool {
        self.data.readonly
    }

    /// Return true if this storage connection was opened with the intention of operating on an
    /// unconfirmed trie -- i.e. this is a storage connection for reading and writing a persisted
    /// scratch space trie, such as one for storing unconfirmed microblock transactions in the
    /// chain state.
    pub fn unconfirmed(&self) -> bool {
        self.data.unconfirmed
    }

    /// Returns true if there are uncommitted writes in the storage.
    pub fn has_uncommitted_writes(&self) -> bool {
        self.data.uncommitted_writes.is_some()
    }

    /// Attach the immutable value source used by logical reads and proof hashing.
    pub fn set_value_extent_resolver(&mut self, resolver: Arc<dyn ValueExtentResolver>) {
        self.data.record_context.value_resolver = Some(resolver);
        if let Some(blobs) = self.blobs.as_mut() {
            blobs.set_record_context(self.data.record_context.clone());
        }
    }

    /// Select the published physical layout before reading or writing this store.
    pub fn set_record_format(&mut self, format: NodeRecordFormat) {
        self.data.record_context.format = format;
        if let Some(blobs) = self.blobs.as_mut() {
            blobs.set_record_context(self.data.record_context.clone());
        }
    }

    /// Physical layout selected when this store opened.
    pub fn record_format(&self) -> NodeRecordFormat {
        self.data.record_context.format
    }

    /// Clone the immutable value source without retaining a storage borrow.
    pub fn value_extent_resolver(&self) -> Option<Arc<dyn ValueExtentResolver>> {
        self.data.record_context.value_resolver.clone()
    }

    /// Reopen this store read-only, sharing its immutable value source.
    pub fn reopen_readonly(&self) -> Result<TrieFileStorage<T>, Error> {
        trace!("Make read-only view of TrieFileStorage: {}", &self.db_path);

        // TODO: borrow self.uncommitted_writes; don't copy them
        let data = TrieStorageTransientData {
            uncommitted_writes: self.data.uncommitted_writes.clone(),
            squash_info: self.data.squash_info.clone(),
            root_node_cache: self.data.root_node_cache.clone(),
            root_node_cache_enabled: self.data.root_node_cache_enabled,
            resolved_patch_cache: self.data.resolved_patch_cache.clone(),
            result_cache_capacity: self.data.result_cache_capacity,
            record_context: self.data.record_context.clone(),
            direct_hash_index: self.data.direct_hash_index.clone(),
            ..TrieStorageTransientData::new(
                self.data.cur_block.clone(),
                self.data.cur_block_id,
                true,
                self.unconfirmed(),
            )
        };

        build_readonly_storage(
            &self.db_path,
            self.blobs.as_ref(),
            self.hash_calculation_mode,
            self.compress,
            self.mmap,
            data,
            #[cfg(test)]
            self.test_genesis_block.clone(),
        )
    }
}

/// Build a fresh read-only `TrieFileStorage` from the given path and pre-constructed
/// transient data. Both `reopen_readonly` implementations delegate here so the
/// open-DB / open-blobs / construct-struct pattern lives in one place.
fn build_readonly_storage<T: MarfTrieId>(
    db_path: &str,
    source_blobs: Option<&TrieFile>,
    hash_calculation_mode: TrieHashCalculationMode,
    compress: bool,
    mmap: bool,
    data: TrieStorageTransientData<T>,
    #[cfg(test)] test_genesis_block: Option<T>,
) -> Result<TrieFileStorage<T>, Error> {
    let db = marf_sqlite_open(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY, false)?;
    let blobs = source_blobs.map(TrieFile::reopen_readonly).transpose()?;
    let cache = BlockHashCache::new();
    Ok(TrieFileStorage {
        db_path: db_path.to_string(),
        db,
        blobs,
        cache,

        hash_calculation_mode,
        compress,
        mmap,
        data,
        #[cfg(test)]
        test_genesis_block,
    })
}

impl<'a, T: MarfTrieId> TrieStorageConnection<'a, T, Transaction<'a>> {
    /// reopen this transaction as a read-only marf.
    ///  _does not_ preserve the cur_block/open tip
    pub fn reopen_readonly(&self) -> Result<TrieFileStorage<T>, Error> {
        trace!(
            "Make read-only view of TrieStorageTransaction: {}",
            &self.db_path
        );

        let data = TrieStorageTransientData {
            squash_info: self.data.squash_info.clone(),
            root_node_cache: self.data.root_node_cache.clone(),
            root_node_cache_enabled: self.data.root_node_cache_enabled,
            resolved_patch_cache: self.data.resolved_patch_cache.clone(),
            result_cache_capacity: self.data.result_cache_capacity,
            record_context: self.data.record_context.clone(),
            direct_hash_index: self.data.direct_hash_index.clone(),
            ..TrieStorageTransientData::new(T::sentinel(), None, true, self.unconfirmed())
        };

        build_readonly_storage(
            self.db_path,
            self.blobs.as_deref(),
            self.hash_calculation_mode,
            self.compress,
            self.mmap,
            data,
            #[cfg(test)]
            self.test_genesis_block.clone(),
        )
    }

    /// Run `cls` with a mutable reference to the inner trie blobs opt.
    pub(crate) fn with_trie_blobs<F, R>(&mut self, cls: F) -> R
    where
        F: FnOnce(&Connection, &mut Option<&mut TrieFile>) -> R,
    {
        let mut blobs = self.blobs.take();
        let res = cls(&self.db, &mut blobs);
        self.blobs = blobs;
        res
    }

    /// Inner method for flushing the UncommittedState's TrieRAM to disk.
    fn inner_flush(&mut self, flush_options: FlushOptions<'_, T>) -> Result<(), Error> {
        self.data.result_cache_control.invalidate();
        // save the currently-buffered Trie to disk, and atomically put it into place (possibly to
        // a different block than the one opened, as indicated by final_bhh).
        // Runs once -- subsequent calls are no-ops.
        // Panics on a failure to rename the Trie file into place (i.e. if the actual commitment
        // fails).
        self.clear_cached_ancestor_hashes_bytes();
        if self.data.readonly {
            return Err(Error::ReadOnlyError);
        }
        if let Some((bhh, trie_ram)) = self.data.uncommitted_writes.take() {
            let ancestry_safe = trie_ram.trie_ram_ref().ancestry_safe;
            trace!("Buffering block flush started.");

            // Enable MARF compression only when:
            // - Compression is explicitly requested, and
            // - The flush option is *not* `FlushOptions::UnconfirmedTable`, which is used when
            //   writing an unconfirmed trie for Stacks 2.x.
            //
            //   Compression is intentionally disabled for unconfirmed tries to avoid regressions
            //   in `TrieRAM::load`, which is responsible for loading these unconfirmed structures.
            let marf_compression_enabled =
                self.compress && !matches!(flush_options, FlushOptions::UnconfirmedTable);

            #[cfg(feature = "commit-residency-diagnostics")]
            let _serialize = stacks_profiler::diagnostic_span!("Flush: Serialize trie");
            let mut cursor = Cursor::new(Vec::new());
            if marf_compression_enabled {
                trie_ram.dump_compressed(self, &mut cursor, &bhh)?;
            } else {
                trie_ram.dump(self, &mut cursor, &bhh)?;
            }
            let buffer = cursor.into_inner();
            #[cfg(feature = "commit-residency-diagnostics")]
            drop(_serialize);
            #[cfg(feature = "commit-residency-diagnostics")]
            let _write = stacks_profiler::diagnostic_span!("Flush: Store trie");

            trace!("Buffering block flush finished.");
            debug!("Flush: {} to {}", &bhh, flush_options);

            let block_id = match flush_options {
                FlushOptions::CurrentHeader => {
                    if self.unconfirmed() {
                        return Err(Error::UnconfirmedError);
                    }
                    self.with_trie_blobs(|db, blobs| match blobs {
                        Some(blobs) => blobs.store_trie_blob(db, &bhh, &buffer),
                        None => {
                            test_debug!("Stored trie blob {bhh} to db");
                            trie_sql::write_trie_blob(db, &bhh, &buffer)
                        }
                    })?
                }
                FlushOptions::NewHeader(real_bhh) => {
                    // If we opened a block with a given hash, but want to store it as a block with a *different*
                    // hash, then call this method to update the internal storage state to make it so.  This is
                    // necessary for validating blocks in the blockchain, since the miner will always build a
                    // block whose hash is all 0's (since it can't know the final block hash).  As such, a peer
                    // will process a block as if it's hash is all 0's (in order to validate the state root), and
                    // then use this method to switch over the block hash to the "real" block hash.
                    if self.data.unconfirmed {
                        return Err(Error::UnconfirmedError);
                    }

                    let new_block_id = self.with_trie_blobs(|db, blobs| match blobs {
                        Some(blobs) => blobs.store_trie_blob(db, real_bhh, &buffer),
                        None => {
                            test_debug!("Stored trie blob {} to db", real_bhh);
                            trie_sql::write_trie_blob(db, real_bhh, &buffer)
                        }
                    })?;
                    self.data.set_block(real_bhh.clone(), Some(new_block_id));
                    new_block_id
                }
                FlushOptions::MinedTable(real_bhh) => {
                    if self.unconfirmed() {
                        return Err(Error::UnconfirmedError);
                    }
                    trie_sql::write_trie_blob_to_mined(&self.db, real_bhh, &buffer)?
                }
                FlushOptions::UnconfirmedTable => {
                    if !self.unconfirmed() {
                        return Err(Error::UnconfirmedError);
                    }
                    self.data.clear_root_node_cache();
                    self.data.resolved_patch_cache.clear();
                    trie_sql::write_trie_blob_to_unconfirmed(&self.db, &bhh, &buffer)?
                }
            };

            let block = match flush_options {
                FlushOptions::CurrentHeader => Some(&bhh),
                FlushOptions::NewHeader(real_bhh) => Some(real_bhh),
                _ => None,
            };
            if let Some(block) = block {
                if !ancestry_safe {
                    super::direct_hash_index::exclude_ancestry(&self.db, block_id)?;
                }
                if let Some(index) = &self.data.direct_hash_index {
                    let header =
                        BlobHeader::<T>::parse_format(self.data.record_context.format, &buffer)?;
                    index.append_with_parent(
                        &self.db,
                        block_id,
                        block,
                        &header.root_hash,
                        &header.parent_hash,
                    )?;
                }
            }
            trie_sql::drop_lock(&self.db, &bhh)?;

            debug!("Flush: identifier of {} is {}", flush_options, block_id);
        }

        Ok(())
    }

    /// Flush uncommitted state to disk.
    pub fn flush(&mut self) -> Result<(), Error> {
        if self.data.unconfirmed {
            self.inner_flush(FlushOptions::UnconfirmedTable)
        } else {
            self.inner_flush(FlushOptions::CurrentHeader)
        }
    }

    /// Flush uncommitted state to disk, but under the given block hash.
    pub fn flush_to(&mut self, bhh: &T) -> Result<(), Error> {
        self.inner_flush(FlushOptions::NewHeader(bhh))
    }

    /// Flush uncommitted state to disk for a mined block (i.e. not part of the chainstate, and not
    /// an ancestor of any block), and do so under a given block hash.
    pub fn flush_mined(&mut self, bhh: &T) -> Result<(), Error> {
        self.inner_flush(FlushOptions::MinedTable(bhh))
    }

    /// Drop the uncommitted state and any associated cached state.
    pub fn drop_extending_trie(&mut self) {
        self.data.result_cache_control.invalidate();
        self.clear_cached_ancestor_hashes_bytes();
        if !self.data.readonly {
            if let Some((ref bhh, _)) = self.data.uncommitted_writes.take() {
                trie_sql::drop_lock(&self.db, bhh)
                    .expect("Corruption: Failed to drop the extended trie lock");
            }
            self.data.clear_root_node_cache();
            self.data.resolved_patch_cache.clear();
            self.data.uncommitted_writes = None;
            self.data.clear_block_id();
            self.data.trie_ancestor_hash_bytes_cache = None;
        }
    }

    /// Drop the unconfirmed state and uncommitted state.
    pub fn drop_unconfirmed_trie(&mut self, bhh: &T) {
        self.data.result_cache_control.invalidate();
        self.clear_cached_ancestor_hashes_bytes();
        if !self.data.readonly && self.data.unconfirmed {
            trie_sql::drop_unconfirmed_trie(&self.db, bhh)
                .expect("Corruption: Failed to drop unconfirmed trie");
            trie_sql::drop_lock(&self.db, bhh)
                .expect("Corruption: Failed to drop the extended trie lock");
            self.data.clear_root_node_cache();
            self.data.resolved_patch_cache.clear();
            self.data.uncommitted_writes = None;
            self.data.clear_block_id();
            self.data.trie_ancestor_hash_bytes_cache = None;
        }
    }

    /// Seal the inner uncommitted TrieRAM and return the MARF root hash.
    /// Only works if there's an uncommitted TrieRAM extension; panics if not.
    pub fn seal(&mut self) -> Result<TrieHash, Error> {
        if let Some((bhh, trie_ram)) = self.data.uncommitted_writes.take() {
            let sealed_trie_ram = trie_ram.seal(self)?;
            let root_hash = match &sealed_trie_ram {
                UncommittedState::Sealed(_, root_hash) => *root_hash,
                _ => {
                    unreachable!("FATAL: .seal() did not make a sealed trieram");
                }
            };
            self.data.uncommitted_writes = Some((bhh, sealed_trie_ram));
            Ok(root_hash)
        } else {
            panic!("FATAL: tried to a .seal() a trie that was not extended");
        }
    }

    /// Extend the forest of Tries to include a new confirmed block.
    /// Fails if the block already exists, or if the storage is read-only, or open
    /// only for unconfirmed state.
    pub fn extend_to_block(&mut self, bhh: &T) -> Result<(), Error> {
        self.clear_cached_ancestor_hashes_bytes();
        if self.data.readonly {
            return Err(Error::ReadOnlyError);
        }
        if self.data.unconfirmed {
            return Err(Error::UnconfirmedError);
        }

        if self.get_block_id_caching(bhh).is_ok() {
            warn!("Block already exists: {}", &bhh);
            return Err(Error::ExistsError);
        }

        self.flush()?;

        let size_hint = match self.data.uncommitted_writes {
            Some((_, ref trie_storage)) => 2 * trie_storage.size_hint(),
            None => 1024, // don't try to guess _byte_ allocation here.
        };

        let trie_buf = TrieRAM::new(bhh, size_hint, &self.data.cur_block);

        // place a lock on this block, so we can't extend to it again
        if !trie_sql::lock_bhh_for_extension(self.sqlite_tx(), bhh, false)? {
            warn!("Block already extended: {}", &bhh);
            return Err(Error::ExistsError);
        }

        self.switch_trie(bhh, UncommittedState::RW(trie_buf));
        Ok(())
    }

    /// Extend the forest of Tries to include a new unconfirmed block.
    /// If the unconfirmed block (bhh) already exists, then load up its trie as the uncommitted_writes
    /// trie.
    pub fn extend_to_unconfirmed_block(&mut self, bhh: &T) -> Result<bool, Error> {
        self.clear_cached_ancestor_hashes_bytes();
        if !self.data.unconfirmed {
            return Err(Error::UnconfirmedError);
        }

        self.flush()?;

        // try to load up the trie
        let (trie_buf, created, unconfirmed_block_id) =
            if let Some(block_id) = trie_sql::get_unconfirmed_block_identifier(&self.db, bhh)? {
                debug!("Reload unconfirmed trie {} ({})", bhh, block_id);

                // restore trie
                let mut fd = trie_sql::open_trie_blob(&self.db, block_id)?;

                test_debug!("Unconfirmed trie block ID for {} is {}", bhh, block_id);
                (
                    TrieRAM::load_with_context(&mut fd, bhh, &self.data.record_context)?,
                    false,
                    Some(block_id),
                )
            } else {
                debug!("Instantiate unconfirmed trie {}", bhh);

                // new trie
                let size_hint = match self.data.uncommitted_writes {
                    Some((_, ref trie_storage)) => 2 * trie_storage.size_hint(),
                    None => 1024, // don't try to guess _byte_ allocation here.
                };

                (
                    TrieRAM::new(bhh, size_hint, &self.data.cur_block),
                    true,
                    None,
                )
            };

        // place a lock on this block, so we can't extend to it again
        if !trie_sql::tx_lock_bhh_for_extension(&self.db, bhh, true)? {
            warn!("Block already extended: {}", &bhh);
            return Err(Error::ExistsError);
        }

        self.data.unconfirmed_block_id = unconfirmed_block_id;
        self.switch_trie(bhh, UncommittedState::RW(trie_buf));
        Ok(created)
    }

    /// Clear out the underlying storage.
    pub fn format(&mut self) -> Result<(), Error> {
        if self.data.readonly {
            return Err(Error::ReadOnlyError);
        }

        debug!("Format TrieFileStorage");

        // Publish a fresh generation before local IDs can be reused.
        if let Some(index) = &mut self.data.direct_hash_index {
            index.reset(&self.db)?;
        }
        trie_sql::clear_tables(self.sqlite_tx())?;

        if let Some((_, ref mut trie_storage)) = self.data.uncommitted_writes {
            trie_storage.format()?
        };

        self.data.set_block(T::sentinel(), None);

        self.data.clear_root_node_cache();
        self.data.resolved_patch_cache.clear();
        self.data.uncommitted_writes = None;
        self.clear_cached_ancestor_hashes_bytes();

        Ok(())
    }

    pub fn sqlite_tx(&self) -> &Transaction<'a> {
        &self.db
    }

    pub fn sqlite_tx_mut(&mut self) -> &mut Transaction<'a> {
        &mut self.db
    }

    pub fn commit_tx(mut self) {
        #[cfg(feature = "commit-residency-diagnostics")]
        let _commit = stacks_profiler::diagnostic_span!("Commit: SQLite transaction");
        if let Some(guard) = &self._direct_hash_guard {
            guard
                .prepare_commit(&self.db)
                .expect("CORRUPTION: Failed to prepare hash tail");
        }
        self.db.commit().expect("CORRUPTION: Failed to commit MARF");
        if let Some(guard) = self._direct_hash_guard.take() {
            guard.succeed();
        }
        if let Some(guard) = self.result_cache_rollback.take() {
            guard.succeed();
        }
        if let Some(guard) = self.patch_cache_rollback.take() {
            guard.succeed();
        }
    }

    pub fn rollback(self) {
        self.db
            .rollback()
            .expect("CORRUPTION: Failed to rollback MARF");
    }
}

impl<'a, T: MarfTrieId, Db: Deref<Target = Connection>> TrieStorageConnection<'a, T, Db> {
    /// Whether a metadata update names the active trie and its actual parent.
    pub fn canonical_ancestry_update(&self, parent: &T, block: &T) -> bool {
        self.data
            .uncommitted_writes
            .as_ref()
            .is_some_and(|(active, state)| {
                active == block && &state.trie_ram_ref().parent == parent
            })
    }

    /// Disable ancestry shortcuts after writes outside the reserved metadata updater.
    pub fn invalidate_ancestry(&mut self) {
        if let Some((_, state)) = &mut self.data.uncommitted_writes {
            state.trie_ram_mut().ancestry_safe = false;
        }
        self.clear_cached_ancestor_hashes_bytes();
    }

    /// Physical layout selected when this store opened.
    pub fn record_format(&self) -> NodeRecordFormat {
        self.data.record_context.format
    }

    /// Clone the immutable value source without retaining a storage borrow.
    pub fn value_extent_resolver(&self) -> Option<Arc<dyn ValueExtentResolver>> {
        self.data.record_context.value_resolver.clone()
    }

    /// Evict one key and guard a semantic update against stale reuse or failure.
    pub fn begin_result_cache_write(
        &mut self,
        block: &T,
        path: &TrieHash,
    ) -> Option<ResultCacheGuard> {
        let (active, UncommittedState::RW(trie)) = self.data.uncommitted_writes.as_mut()? else {
            return None;
        };
        if active != block {
            return None;
        }
        Some(trie.result_cache.begin_write(path))
    }

    /// Invalidate all active results if a multi-step operation fails or unwinds.
    pub fn result_cache_failure_guard(&self) -> ResultCacheGuard {
        self.data.result_cache_control.guard()
    }

    /// Number of active cached results, used to verify invalidation and admission.
    #[cfg(test)]
    pub fn result_cache_len(&mut self) -> usize {
        self.data
            .uncommitted_writes
            .as_mut()
            .map_or(0, |(_, trie)| trie.trie_ram_mut().result_cache.len())
    }

    pub fn readonly(&self) -> bool {
        self.data.readonly
    }

    pub fn unconfirmed(&self) -> bool {
        self.data.unconfirmed
    }

    /// Returns true when this storage represents a squashed MARF.
    pub fn is_squashed(&self) -> bool {
        self.data.squash_info.is_some()
    }

    /// Returns cached squashing metadata, if present.
    pub fn squash_info(&self) -> Option<&SquashInfo> {
        self.data.squash_info.as_ref()
    }

    /// MARF height at the squash boundary, if this storage is squashed.
    pub fn squash_height(&self) -> Option<u32> {
        self.squash_info().map(|info| info.squash_height)
    }

    /// Set cached squashing metadata for this storage connection.
    pub(crate) fn set_squash_info(&mut self, squash_info: Option<SquashInfo>) {
        self.data.set_squash_info(squash_info);
    }

    /// Returns a reference to the underlying SQLite connection.
    pub(crate) fn sqlite_conn(&self) -> &Connection {
        &self.db
    }

    /// Warm the file-backed blob offset cache from rows already loaded by the caller.
    ///
    /// No-op for SQLite-internal storage.
    pub(super) fn warm_trie_offsets_from_entries(
        &mut self,
        block_entries: &[MarfDataEntry<T>],
    ) -> Result<(), Error> {
        if let Some(trie_file) = self.blobs.as_deref_mut() {
            for entry in block_entries {
                trie_file.cache_trie_offset(entry.block_id, entry.external_offset)?;
            }
        }
        Ok(())
    }

    /// Forwards to [`TrieFile::prefetch_node`].
    /// No-op for SQLite-internal storage.
    pub(super) fn prefetch_node(&self, block_id: u32, in_block_ptr: u64, node_id: u8) {
        let u64_ptr_offsets = self.squash_info().is_some();
        if let Some(trie_file) = self.blobs.as_deref() {
            trie_file.prefetch_node(block_id, in_block_ptr, node_id, u64_ptr_offsets);
        }
    }

    /// Bulk-read the [`BlobHeader`] of many blocks. Entries should be
    /// sorted by `external_offset` ascending so each parallel reader
    /// works a contiguous file region. Only disk-backed `TrieFile`s use the
    /// parallel path; RAM-backed `TrieFile`s and SQLite-internal storage
    /// fall back to per-block reads.
    pub(super) fn bulk_read_blob_headers_sorted(
        &mut self,
        sorted_entries: &[MarfDataEntry<T>],
    ) -> Result<HashMap<T, BlobHeader<T>>, Error>
    where
        T: Send + Sync,
    {
        if let Some(trie_file @ TrieFile::Disk(_)) = self.blobs.as_deref() {
            return trie_file.bulk_read_blob_headers_sorted(sorted_entries);
        }
        // No flat file to stream from (RAM-backed or SQLite-internal blobs):
        // fall back to per-row reads.
        let mut headers = HashMap::with_capacity(sorted_entries.len());
        for entry in sorted_entries {
            let header = self.read_blob_header(entry.block_id)?;
            headers.insert(entry.block_hash.clone(), header);
        }
        Ok(headers)
    }

    /// Read a block's [`BlobHeader`].
    pub(super) fn read_blob_header(&mut self, block_id: u32) -> Result<BlobHeader<T>, Error> {
        let db: &Connection = &self.db;
        match self.blobs.as_deref_mut() {
            Some(trie_file) => trie_file.read_blob_header::<T>(db, block_id),
            None => {
                let mut blob = db.blob_open(
                    rusqlite::DatabaseName::Main,
                    "marf_data",
                    "data",
                    block_id.into(),
                    true,
                )?;
                let format = self.data.record_context.format;
                let mut buf = vec![0u8; format.reader_prefix_len()];
                blob.read_exact(&mut buf)?;
                BlobHeader::parse_format(format, &buf)
            }
        }
    }

    /// Read this block's height from the squashed-block side table.
    ///
    /// Returns `None` for archival MARFs and for blocks outside the squashed
    /// range.
    pub fn squashed_block_height(&self, block_hash: &T) -> Result<Option<u32>, Error> {
        if !self.is_squashed() {
            return Ok(None);
        }

        trie_sql::read_squashed_block_height_by_hash(self.sqlite_conn(), block_hash)
    }

    /// Read this block's archival MARF root hash from the squashed-block side
    /// table.
    ///
    /// Returns `None` for archival MARFs and for blocks outside the squashed
    /// range.
    pub fn squashed_block_root_hash(&self, block_hash: &T) -> Result<Option<TrieHash>, Error> {
        if !self.is_squashed() {
            return Ok(None);
        }

        trie_sql::read_squashed_block_root_hash_by_hash(self.sqlite_conn(), block_hash)
    }

    /// Reject trie traversal below the squash height, where blocks share the
    /// squash blob.
    pub fn check_historical_read_allowed(&self, block_hash: &T) -> Result<(), Error> {
        let Some(squash_height) = self.squash_height() else {
            return Ok(());
        };

        // A block being extended in RAM is always above the squash height, so it is never in
        // `marf_squashed_blocks`. Skip the per-read SQL probe for it.
        if let Some((ref uncommitted_bhh, _)) = self.data.uncommitted_writes {
            if block_hash == uncommitted_bhh {
                return Ok(());
            }
        }

        let Some(block_height) = self.squashed_block_height(block_hash)? else {
            return Ok(());
        };

        if block_height < squash_height {
            return Err(Error::HistoricalReadInSquashedRange {
                block_height,
                squash_height,
            });
        }

        Ok(())
    }

    pub fn set_cached_ancestor_hashes_bytes(&mut self, bhh: &T, bytes: Arc<[TrieHash]>) {
        self.data.trie_ancestor_hash_bytes_cache = Some((bhh.clone(), bytes));
    }

    pub fn clear_cached_ancestor_hashes_bytes(&mut self) {
        self.data.clear_ancestor_hashes_bytes();
    }

    pub fn get_root_hash_at(&mut self, tip: &T) -> Result<TrieHash, Error> {
        // Squashed historical blocks keep their archival roots in SQL.
        if let Some(root_hash) = self.squashed_block_root_hash(tip)? {
            return Ok(root_hash);
        }

        let cur_block_hash = self.get_cur_block();

        self.open_block(tip)?;
        let root_hash_res = bits::read_root_hash(self);

        // restore
        self.open_block(&cur_block_hash)?;
        root_hash_res
    }

    /// Recover from partially-written state -- i.e. blow it away.
    /// Doesn't get called automatically.
    pub fn recover(db_path: &String) -> Result<(), Error> {
        let conn = marf_sqlite_open(db_path, OpenFlags::SQLITE_OPEN_READ_WRITE, false)?;
        trie_sql::clear_lock_data(&conn)
    }

    /// Invalidate shared nodes for reopen and rollback correctness tests.
    #[cfg(test)]
    pub fn clear_resolved_patch_cache(&self) {
        self.data.resolved_patch_cache.clear();
    }

    /// Return the number of cached resolved committed patched nodes.
    #[cfg(test)]
    pub fn resolved_patch_cache_len(&self) -> usize {
        self.data.resolved_patch_cache.len()
    }

    /// Return the number of cached committed roots.
    #[cfg(test)]
    pub fn root_node_cache_len(&self) -> usize {
        self.data.root_node_cache.as_ref().map_or(0, |cache| cache.len())
    }

    /// Return whether the current committed block has a cached root.
    #[cfg(test)]
    pub fn root_node_cache_has_current_block(&self) -> bool {
        self.data
            .cur_block_id
            .is_some_and(|id| {
                self.data
                    .root_node_cache
                    .as_ref()
                    .is_some_and(|cache| cache.contains_key(&id))
            })
    }

    /// Read the Trie root node's hash from the block table.
    #[cfg(any(test, feature = "testing"))]
    pub fn read_block_root_hash(&mut self, bhh: &T) -> Result<TrieHash, Error> {
        let root_hash_ptr = TriePtr::new(
            TrieNodeID::Node256 as u8,
            0,
            TrieStorageConnection::<T>::root_ptr_disk(),
        );
        if let Some(blobs) = self.blobs.as_mut() {
            // stored in a blobs file
            blobs.get_node_hash_by_bhh(&self.db, bhh, &root_hash_ptr)
        } else {
            // stored to DB
            trie_sql::get_node_hash_bytes_by_bhh(
                &self.db,
                bhh,
                &root_hash_ptr,
                &self.data.record_context,
            )
        }
    }

    #[cfg(test)]
    fn inner_read_persisted_root_to_blocks(&mut self) -> Result<HashMap<TrieHash, T>, Error> {
        let ret = match self.blobs.as_mut() {
            Some(blobs) => HashMap::from_iter(blobs.read_all_block_hashes_and_roots(&self.db)?),
            None => HashMap::from_iter(trie_sql::read_all_block_hashes_and_roots(&self.db)?),
        };
        Ok(ret)
    }

    /// Generate a mapping between Trie root hashes and the blocks that contain them.
    ///
    /// For squashed MARFs, blocks within the squashed range (0..=H) share a
    /// single shared trie storage whose stored trie hash was computed at height H.
    /// The standard blob-scanning approach would produce collisions (all blocks
    /// get the same trie hash). For each squashed block at height K we
    /// substitute the per-height archival root hash recorded in
    /// `marf_squashed_blocks` so that the table maps each historical block to
    /// its own archival root.
    #[cfg(test)]
    pub fn read_root_to_block_table(&mut self) -> Result<HashMap<TrieHash, T>, Error> {
        let mut ret = self.inner_read_persisted_root_to_blocks()?;

        // Override entries for blocks in the squashed range.
        // All blocks at heights 0..=H share a single squash trie, so
        // `inner_read_persisted_root_to_blocks` maps them all to the same
        // trie hash. Replace those entries with the per-height archival
        // trie hashes stored during squashing.
        if let Some(info) = self.data.squash_info.clone() {
            for h in 0..=info.squash_height {
                let Some(bh) =
                    trie_sql::read_squashed_block_hash_by_height::<T>(self.sqlite_conn(), h)?
                else {
                    continue;
                };

                let Some(archival_trie_hash) =
                    trie_sql::read_squashed_block_root_hash_by_height(self.sqlite_conn(), h)?
                else {
                    continue;
                };

                ret.insert(archival_trie_hash, bh);
            }
        }

        let uncommitted_writes = match self.data.uncommitted_writes.take() {
            Some((bhh, trie_ram)) => {
                let ptr = TriePtr::new(set_backptr(TrieNodeID::Node256 as u8), 0, 0);

                let root_hash = trie_ram.read_node_hash(&ptr)?;

                ret.insert(root_hash, bhh.clone());
                Some((bhh, trie_ram))
            }
            _ => None,
        };

        self.data.uncommitted_writes = uncommitted_writes;

        Ok(ret)
    }

    /// internal procedure for locking a trie hash for work
    fn switch_trie(&mut self, bhh: &T, mut trie_buf: UncommittedState<T>) {
        self.data.result_cache_control.invalidate();
        trie_buf.trie_ram_mut().result_cache = ResultCache::new(
            self.data.result_cache_capacity,
            Arc::clone(&self.data.result_cache_control),
        );
        trace!("Extended from {} to {}", &self.data.cur_block, bhh);

        // update internal structures
        self.data.set_block(bhh.clone(), None);
        self.clear_cached_ancestor_hashes_bytes();

        self.data.uncommitted_writes = Some((bhh.clone(), trie_buf));
    }

    /// Is the given block in the marf_data DB table, and is it part of the block history (i.e. it's not mined and
    /// its not unconfirmed)?
    pub fn has_confirmed_block(&self, bhh: &T) -> Result<bool, Error> {
        match trie_sql::get_confirmed_block_identifier(&self.db, bhh) {
            Ok(Some(_)) => Ok(true),
            Ok(None) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Is the given block in the marf_data DB table, and is it unconfirmed?
    pub fn has_unconfirmed_block(&self, bhh: &T) -> Result<bool, Error> {
        match trie_sql::get_unconfirmed_block_identifier(&self.db, bhh) {
            Ok(Some(_)) => Ok(true),
            Ok(None) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Is the given block represented in either the confirmed or unconfirmed block tables?
    /// The mined table is ignored.
    pub fn has_block(&self, bhh: &T) -> Result<bool, Error> {
        Ok(self.has_confirmed_block(bhh)? || self.has_unconfirmed_block(bhh)?)
    }

    /// Return the block_identifier / row_id for a given bhh. If that bhh
    ///  is currently being extended, return None, since the row_id won't
    ///  be known until the extended trie is flushed.
    pub fn get_block_identifier(&mut self, bhh: &T) -> Option<u32> {
        if let Some((ref uncommitted_bhh, _)) = self.data.uncommitted_writes {
            if bhh == uncommitted_bhh {
                return None;
            }
        }

        self.get_block_id_caching(bhh).ok()
    }

    /// Get the currently-open block identifier (its row ID)
    pub fn get_cur_block_identifier(&mut self) -> Result<u32, Error> {
        if let Some((ref uncommitted_bhh, _)) = self.data.uncommitted_writes {
            if &self.data.cur_block == uncommitted_bhh {
                return Err(Error::RequestedIdentifierForExtensionTrie);
            }
        }

        self.data.cur_block_id.ok_or_else(|| Error::NotOpenedError)
    }

    /// Get the TriePtr::ptr() value for the root node in the currently-open block.
    pub fn root_ptr(&self) -> u64 {
        if let Some((ref uncommitted_bhh, _)) = self.data.uncommitted_writes {
            if &self.data.cur_block == uncommitted_bhh {
                return 0;
            }
        }

        TrieStorageConnection::<T>::root_ptr_disk()
    }

    /// Get a TriePtr to the currently-open block's trie's root node.
    pub fn root_trieptr(&self) -> TriePtr {
        TriePtr::new(TrieNodeID::Node256 as u8, 0, self.root_ptr())
    }

    /// Get the TriePtr::ptr() value for a trie's root node if the node is stored to disk.
    pub fn root_ptr_disk() -> u64 {
        blob_layout::ROOT_NODE_OFFSET as u64
    }

    /// Read a node's children's hashes into the provided <Write> implementation.
    /// This only works for intermediate nodes and leafs (the latter of which have no children).
    ///
    /// This method is designed to only access hashes that are either (1) in this Trie, or (2) in
    /// RAM already (i.e. as part of the block map)
    ///
    /// This means that the hash of a node that is in a previous Trie will _not_ be its
    /// hash (as that would require a disk access), but would instead be the root hash of the Trie
    /// that contains it.  While this makes the Merkle proof construction a bit more complicated,
    /// it _significantly_ improves the performance of this method (which is crucial since this is on
    /// the write path, which must be as short as possible).
    ///
    /// Rules:
    /// If a node is empty, pass in an empty hash.
    /// If a node is in this Trie, pass its hash.
    /// If a node is in a previous Trie, pass the root hash of its Trie.
    ///
    /// On err, S may point to a prior block.  The caller should call s.open(...) if an error
    /// occurs.
    ///
    /// NOTE: this method should only be called if `hash_calculation_mode` is set to
    /// `TrieHashCalculationMode::All` or `TrieHashCalculationMode::Immediate`.  There is no need
    /// to call if the hash mode is `::Deferred`.  The only way this gets called while not in
    /// `::Deferred` mode is when generating a Merkle proof.
    pub fn write_children_hashes<W: Write>(
        &mut self,
        node: &TrieNodeType,
        w: &mut W,
    ) -> Result<(), Error> {
        self.write_children_hashes_by_ptrs(node.ptrs(), w)
    }

    /// Inner method for calculating a node's hash, by hashing its children.
    fn inner_write_children_hashes<W: Write + ?Sized, H: NodeHashReader, M: BlockMap>(
        hash_reader: &mut H,
        map: &mut M,
        ptrs: &[TriePtr],
        w: &mut W,
    ) -> Result<(), Error> {
        trace!("inner_write_children_hashes begin for ptrs {:?}:", &ptrs);
        for ptr in ptrs.iter() {
            if ptr.id() == TrieNodeID::Empty as u8 {
                // hash of empty string
                trace!(
                    "inner_write_children_hashes for ptrs {:?}: {:?} empty",
                    &ptrs,
                    &ptr
                );
                w.write_all(TrieHash::EMPTY.as_bytes())?;
            } else if !is_backptr(ptr.id()) {
                // hash is in the same block as this node

                let mut buf = [0u8; TRIEHASH_ENCODED_SIZE];
                {
                    let mut hash_bytes = &mut buf[..];
                    hash_reader.read_node_hash(ptr, &mut hash_bytes)?;
                }
                trace!(
                    "inner_write_children_hashes for ptrs {:?}: {:?} same block {}",
                    &ptrs,
                    &ptr,
                    &to_hex(&buf)
                );
                w.write_all(&buf)?;
            } else {
                // hash is in a different block altogether, so we just use the ancestor block hash.  The
                // ptr.ptr() value points to the actual node in the ancestor block.
                let block_hash = map.get_block_hash_caching(ptr.back_block())?;
                trace!(
                    "inner_write_children_hashes for ptrs {:?}: {:?} back block {:?}",
                    &ptrs,
                    &ptr,
                    &block_hash
                );
                w.write_all(block_hash.as_bytes())?;
            }
        }
        trace!("inner_write_children_hashes end for ptrs {:?}:", &ptrs);

        Ok(())
    }

    /// read a persisted node's hash
    fn inner_read_persisted_node_hash(
        &mut self,
        block_id: u32,
        ptr: &TriePtr,
    ) -> Result<TrieHash, Error> {
        if self.data.unconfirmed_block_id == Some(block_id) {
            // read from unconfirmed trie
            test_debug!(
                "Read persisted node hash from unconfirmed block id {}",
                block_id
            );
            return trie_sql::get_node_hash_bytes(
                &self.db,
                block_id,
                ptr,
                &self.data.record_context,
            );
        }
        let trie_offset = self.current_trie_offset_hint(block_id);
        let node_hash = match self.blobs.as_ref() {
            Some(blobs) => blobs.get_node_hash(&self.db, block_id, ptr, trie_offset),
            None => {
                trie_sql::get_node_hash_bytes(&self.db, block_id, ptr, &self.data.record_context)
            }
        }?;
        Ok(node_hash)
    }

    fn current_trie_offset_hint(&mut self, block_id: u32) -> Option<u64> {
        if self.data.cur_block_trie_offset.is_none() {
            self.data.cur_block_trie_offset = self
                .blobs
                .as_ref()?
                .get_trie_offset(&self.db, block_id)
                .ok();
        }
        self.data.cur_block_trie_offset
    }

    #[inline]
    fn has_open_uncommitted_trie(&self) -> bool {
        matches!(
            self.data.uncommitted_writes.as_ref(),
            Some((uncommitted_bhh, _)) if &self.data.cur_block == uncommitted_bhh
        )
    }

    /// Store a node and its hash to the uncommitted state at the given
    /// in-memory node index.
    ///
    /// Panics if the uncommitted state is not instantiated or if the
    /// current block does not match the uncommitted block.
    pub fn write_nodetype(
        &mut self,
        node_array_ptr: u32,
        node: &TrieNodeType,
        hash: TrieHash,
    ) -> Result<(), Error> {
        if self.data.readonly {
            return Err(Error::ReadOnlyError);
        }

        trace!(
            "write_nodetype({:?}): at {}: {:?} {:?}",
            &self.data.cur_block,
            node_array_ptr,
            &hash,
            node
        );

        self.data.write_count += 1;
        match node {
            TrieNodeType::Leaf(_) => {
                self.data.write_leaf_count += 1;
            }
            _ => {
                self.data.write_node_count += 1;
            }
        }

        // Only allow writes when the cur_block is the current in-RAM extending block.
        if let Some((ref uncommitted_bhh, ref mut uncommitted_trie)) = self.data.uncommitted_writes
        {
            if &self.data.cur_block == uncommitted_bhh {
                return uncommitted_trie.write_nodetype(node_array_ptr, node, hash);
            }
        }

        panic!(
            "Tried to write to another Trie besides the currently-buffered one.  This should never happen -- only flush() can write to disk!"
        );
    }

    /// Take a node+hash out of the uncommitted TrieRAM via O(1) swap, leaving a placeholder.
    ///
    /// This is a performance optimization for the hash-recalculation hot path: it avoids
    /// the heap allocation that `into_owned_node()` → `to_owned_node()` would require.
    ///
    /// The caller MUST call [`restore_ram_node`] before this slot is read again. Any error
    /// between take and restore is unrecoverable (hash computation failure = block abandoned),
    /// so the placeholder cannot be observed by other readers.
    pub fn take_ram_node(&mut self, ptr: u32) -> Result<(TrieNodeType, TrieHash), Error> {
        if let Some((ref uncommitted_bhh, ref mut uncommitted_trie)) = self.data.uncommitted_writes
        {
            if &self.data.cur_block == uncommitted_bhh {
                return uncommitted_trie.take_node(ptr);
            }
        }
        panic!("take_ram_node: no uncommitted trie is open");
    }

    /// Restore a node+hash into the uncommitted TrieRAM at the given slot.
    pub fn restore_ram_node(
        &mut self,
        ptr: u32,
        node: TrieNodeType,
        hash: TrieHash,
    ) -> Result<(), Error> {
        if let Some((ref uncommitted_bhh, ref mut uncommitted_trie)) = self.data.uncommitted_writes
        {
            if &self.data.cur_block == uncommitted_bhh {
                return uncommitted_trie.restore_node(ptr, node, hash);
            }
        }
        panic!("restore_ram_node: no uncommitted trie is open");
    }

    /// Store a node and its hash to uncommitted state.
    pub fn write_node<N: TrieNode + std::fmt::Debug>(
        &mut self,
        node_array_ptr: u32,
        node: &N,
        hash: TrieHash,
    ) -> Result<(), Error> {
        if self.data.readonly {
            return Err(Error::ReadOnlyError);
        }

        let node_type = node.as_trie_node_type();
        self.write_nodetype(node_array_ptr, &node_type, hash)
    }

    /// Store only a node hash to the uncommitted state.
    /// If the uncommitted state is not instantiated, then this panics.
    pub fn write_node_hash(&mut self, node_array_ptr: u32, hash: TrieHash) -> Result<(), Error> {
        if self.data.readonly {
            return Err(Error::ReadOnlyError);
        }

        // Only allow writes when the cur_block is the current in-RAM extending block.
        if let Some((ref uncommitted_bhh, ref mut uncommitted_trie)) = self.data.uncommitted_writes
        {
            if &self.data.cur_block == uncommitted_bhh {
                return uncommitted_trie.write_node_hash(node_array_ptr, hash);
            }
        }

        panic!(
            "Tried to write to another Trie besides the currently-buffered one.  This should never happen -- only flush() can write to disk!"
        );
    }

    /// Get the next node index into which a node will be inserted in the
    /// uncommitted state.
    ///
    /// Panics if there is no uncommitted state instantiated.
    pub fn last_ptr(&mut self) -> Result<u32, Error> {
        if let Some((_, ref mut uncommitted_trie)) = self.data.uncommitted_writes {
            uncommitted_trie.last_ptr()
        } else {
            panic!("Cannot allocate new ptrs in a Trie that is not in RAM");
        }
    }

    /// Count up the number of trie blocks this storage represents
    pub fn num_blocks(&self) -> usize {
        let result = if self.data.uncommitted_writes.is_some() {
            1
        } else {
            0
        };
        result
            + (trie_sql::count_blocks(&self.db)
                .expect("Corruption: SQL Error on a non-fallible query.") as usize)
    }
}

#[cfg(test)]
pub mod testing {
    use super::*;
    use stacks_common::types::chainstate::StacksBlockId;

    /// Temporary reinstatement retains cache entries, admission and owner invalidation on error.
    #[test]
    fn reinstated_result_cache_retains_owner_and_entries() {
        for capacity in [0, 2] {
            let mut storage =
                TrieFileStorage::<StacksBlockId>::new_memory(MARFOpenOpts::default()).unwrap();
            let mut tx = storage.transaction().unwrap();
            let tip = StacksBlockId([91; 32]);
            let mut trie = TrieRAM::new(&tip, 0, &StacksBlockId::sentinel());
            let control = Arc::clone(&tx.data.result_cache_control);
            trie.result_cache = ResultCache::new(capacity, Arc::clone(&control));
            let key = TrieHash::from_key("cached");
            let missing = TrieHash::from_key("absent");
            let value = Some(TrieLeaf::from_value(&[], MARFValue::from(42)));
            let expected = (capacity > 0).then_some(value.clone());
            trie.result_cache.put(key, value.clone());

            let result: Result<(), Error> = trie.with_reinstated_data(&mut tx, |_, tx| {
                assert_eq!(tx.cached_result(&tip, &key), expected);
                tx.cache_result(&tip, missing, None);
                assert_eq!(
                    tx.cached_result(&tip, &missing),
                    (capacity > 0).then_some(None)
                );
                // The moved cache must still observe its storage owner's rollback signal.
                control.invalidate();
                assert_eq!(tx.cached_result(&tip, &key), None);
                assert_eq!(tx.cached_result(&tip, &missing), None);
                tx.cache_result(&tip, key, value.clone());
                tx.cache_result(&tip, missing, None);
                Err(Error::NotFoundError)
            });
            std::assert_matches!(result, Err(Error::NotFoundError));
            assert!(tx.data.uncommitted_writes.is_none());
            assert_eq!(trie.result_cache.get(&key), expected);
            assert_eq!(
                trie.result_cache.get(&missing),
                (capacity > 0).then_some(None)
            );
            control.invalidate();
            assert_eq!(trie.result_cache.get(&key), None);
            assert_eq!(trie.result_cache.get(&missing), None);
        }
    }

    pub trait MarfTestStorage<T: MarfTrieId> {
        fn read_root_to_block_table(&mut self) -> Result<HashMap<TrieHash, T>, Error>;
    }

    impl<T: MarfTrieId, Db: Deref<Target = Connection>> MarfTestStorage<T>
        for TrieStorageConnection<'_, T, Db>
    {
        fn read_root_to_block_table(&mut self) -> Result<HashMap<TrieHash, T>, Error> {
            Self::read_root_to_block_table(self)
        }
    }

    impl<T: MarfTrieId> MarfTestStorage<T> for ReopenedTrieStorageConnection<'_, T> {
        fn read_root_to_block_table(&mut self) -> Result<HashMap<TrieHash, T>, Error> {
            self.connection().read_root_to_block_table()
        }
    }

    impl<'a, T: MarfTrieId, Db: Deref<Target = Connection>> TrieStorageConnection<'a, T, Db> {
        pub fn stats(&mut self) -> (u64, u64) {
            let r = self.data.read_count;
            let w = self.data.write_count;
            self.data.read_count = 0;
            self.data.write_count = 0;
            (r, w)
        }

        pub fn node_stats(&mut self) -> (u64, u64, u64) {
            let nr = self.data.read_node_count;
            let br = self.data.read_backptr_count;
            let nw = self.data.write_node_count;

            self.data.read_node_count = 0;
            self.data.read_backptr_count = 0;
            self.data.write_node_count = 0;

            (nr, br, nw)
        }

        pub fn leaf_stats(&mut self) -> (u64, u64) {
            let lr = self.data.read_leaf_count;
            let lw = self.data.write_leaf_count;

            self.data.read_leaf_count = 0;
            self.data.write_leaf_count = 0;

            (lr, lw)
        }

        pub fn transient_data(&self) -> &TrieStorageTransientData<T> {
            self.data
        }

        pub fn transient_data_mut(&mut self) -> &mut TrieStorageTransientData<T> {
            self.data
        }
    }
}

#[cfg(test)]
mod sealing_hash_tests {
    use super::*;
    use crate::chainstate::stacks::index::node::{TrieNode16, TrieNode256, TrieNode4, TrieNode48};
    use crate::chainstate::stacks::BlockHeaderHash;

    /// Resolved-hash encoding must preserve every consensus byte for every branch size.
    #[test]
    fn resolved_child_hash_consensus_bytes_match() {
        let mut map =
            TrieFileStorage::<BlockHeaderHash>::new_memory(MARFOpenOpts::default()).unwrap();
        map.cache.store_block_hash(7, BlockHeaderHash([0xa5; 32]));
        let mut nodes = [
            TrieNodeType::Node4(TrieNode4::new(&[3, 4])),
            TrieNodeType::Node16(TrieNode16::new(&[3, 4])),
            TrieNodeType::Node48(Box::new(TrieNode48::new(&[3, 4]))),
            TrieNodeType::Node256(Box::new(TrieNode256::new(&[3, 4]))),
        ];
        for node in &mut nodes {
            let mut back = TriePtr::new(set_backptr(TrieNodeID::Node4 as u8), 9, 123);
            back.back_block = 7;
            assert!(node.insert(&back));
            assert!(node.insert(&TriePtr::new(TrieNodeID::Leaf as u8, 10, 1)));
            let hashes: Vec<_> = node
                .ptrs()
                .iter()
                .map(|p| {
                    if is_backptr(p.id()) {
                        TrieHash([0xa5; 32])
                    } else {
                        TrieHash([0x55; 32])
                    }
                })
                .collect();
            let mut old = Vec::new();
            node.write_consensus_bytes(&mut map, &mut old).unwrap();
            let mut new = Vec::new();
            node.write_consensus_bytes_with_child_hashes(&hashes, &mut new)
                .unwrap();
            assert_eq!(old, new);
        }
    }
}
