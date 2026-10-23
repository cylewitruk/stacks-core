//! The forest: blocks, forks, lookups, writes, ancestry.
//!
//! A [`MARF`] owns a SQLite connection and an [`Inner`] (storage state plus
//! the trie currently being built, if any). Every operation runs through a
//! [`View`], which pairs a connection (plain or inside a transaction) with a
//! mutable borrow of the `Inner`, so the same code serves reads outside and
//! inside write transactions.
//!
//! Reads name the block they target: there is no mutable "current block"
//! cursor. The only piece of cursor-like state kept for API compatibility is
//! the block last passed to [`View::open_block`], which
//! [`View::check_ancestor_block_hash`] uses as its tip.

use rusqlite::{Connection, Transaction};
use stacks_common::types::chainstate::TrieHash;
use stacks_common::util::db::tx_begin_immediate;
use stacks_common::util::hash::Sha512Trunc256Sum;

use crate::codec::{self, Descent, Patch};
use crate::hash::marf_root_hash;
use crate::node::{backptr_children, id, Branch, Kind, Node, Path, Ptr, BACKPTR};
use crate::proofs::TrieMerkleProof;
use crate::pruning::PruneInfo;
use crate::storage::{self, descend_branch, Store, StoreNode};
use crate::trie::{Origin, Trie, ROOT_OFFSET};
use crate::{corrupt, Error, MARFValue, MarfTrieId};

pub const BLOCK_HASH_TO_HEIGHT_MAPPING_KEY: &str = "__MARF_BLOCK_HASH_TO_HEIGHT";
pub const BLOCK_HEIGHT_TO_HASH_MAPPING_KEY: &str = "__MARF_BLOCK_HEIGHT_TO_HASH";
pub const OWN_BLOCK_HEIGHT_KEY: &str = "__MARF_BLOCK_HEIGHT_SELF";

/// Hash calculation mode. Accepted for compatibility; hashes are always
/// computed once when a trie is sealed, which yields identical results.
#[derive(Debug, Clone, PartialEq, Copy)]
pub enum TrieHashCalculationMode {
    Immediate,
    Deferred,
    All,
}

/// Options for opening a MARF
#[derive(Debug, PartialEq, Clone)]
pub struct MARFOpenOpts {
    pub hash_calculation_mode: TrieHashCalculationMode,
    /// store trie blobs externally from the DB, in a flat file
    pub external_blobs: bool,
    /// unconditionally do a DB migration (used for testing)
    pub force_db_migrate: bool,
    /// compress the MARF
    pub compress: bool,
    /// Entries of the per-store cache of resolved patch chains
    /// (`(block_id, offset)` -> `Branch`); 0 disables lookup and retention.
    /// Production value: [`storage::PATCH_CACHE_CAPACITY`]. An experimental
    /// control for benchmarks; consumers leave it at the default.
    pub patch_cache_capacity: usize,
}

impl MARFOpenOpts {
    #[allow(clippy::should_implement_trait)]
    pub fn default() -> MARFOpenOpts {
        MARFOpenOpts {
            hash_calculation_mode: TrieHashCalculationMode::Deferred,
            external_blobs: false,
            force_db_migrate: false,
            patch_cache_capacity: storage::PATCH_CACHE_CAPACITY,
            compress: false,
        }
    }

    pub fn new(
        hash_calculation_mode: TrieHashCalculationMode,
        external_blobs: bool,
    ) -> MARFOpenOpts {
        MARFOpenOpts {
            hash_calculation_mode,
            external_blobs,
            force_db_migrate: false,
            patch_cache_capacity: storage::PATCH_CACHE_CAPACITY,
            compress: false,
        }
    }

    pub fn with_compression(mut self, compression: bool) -> Self {
        self.compress = compression;
        self
    }

    /// Benchmark control: size of the resolved-patch cache (0 disables it).
    pub fn with_patch_cache_capacity(mut self, capacity: usize) -> Self {
        self.patch_cache_capacity = capacity;
        self
    }
}

