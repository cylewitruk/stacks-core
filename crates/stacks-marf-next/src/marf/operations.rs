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

//! Borrowed forest traversal, ancestry and connection-backed operations.
use rusqlite::{Connection, Transaction};
use stacks_common::types::chainstate::TrieHash;

pub use self::connection::MarfConnection;
#[cfg(any(test, feature = "testing"))]
use super::WalkProfile;
use super::{
    BLOCK_HASH_TO_HEIGHT_MAPPING_KEY, BLOCK_HEIGHT_TO_HASH_MAPPING_KEY, Inner, MARF, MARFOpenOpts,
    MarfTransaction, OWN_BLOCK_HEIGHT_KEY, ReopenedMarf, TrieFileStorage, View,
};
use crate::access::{AuthenticatedRead, NodeRead, TrieLocation as Site};
use crate::archive::PruneInfo;
use crate::codec::{self, Descent};
use crate::node::{Node, Ptr, id};
use crate::proofs::{Prove, TrieMerkleProof};
use crate::storage::{StorageContext, StoreNode, descend_branch};
use crate::trie::{ROOT_OFFSET, TrieIdentity};
use crate::{Error, MARFValue, MarfTrieId, corrupt};

mod api;
mod archive;
mod connection;
mod write;

fn root_ptr(site: Site) -> Ptr {
    match site {
        Site::Mem => Ptr::new(id::NODE256, 0, 0),
        Site::Block(_) => Ptr::new(id::NODE256, 0, ROOT_OFFSET),
    }
}

/// A node borrowed from wherever it lives: the arena of the trie being
/// built, or storage (a record, or a patch chain resolved into scratch).
/// Traversal (path, child selection, leaf value) never builds an owned node.
#[derive(Clone, Copy)]
enum NodeView<'a> {
    Mem(&'a Node),
    Store(StoreNode<'a>),
}
impl<'a> NodeView<'a> {
    fn path(&self) -> Result<&'a [u8], Error> {
        match self {
            NodeView::Mem(n) => Ok(n.path().as_slice()),
            NodeView::Store(n) => n.path(),
        }
    }

    fn is_leaf(&self) -> Result<bool, Error> {
        match self {
            NodeView::Mem(n) => Ok(n.is_leaf()),
            NodeView::Store(n) => n.is_leaf(),
        }
    }

    fn child(&self, chr: u8) -> Result<Option<Ptr>, Error> {
        match self {
            NodeView::Mem(Node::Leaf(_)) => Ok(None),
            NodeView::Mem(Node::Branch(b)) => Ok(b.child(chr).copied()),
            NodeView::Store(n) => n.child(chr),
        }
    }

    /// One step of a key walk (see [`codec::Record::descend`]).
    fn descend(&self, key: &[u8], depth: usize) -> Result<Descent<'a>, Error> {
        match self {
            NodeView::Mem(Node::Leaf(l)) => Ok(if key[depth..].starts_with(l.path.as_slice()) {
                Descent::Leaf {
                    value: codec::LeafView::borrow(l),
                    consumed: l.path.len(),
                }
            } else {
                Descent::Mismatch
            }),
            NodeView::Mem(Node::Branch(b)) => descend_branch(b, key, depth),
            NodeView::Store(n) => n.descend(key, depth),
        }
    }

    fn leaf_value(&self) -> Result<Option<MARFValue>, Error> {
        match self {
            NodeView::Mem(Node::Leaf(l)) => Ok(Some(l.value.clone())),
            NodeView::Mem(Node::Branch(_)) => Ok(None),
            NodeView::Store(n) => n.leaf_value(),
        }
    }

    fn for_each_child(&self, mut visit: impl FnMut(Ptr)) -> Result<(), Error> {
        match self {
            NodeView::Mem(Node::Leaf(_)) => Ok(()),
            NodeView::Mem(Node::Branch(b)) => {
                b.children.iter().for_each(|p| visit(*p));
                Ok(())
            }
            NodeView::Store(n) => n.for_each_child(visit),
        }
    }

    fn to_node(self) -> Result<Node, Error> {
        match self {
            NodeView::Mem(n) => Ok((*n).clone()),
            NodeView::Store(n) => n.to_node(),
        }
    }
}