#[cfg(any(test, feature = "testing"))]
thread_local! {
    static TEST_MARF_COMPRESSION_FLAG: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Test hook: force MARF compression on or off for MARFs opened on this
/// thread through [`test_override_marf_compression`].
#[cfg(any(test, feature = "testing"))]
pub fn fault_injection_marf_compression(enabled: bool) {
    TEST_MARF_COMPRESSION_FLAG.set(Some(enabled));
}

/// Apply test-specific overrides to the MARF compression configuration.
#[cfg(any(test, feature = "testing"))]
pub fn test_override_marf_compression(marf_opts: &mut MARFOpenOpts) {
    if let Some(enabled) = TEST_MARF_COMPRESSION_FLAG.get() {
        marf_opts.compress = enabled;
    }
}

#[cfg(not(any(test, feature = "testing")))]
pub fn test_override_marf_compression(_marf_opts: &mut MARFOpenOpts) {}

/// The trie being built for one block.
struct Building<T> {
    trie: Trie<T>,
    height: u32,
}

/// Everything about a MARF except its SQLite connection.
pub struct Inner<T: MarfTrieId> {
    pub(crate) store: Store<T>,
    building: Option<Building<T>>,
    /// Block most recently targeted by `open_block`.
    current: T,
}

/// Open storage without a MARF wrapper (kept for API compatibility).
pub struct TrieFileStorage<T: MarfTrieId> {
    db: Connection,
    store: Store<T>,
}

impl<T: MarfTrieId> TrieFileStorage<T> {
    fn open_opts(
        db_path: impl AsRef<std::path::Path>,
        readonly: bool,
        unconfirmed: bool,
        opts: MARFOpenOpts,
    ) -> Result<Self, Error> {
        let (db, store) = storage::open::<T>(
            db_path,
            opts.external_blobs,
            opts.compress,
            readonly,
            unconfirmed,
            opts.force_db_migrate,
            opts.patch_cache_capacity,
        )?;
        Ok(TrieFileStorage { db, store })
    }

    pub fn open(db_path: impl AsRef<std::path::Path>, opts: MARFOpenOpts) -> Result<Self, Error> {
        Self::open_opts(db_path, false, false, opts)
    }

    pub fn open_readonly(
        db_path: impl AsRef<std::path::Path>,
        opts: MARFOpenOpts,
    ) -> Result<Self, Error> {
        Self::open_opts(db_path, true, false, opts)
    }

    pub fn open_unconfirmed(
        db_path: impl AsRef<std::path::Path>,
        opts: MARFOpenOpts,
    ) -> Result<Self, Error> {
        Self::open_opts(db_path, false, true, opts)
    }

    pub fn sqlite_conn(&self) -> &Connection {
        &self.db
    }

    pub fn prune_info(&self) -> Option<&PruneInfo> {
        self.store.prune.as_ref()
    }
}

/// Merklized Adaptive-Radix Forest -- a collection of Merklized Adaptive-Radix Tries.
pub struct MARF<T: MarfTrieId> {
    pub(crate) db: Connection,
    pub(crate) inner: Inner<T>,
}

/// A MARF with an open SQLite transaction. Dropping it without `commit`
/// rolls the transaction back (the in-memory trie, if any, is kept).
pub struct MarfTransaction<'a, T: MarfTrieId> {
    /// `None` once committed, rolled back or handed out.
    tx: Option<Transaction<'a>>,
    pub(crate) inner: &'a mut Inner<T>,
}

impl<T: MarfTrieId> Drop for MarfTransaction<'_, T> {
    fn drop(&mut self) {
        // dropping an open SQLite transaction rolls it back; anything cached
        // from rows written inside it must go with it
        if self.tx.take().is_some() {
            self.inner.store.invalidate();
        }
    }
}

/// A connection plus mutable MARF state: where all operations are implemented.
pub struct View<'a, T: MarfTrieId> {
    pub(crate) db: &'a Connection,
    pub(crate) inner: &'a mut Inner<T>,
}

/// A read-only view that shares a MARF's SQLite connection but has its own
/// caches and `open_block` state.
pub struct ReopenedMarf<'a, T: MarfTrieId> {
    db: &'a Connection,
    inner: Inner<T>,
}

/// Where a trie's nodes are read from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Site {
    /// The trie being built, in memory.
    Mem,
    /// A persisted trie with this storage id.
    Block(u32),
}

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
pub(crate) enum NodeView<'a> {
    Mem(&'a Node),
    Store(StoreNode<'a>),
}

impl<'a> NodeView<'a> {
    pub(crate) fn path(&self) -> Result<&'a [u8], Error> {
        match self {
            NodeView::Mem(n) => Ok(n.path().as_slice()),
            NodeView::Store(n) => n.path(),
        }
    }

    pub(crate) fn is_leaf(&self) -> Result<bool, Error> {
        match self {
            NodeView::Mem(n) => Ok(n.is_leaf()),
            NodeView::Store(n) => n.is_leaf(),
        }
    }

    pub(crate) fn child(&self, chr: u8) -> Result<Option<Ptr>, Error> {
        match self {
            NodeView::Mem(Node::Leaf(_)) => Ok(None),
            NodeView::Mem(Node::Branch(b)) => Ok(b.child(chr).copied()),
            NodeView::Store(n) => n.child(chr),
        }
    }

    /// One step of a key walk (see [`codec::Record::descend`]).
    pub(crate) fn descend(&self, key: &[u8], depth: usize) -> Result<Descent<'a>, Error> {
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

    pub(crate) fn leaf_value(&self) -> Result<Option<MARFValue>, Error> {
        match self {
            NodeView::Mem(Node::Leaf(l)) => Ok(Some(l.value.clone())),
            NodeView::Mem(Node::Branch(_)) => Ok(None),
            NodeView::Store(n) => n.leaf_value(),
        }
    }

    pub(crate) fn for_each_child(&self, mut visit: impl FnMut(Ptr)) -> Result<(), Error> {
        match self {
            NodeView::Mem(Node::Leaf(_)) => Ok(()),
            NodeView::Mem(Node::Branch(b)) => {
                b.children.iter().for_each(|p| visit(*p));
                Ok(())
            }
            NodeView::Store(n) => n.for_each_child(visit),
        }
    }

    pub(crate) fn to_node(self) -> Result<Node, Error> {
        match self {
            NodeView::Mem(n) => Ok((*n).clone()),
            NodeView::Store(n) => n.to_node(),
        }
    }
}

/// Shape of one lookup's walk (see [`View::lookup_profile`]).
#[cfg(any(test, feature = "testing"))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WalkProfile {
    /// Locations `(block_id, offset)` of the patched records met, in walk
    /// order (the resolved-patch cache's keys); at most 8 are kept.
    pub patched_sites: [(u32, u64); 8],
    /// Records read, root and leaf included.
    pub nodes: u32,
    /// Back-pointers followed (each one moves the walk to an older block).
    pub backptr_hops: u32,
    /// Records stored as patch chains.
    pub patched_nodes: u32,
    /// Deepest patch chain met (0 if none).
    pub max_patch_depth: u32,
    /// Whether the key is present.
    pub found: bool,
}