impl<'a, T: MarfTrieId> View<'a, T> {
    #[cfg_attr(
        feature = "profiler",
        stacks_profiler::profile(sample_rate = 8, unsampled = "count_only")
    )]
    fn site(&mut self, bhh: &T) -> Result<Site, Error> {
        if let Some(b) = self.inner.building.as_ref()
            && b.trie.block_id() == bhh
        {
            return Ok(Site::Mem);
        }
        Ok(Site::Block(self.inner.store.block_id(self.db, bhh)?))
    }

    /// Abandon the trie being built (does not touch the SQLite transaction).
    fn drop_extending_trie(&mut self) -> Result<(), Error> {
        if self.inner.store.policy().readonly {
            return Ok(());
        }
        if let Some(b) = self.inner.building.take() {
            self.inner.store.unlock(self.db, b.trie.block_id())?;
        }
        Ok(())
    }

    /// Read the node at `ptr` in `site`, following a back-pointer if `ptr` is
    /// one. The view borrows the arena or storage; nothing is materialized.
    fn read(&mut self, site: Site, ptr: &Ptr) -> Result<(Site, NodeView<'_>), Error> {
        let (site, node, _) = self.read_with_depth(site, ptr)?;
        Ok((site, node))
    }

    /// [`View::read`] that also reports the patch-chain depth of the record
    /// (0 for a plain record or an arena node).
    #[cfg_attr(
        feature = "profiler",
        stacks_profiler::profile(sample_rate = 8, unsampled = "count_only")
    )]
    fn read_with_depth(
        &mut self,
        site: Site,
        ptr: &Ptr,
    ) -> Result<(Site, NodeView<'_>, u32), Error> {
        count!(node_visits);
        if ptr.is_back() {
            count!(backptr_follows);
            let r = self.inner.store.node(self.db, ptr.back_block, ptr)?;
            return Ok((
                Site::Block(ptr.back_block),
                NodeView::Store(r.node),
                r.depth,
            ));
        }
        match site {
            Site::Mem => {
                let idx = u32::try_from(ptr.ptr).map_err(|_| Error::OverflowError)?;
                let trie = &self
                    .inner
                    .building
                    .as_ref()
                    .expect("Mem site implies a trie")
                    .trie;
                Ok((site, NodeView::Mem(trie.node(idx)?), 0))
            }
            Site::Block(id) => {
                let r = self.inner.store.node(self.db, id, ptr)?;
                Ok((site, NodeView::Store(r.node), r.depth))
            }
        }
    }
    /// MARF root hashes of the ancestors at heights `h - 2^i` of a trie at
    /// `tip` whose height is `height`.
    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    fn collect_ancestor_hashes(&mut self, tip: &T, height: u32) -> Result<Vec<TrieHash>, Error> {
        let mut out = Vec::with_capacity(33);
        let mut i = 0;
        while i < 32 && (1u32 << i) <= height {
            let h = height - (1u32 << i);
            let ancestor = self
                .get_block_at_height(h, tip)?
                .ok_or_else(|| corrupt!("Could not obtain block hash at block height {h}"))?;
            let root = if self.inner.store.prune_height().is_some_and(|sh| h <= sh) {
                self.inner
                    .store
                    .pruned_root_hash_at_height(self.db, h)?
                    .ok_or_else(|| corrupt!("Could not obtain squashed root hash at height {h}"))?
            } else {
                let id = self.inner.store.block_id(self.db, &ancestor)?;
                self.inner.store.read_root_hash(self.db, id)?
            };
            out.push(root);
            i += 1;
        }
        Ok(out)
    }
}
impl<T: MarfTrieId> MARF<T> {
    fn view(&mut self) -> View<'_, T> {
        View {
            db: &self.db,
            inner: &mut self.inner,
        }
    }
}
impl<'a, T: MarfTrieId> MarfTransaction<'a, T> {
    fn tx(&self) -> &Transaction<'a> {
        self.tx.as_ref().expect("transaction still open")
    }

    fn view(&mut self) -> View<'_, T> {
        View {
            db: self.tx.as_ref().expect("transaction still open"),
            inner: self.inner,
        }
    }
    fn take_tx(&mut self) -> Transaction<'a> {
        self.tx.take().expect("transaction still open")
    }
    fn into_sqlite_tx(mut self) -> Transaction<'a> {
        // the caller decides the transaction's fate; be conservative
        self.inner.store.invalidate();
        self.take_tx()
    }
}

impl<T: MarfTrieId> MARF<T> {
    fn reopen_readonly_inner(inner: &Inner<T>) -> Result<MARF<T>, Error> {
        let db = inner.store.reopen_readonly_connection()?;
        Ok(MARF {
            db,
            inner: Inner {
                store: inner.store.reopen_readonly(),
                building: None,
                current: inner.current.clone(),
            },
        })
    }
}

impl<T: MarfTrieId> View<'_, T> {
    /// `OWN_BLOCK_HEIGHT_KEY` at `tip`, or the side-table height for a block
    /// inside a squashed range.
    fn own_height(&mut self, tip: &T) -> Result<Option<u32>, Error> {
        match self.lookup(tip, &TrieHash::from_key(OWN_BLOCK_HEIGHT_KEY)) {
            Ok(value) => Ok(value.map(u32::from)),
            Err(Error::NotFoundError) => Ok(None),
            Err(Error::HistoricalReadInPrunedRange { block_height, .. }) => Ok(Some(block_height)),
            Err(e) => Err(e),
        }
    }
}

impl<'a, T: MarfTrieId> View<'a, T> {
    pub fn sqlite_conn(&self) -> &Connection {
        self.db
    }

    pub fn readonly(&self) -> bool {
        self.inner.store.policy().readonly
    }

    pub fn unconfirmed(&self) -> bool {
        self.inner.store.policy().unconfirmed
    }

    pub fn is_pruned(&self) -> bool {
        self.inner.store.prune_info().is_some()
    }

    pub fn prune_info(&self) -> Option<&PruneInfo> {
        self.inner.store.prune_info()
    }

    /// The block being built, if any.
    pub fn open_chain_tip(&self) -> Option<&T> {
        self.inner.building.as_ref().map(|b| b.trie.block_id())
    }

    pub fn open_chain_tip_height(&self) -> Option<u32> {
        self.inner.building.as_ref().map(|b| b.height)
    }

    /// Is `bhh` a stored (confirmed or unconfirmed) or in-progress block?
    pub fn has_block(&mut self, bhh: &T) -> Result<bool, Error> {
        if self.open_chain_tip() == Some(bhh) {
            return Ok(true);
        }
        self.inner.store.has_block(self.db, bhh)
    }

    /// Block most recently targeted by [`View::open_block`].
    pub fn get_cur_block(&self) -> T {
        self.inner.current.clone()
    }

    /// Target `bhh` for subsequent ancestor checks. Fails with `NotFoundError`
    /// if the block is neither stored nor being built (the sentinel is always
    /// accepted).
    pub fn open_block(&mut self, bhh: &T) -> Result<(), Error> {
        if *bhh != T::sentinel() {
            self.site(bhh)?;
        }
        self.inner.current = bhh.clone();
        Ok(())
    }
    /// The shape of the walk a lookup of `path` at `tip` performs: how many
    /// records it reads, how many back-pointers it follows (block changes),
    /// how many of the records are stored as patch chains and the deepest
    /// chain. The shape is a property of the stored trie, which every
    /// compatible implementation shares, so it qualifies benchmark keys.
    #[cfg(any(test, feature = "testing"))]
    pub fn lookup_profile(&mut self, tip: &T, path: &TrieHash) -> Result<WalkProfile, Error> {
        let mut profile = WalkProfile::default();
        let mut site = self.site(tip)?;
        let mut ptr = root_ptr(site);
        let key = path.as_bytes();
        let mut depth = 0usize;
        loop {
            if ptr.is_back() {
                profile.backptr_hops += 1;
            }
            let location = match (site, ptr.is_back()) {
                (_, true) => Some((ptr.back_block, ptr.ptr)),
                (Site::Block(id), false) => Some((id, ptr.ptr)),
                (Site::Mem, false) => None,
            };
            let (next_site, node, patch_depth) = self.read_with_depth(site, &ptr)?;
            site = next_site;
            profile.nodes += 1;
            if patch_depth > 0 {
                if let (Some(loc), true) = (location, (profile.patched_nodes as usize) < 8) {
                    profile.patched_sites[profile.patched_nodes as usize] = loc;
                }
                profile.patched_nodes += 1;
                profile.max_patch_depth = profile.max_patch_depth.max(patch_depth);
            }
            match node.descend(key, depth)? {
                Descent::Mismatch => return Ok(profile),
                Descent::Leaf { .. } => {
                    profile.found = true;
                    return Ok(profile);
                }
                Descent::Child { child: None, .. } => return Ok(profile),
                Descent::Child {
                    child: Some(p),
                    consumed,
                } => {
                    ptr = p;
                    depth += consumed;
                }
            }
        }
    }