/// Which table and name a flushed trie is stored under.
enum FlushTarget<'t, T> {
    Confirmed(&'t T),
    Mined(&'t T),
    Unconfirmed,
}

/// Shared implementation of the read API over [`View`]-like handles.
pub trait MarfConnection<T: MarfTrieId> {
    fn with_conn<F, R>(&mut self, exec: F) -> R
    where
        F: FnOnce(&mut View<'_, T>) -> R;

    fn sqlite_conn(&self) -> &Connection;

    /// Resolve a key from the MARF to a MARFValue with respect to the given block height.
    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    fn get(&mut self, block_hash: &T, key: &str) -> Result<Option<MARFValue>, Error> {
        self.with_conn(|c| c.get_by_key(block_hash, key))
    }

    /// Resolve a TrieHash from the MARF to a MARFValue with respect to the given block height.
    fn get_from_hash(&mut self, block_hash: &T, th: &TrieHash) -> Result<Option<MARFValue>, Error> {
        self.with_conn(|c| c.get_by_path(block_hash, th))
    }

    fn get_block_at_height(&mut self, height: u32, tip: &T) -> Result<Option<T>, Error> {
        self.with_conn(|c| c.get_block_at_height(height, tip))
    }

    fn get_block_height(&mut self, ancestor: &T, tip: &T) -> Result<Option<u32>, Error> {
        self.with_conn(|c| c.get_block_height(ancestor, tip))
    }

    /// Get the root trie hash at a particular block
    fn get_root_hash_at(&mut self, block_hash: &T) -> Result<TrieHash, Error> {
        self.with_conn(|c| c.get_root_hash_at(block_hash))
    }

    fn get_with_proof(
        &mut self,
        block_hash: &T,
        key: &str,
    ) -> Result<Option<(MARFValue, TrieMerkleProof<T>)>, Error> {
        self.with_conn(|c| c.get_with_proof(block_hash, key))
    }

    fn get_with_proof_from_hash(
        &mut self,
        block_hash: &T,
        hash: &TrieHash,
    ) -> Result<Option<(MARFValue, TrieMerkleProof<T>)>, Error> {
        self.with_conn(|c| c.get_with_proof_from_hash(block_hash, hash))
    }

    /// Check if a block can open successfully, i.e., it's a known block, the
    /// storage system isn't issuing IOErrors, and it's in the same fork as the
    /// block last opened with `open_block`.
    fn check_ancestor_block_hash(&mut self, bhh: &T) -> Result<(), Error> {
        self.with_conn(|c| c.check_ancestor_block_hash(bhh))
    }
}

impl<T: MarfTrieId> MarfConnection<T> for MARF<T> {
    fn with_conn<F, R>(&mut self, exec: F) -> R
    where
        F: FnOnce(&mut View<'_, T>) -> R,
    {
        exec(&mut self.view())
    }

    fn sqlite_conn(&self) -> &Connection {
        &self.db
    }
}

impl<T: MarfTrieId> MarfConnection<T> for MarfTransaction<'_, T> {
    fn with_conn<F, R>(&mut self, exec: F) -> R
    where
        F: FnOnce(&mut View<'_, T>) -> R,
    {
        exec(&mut self.view())
    }

    fn sqlite_conn(&self) -> &Connection {
        self.tx()
    }
}

impl<T: MarfTrieId> MarfConnection<T> for ReopenedMarf<'_, T> {
    fn with_conn<F, R>(&mut self, exec: F) -> R
    where
        F: FnOnce(&mut View<'_, T>) -> R,
    {
        exec(&mut View {
            db: self.db,
            inner: &mut self.inner,
        })
    }

    fn sqlite_conn(&self) -> &Connection {
        self.db
    }
}

// ---------------------------------------------------------------------------
// View: reads

impl<'a, T: MarfTrieId> View<'a, T> {
    pub fn sqlite_conn(&self) -> &Connection {
        self.db
    }

    pub fn readonly(&self) -> bool {
        self.inner.store.readonly
    }

    pub fn unconfirmed(&self) -> bool {
        self.inner.store.unconfirmed
    }

    pub fn is_pruned(&self) -> bool {
        self.inner.store.prune.is_some()
    }

    pub fn prune_info(&self) -> Option<&PruneInfo> {
        self.inner.store.prune.as_ref()
    }

    /// The block being built, if any.
    pub fn open_chain_tip(&self) -> Option<&T> {
        self.inner.building.as_ref().map(|b| &b.trie.block)
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

    #[cfg_attr(
        feature = "profiler",
        stacks_profiler::profile(sample_rate = 8, unsampled = "count_only")
    )]
    fn site(&mut self, bhh: &T) -> Result<Site, Error> {
        if let Some(b) = self.inner.building.as_ref() {
            if b.trie.block == *bhh {
                return Ok(Site::Mem);
            }
        }
        Ok(Site::Block(self.inner.store.block_id(self.db, bhh)?))
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
    fn read_with_depth(&mut self, site: Site, ptr: &Ptr) -> Result<(Site, NodeView<'_>, u32), Error> {
        count!(node_visits);
        if ptr.is_back() {
            count!(backptr_follows);
            let r = self.inner.store.node(self.db, ptr.back_block, ptr)?;
            return Ok((Site::Block(ptr.back_block), NodeView::Store(r.node), r.depth));
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
        if let Some(squash_height) = self.inner.store.prune_height() {
            if tip_height <= squash_height {
                if height > tip_height {
                    return Ok(None);
                }
                return self.inner.store.pruned_block_at_height(self.db, height);
            }
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

    /// MARF root hashes of the ancestors at heights `h - 2^i` of a trie at
    /// `tip` whose height is `height`.
    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    fn ancestor_hashes(&mut self, tip: &T, height: u32) -> Result<Vec<TrieHash>, Error> {
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

// ---------------------------------------------------------------------------
// View: leaf traversal

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

// ---------------------------------------------------------------------------
// View: helpers for proof generation (crate-private)

impl<'a, T: MarfTrieId> View<'a, T> {
    pub(crate) fn proof_site(&mut self, tip: &T) -> Result<Site, Error> {
        self.site(tip)
    }

    pub(crate) fn proof_root_ptr(&self, site: Site) -> Ptr {
        root_ptr(site)
    }

    /// Read an inline node of `site`'s trie (never follows back-pointers).
    pub(crate) fn proof_read(&mut self, site: Site, ptr: &Ptr) -> Result<Node, Error> {
        debug_assert!(!ptr.is_back());
        self.read(site, &ptr.without_back())?.1.to_node()
    }

    /// Borrow an inline node of `site`'s trie (never follows back-pointers).
    pub(crate) fn proof_view(&mut self, site: Site, ptr: &Ptr) -> Result<NodeView<'_>, Error> {
        debug_assert!(!ptr.is_back());
        Ok(self.read(site, &ptr.without_back())?.1)
    }

    /// The hash stored for an inline child of `site`'s trie.
    pub(crate) fn proof_child_hash(&mut self, site: Site, ptr: &Ptr) -> Result<TrieHash, Error> {
        match site {
            Site::Mem => {
                let idx = u32::try_from(ptr.ptr).map_err(|_| Error::OverflowError)?;
                self.inner.building.as_ref().expect("Mem").trie.hash(idx)
            }
            Site::Block(id) => self.inner.store.read_hash(self.db, id, ptr.ptr),
        }
    }

    pub(crate) fn proof_block_hash(&mut self, block_id: u32) -> Result<T, Error> {
        Ok(self.inner.store.block_hash(self.db, block_id)?.clone())
    }

    pub(crate) fn proof_ancestor_hashes(
        &mut self,
        tip: &T,
        height: u32,
    ) -> Result<Vec<TrieHash>, Error> {
        self.ancestor_hashes(tip, height)
    }

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

// ---------------------------------------------------------------------------
// View: writes

impl<'a, T: MarfTrieId> View<'a, T> {
    fn require_writable(&self) -> Result<(), Error> {
        if self.inner.store.readonly {
            return Err(Error::ReadOnlyError);
        }
        Ok(())
    }

    /// Height of the block that would extend `parent` (also validates that
    /// `parent` exists).
    fn extension_height(&mut self, parent: &T) -> Result<u32, Error> {
        if *parent == T::sentinel() {
            return Ok(0);
        }
        self.site(parent)?;
        let h = self
            .get_block_height(parent, parent)?
            .ok_or_else(|| corrupt!("Failed to find block height for `{parent:?}`"))?;
        h.checked_add(1).ok_or(Error::OverflowError)
    }

    /// The root of a new trie extending `parent`: either a fresh empty root
    /// or the parent's root with its children turned into back-pointers.
    fn root_for(&mut self, parent: &T) -> Result<(Branch, Option<Origin>), Error> {
        if *parent == T::sentinel() || self.inner.store.num_blocks(self.db)? == 0 {
            return Ok((Branch::new(Kind::Node256, Path::EMPTY), None));
        }
        let pid = self.inner.store.block_id(self.db, parent)?;
        let ptr = Ptr {
            id: id::NODE256 | BACKPTR,
            chr: 0,
            ptr: ROOT_OFFSET,
            back_block: pid,
        };
        let (node, _, depth) = self.inner.store.read_node(self.db, pid, &ptr)?;
        let Node::Branch(mut root) = node else {
            return Err(corrupt!("root of {parent} is not a branch"));
        };
        if root.kind != Kind::Node256 {
            return Err(corrupt!("root of {parent} is not a Node256"));
        }
        backptr_children(&mut root.children, pid);
        Ok((
            root,
            Some(Origin {
                block: pid,
                ptr,
                depth,
            }),
        ))
    }

    /// Begin building `child` on top of `parent`.
    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn begin(&mut self, parent: &T, child: &T) -> Result<(), Error> {
        self.require_writable()?;
        if self.inner.building.is_some() {
            return Err(Error::InProgressError);
        }
        if self.inner.store.has_block(self.db, child)? {
            return Err(Error::ExistsError);
        }
        let height = self.extension_height(parent)?;
        if self.inner.store.unconfirmed {
            return Err(Error::UnconfirmedError);
        }
        let (root, origin) = self.root_for(parent)?;
        if !self.inner.store.lock(self.db, child, false)? {
            return Err(Error::ExistsError);
        }
        self.inner.building = Some(Building {
            trie: Trie::new(child.clone(), parent.clone(), root, origin),
            height,
        });
        self.inner.current = child.clone();
        if let Err(e) = self.set_block_heights(parent, child, height) {
            self.inner.building = None;
            return Err(e);
        }
        Ok(())
    }

    /// Begin (or resume) the unconfirmed trie on top of the confirmed `parent`.
    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn begin_unconfirmed(&mut self, parent: &T) -> Result<T, Error> {
        self.require_writable()?;
        if self.inner.building.is_some() {
            return Err(Error::InProgressError);
        }
        if !self.inner.store.unconfirmed {
            return Err(Error::UnconfirmedError);
        }
        if self
            .inner
            .store
            .confirmed_block_id(self.db, parent)?
            .is_none()
        {
            return Err(Error::NotFoundError);
        }
        let tip = MARF::<T>::make_unconfirmed_chain_tip(parent);
        let height = self.extension_height(parent)?;
        let (trie, created) = match self.inner.store.unconfirmed_block_id(self.db, &tip)? {
            Some(id) => {
                let blob = self.inner.store.read_trie_blob(self.db, id)?;
                (Trie::load(&blob, tip.clone())?, false)
            }
            None => {
                let (root, origin) = self.root_for(parent)?;
                (Trie::new(tip.clone(), parent.clone(), root, origin), true)
            }
        };
        if !self.inner.store.lock(self.db, &tip, true)? {
            return Err(Error::ExistsError);
        }
        self.inner.building = Some(Building { trie, height });
        self.inner.current = tip.clone();
        if created {
            if let Err(e) = self.set_block_heights(parent, &tip, height) {
                self.inner.building = None;
                return Err(e);
            }
        }
        Ok(tip)
    }

    /// Record the height mapping keys of a new block. Insertion order is
    /// consensus-relevant (it fixes child slot order) and matches the original.
    pub fn set_block_heights(&mut self, parent: &T, child: &T, height: u32) -> Result<(), Error> {
        let mut keys = vec![
            OWN_BLOCK_HEIGHT_KEY.to_string(),
            format!("{BLOCK_HEIGHT_TO_HASH_MAPPING_KEY}::{height}"),
            format!("{BLOCK_HASH_TO_HEIGHT_MAPPING_KEY}::{child}"),
        ];
        let mut values = vec![
            MARFValue::from(height),
            MARFValue::from(child.clone()),
            MARFValue::from(height),
        ];
        if height > 0 {
            keys.push(format!(
                "{BLOCK_HEIGHT_TO_HASH_MAPPING_KEY}::{}",
                height - 1
            ));
            values.push(MARFValue::from(parent.clone()));
            keys.push(format!("{BLOCK_HASH_TO_HEIGHT_MAPPING_KEY}::{parent}"));
            values.push(MARFValue::from(height - 1));
        }
        self.insert_batch(&keys, values)
    }

    /// Insert key/value pairs into the trie being built.
    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn insert_batch(&mut self, keys: &[String], values: Vec<MARFValue>) -> Result<(), Error> {
        self.require_writable()?;
        assert_eq!(keys.len(), values.len());
        let inner = &mut *self.inner;
        let Some(building) = inner.building.as_mut() else {
            return Err(Error::WriteNotBegunError);
        };
        let db = self.db;
        for (key, value) in keys.iter().zip(values) {
            let path = TrieHash::from_key(key);
            building
                .trie
                .insert(&path.0, value, |ptr| inner.store.fetch(db, ptr))?;
        }
        Ok(())
    }

    /// Insert a single already-hashed path.
    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn insert_raw(&mut self, path: TrieHash, value: MARFValue) -> Result<(), Error> {
        self.require_writable()?;
        let inner = &mut *self.inner;
        let Some(building) = inner.building.as_mut() else {
            return Err(Error::WriteNotBegunError);
        };
        let db = self.db;
        building
            .trie
            .insert(&path.0, value, |ptr| inner.store.fetch(db, ptr))
    }

    /// Compute all hashes of the trie being built and return its MARF root hash.
    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn seal(&mut self) -> Result<TrieHash, Error> {
        self.require_writable()?;
        let (tip, height, sealed) = {
            let b = self
                .inner
                .building
                .as_ref()
                .ok_or(Error::WriteNotBegunError)?;
            (b.trie.block.clone(), b.height, b.trie.is_sealed())
        };
        if sealed {
            return Ok(self
                .inner
                .building
                .as_ref()
                .expect("checked")
                .trie
                .root_hash());
        }
        let root_node_hash = {
            let inner = &mut *self.inner;
            let db = self.db;
            let b = inner.building.as_mut().expect("checked");
            b.trie.seal(|block_id| {
                inner
                    .store
                    .block_hash(db, block_id)
                    .map(|h| h.clone().to_bytes())
            })?
        };
        let ancestors = self.ancestor_hashes(&tip, height)?;
        let root = marf_root_hash(&root_node_hash, &ancestors);
        self.inner
            .building
            .as_mut()
            .expect("checked")
            .trie
            .set_root_hash(root);
        Ok(root)
    }

    /// Build the delta for a copied-forward node, if a patch applies.
    ///
    /// The delta is computed against the node as resolved at its origin (with
    /// any patches there already applied), so a chain of amendments stays
    /// small. The patch's base points at the origin record: the node itself
    /// when it is a plain node, or the outermost patch of the chain.
    fn make_patch(&mut self, branch: &Branch, origin: &Origin) -> Result<Option<Patch>, Error> {
        let Some((old, _)) = self
            .inner
            .store
            .branch(self.db, origin.block, &origin.ptr)?
        else {
            return Ok(None);
        };
        if old.path != branch.path || old.kind != branch.kind {
            return Ok(None);
        }
        let base = Ptr {
            id: if origin.depth == 0 {
                origin.ptr.kind_id()
            } else {
                id::PATCH
            } | BACKPTR,
            chr: origin.ptr.chr,
            ptr: origin.ptr.ptr,
            back_block: origin.block,
        };
        Ok(codec::make_patch(base, &old.children, branch))
    }

    /// Seal, serialize and store the trie being built.
    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    fn flush(&mut self, target: FlushTarget<'_, T>) -> Result<(), Error> {
        self.require_writable()?;
        if self.inner.building.is_none() {
            return Ok(());
        }
        match target {
            FlushTarget::Unconfirmed if !self.inner.store.unconfirmed => {
                return Err(Error::UnconfirmedError)
            }
            FlushTarget::Confirmed(_) | FlushTarget::Mined(_) if self.inner.store.unconfirmed => {
                return Err(Error::UnconfirmedError)
            }
            _ => {}
        }
        self.seal()?;
        let compress = self.inner.store.compress && !matches!(target, FlushTarget::Unconfirmed);
        let trie = self.inner.building.take().expect("checked").trie;
        let blob = trie.serialize(compress, |branch, origin, _| {
            self.make_patch(branch, origin)
        })?;
        let built = &trie.block;
        match target {
            FlushTarget::Confirmed(bhh) => {
                self.inner.store.write_confirmed(self.db, bhh, &blob)?;
                self.inner.current = bhh.clone();
            }
            FlushTarget::Mined(bhh) => {
                self.inner.store.write_mined(self.db, bhh, &blob)?;
            }
            FlushTarget::Unconfirmed => {
                self.inner.store.write_unconfirmed(self.db, built, &blob)?;
            }
        }
        self.inner.store.unlock(self.db, built)?;
        Ok(())
    }

    /// Abandon the trie being built (does not touch the SQLite transaction).
    pub(crate) fn drop_extending_trie(&mut self) -> Result<(), Error> {
        if self.inner.store.readonly {
            return Ok(());
        }
        if let Some(b) = self.inner.building.take() {
            self.inner.store.unlock(self.db, &b.trie.block)?;
        }
        Ok(())
    }

    /// Abandon and delete the unconfirmed trie being built.
    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn drop_unconfirmed_trie(&mut self) -> Result<(), Error> {
        if self.inner.store.readonly || !self.inner.store.unconfirmed {
            return Ok(());
        }
        if let Some(b) = self.inner.building.take() {
            self.inner.store.drop_unconfirmed(self.db, &b.trie.block)?;
            self.inner.store.unlock(self.db, &b.trie.block)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// MarfTransaction

impl<'a, T: MarfTrieId> MarfTransaction<'a, T> {
    fn tx(&self) -> &Transaction<'a> {
        self.tx.as_ref().expect("transaction still open")
    }

    pub(crate) fn view(&mut self) -> View<'_, T> {
        View {
            db: self.tx.as_ref().expect("transaction still open"),
            inner: self.inner,
        }
    }

    pub fn sqlite_tx(&self) -> &Transaction<'a> {
        self.tx()
    }

    pub fn sqlite_tx_mut(&mut self) -> &mut Transaction<'a> {
        self.tx.as_mut().expect("transaction still open")
    }

    fn take_tx(&mut self) -> Transaction<'a> {
        self.tx.take().expect("transaction still open")
    }

    fn finish(mut self) -> Result<(), Error> {
        match self.take_tx().commit() {
            Ok(()) => Ok(()),
            Err(e) => {
                self.inner.store.invalidate();
                Err(e.into())
            }
        }
    }

    pub(crate) fn into_sqlite_tx(mut self) -> Transaction<'a> {
        // the caller decides the transaction's fate; be conservative
        self.inner.store.invalidate();
        self.take_tx()
    }

    /// Commit the SQLite transaction without flushing the trie being built.
    pub fn commit_tx(mut self) {
        self.take_tx()
            .commit()
            .expect("CORRUPTION: Failed to commit MARF");
    }

    pub fn rollback(mut self) {
        self.take_tx()
            .rollback()
            .expect("CORRUPTION: Failed to roll back MARF");
        self.inner.store.invalidate();
    }

    /// Begin writing the next trie in the MARF. Fails if the block already exists.
    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn begin(&mut self, chain_tip: &T, next_chain_tip: &T) -> Result<(), Error> {
        self.view().begin(chain_tip, next_chain_tip)
    }

    /// Begin extending the MARF to an unconfirmed trie built on `chain_tip`.
    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn begin_unconfirmed(&mut self, chain_tip: &T) -> Result<T, Error> {
        self.view().begin_unconfirmed(chain_tip)
    }

    pub fn set_block_heights(
        &mut self,
        block_hash: &T,
        next_block_hash: &T,
        height: u32,
    ) -> Result<(), Error> {
        self.view()
            .set_block_heights(block_hash, next_block_hash, height)
    }

    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn insert_batch(&mut self, keys: &[String], values: Vec<MARFValue>) -> Result<(), Error> {
        self.view().insert_batch(keys, values)
    }

    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn insert(&mut self, key: &str, value: MARFValue) -> Result<(), Error> {
        self.view().insert_batch(&[key.to_string()], vec![value])
    }

    /// Seal the trie being built and return the MARF root hash.
    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn seal(&mut self) -> Result<TrieHash, Error> {
        self.view().seal()
    }

    pub fn get_open_chain_tip(&self) -> Option<&T> {
        self.inner.building.as_ref().map(|b| &b.trie.block)
    }

    pub fn get_open_chain_tip_height(&self) -> Option<u32> {
        self.inner.building.as_ref().map(|b| b.height)
    }

    pub fn get_block_height_of(
        &mut self,
        bhh: &T,
        current_block_hash: &T,
    ) -> Result<Option<u32>, Error> {
        if Some(bhh) == self.get_open_chain_tip() {
            return Ok(self.get_open_chain_tip_height());
        }
        self.view().get_block_height(bhh, current_block_hash)
    }

    /// Persist the trie being built under the block it was begun with and commit.
    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn commit(mut self) -> Result<(), Error> {
        if self.inner.store.readonly {
            return Err(Error::ReadOnlyError);
        }
        if let Some(tip) = self.get_open_chain_tip().cloned() {
            let target = if self.inner.store.unconfirmed {
                FlushTarget::Unconfirmed
            } else {
                FlushTarget::Confirmed(&tip)
            };
            self.view().flush(target)?;
        }
        self.finish()
    }

    /// Persist the trie being built under `real_bhh` and commit.
    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn commit_to(mut self, real_bhh: &T) -> Result<(), Error> {
        if self.inner.store.readonly {
            return Err(Error::ReadOnlyError);
        }
        if self.inner.store.unconfirmed {
            return Err(Error::UnconfirmedError);
        }
        if self.inner.building.is_some() {
            self.view().flush(FlushTarget::Confirmed(real_bhh))?;
            self.finish()?;
        }
        Ok(())
    }

    /// Persist the trie being built into the mined-blocks table and commit.
    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn commit_mined(mut self, bhh: &T) -> Result<(), Error> {
        if self.inner.store.readonly {
            return Err(Error::ReadOnlyError);
        }
        if self.inner.store.unconfirmed {
            return Err(Error::UnconfirmedError);
        }
        if self.inner.building.is_some() {
            self.view().flush(FlushTarget::Mined(bhh))?;
            self.finish()?;
        }
        Ok(())
    }

    /// Drop the trie being built and roll back.
    pub fn drop_current(mut self) {
        if !self.inner.store.readonly {
            self.view()
                .drop_extending_trie()
                .expect("Corruption: Failed to drop the extended trie lock");
            self.inner.current = T::sentinel();
            self.rollback();
        }
    }

    /// Drop the unconfirmed trie being built, delete its persisted state, and commit.
    pub fn drop_unconfirmed(mut self) {
        if !self.inner.store.readonly
            && self.inner.store.unconfirmed
            && self.inner.building.is_some()
        {
            self.view()
                .drop_unconfirmed_trie()
                .expect("Corruption: Failed to drop unconfirmed trie");
            self.inner.current = T::sentinel();
            // unconfirmed state may already be on disk, so this cannot be a rollback
            self.commit_tx();
        }
    }

    /// Reopen this MARF read-only on a fresh connection. Fails if a trie is
    /// being built.
    pub fn reopen_readonly(&self) -> Result<MARF<T>, Error> {
        if self.inner.building.is_some() {
            return Err(Error::InProgressError);
        }
        MARF::reopen_readonly_inner(self.inner)
    }
}

// ---------------------------------------------------------------------------
// MARF

impl<T: MarfTrieId> MARF<T> {
    fn view(&mut self) -> View<'_, T> {
        View {
            db: &self.db,
            inner: &mut self.inner,
        }
    }

    pub fn from_storage(storage: TrieFileStorage<T>) -> MARF<T> {
        MARF {
            db: storage.db,
            inner: Inner {
                store: storage.store,
                building: None,
                current: T::sentinel(),
            },
        }
    }

    pub fn from_path(
        path: impl AsRef<std::path::Path>,
        open_opts: MARFOpenOpts,
    ) -> Result<MARF<T>, Error> {
        Ok(MARF::from_storage(TrieFileStorage::open(path, open_opts)?))
    }

    pub fn from_path_unconfirmed(
        path: impl AsRef<std::path::Path>,
        open_opts: MARFOpenOpts,
    ) -> Result<MARF<T>, Error> {
        Ok(MARF::from_storage(TrieFileStorage::open_unconfirmed(
            path, open_opts,
        )?))
    }

    /// Entries in the resolved-patch cache (benchmark instrumentation).
    #[cfg(any(test, feature = "testing", feature = "counters"))]
    pub fn patch_cache_len(&self) -> usize {
        self.inner.store.patch_cache_len()
    }

    pub fn begin_tx(&mut self) -> Result<MarfTransaction<'_, T>, Error> {
        if self.inner.store.readonly {
            return Err(Error::ReadOnlyError);
        }
        let tx = tx_begin_immediate(&mut self.db)?;
        Ok(MarfTransaction {
            tx: Some(tx),
            inner: &mut self.inner,
        })
    }

    /// Target the MARF at a given block (for `check_ancestor_block_hash`).
    pub fn open_block(&mut self, block_hash: &T) -> Result<(), Error> {
        self.view().open_block(block_hash)
    }

    /// See [`View::lookup_profile`].
    #[cfg(any(test, feature = "testing"))]
    pub fn lookup_profile(&mut self, tip: &T, key: &str) -> Result<WalkProfile, Error> {
        self.view().lookup_profile(tip, &TrieHash::from_key(key))
    }

    /// The MARF root hash of `block_hash` (also available through `MarfConnection`).
    pub fn get_root_hash_at(&mut self, block_hash: &T) -> Result<TrieHash, Error> {
        self.view().get_root_hash_at(block_hash)
    }

    pub fn get_bhh_at_height(&mut self, block_hash: &T, height: u32) -> Result<Option<T>, Error> {
        self.view().get_block_at_height(height, block_hash)
    }

    pub fn get_block_height_of(
        &mut self,
        bhh: &T,
        current_block_hash: &T,
    ) -> Result<Option<u32>, Error> {
        if Some(bhh) == self.get_open_chain_tip() {
            return Ok(self.get_open_chain_tip_height());
        }
        self.view().get_block_height(bhh, current_block_hash)
    }

    pub fn get_open_chain_tip(&self) -> Option<&T> {
        self.inner.building.as_ref().map(|b| &b.trie.block)
    }

    pub fn get_open_chain_tip_height(&self) -> Option<u32> {
        self.inner.building.as_ref().map(|b| b.height)
    }

    /// Insert a batch of key/value pairs into the trie being built.
    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn insert_batch(&mut self, keys: &[String], values: Vec<MARFValue>) -> Result<(), Error> {
        let mut tx = self.begin_tx()?;
        tx.insert_batch(keys, values)?;
        tx.commit_tx();
        Ok(())
    }

    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn insert(&mut self, key: &str, value: MARFValue) -> Result<(), Error> {
        self.insert_batch(&[key.to_string()], vec![value])
    }

    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn insert_raw(&mut self, path: TrieHash, value: MARFValue) -> Result<(), Error> {
        let mut tx = self.begin_tx()?;
        tx.view().insert_raw(path, value)?;
        tx.commit_tx();
        Ok(())
    }

    /// Drop the trie being built and release its extension lock. Unlike the
    /// transaction-level [`MarfTransaction::drop_current`], which rolls its
    /// transaction back, this commits the lock removal: the begin may already
    /// have been committed by an earlier transaction.
    pub fn drop_current(&mut self) {
        if !self.inner.store.readonly {
            let mut tx = self
                .begin_tx()
                .expect("BUG: failed to start transaction to drop trie");
            tx.view()
                .drop_extending_trie()
                .expect("Corruption: Failed to drop the extended trie lock");
            tx.inner.current = T::sentinel();
            tx.commit_tx();
        }
    }

    pub fn drop_unconfirmed(&mut self) {
        if !self.inner.store.readonly && self.inner.store.unconfirmed {
            let tx = self
                .begin_tx()
                .expect("BUG: failed to start transaction to drop trie");
            tx.drop_unconfirmed();
        }
    }

    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn commit(&mut self) -> Result<(), Error> {
        self.begin_tx()?.commit()
    }

    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn commit_mined(&mut self, bhh: &T) -> Result<(), Error> {
        self.begin_tx()?.commit_mined(bhh)
    }

    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn commit_to(&mut self, real_bhh: &T) -> Result<(), Error> {
        self.begin_tx()?.commit_to(real_bhh)
    }

    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn seal(&mut self) -> Result<TrieHash, Error> {
        let mut tx = self.begin_tx()?;
        let h = tx.seal()?;
        tx.commit_tx();
        Ok(h)
    }

    /// Test helper: begin a block in its own committed SQLite transaction,
    /// leaving the trie open in memory for `insert`/`commit` on this handle.
    #[cfg(any(test, feature = "testing"))]
    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn begin(&mut self, chain_tip: &T, next_chain_tip: &T) -> Result<(), Error> {
        let mut tx = self.begin_tx()?;
        tx.begin(chain_tip, next_chain_tip)?;
        tx.commit_tx();
        Ok(())
    }

    #[cfg(any(test, feature = "testing"))]
    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn begin_unconfirmed(&mut self, chain_tip: &T) -> Result<T, Error> {
        let mut tx = self.begin_tx()?;
        let tip = tx.begin_unconfirmed(chain_tip)?;
        tx.commit_tx();
        Ok(tip)
    }

    /// Access the storage handle (test helper).
    #[cfg(any(test, feature = "testing"))]
    pub fn borrow_storage_backend(&mut self) -> View<'_, T> {
        self.view()
    }

    /// Make a raw transaction to the underlying storage
    pub fn storage_tx(&mut self) -> Result<Transaction<'_>, rusqlite::Error> {
        tx_begin_immediate(&mut self.db)
    }

    /// Build a read-only view which reuses this MARF's SQLite connection but
    /// has its own `open_block` target and caches.
    pub fn reopen_connection(&self) -> Result<ReopenedMarf<'_, T>, Error> {
        if self.inner.building.is_some() {
            return Err(Error::InProgressError);
        }
        Ok(ReopenedMarf {
            db: &self.db,
            inner: Inner {
                store: self.inner.store.reopen_readonly(),
                building: None,
                current: self.inner.current.clone(),
            },
        })
    }

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

    /// Reopen this MARF read-only on a fresh connection. Fails if a trie is
    /// being built.
    pub fn reopen_readonly(&self) -> Result<MARF<T>, Error> {
        if self.inner.building.is_some() {
            return Err(Error::InProgressError);
        }
        Self::reopen_readonly_inner(&self.inner)
    }

    pub fn into_sqlite_conn(self) -> Connection {
        self.db
    }

    pub fn get_db_path(&self) -> &std::path::Path {
        &self.inner.store.db_path
    }

    pub fn prune_info(&self) -> Option<&PruneInfo> {
        self.inner.store.prune.as_ref()
    }

    /// Make an unconfirmed chain tip from an existing chain tip, so that it
    /// won't conflict with the "true" chain tip after the state it represents
    /// is later reprocessed and confirmed.
    pub fn make_unconfirmed_chain_tip(chain_tip: &T) -> T {
        let mut bytes = [0u8; 64];
        bytes[0..32].copy_from_slice(chain_tip.as_bytes());
        bytes[32..64].copy_from_slice(chain_tip.as_bytes());
        let h = Sha512Trunc256Sum::from_data(&bytes);
        let mut res = [0u8; 32];
        res.copy_from_slice(h.as_bytes());
        T::from_bytes(res)
    }
}

impl<'a, T: MarfTrieId> ReopenedMarf<'a, T> {
    pub fn db_conn(&self) -> &Connection {
        self.db
    }

    pub fn connection(&mut self) -> View<'_, T> {
        View {
            db: self.db,
            inner: &mut self.inner,
        }
    }
}