    /// Walk `path` from the root of `tip`'s trie. `Err(NotFoundError)` if the
    /// block is unknown; `Ok(None)` if the key is absent.
    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn lookup(&mut self, tip: &T, path: &TrieHash) -> Result<Option<MARFValue>, Error> {
        self.inner
            .store
            .check_historical_read_allowed(self.db, tip)?;
        let mut site = self.site(tip)?;
        let mut ptr = root_ptr(site);
        let key = path.as_bytes();
        let mut depth = 0usize;
        loop {
            let (next_site, node) = self.read(site, &ptr)?;
            site = next_site;
            match node.descend(key, depth)? {
                Descent::Mismatch => return Ok(None),
                Descent::Leaf { value, consumed } => {
                    if depth + consumed != key.len() {
                        return Err(corrupt!("Path reached a leaf before its end"));
                    }
                    return Ok(Some(value.value()));
                }
                Descent::Child { child: None, .. } => return Ok(None),
                Descent::Child {
                    child: Some(p),
                    consumed,
                } => {
                    ptr = p;
                    depth += consumed;
                }
            }
        }
    }

    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn get_by_path(&mut self, tip: &T, path: &TrieHash) -> Result<Option<MARFValue>, Error> {
        #[cfg(feature = "profiler")]
        stacks_profiler::record_if!(
            crate::profiler::capture_marf_hash_lookups(),
            "BH",
            tip.as_bytes()
        );
        match self.lookup(tip, path) {
            Ok(value) => Ok(value),
            Err(Error::NotFoundError) => Ok(None),
            Err(e) => Err(e),
        }
    }

    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn get_by_key(&mut self, tip: &T, key: &str) -> Result<Option<MARFValue>, Error> {
        #[cfg(feature = "profiler")]
        stacks_profiler::record_if!(crate::profiler::capture_marf_keys(), "KEY", key);
        self.get_by_path(tip, &TrieHash::from_key(key))
    }

    /// The original's name for [`View::get_root_hash_at`].
    pub fn read_block_root_hash(&mut self, bhh: &T) -> Result<TrieHash, Error> {
        self.get_root_hash_at(bhh)
    }

    pub fn get_root_hash_at(&mut self, tip: &T) -> Result<TrieHash, Error> {
        if let Some(h) = self.inner.store.pruned_block_root_hash(self.db, tip)? {
            return Ok(h);
        }
        match self.site(tip)? {
            Site::Mem => Ok(self.inner.building.as_ref().expect("Mem").trie.root_hash()),
            Site::Block(id) => self.inner.store.read_root_hash(self.db, id),
        }
    }
}
impl<'a, T: MarfTrieId> View<'a, T> {
    /// Visit every `(path, value)` visible at `tip`, following back-pointers
    /// into older blocks. Returns the number of leaves visited.
    pub fn for_each_leaf<F>(&mut self, tip: &T, mut handle_leaf: F) -> Result<u64, Error>
    where
        F: FnMut(TrieHash, MARFValue) -> Result<(), Error>,
    {
        self.inner
            .store
            .check_historical_read_allowed(self.db, tip)?;
        let site = self.site(tip)?;
        // (site, pointer, length of the path prefix above this node, the
        // child character leading to it)
        let mut stack: Vec<(Site, Ptr, usize, Option<u8>)> = vec![(site, root_ptr(site), 0, None)];
        let mut path: Vec<u8> = Vec::with_capacity(32);
        let mut count = 0u64;
        while let Some((site, ptr, prefix_len, chr)) = stack.pop() {
            path.truncate(prefix_len);
            path.extend(chr);
            let (site, node) = self.read(site, &ptr)?;
            path.extend_from_slice(node.path()?);
            if let Some(value) = node.leaf_value()? {
                let full = TrieHash::from_bytes(&path)
                    .ok_or_else(|| corrupt!("Leaf path length invalid"))?;
                handle_leaf(full, value)?;
                count += 1;
                continue;
            }
            let first = stack.len();
            let len = path.len();
            node.for_each_child(|child| stack.push((site, child, len, Some(child.chr))))?;
            // children were pushed in slot order; pop them in slot order too
            stack[first..].reverse();
        }
        Ok(count)
    }
}

impl<T: MarfTrieId> MARF<T> {
    /// Walk all leaves in the trie at `block_hash`, yielding full paths and values.
    pub fn for_each_leaf<F>(
        view: &mut View<'_, T>,
        block_hash: &T,
        handle_leaf: F,
    ) -> Result<u64, Error>
    where
        F: FnMut(TrieHash, MARFValue) -> Result<(), Error>,
    {
        view.for_each_leaf(block_hash, handle_leaf)
    }
}

impl<'a, T: MarfTrieId> View<'a, T> {
    pub fn get_block_height(&mut self, block: &T, tip: &T) -> Result<Option<u32>, Error> {
        if block == tip {
            return self.own_height(tip);
        }
        let key = format!("{BLOCK_HASH_TO_HEIGHT_MAPPING_KEY}::{block}");
        match self.lookup(tip, &TrieHash::from_key(&key)) {
            Ok(value) => Ok(value.map(u32::from)),
            Err(Error::NotFoundError) => Ok(None),
            Err(Error::HistoricalReadInPrunedRange { .. }) => {
                self.inner.store.pruned_block_height(self.db, block)
            }
            Err(e) => Err(e),
        }
    }

    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn get_block_at_height(&mut self, height: u32, tip: &T) -> Result<Option<T>, Error> {
        let Some(tip_height) = self.get_block_height(tip, tip)? else {
            return Ok(None);
        };
        if height == tip_height {
            return Ok(Some(tip.clone()));
        }
        if let Some(squash_height) = self.inner.store.prune_height()
            && tip_height <= squash_height
        {
            if height > tip_height {
                return Ok(None);
            }
            return self.inner.store.pruned_block_at_height(self.db, height);
        }
        let key = format!("{BLOCK_HEIGHT_TO_HASH_MAPPING_KEY}::{height}");
        Ok(self.get_by_key(tip, &key)?.map(T::from))
    }

    pub fn check_ancestor_block_hash(&mut self, bhh: &T) -> Result<(), Error> {
        let cur = self.inner.current.clone();
        if cur == *bhh {
            return Ok(());
        }
        let height = self.get_block_height(bhh, &cur)?.ok_or_else(|| {
            Error::NonMatchingForks(bhh.clone().to_bytes(), cur.clone().to_bytes())
        })?;
        let actual = self.get_block_at_height(height, &cur)?.ok_or_else(|| {
            corrupt!("ERROR: Could not find block for height {height}, but it was returned by MARF::get_block_height()")
        })?;
        if actual != *bhh {
            return Err(Error::NonMatchingForks(
                bhh.clone().to_bytes(),
                cur.to_bytes(),
            ));
        }
        // the block must be openable
        self.site(bhh).map(|_| ())
    }
}

impl<T: MarfTrieId> AuthenticatedRead<T> for View<'_, T> {
    fn select_trie(&mut self, tip: &T) -> Result<Site, Error> {
        self.site(tip)
    }

    fn root_pointer(&self, site: Site) -> Ptr {
        root_ptr(site)
    }

    /// Read an inline node of `site`'s trie (never follows back-pointers).
    fn read_node(&mut self, site: Site, ptr: &Ptr) -> Result<Node, Error> {
        debug_assert!(!ptr.is_back());
        self.read(site, &ptr.without_back())?.1.to_node()
    }

    /// Borrow an inline node of `site`'s trie (never follows back-pointers).
    fn borrow_node(&mut self, site: Site, ptr: &Ptr) -> Result<impl NodeRead + '_, Error> {
        debug_assert!(!ptr.is_back());
        Ok(self.read(site, &ptr.without_back())?.1)
    }

    /// The hash stored for an inline child of `site`'s trie.
    fn child_hash(&mut self, site: Site, ptr: &Ptr) -> Result<TrieHash, Error> {
        match site {
            Site::Mem => {
                let idx = u32::try_from(ptr.ptr).map_err(|_| Error::OverflowError)?;
                self.inner.building.as_ref().expect("Mem").trie.hash(idx)
            }
            Site::Block(id) => self.inner.store.read_hash(self.db, id, ptr.ptr),
        }
    }

    fn block_hash(&mut self, block_id: u32) -> Result<T, Error> {
        Ok(self.inner.store.block_hash(self.db, block_id)?.clone())
    }

    fn ancestor_hashes(&mut self, tip: &T, height: u32) -> Result<Vec<TrieHash>, Error> {
        self.collect_ancestor_hashes(tip, height)
    }

    fn is_pruned(&self) -> bool {
        View::is_pruned(self)
    }
    fn get_block_height(&mut self, block: &T, tip: &T) -> Result<Option<u32>, Error> {
        View::get_block_height(self, block, tip)
    }
    fn get_root_hash_at(&mut self, tip: &T) -> Result<TrieHash, Error> {
        View::get_root_hash_at(self, tip)
    }
    fn get_block_at_height(&mut self, height: u32, tip: &T) -> Result<Option<T>, Error> {
        View::get_block_at_height(self, height, tip)
    }
}
impl<T: MarfTrieId> View<'_, T> {
    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn get_with_proof(
        &mut self,
        block_hash: &T,
        key: &str,
    ) -> Result<Option<(MARFValue, TrieMerkleProof<T>)>, Error> {
        if self.is_pruned() {
            return Err(Error::UnsupportedOnPrunedMarf("get_with_proof"));
        }
        let Some(value) = self.get_by_key(block_hash, key)? else {
            return Ok(None);
        };
        let proof = self.make_proof(&TrieHash::from_key(key), &value, block_hash)?;
        Ok(Some((value, proof)))
    }

    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn get_with_proof_from_hash(
        &mut self,
        block_hash: &T,
        path: &TrieHash,
    ) -> Result<Option<(MARFValue, TrieMerkleProof<T>)>, Error> {
        if self.is_pruned() {
            return Err(Error::UnsupportedOnPrunedMarf("get_with_proof_from_hash"));
        }
        let Some(value) = self.get_by_path(block_hash, path)? else {
            return Ok(None);
        };
        let proof = self.make_proof(path, &value, block_hash)?;
        Ok(Some((value, proof)))
    }
}
impl NodeRead for NodeView<'_> {
    fn path(&self) -> Result<&[u8], Error> {
        NodeView::path(self)
    }
    fn is_leaf(&self) -> Result<bool, Error> {
        NodeView::is_leaf(self)
    }
    fn child(&self, chr: u8) -> Result<Option<Ptr>, Error> {
        NodeView::child(self, chr)
    }
    fn to_node(self) -> Result<Node, Error> {
        NodeView::to_node(self)
    }
}

impl<T: MarfTrieId> View<'_, T> {
    /// Construct an inclusion proof through the authenticated-read capability.
    pub fn make_proof(
        &mut self,
        path: &TrieHash,
        value: &MARFValue,
        block: &T,
    ) -> Result<TrieMerkleProof<T>, Error> {
        self.prove(path, value, block)
    }
}
