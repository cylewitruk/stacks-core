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
use super::record::NodeRecordFormat;
use super::ValueExtentResolver;
use std::ops::Deref;
use std::sync::Arc;
#[cfg(any(test, feature = "testing"))]
use std::sync::LazyLock;
use std::time::Instant;

#[cfg(any(test, feature = "testing"))]
use clarity::util::tests::TestFlag;
use rusqlite::{Connection, Transaction};
use stacks_common::types::chainstate::{TrieHash, TRIEHASH_ENCODED_SIZE};
use stacks_common::util::hash::Sha512Trunc256Sum;

pub use super::squash::SquashStats;
use super::storage::ReopenedTrieStorageConnection;
use crate::chainstate::stacks::index::node::{
    clear_backptr, is_backptr, node_copy_update_ptrs, set_backptr, CursorError, TrieCowPtr,
    TrieCursor, TrieNode256, TrieNodeID, TrieNodeType, TriePtr,
};
use crate::chainstate::stacks::index::result_cache::DEFAULT_RESULT_CACHE_CAPACITY;
use crate::chainstate::stacks::index::scratch::MarfReadState;
use crate::chainstate::stacks::index::storage::{
    SquashInfo, TrieFileStorage, TrieHashCalculationMode, TrieStorageConnection,
    TrieStorageTransaction,
};
use crate::chainstate::stacks::index::trie::Trie;
use crate::chainstate::stacks::index::{
    bits, Error, MARFValue, MarfTrieId, NodeParking, NodePatching, ReadNodeBacking, ReadTrieNode,
    ReadTrieNodeCursorStep, TrieLeaf, TrieMerkleProof, TrieReadSession, TrieReadStorage,
};
use crate::util_lib::db::Error as db_error;

pub const BLOCK_HASH_TO_HEIGHT_MAPPING_KEY: &str = "__MARF_BLOCK_HASH_TO_HEIGHT";
pub const BLOCK_HEIGHT_TO_HASH_MAPPING_KEY: &str = "__MARF_BLOCK_HEIGHT_TO_HASH";
pub const OWN_BLOCK_HEIGHT_KEY: &str = "__MARF_BLOCK_HEIGHT_SELF";

/// Reserved metadata writes outside `set_block_heights` require authoritative trie traversal.
fn reserved_height_key(key: &str) -> bool {
    key == OWN_BLOCK_HEIGHT_KEY
        || key.starts_with(BLOCK_HEIGHT_TO_HASH_MAPPING_KEY)
        || key.starts_with(BLOCK_HASH_TO_HEIGHT_MAPPING_KEY)
}

#[cfg(any(test, feature = "testing"))]
/// Global default override for MARF compression used in tests.
///
/// This constant allows forcing *all* MARF instances created in tests
/// to use compression (`Some(true)`) or to disable it (`Some(false)`),
/// regardless of the test’s local configuration.
///
/// When set to `None`, test's own MARF configuration is used.
const TEST_MARF_COMPRESSION_DEFAULT: Option<bool> = None;

#[cfg(any(test, feature = "testing"))]
/// Test flag used to override MARF compression during test execution.
///
/// This flag enables tests to dynamically enable or disable MARF compression
/// *after* process startup, allowing scenarios where compression is switched
/// on and off within the same test.
static TEST_MARF_COMPRESSION_FLAG: LazyLock<TestFlag<Option<bool>>> =
    LazyLock::new(TestFlag::default);

#[cfg(any(test, feature = "testing"))]
/// Inject a runtime override for MARF compression in tests.
pub fn fault_injection_marf_compression(enabled: bool) {
    TEST_MARF_COMPRESSION_FLAG.set(Some(enabled));
}

#[cfg(any(test, feature = "testing"))]
/// Apply test-specific overrides to the MARF compression configuration.
///
/// This function mutates the provided [`MARFOpenOpts`], according to the
/// following precedence order:
///
/// 1. Runtime test override via [`TEST_MARF_COMPRESSION_FLAG`]
/// 2. Global test default via [`TEST_MARF_COMPRESSION_DEFAULT`]
/// 3. The original value in [`MARFOpenOpts`] (no override)
///
/// In non-test builds, this function is compiled to a no-op.
pub fn test_override_marf_compression(marf_opts: &mut MARFOpenOpts) {
    if let Some(enabled) = TEST_MARF_COMPRESSION_FLAG.get() {
        marf_opts.compress = enabled;
        info!("Test flag used. MARF Compression overridden to {enabled}");
        return;
    }

    if let Some(enabled) = TEST_MARF_COMPRESSION_DEFAULT {
        marf_opts.compress = enabled;
        info!("Test default used. MARF Compression overridden to {enabled}");
    }
}

#[cfg(not(any(test, feature = "testing")))]
/// No-op stub for non-test builds.
pub fn test_override_marf_compression(_marf_opts: &mut MARFOpenOpts) {}

/// Merklized Adaptive-Radix Forest -- a collection of Merklized Adaptive-Radix Tries.
pub struct MARF<T: MarfTrieId> {
    storage: TrieFileStorage<T>,
    open_chain_tip: Option<WriteChainTip<T>>,
    read_cursor: Option<TrieCursor<T>>,
    nested_read_cursor: Option<TrieCursor<T>>,
    read_state: MarfReadState,
}

pub struct MarfTransaction<'a, T: MarfTrieId> {
    pub(crate) storage: TrieStorageTransaction<'a, T>,
    open_chain_tip: &'a mut Option<WriteChainTip<T>>,
    read_cursor: &'a mut Option<TrieCursor<T>>,
    nested_read_cursor: &'a mut Option<TrieCursor<T>>,
    read_state: &'a mut MarfReadState,
}

pub struct MarfReadCtx<'a, T: MarfTrieId, S: NodePatching, R: TrieReadStorage<T> + ?Sized> {
    storage: &'a mut R,
    read_cursor: &'a mut Option<TrieCursor<T>>,
    nested_read_cursor: &'a mut Option<TrieCursor<T>>,
    read_state: &'a mut S,
}

struct ResolvedBackptrNode<'a> {
    node: ReadTrieNode<'a>,
    ptr: TriePtr,
    block_id: u32,
}

struct CopiedChild<T> {
    node: TrieNodeType,
    ptr: TriePtr,
    source_block_hash: T,
}

struct ResolvedReadNode<'a, T> {
    node: ReadTrieNode<'a>,
    block_hash: T,
    block_id: Option<u32>,
}

pub trait MarfCore<T: MarfTrieId> {
    type ReadStorage<'a>: TrieReadStorage<T> + ?Sized
    where
        Self: 'a;
    type ReadState: NodePatching;

    fn with_storage<'a, F, Ret>(&'a mut self, exec: F) -> Ret
    where
        F: FnOnce(&mut Self::ReadStorage<'a>) -> Ret;

    fn with_read_ctx<'a, F, Ret>(&'a mut self, exec: F) -> Ret
    where
        F: for<'ctx> FnOnce(
            &mut MarfReadCtx<'ctx, T, Self::ReadState, Self::ReadStorage<'a>>,
        ) -> Ret;

    fn get_path(&mut self, block_hash: &T, path: &TrieHash) -> Result<Option<TrieLeaf>, Error> {
        self.with_read_ctx(|ctx| ctx.get_path(block_hash, path))
    }

    /// Open the MARF's storage to a given block, optionally with a specific block ID.
    fn open_block(&mut self, bhh: &T, block_id: Option<u32>) -> Result<(), Error> {
        self.with_read_ctx(|ctx| ctx.open_block(bhh, block_id))
    }

    fn get_by_path(&mut self, block_hash: &T, path: &TrieHash) -> Result<Option<MARFValue>, Error> {
        self.with_read_ctx(|ctx| ctx.get_by_path(block_hash, path))
    }

    fn get_by_key(&mut self, block_hash: &T, key: &str) -> Result<Option<MARFValue>, Error> {
        self.with_read_ctx(|ctx| ctx.get_by_key(block_hash, key))
    }

    fn prove_path(
        &mut self,
        block_hash: &T,
        path: &TrieHash,
        marf_value: &MARFValue,
    ) -> Result<TrieMerkleProof<T>, Error> {
        self.with_read_ctx(|ctx| ctx.prove_path(block_hash, path, marf_value))
    }

    fn prove_raw_entry(
        &mut self,
        block_hash: &T,
        key: &str,
        marf_value: &MARFValue,
    ) -> Result<TrieMerkleProof<T>, Error> {
        self.with_read_ctx(|ctx| ctx.prove_raw_entry(block_hash, key, marf_value))
    }

    fn get_block_height_miner_tip(
        &mut self,
        block_hash: &T,
        current_block_hash: &T,
    ) -> Result<Option<u32>, Error> {
        self.with_read_ctx(|ctx| ctx.get_block_height_miner_tip(block_hash, current_block_hash))
    }

    fn get_block_height(
        &mut self,
        block_hash: &T,
        current_block_hash: &T,
    ) -> Result<Option<u32>, Error> {
        self.with_read_ctx(|ctx| ctx.get_block_height(block_hash, current_block_hash))
    }

    fn get_block_at_height(
        &mut self,
        height: u32,
        current_block_hash: &T,
    ) -> Result<Option<T>, Error> {
        self.with_read_ctx(|ctx| ctx.get_block_at_height(height, current_block_hash))
    }
}

fn with_read_storage_read_ctx<'ctx, T, S, R, F, Ret>(
    storage: &'ctx mut R,
    cursor: &'ctx mut Option<TrieCursor<T>>,
    nested_cursor: &'ctx mut Option<TrieCursor<T>>,
    read_state: &'ctx mut S,
    exec: F,
) -> Ret
where
    T: MarfTrieId,
    S: NodePatching,
    R: TrieReadStorage<T> + ?Sized,
    F: FnOnce(&mut MarfReadCtx<'ctx, T, S, R>) -> Ret,
{
    let mut read_ctx = MarfReadCtx::<T, S, R>::new(storage, cursor, nested_cursor, read_state);
    exec(&mut read_ctx)
}

impl<'ctx, T: MarfTrieId, S: NodePatching, R: TrieReadStorage<T> + ?Sized> MarfCore<T>
    for MarfReadCtx<'ctx, T, S, R>
{
    type ReadStorage<'a>
        = R
    where
        Self: 'a;
    type ReadState = S;

    fn with_storage<'a, F, Ret>(&'a mut self, exec: F) -> Ret
    where
        F: FnOnce(&mut Self::ReadStorage<'a>) -> Ret,
    {
        exec(self.storage)
    }

    fn with_read_ctx<'a, F, Ret>(&'a mut self, exec: F) -> Ret
    where
        F: for<'read> FnOnce(&mut MarfReadCtx<'read, T, S, Self::ReadStorage<'a>>) -> Ret,
    {
        exec(self)
    }
}

impl<T: MarfTrieId> MarfCore<T> for MARF<T> {
    type ReadStorage<'a>
        = TrieStorageConnection<'a, T>
    where
        Self: 'a;
    type ReadState = MarfReadState;

    fn with_storage<'a, F, Ret>(&'a mut self, exec: F) -> Ret
    where
        F: FnOnce(&mut Self::ReadStorage<'a>) -> Ret,
    {
        let mut conn = self.storage.connection();
        exec(&mut conn)
    }

    fn with_read_ctx<'a, F, Ret>(&'a mut self, exec: F) -> Ret
    where
        F: for<'ctx> FnOnce(
            &mut MarfReadCtx<'ctx, T, Self::ReadState, Self::ReadStorage<'a>>,
        ) -> Ret,
    {
        let mut conn = self.storage.connection();
        with_read_storage_read_ctx(
            &mut conn,
            &mut self.read_cursor,
            &mut self.nested_read_cursor,
            &mut self.read_state,
            exec,
        )
    }
}

impl<'tx, T: MarfTrieId> MarfCore<T> for MarfTransaction<'tx, T> {
    type ReadStorage<'a>
        = TrieStorageConnection<'tx, T, Transaction<'tx>>
    where
        Self: 'a;
    type ReadState = MarfReadState;

    fn with_storage<'a, F, Ret>(&'a mut self, exec: F) -> Ret
    where
        F: FnOnce(&mut Self::ReadStorage<'a>) -> Ret,
    {
        exec(&mut self.storage)
    }

    fn with_read_ctx<'a, F, Ret>(&'a mut self, exec: F) -> Ret
    where
        F: for<'ctx> FnOnce(
            &mut MarfReadCtx<'ctx, T, Self::ReadState, Self::ReadStorage<'a>>,
        ) -> Ret,
    {
        with_read_storage_read_ctx(
            &mut self.storage,
            &mut self.read_cursor,
            &mut self.nested_read_cursor,
            &mut *self.read_state,
            exec,
        )
    }
}

impl<'conn, T: MarfTrieId> MarfCore<T> for ReopenedTrieStorageConnection<'conn, T> {
    type ReadStorage<'a>
        = TrieStorageConnection<'a, T>
    where
        Self: 'a;
    type ReadState = MarfReadState;

    fn with_storage<'a, F, Ret>(&'a mut self, exec: F) -> Ret
    where
        F: FnOnce(&mut Self::ReadStorage<'a>) -> Ret,
    {
        let mut conn = self.connection();
        exec(&mut conn)
    }

    fn with_read_ctx<'a, F, Ret>(&'a mut self, exec: F) -> Ret
    where
        F: for<'ctx> FnOnce(
            &mut MarfReadCtx<'ctx, T, Self::ReadState, Self::ReadStorage<'a>>,
        ) -> Ret,
    {
        let mut conn = self.connection();
        let mut read_cursor = None;
        let mut nested_read_cursor = None;
        let mut read_state = MarfReadState::new();
        with_read_storage_read_ctx(
            &mut conn,
            &mut read_cursor,
            &mut nested_read_cursor,
            &mut read_state,
            exec,
        )
    }
}

#[derive(Clone)]
struct WriteChainTip<T> {
    block_hash: T,
    height: u32,
}

/// Options for opening a MARF
#[derive(Debug, PartialEq, Clone)]
pub struct MARFOpenOpts {
    /// Hash calculation mode for calculating a trie root hash
    pub hash_calculation_mode: TrieHashCalculationMode,
    /// store trie blobs externally from the DB, in a flat file
    pub external_blobs: bool,
    /// unconditionally do a DB migration (used for testing)
    pub force_db_migrate: bool,
    /// compress the MARF
    pub compress: bool,
    /// use memory-mapped I/O for reading trie blobs
    pub mmap: bool,
    /// Cache the decoded roots of the four most recently accessed committed tries.
    pub root_node_cache: bool,
    /// Maximum resolved committed patched nodes to retain; zero disables the cache.
    pub resolved_patch_cache_capacity: usize,
    /// Complete mutable-state lookup results to retain; zero disables admission.
    pub result_cache_capacity: usize,
}

impl MARFOpenOpts {
    pub fn default() -> MARFOpenOpts {
        MARFOpenOpts {
            hash_calculation_mode: TrieHashCalculationMode::Deferred,
            external_blobs: false,
            force_db_migrate: false,
            compress: false,
            mmap: false,
            root_node_cache: true,
            resolved_patch_cache_capacity: 16,
            result_cache_capacity: DEFAULT_RESULT_CACHE_CAPACITY,
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
            compress: false,
            mmap: false,
            root_node_cache: true,
            resolved_patch_cache_capacity: 16,
            result_cache_capacity: DEFAULT_RESULT_CACHE_CAPACITY,
        }
    }

    pub fn with_compression(mut self, compression: bool) -> Self {
        self.compress = compression;
        self
    }

    pub fn with_mmap(mut self, mmap: bool) -> Self {
        self.mmap = mmap;
        self
    }
}

/// This trait defines functions that are defined for both MARF structs and MarfTransactions.
pub trait MarfConnection<T: MarfTrieId>: MarfCore<T> + Sized {
    fn sqlite_conn(&self) -> &Connection;

    /// Resolve the complete leaf while retaining the caller's block context.
    #[stacks_profiler::profile(name = "MARF lookup hash")]
    fn get_leaf(&mut self, block_hash: &T, path: &TrieHash) -> Result<Option<TrieLeaf>, Error> {
        self.with_read_ctx(|ctx| ctx.get_leaf_by_path(block_hash, path))
    }

    /// Resolve a key and retain its physical leaf metadata.
    #[stacks_profiler::profile(name = "MARF lookup key")]
    fn get_leaf_by_key(&mut self, block_hash: &T, key: &str) -> Result<Option<TrieLeaf>, Error> {
        self.with_read_ctx(|ctx| ctx.get_leaf_by_path(block_hash, &TrieHash::from_key(key)))
    }

    /// Resolve a key from the MARF to a MARFValue with respect to the given block height.
    #[stacks_profiler::profile(name = "MARF lookup key")]
    fn get(&mut self, block_hash: &T, key: &str) -> Result<Option<MARFValue>, Error> {
        self.with_read_ctx(|ctx| {
            let leaf = ctx.get_by_key(block_hash, key);

            #[cfg(test)]
            {
                let trie_hash = TrieHash::from_key(key);
                let leaf_with_hash = ctx.get_by_path(block_hash, &trie_hash);

                match (&leaf, &leaf_with_hash) {
                    (Ok(Some(x)), Ok(Some(y))) => assert_eq!(x, y),
                    (Ok(None), Ok(None)) => {}
                    (Err(_), Err(_)) => {}
                    _ => panic!("Inconsistency: {leaf:?} != {leaf_with_hash:?}"),
                }
            }

            leaf
        })
    }

    /// Resolve a TrieHash from the MARF to a MARFValue with respect to the given block height.
    #[stacks_profiler::profile(name = "MARF lookup hash")]
    fn get_from_hash(&mut self, block_hash: &T, th: &TrieHash) -> Result<Option<MARFValue>, Error> {
        self.with_read_ctx(|ctx| ctx.get_by_path(block_hash, th))
    }

    /// Resolve a key from the MARF to a MARFValue with respect to the given block height, and return a proof.
    fn get_with_proof(
        &mut self,
        block_hash: &T,
        key: &str,
    ) -> Result<Option<(MARFValue, TrieMerkleProof<T>)>, Error> {
        self.with_read_ctx(|ctx| ctx.get_with_proof(block_hash, key))
    }

    /// Resolve a TrieHash from the MARF to a MARFValue with respect to the given block height, and return a proof.
    fn get_with_proof_from_hash(
        &mut self,
        block_hash: &T,
        hash: &TrieHash,
    ) -> Result<Option<(MARFValue, TrieMerkleProof<T>)>, Error> {
        self.with_read_ctx(|ctx| ctx.get_with_proof_from_hash(block_hash, hash))
    }

    /// Get the root trie hash at a particular block
    fn get_root_hash_at(&mut self, block_hash: &T) -> Result<TrieHash, Error> {
        self.with_read_ctx(|ctx| {
            if let Some(block_height) = ctx.storage().squashed_block_height(block_hash)? {
                if let Some(root_hash) = ctx
                    .storage()
                    .squashed_block_root_hash_by_height(block_height)?
                {
                    return Ok(root_hash);
                }
            }

            let (cur_block_hash, cur_block_id) = ctx.storage().get_cur_block_and_id();
            ctx.open_block(block_hash, None)?;
            let root_ptr = ctx.storage().root_trieptr();
            let root_hash = ctx.storage().read_node_hash(&root_ptr);
            ctx.storage()
                .open_block_maybe_id(&cur_block_hash, cur_block_id)?;
            root_hash
        })
    }

    /// Check if a block can open successfully, i.e.,
    ///   it's a known block, the storage system isn't issueing IOErrors, _and_ it's in the same fork
    ///   as the current block
    /// The MARF _must_ be open to a valid block for this check to be evaluated.
    fn check_ancestor_block_hash(&mut self, bhh: &T) -> Result<(), Error> {
        self.with_read_ctx(|ctx| {
            let cur_block_hash = ctx.storage().get_cur_block();
            if cur_block_hash == *bhh {
                // a block is in its own fork
                return Ok(());
            }

            let bhh_height = ctx
                .get_block_height(bhh, &cur_block_hash)?
                .ok_or_else(|| {
                    Error::NonMatchingForks(bhh.clone().to_bytes(), cur_block_hash.clone().to_bytes())
                })?;

            let actual_block_at_height = ctx
                .get_block_at_height(bhh_height, &cur_block_hash)?
                .ok_or_else(|| Error::CorruptionError(format!(
                    "ERROR: Could not find block for height {}, but it was returned by MARF::get_block_height()", bhh_height)))?;

            if bhh != &actual_block_at_height {
                test_debug!("non-matching forks: {} != {}", bhh, &actual_block_at_height);
                return Err(Error::NonMatchingForks(
                    bhh.clone().to_bytes(),
                    cur_block_hash.to_bytes(),
                ));
            }

            // test open
            let result = ctx.storage().open_block(bhh);

            // restore
            ctx
                .storage()
                .open_block(&cur_block_hash)
                .map_err(|e| Error::RestoreMarfBlockError(Box::new(e)))?;

            result
        })
    }
}

impl<T: MarfTrieId> MarfConnection<T> for MarfTransaction<'_, T> {
    fn sqlite_conn(&self) -> &Connection {
        self.storage.sqlite_tx()
    }
}

impl<T: MarfTrieId> MarfConnection<T> for ReopenedTrieStorageConnection<'_, T> {
    fn sqlite_conn(&self) -> &Connection {
        self.db_conn()
    }
}

impl<T: MarfTrieId> MarfConnection<T> for MARF<T> {
    fn sqlite_conn(&self) -> &Connection {
        self.storage.sqlite_conn()
    }
}

impl<'a, T: MarfTrieId, S: NodePatching, R: TrieReadStorage<T> + ?Sized> MarfReadCtx<'a, T, S, R> {
    pub fn new(
        storage: &'a mut R,
        cursor: &'a mut Option<TrieCursor<T>>,
        nested_cursor: &'a mut Option<TrieCursor<T>>,
        scratch: &'a mut S,
    ) -> Self {
        Self {
            storage,
            read_cursor: cursor,
            nested_read_cursor: nested_cursor,
            read_state: scratch,
        }
    }

    pub fn storage(&mut self) -> &mut R {
        self.storage
    }

    pub fn read_node(&mut self, ptr: &TriePtr) -> Result<ReadTrieNode<'_>, Error> {
        self.storage.read_node_with_state(ptr, self.read_state)
    }

    pub fn with_read_state<F, Ret>(&mut self, exec: F) -> Ret
    where
        F: FnOnce(&mut R, &mut Option<TrieCursor<T>>, &mut S) -> Ret,
    {
        exec(self.storage, self.read_cursor, self.read_state)
    }

    fn with_preserved_read_cursor<F, Ret>(&mut self, exec: F) -> Ret
    where
        F: FnOnce(&mut Self) -> Ret,
    {
        let saved_cursor = self.read_cursor.take();
        let reusable_cursor = self.nested_read_cursor.take();
        *self.read_cursor = reusable_cursor;

        let result = exec(self);

        *self.nested_read_cursor = self.read_cursor.take();
        *self.read_cursor = saved_cursor;
        result
    }

    pub fn with_restored_block_context<F, Ret>(&mut self, exec: F) -> Result<Ret, Error>
    where
        F: FnOnce(&mut Self) -> Result<Ret, Error>,
    {
        let block_ctx = self.storage.get_cur_block_and_id();
        let result = exec(self);

        self.storage
            .open_block_maybe_id(&block_ctx.0, block_ctx.1)
            .inspect_err(|e| {
                warn!(
                    "Failed to restore original block context {} {:?}: {e:?}",
                    block_ctx.0, block_ctx.1
                );
            })?;

        result
    }

    pub fn open_block(&mut self, bhh: &T, block_id: Option<u32>) -> Result<(), Error> {
        self.storage.open_block_maybe_id(bhh, block_id)
    }

    pub fn get_path(&mut self, block_hash: &T, path: &TrieHash) -> Result<Option<TrieLeaf>, Error> {
        self.storage.check_historical_read_allowed(block_hash)?;
        MARF::walk(self, block_hash, path)
            .inspect_err(|_e| {
                trace!("Failed to look up key {block_hash:?} {path:?}: {_e:?}");
            })
            .map(Some)
    }

    /// Resolve a logical value through the locator-preserving result cache.
    pub fn get_by_path(
        &mut self,
        block_hash: &T,
        path: &TrieHash,
    ) -> Result<Option<MARFValue>, Error> {
        let Some(mut leaf) = self.get_leaf_by_path(block_hash, path)? else {
            return Ok(None);
        };
        self.storage.resolve_leaf_value(&mut leaf)?;
        Ok(Some(leaf.value()?.clone()))
    }

    /// Resolve a complete leaf through the active-trie result cache.
    pub fn get_leaf_by_path(
        &mut self,
        block_hash: &T,
        path: &TrieHash,
    ) -> Result<Option<TrieLeaf>, Error> {
        if let Some(value) = self.storage.cached_result(block_hash, path) {
            self.storage.check_historical_read_allowed(block_hash)?;
            return Ok(value);
        }
        let result = self.with_restored_block_context(|this| {
            this.get_path(block_hash, path).or_else(|e| match e {
                Error::NotFoundError => Ok(None),
                _ => Err(e),
            })
        });
        if let Ok(value) = &result {
            self.storage.cache_result(block_hash, *path, value.clone());
        }
        result
    }

    pub fn get_by_key(&mut self, block_hash: &T, key: &str) -> Result<Option<MARFValue>, Error> {
        self.get_by_path(block_hash, &TrieHash::from_key(key))
    }

    pub fn prove_path(
        &mut self,
        block_hash: &T,
        path: &TrieHash,
        marf_value: &MARFValue,
    ) -> Result<TrieMerkleProof<T>, Error> {
        TrieMerkleProof::from_path(self, path, marf_value, block_hash)
    }

    pub fn prove_raw_entry(
        &mut self,
        block_hash: &T,
        key: &str,
        marf_value: &MARFValue,
    ) -> Result<TrieMerkleProof<T>, Error> {
        let path = TrieHash::from_key(key);
        self.prove_path(block_hash, &path, marf_value)
    }

    pub fn get_with_proof(
        &mut self,
        block_hash: &T,
        key: &str,
    ) -> Result<Option<(MARFValue, TrieMerkleProof<T>)>, Error> {
        if self.storage().is_squashed() {
            return Err(Error::UnsupportedOnSquashedMarf("get_with_proof"));
        }

        let marf_value = match self.get_by_key(block_hash, key)? {
            None => return Ok(None),
            Some(x) => x,
        };
        let proof = self.prove_raw_entry(block_hash, key, &marf_value)?;
        Ok(Some((marf_value, proof)))
    }

    pub fn get_with_proof_from_hash(
        &mut self,
        block_hash: &T,
        hash: &TrieHash,
    ) -> Result<Option<(MARFValue, TrieMerkleProof<T>)>, Error> {
        if self.storage().is_squashed() {
            return Err(Error::UnsupportedOnSquashedMarf("get_with_proof_from_hash"));
        }

        let marf_value = match self.get_by_path(block_hash, hash)? {
            None => return Ok(None),
            Some(x) => x,
        };
        let proof = self.prove_path(block_hash, hash, &marf_value)?;
        Ok(Some((marf_value, proof)))
    }

    pub fn get_block_height_miner_tip(
        &mut self,
        block_hash: &T,
        current_block_hash: &T,
    ) -> Result<Option<u32>, Error> {
        self.with_preserved_read_cursor(|this| {
            #[cfg(test)]
            {
                let test_genesis_block = this.storage().test_genesis_block();
                if test_genesis_block.as_ref() == Some(current_block_hash) {
                    return Ok(Some(0));
                }
            }

            if block_hash == current_block_hash {
                return match this.get_by_key(current_block_hash, OWN_BLOCK_HEIGHT_KEY) {
                    Err(Error::HistoricalReadInSquashedRange { block_height, .. }) => {
                        Ok(Some(block_height))
                    }
                    Err(e) => Err(e),
                    Ok(value) => Ok(value.map(u32::from)),
                };
            }

            let hash_key = format!("{BLOCK_HASH_TO_HEIGHT_MAPPING_KEY}::{block_hash}");
            match this.get_by_key(current_block_hash, &hash_key) {
                Ok(value) => Ok(value.map(u32::from)),
                Err(Error::HistoricalReadInSquashedRange { .. }) => {
                    this.storage().squashed_block_height(block_hash)
                }
                Err(e) => Err(e),
            }
        })
    }

    pub fn get_block_height(
        &mut self,
        block_hash: &T,
        current_block_hash: &T,
    ) -> Result<Option<u32>, Error> {
        self.get_block_height_miner_tip(block_hash, current_block_hash)
    }

    pub fn get_block_at_height(
        &mut self,
        height: u32,
        current_block_hash: &T,
    ) -> Result<Option<T>, Error> {
        self.resolve_block_at_height(height, current_block_hash, None)
    }

    /// Resolve an ancestor using the current height already read from the same trie view.
    pub fn get_block_at_height_with_current_height(
        &mut self,
        height: u32,
        current_block_hash: &T,
        current_block_height: u32,
    ) -> Result<Option<T>, Error> {
        self.resolve_block_at_height(height, current_block_hash, Some(current_block_height))
    }

    /// Resolve a height while preserving the cursor, optionally reusing the current height.
    fn resolve_block_at_height(
        &mut self,
        height: u32,
        current_block_hash: &T,
        known_current_height: Option<u32>,
    ) -> Result<Option<T>, Error> {
        self.with_preserved_read_cursor(|this| {
            #[cfg(test)]
            if height == 0 {
                let test_genesis_block = this.storage().test_genesis_block();
                if let Some(s) = test_genesis_block {
                    return Ok(Some(s));
                }
            }

            let current_block_height = match known_current_height {
                Some(height) => Some(height),
                None => this.get_block_height(current_block_hash, current_block_hash)?,
            };
            let current_block_height = match current_block_height {
                Some(x) => x,
                None => {
                    error!(
                            "Could not fetch block height for {current_block_hash}, likely not a known block",
                        );
                    return Ok(None);
                }
            };

            if height == current_block_height {
                return Ok(Some(current_block_hash.clone()));
            }

            if this
                .storage()
                .squash_height()
                .is_some_and(|h| current_block_height <= h)
            {
                if height > current_block_height {
                    return Ok(None);
                }
                return this.storage().squashed_block_hash_by_height(height);
            }

            if let Some(ancestor) = this.storage().indexed_ancestor(current_block_hash, current_block_height, height)? {
                return Ok(Some(ancestor));
            }

            let height_key = format!("{BLOCK_HEIGHT_TO_HASH_MAPPING_KEY}::{height}");

            this.get_by_key(current_block_hash, &height_key)
                .map(|option_result| option_result.map(T::from))
        })
    }

    pub fn get_trie_ancestor_hashes_bytes(&mut self) -> Result<Vec<TrieHash>, Error> {
        self.with_read_state(|storage, cursor_opt, decode_scratch| {
            Trie::get_trie_ancestor_hashes_bytes(storage, cursor_opt, decode_scratch)
                .map(|hashes| hashes.to_vec())
        })
    }
}

#[cfg(test)]
impl<'a, 'conn, T: MarfTrieId, Db: Deref<Target = Connection>>
    MarfReadCtx<'a, T, MarfReadState, TrieStorageConnection<'conn, T, Db>>
{
    pub fn with_ephemeral<F, R>(storage: &'a mut TrieStorageConnection<'conn, T, Db>, exec: F) -> R
    where
        F: for<'ephemeral> FnOnce(
            &mut MarfReadCtx<'ephemeral, T, MarfReadState, TrieStorageConnection<'conn, T, Db>>,
        ) -> R,
    {
        let mut cursor = None;
        let mut nested_cursor = None;
        let mut scratch = MarfReadState::new();
        let mut ephemeral_ctx =
            MarfReadCtx::new(storage, &mut cursor, &mut nested_cursor, &mut scratch);
        exec(&mut ephemeral_ctx)
    }
}

///
/// MarfTransaction represents a connection to a MARF index,
///   with an open storage transaction. If this struct is
///   dropped without calling commit(), the storage transaction is
///   aborted
///
impl<'a, T: MarfTrieId> MarfTransaction<'a, T> {
    pub fn commit(mut self) -> Result<(), Error> {
        if self.storage.readonly() {
            return Err(Error::ReadOnlyError);
        }
        if let Some(_tip) = self.open_chain_tip.take() {
            self.storage.flush()?;
        }
        self.storage.commit_tx();
        Ok(())
    }

    /// Finish writing the next trie in the MARF, but change the hash of the current Trie's
    /// block hash to something other than what we opened it as.  This persists all changes.
    pub fn commit_to(mut self, real_bhh: &T) -> Result<(), Error> {
        if self.storage.readonly() {
            return Err(Error::ReadOnlyError);
        }
        if self.storage.unconfirmed() {
            return Err(Error::UnconfirmedError);
        }
        if let Some(_tip) = self.open_chain_tip.take() {
            self.storage.flush_to(real_bhh)?;
            self.storage.commit_tx();
        }
        Ok(())
    }

    /// Finish writing the next trie in the MARF -- this is used by miners
    ///   to commit the mined block, but write it to the mined_block table,
    ///   rather than out to the marf_data table (this prevents the
    ///   miner's block from getting stepped on after the sortition).
    pub fn commit_mined(mut self, bhh: &T) -> Result<(), Error> {
        if self.storage.readonly() {
            return Err(Error::ReadOnlyError);
        }
        if self.storage.unconfirmed() {
            return Err(Error::UnconfirmedError);
        }
        if let Some(_tip) = self.open_chain_tip.take() {
            self.storage.flush_mined(bhh)?;
            self.storage.commit_tx();
        }
        Ok(())
    }

    pub fn get_open_chain_tip(&self) -> Option<&T> {
        self.open_chain_tip.as_ref().map(|tip| &tip.block_hash)
    }

    pub fn get_open_chain_tip_height(&self) -> Option<u32> {
        self.open_chain_tip.as_ref().map(|tip| tip.height)
    }

    pub fn get_block_height_of(
        &mut self,
        bhh: &T,
        current_block_hash: &T,
    ) -> Result<Option<u32>, Error> {
        if Some(bhh) == self.get_open_chain_tip() {
            return Ok(self.get_open_chain_tip_height());
        } else {
            Self::get_block_height_miner_tip(self, bhh, current_block_hash)
        }
    }

    #[cfg(test)]
    fn commit_tx(self) {
        self.storage.commit_tx()
    }

    /// Physical layout used by this transaction's trie records.
    pub fn record_format(&self) -> NodeRecordFormat {
        self.storage.record_format()
    }

    pub fn sqlite_tx(&self) -> &Transaction<'a> {
        self.storage.sqlite_tx()
    }

    pub fn sqlite_tx_mut(&mut self) -> &mut Transaction<'a> {
        self.storage.sqlite_tx_mut()
    }

    /// Commit the SQL transaction without flushing TrieRAM to disk.
    ///
    /// Used by `squash_to_path` which writes the blob directly, bypassing
    /// the normal TrieRAM flush path.
    pub fn commit_squash(mut self) -> Result<(), Error> {
        if self.storage.readonly() {
            return Err(Error::ReadOnlyError);
        }
        self.open_chain_tip.take();
        self.storage.drop_extending_trie();
        self.storage.commit_tx();
        Ok(())
    }

    /// Set squash metadata on the underlying storage connection.
    ///
    /// Called during `squash_to_path` so the ancestor-hash computation can
    /// use stored root hashes instead of opening pruned historical blocks.
    pub fn set_squash_info(&mut self, info: Option<SquashInfo>) {
        self.storage.set_squash_info(info);
    }

    /// Reopen this MARF transaction with readonly storage.
    ///   NOTE: any pending operations in the SQLite transaction _will not_
    ///         have materialized in the reopened view.
    pub fn reopen_readonly(&self) -> Result<MARF<T>, Error> {
        if self.open_chain_tip.is_some() {
            error!(
                "MARF at {} is already in the process of writing",
                &self.storage.db_path
            );
            return Err(Error::InProgressError);
        }

        let ro_storage = self.storage.reopen_readonly()?;
        Ok(MARF {
            storage: ro_storage,
            open_chain_tip: None,
            read_cursor: None,
            nested_read_cursor: None,
            read_state: MarfReadState::new(),
        })
    }

    /// Begin writing the next trie in the MARF, given the block header hash that will contain the
    /// associated block's new state.  Call commit() or commit_to() to persist the changes.
    /// Fails if the block already exists.
    /// Storage will point to new chain tip on success.
    pub fn begin(&mut self, chain_tip: &T, next_chain_tip: &T) -> Result<(), Error> {
        if self.storage.readonly() {
            return Err(Error::ReadOnlyError);
        }
        if self.open_chain_tip.is_some() {
            error!(
                "MARF at {} is already in the process of writing",
                &self.storage.db_path
            );
            return Err(Error::InProgressError);
        }
        if self.storage.has_block(next_chain_tip)? {
            error!("Block data already exists: {}", next_chain_tip);
            return Err(Error::ExistsError);
        }

        let block_height = self.inner_get_extension_height(chain_tip, next_chain_tip)?;
        MARF::extend_trie(&mut self.storage, next_chain_tip, self.read_state)?;
        self.inner_setup_extension(chain_tip, next_chain_tip, block_height, true)
    }

    /// Set up the trie extension we're making.
    /// Sets storage pointer to chain_tip.
    /// Returns the height next_chain_tip would be at.
    fn inner_get_extension_height(
        &mut self,
        chain_tip: &T,
        next_chain_tip: &T,
    ) -> Result<u32, Error> {
        // current chain tip must exist if it's not the "sentinel"
        let is_parent_sentinel = chain_tip == &T::sentinel();
        if !is_parent_sentinel {
            debug!("Extending off of existing node {}", chain_tip);
        } else {
            debug!("First-ever block {}", next_chain_tip; "block" => %next_chain_tip);
        }

        self.storage.open_block(chain_tip)?;

        let block_height = if !is_parent_sentinel {
            let height = Self::get_block_height_miner_tip(self, chain_tip, chain_tip)?.ok_or(
                Error::CorruptionError(format!(
                    "Failed to find block height for `{:?}`",
                    chain_tip
                )),
            )?;
            height
                .checked_add(1)
                .expect("FATAL: block height overflow!")
        } else {
            0
        };

        Ok(block_height)
    }

    /// Set up a new extension.
    /// Opens storage to chain_tip/
    fn inner_setup_extension(
        &mut self,
        chain_tip: &T,
        next_chain_tip: &T,
        block_height: u32,
        new_extension: bool,
    ) -> Result<(), Error> {
        self.storage.open_block(next_chain_tip)?;
        self.open_chain_tip.replace(WriteChainTip {
            block_hash: next_chain_tip.clone(),
            height: block_height,
        });

        if new_extension {
            self.set_block_heights(chain_tip, next_chain_tip, block_height)
                .inspect_err(|_e| {
                    self.open_chain_tip.take();
                })?;
        }

        debug!("Opened {chain_tip} to {next_chain_tip}");
        Ok(())
    }

    pub fn set_block_heights(
        &mut self,
        block_hash: &T,
        next_block_hash: &T,
        height: u32,
    ) -> Result<(), Error> {
        if self.storage.readonly() {
            return Err(Error::ReadOnlyError);
        }
        let mut keys = vec![];
        let mut values = vec![];

        let height_key = format!("{}::{}", BLOCK_HEIGHT_TO_HASH_MAPPING_KEY, height);
        let hash_key = format!("{}::{}", BLOCK_HASH_TO_HEIGHT_MAPPING_KEY, next_block_hash);

        debug!(
            "Set {}::{} = {}",
            BLOCK_HEIGHT_TO_HASH_MAPPING_KEY, height, next_block_hash
        );
        debug!(
            "Set {}::{} = {}",
            BLOCK_HASH_TO_HEIGHT_MAPPING_KEY, next_block_hash, height
        );
        debug!("Set {} = {}", OWN_BLOCK_HEIGHT_KEY, height);

        keys.push(OWN_BLOCK_HEIGHT_KEY.to_string());
        values.push(MARFValue::from(height));

        keys.push(height_key);
        values.push(MARFValue::from(next_block_hash.clone()));

        keys.push(hash_key);
        values.push(MARFValue::from(height));

        if height > 0 {
            let prev_height_key = format!("{}::{}", BLOCK_HEIGHT_TO_HASH_MAPPING_KEY, height - 1);
            let prev_hash_key = format!("{}::{}", BLOCK_HASH_TO_HEIGHT_MAPPING_KEY, block_hash);

            debug!(
                "Set {}::{} = {}",
                BLOCK_HEIGHT_TO_HASH_MAPPING_KEY,
                height - 1,
                block_hash
            );
            debug!(
                "Set {}::{} = {}",
                BLOCK_HASH_TO_HEIGHT_MAPPING_KEY,
                block_hash,
                height - 1
            );

            keys.push(prev_height_key);
            values.push(MARFValue::from(block_hash.clone()));

            keys.push(prev_hash_key);
            values.push(MARFValue::from(height - 1));
        }

        let tip = self
            .open_chain_tip
            .as_ref()
            .ok_or(Error::WriteNotBegunError)?;
        let canonical = tip.height == height
            && self
                .storage
                .canonical_ancestry_update(block_hash, next_block_hash);
        let block = tip.block_hash.clone();
        let leaves: Vec<_> = values
            .iter()
            .map(|v| TrieLeaf::from_value(&[], v.clone()))
            .collect();
        MARF::inner_insert_leaf_batch(
            &mut self.storage,
            &block,
            &keys,
            &leaves,
            self.read_state,
            canonical,
        )?;
        Ok(())
    }

    /// Insert a batch of key/value pairs.  More efficient than inserting them individually, since
    /// the trie root hash will only be calculated once (which is an O(log B) operation).
    pub fn insert_batch(
        &mut self,
        keys: impl AsRef<[String]>,
        values: impl AsRef<[MARFValue]>,
    ) -> Result<(), Error> {
        let keys = keys.as_ref();
        let values = values.as_ref();

        if self.storage.readonly() {
            return Err(Error::ReadOnlyError);
        }
        assert_eq!(keys.len(), values.len());

        let block_hash = match self.open_chain_tip {
            None => Err(Error::WriteNotBegunError),
            Some(WriteChainTip { ref block_hash, .. }) => Ok(block_hash.clone()),
        }?;

        if keys.is_empty() {
            return Ok(());
        }

        MARF::inner_insert_batch(
            &mut self.storage,
            &block_hash,
            keys,
            values,
            self.read_state,
        )?;
        Ok(())
    }

    /// Insert physical value locators with the same batched root and cache invalidation rules.
    pub fn insert_leaf_batch(
        &mut self,
        keys: &[String],
        leaves: Vec<TrieLeaf>,
    ) -> Result<(), Error> {
        if self.storage.readonly() {
            return Err(Error::ReadOnlyError);
        }
        let block_hash = self
            .open_chain_tip
            .as_ref()
            .ok_or(Error::WriteNotBegunError)?
            .block_hash
            .clone();
        MARF::inner_insert_leaf_batch(
            &mut self.storage,
            &block_hash,
            keys,
            &leaves,
            self.read_state,
            false,
        )
    }

    /// Begin extending the MARF to an unconfirmed trie. The resulting trie will have a block hash
    /// equal to `MARF::make_unconfirmed_chain_tip(chain_tip)` to avoid collision and block hash
    /// reuse.
    pub fn begin_unconfirmed(&mut self, chain_tip: &T) -> Result<T, Error> {
        if self.storage.readonly() {
            return Err(Error::ReadOnlyError);
        }
        if self.open_chain_tip.is_some() {
            error!(
                "MARF at {} is already in the process of writing",
                &self.storage.db_path
            );
            return Err(Error::InProgressError);
        }
        if !self.storage.unconfirmed() {
            return Err(Error::UnconfirmedError);
        }

        // chain_tip must exist and must be confirmed
        if !self.storage.has_confirmed_block(chain_tip)? {
            error!("No such confirmed block {}", chain_tip);
            return Err(Error::NotFoundError);
        }

        let unconfirmed_tip = MARF::make_unconfirmed_chain_tip(chain_tip);

        let block_height = self.inner_get_extension_height(chain_tip, &unconfirmed_tip)?;

        let created = self.storage.extend_to_unconfirmed_block(&unconfirmed_tip)?;
        if created {
            MARF::root_copy(&mut self.storage, chain_tip, self.read_state)?;
        }

        self.inner_setup_extension(chain_tip, &unconfirmed_tip, block_height, created)?;
        Ok(unconfirmed_tip)
    }

    /// Drop the current trie from the MARF. This rolls back all
    ///   changes in the block, and closes the current chain tip.
    pub fn drop_current(mut self) {
        if !self.storage.readonly() {
            self.storage.drop_extending_trie();
            self.open_chain_tip.take();
            self.storage
                .open_block(&T::sentinel())
                .expect("BUG: should never fail to open the block sentinel");
            self.storage.rollback()
        }
    }

    /// Drop the current trie from the MARF, and roll back all unconfirmed state
    pub fn drop_unconfirmed(mut self) {
        if !self.storage.readonly() && self.storage.unconfirmed() {
            if let Some(tip) = self.open_chain_tip.take() {
                trace!("Dropping unconfirmed trie {}", &tip.block_hash);
                self.storage.drop_unconfirmed_trie(&tip.block_hash);
                self.storage
                    .open_block(&T::sentinel())
                    .expect("BUG: should never fail to open the block sentinel");
                // Dropping unconfirmed state cannot be done with a tx rollback,
                //   because the unconfirmed state may already have been written
                //   to the sqlite table before this transaction began
                self.storage.commit_tx()
            } else {
                trace!("drop_unconfirmed() noop");
            }
        }
    }

    /// Seal the in-RAM MARF state so that no subsequent writes will be permitted.
    /// Returns the new root hash of the MARF.
    /// Runtime-panics if the MARF was already sealed.
    pub fn seal(&mut self) -> Result<TrieHash, Error> {
        if self.storage.readonly() {
            return Err(Error::ReadOnlyError);
        }
        let root_hash = self.storage.seal()?;
        Ok(root_hash)
    }
}

// static methods
impl<T: MarfTrieId> MARF<T> {
    #[cfg(test)]
    pub fn from_storage_opened(storage: TrieFileStorage<T>, opened_to: &T) -> MARF<T> {
        MARF {
            storage,
            open_chain_tip: Some(WriteChainTip {
                block_hash: opened_to.clone(),
                height: 0,
            }),
            read_cursor: None,
            nested_read_cursor: None,
            read_state: MarfReadState::new(),
        }
    }

    #[cfg(test)]
    pub fn begin(&mut self, chain_tip: &T, next_chain_tip: &T) -> Result<(), Error> {
        let mut tx = self.begin_tx()?;
        tx.begin(chain_tip, next_chain_tip)?;
        tx.commit_tx();
        Ok(())
    }

    #[cfg(test)]
    pub fn begin_unconfirmed(&mut self, chain_tip: &T) -> Result<T, Error> {
        let mut tx = self.begin_tx()?;
        let result = tx.begin_unconfirmed(chain_tip)?;
        tx.commit_tx();
        Ok(result)
    }

    #[cfg(test)]
    pub fn seal(&mut self) -> Result<TrieHash, Error> {
        let mut tx = self.begin_tx()?;
        let h = tx.seal()?;
        Ok(h)
    }

    // helper method for resolving a backptr child while walking a MARF
    fn walk_backptr<'a, S: NodePatching, Db: Deref<Target = Connection>>(
        storage: &'a mut TrieStorageConnection<T, Db>,
        child_backptr: TriePtr,
        cursor: &mut TrieCursor<T>,
        decode_scratch: &'a mut S,
    ) -> Result<ResolvedBackptrNode<'a>, Error> {
        trace!("Walk backptrs for {:?} to {:?}", cursor, &child_backptr);

        let (read, node_ptr) = Trie::walk_backptr(storage, &child_backptr, cursor, decode_scratch)?;
        Ok(ResolvedBackptrNode {
            node: read,
            ptr: node_ptr,
            block_id: child_backptr.back_block,
        })
    }

    /// Copy a node forward from an ancestor trie by converting its inline children into
    /// back-pointers. Returns the node hash (leaf hash for leaves, empty hash for internal
    /// nodes whose hash will be computed at commit time).
    fn node_copy_update(node: &mut TrieNodeType, child_block_id: u32) -> TrieHash {
        match node {
            TrieNodeType::Leaf(leaf) => bits::get_leaf_hash(leaf),
            _ => {
                node_copy_update_ptrs(node.ptrs_mut(), child_block_id);
                TrieHash::EMPTY
            }
        }
    }

    /// Given a node, and the chr of one of its children, go find the last instance of that child in
    /// the MARF and copy it forward.  Update its ptrs to point to its descendents.
    /// s must point to the block hash in which this node lives, to which the child will be copied.
    fn node_child_copy<S: NodePatching, Db: Deref<Target = Connection>>(
        storage: &mut TrieStorageConnection<T, Db>,
        child_backptr: TriePtr,
        cursor: &mut TrieCursor<T>,
        decode_scratch: &mut S,
    ) -> Result<CopiedChild<T>, Error> {
        trace!(
            "Copy to {:?} child {:?}",
            storage.get_cur_block(),
            &child_backptr,
        );

        let (cur_block_hash, cur_block_id) = storage.get_cur_block_and_id();
        let chr = child_backptr.chr();

        let (mut child_node, child_ptr_id, child_block_identifier) = {
            let child_backptr_node =
                MARF::walk_backptr(storage, child_backptr, cursor, decode_scratch)?;
            let child_ptr_id = child_backptr_node.ptr.id();
            let child_block_identifier = child_backptr_node.block_id;
            let (child_node, _) = child_backptr_node.node.into_owned_node()?;
            (child_node, child_ptr_id, child_block_identifier)
        };

        let child_block_hash = storage.get_cur_block();

        child_node.set_cow_ptr(TrieCowPtr::new(child_block_hash.clone(), child_backptr));

        // update child_node with new ptrs and hashes
        storage.open_block_maybe_id(&cur_block_hash, cur_block_id)?;
        if let TrieNodeType::Leaf(ref mut leaf) = child_node {
            storage.resolve_leaf_value(leaf)?;
        }
        let child_hash = MARF::<T>::node_copy_update(&mut child_node, child_block_identifier);

        // store it in this trie
        storage.open_block_maybe_id(&cur_block_hash, cur_block_id)?;
        let child_array_ptr = storage.last_ptr()?;
        let child_ptr = TriePtr::new(child_ptr_id, chr, child_array_ptr.into());
        storage.write_nodetype(child_array_ptr, &child_node, child_hash)?;

        trace!(
            "Copied child 0x{:02x} to {:?}: ptr={:?} child={:?}",
            chr,
            &cur_block_hash,
            &child_ptr,
            &child_node
        );
        Ok(CopiedChild {
            node: child_node,
            ptr: child_ptr,
            source_block_hash: child_block_hash,
        })
    }

    /// Copy the root node from the previous Trie to this Trie, updating its ptrs.
    /// s must point to the target Trie
    fn root_copy<S: NodePatching, Db: Deref<Target = Connection>>(
        storage: &mut TrieStorageConnection<T, Db>,
        prev_block_hash: &T,
        decode_scratch: &mut S,
    ) -> Result<(), Error> {
        let (cur_block_hash, cur_block_id) = storage.get_cur_block_and_id();
        storage.open_block(prev_block_hash)?;
        let prev_block_identifier = storage.get_cur_block_identifier().unwrap_or_else(|_| {
            panic!(
                "called open_block on {}, but found no identifier",
                prev_block_hash
            )
        });

        let mut prev_root = {
            let root_read = Trie::read_root(storage, decode_scratch)?;
            root_read.into_owned_node()?.0
        };
        if prev_block_hash != &T::sentinel() {
            let mut prev_root_backptr = TriePtr::new(
                set_backptr(TrieNodeID::Node256 as u8),
                0,
                storage.root_ptr(),
            );
            prev_root_backptr.back_block = prev_block_identifier;
            prev_root.set_cow_ptr(TrieCowPtr::new(prev_block_hash.clone(), prev_root_backptr));
        }
        let new_root_hash = Self::node_copy_update(&mut prev_root, prev_block_identifier);

        storage.open_block_maybe_id(&cur_block_hash, cur_block_id)?;

        let root_ptr = u32::try_from(storage.root_ptr()).map_err(|_| Error::OverflowError)?;
        storage.write_nodetype(root_ptr, &prev_root, new_root_hash)?;
        Ok(())
    }

    /// create or open a particular Trie.
    /// If the trie doesn't exist, then extend it from the current Trie and create a root node that
    /// has back pointers to its immediate children in the current trie.
    /// On Ok, s will point to new_bhh and will be open for reading.
    /// Returns true/false, based on whether or not the trie will be created (this can return false
    /// if we're resuming work on an unconfirmed trie)
    pub fn extend_trie<S: NodePatching>(
        storage: &mut TrieStorageTransaction<T>,
        new_bhh: &T,
        decode_scratch: &mut S,
    ) -> Result<(), Error> {
        if storage.readonly() {
            unreachable!("CORRUPTION: constructed read-only TrieStorageTransaction instance");
        }

        let (cur_bhh, cur_block_id) = storage.get_cur_block_and_id();
        if storage.num_blocks() == 0 || cur_bhh == T::sentinel() {
            // brand new storage
            trace!("Brand new storage -- start with {:?}", new_bhh);
            storage.extend_to_block(new_bhh)?;
            let node = TrieNode256::new(&[]);
            let hash = bits::get_node_hash(&node, &[], storage);
            let root_ptr = u32::try_from(storage.root_ptr()).map_err(|_| Error::OverflowError)?;
            storage.write_nodetype(root_ptr, &TrieNodeType::Node256(Box::new(node)), hash)?;
            Ok(())
        } else {
            // existing storage
            match storage.open_block(new_bhh) {
                Ok(_) => {
                    trace!("Switch to Trie {:?}", new_bhh);
                    Ok(())
                }
                Err(e) => {
                    match e {
                        Error::NotFoundError => {
                            // bring root forward
                            debug!("Extend {:?} to {:?}", &cur_bhh, new_bhh);
                            storage.open_block_maybe_id(&cur_bhh, cur_block_id)?;
                            storage.extend_to_block(new_bhh)?;
                            MARF::root_copy(storage, &cur_bhh, decode_scratch)?;
                            storage.open_block(new_bhh)?;
                            Ok(())
                        }
                        _ => Err(e),
                    }
                }
            }
        }
    }

    /// Walk down this MARF at the given block hash, doing a copy-on-write for intermediate nodes in
    /// this block's Trie from any prior Tries.
    ///
    /// `storage` must point to the last filled-in Trie -- i.e. block_hash points to the _new_ Trie
    /// that is being filled in.
    fn walk_cow<S: NodePatching + NodeParking>(
        storage: &mut TrieStorageTransaction<T>,
        block_hash: &T,
        path: &TrieHash,
        decode_scratch: &mut S,
    ) -> Result<TrieCursor<T>, Error> {
        let block_id = storage.get_block_identifier(block_hash);
        MARF::extend_trie(storage, block_hash, decode_scratch)?;
        decode_scratch.clear_parked_nodes();

        let mut cursor = TrieCursor::new(path, storage.root_trieptr());

        let mut node_ptr = storage.root_trieptr();
        let mut owned_node: Option<TrieNodeType> = None;

        for _ in 0..(cursor.path.len() + 1) {
            let cur_block = storage.get_cur_block();
            let action = if let Some(node) = owned_node.as_ref() {
                cursor.walk_step(node, &cur_block)
            } else {
                decode_scratch.clear_current_node();
                let read = storage.read_node_with_state(&node_ptr, decode_scratch)?;
                match read.backing {
                    ReadNodeBacking::VolatileDecoded(node) => {
                        let action = cursor.walk_ref_step(&node, &cur_block);
                        let parked_handle = decode_scratch.park_current_node()?;
                        cursor.promote_last_node_to_parked(parked_handle);
                        action
                    }
                    ReadNodeBacking::PersistedDecoded(node) => {
                        cursor.walk_ref_step(&node, &cur_block)
                    }
                    ReadNodeBacking::PersistedBytes(node) => {
                        return Err(Error::CorruptionError(format!(
                            "Stable byte-backed {:?} nodes cannot be walked in COW mode yet",
                            node.node_type()
                        )));
                    }
                    ReadNodeBacking::Owned(node) => {
                        let parked_handle = decode_scratch.park_owned_node(node);
                        let parked_node = decode_scratch.get_parked_ref(parked_handle);
                        cursor.walk_parked_step(&parked_node, parked_handle, &cur_block)
                    }
                }
            };

            match action {
                ReadTrieNodeCursorStep::Next(next_node_ptr) => {
                    owned_node = None;
                    node_ptr = next_node_ptr;
                    continue;
                }
                ReadTrieNodeCursorStep::EndOfPath { is_leaf } => {
                    if !is_leaf || clear_backptr(node_ptr.id()) != TrieNodeID::Leaf as u8 {
                        trace!("Out-of-path but encountered a non-leaf at {:?}", &node_ptr);
                        error!("Out-of-path but encountered a non-leaf");
                        return Err(Error::CorruptionError(
                            "Non-leaf encountered at end of path".to_string(),
                        ));
                    }

                    trace!(
                        "Out of path in {:?} -- we're done. Node at {:?}",
                        storage.get_cur_block(),
                        &node_ptr
                    );
                    storage.open_block_maybe_id(block_hash, block_id)?;
                    return Ok(cursor);
                }
                ReadTrieNodeCursorStep::Diverged => {
                    trace!("Path diverged -- we're done.");
                    storage.open_block_maybe_id(block_hash, block_id)?;
                    return Ok(cursor);
                }
                ReadTrieNodeCursorStep::ChrNotFound => {
                    trace!(
                        "ChrNotFound encountered at {:?} -- we're done (node not found)",
                        storage.get_cur_block()
                    );
                    storage.open_block_maybe_id(block_hash, block_id)?;
                    return Ok(cursor);
                }
                ReadTrieNodeCursorStep::FollowBackptr(ptr) => {
                    storage.open_block_maybe_id(block_hash, block_id)?;
                    let copied_child =
                        MARF::node_child_copy(storage, ptr, &mut cursor, decode_scratch)?;

                    cursor.repair_backptr_finish(&copied_child.ptr, copied_child.source_block_hash);

                    owned_node = Some(copied_child.node);
                    node_ptr = copied_child.ptr;

                    storage.open_block_maybe_id(block_hash, block_id)?;
                }
            }
        }

        trace!("Trie has a cycle");
        return Err(Error::CorruptionError("Trie has a cycle".to_string()));
    }

    /// Walk down this MARF at the given block hash, resolving backptrs to previous tries.
    /// Return the cursor and the last node visited.
    /// s will point to the block in which the leaf was found, or the last block visited.
    fn walk<S: NodePatching, R: TrieReadStorage<T> + ?Sized>(
        ctx: &mut MarfReadCtx<'_, T, S, R>,
        block_hash: &T,
        path: &TrieHash,
    ) -> Result<TrieLeaf, Error> {
        ctx.with_read_state(|storage, cursor_opt, decode_scratch| {
            let cursor =
                cursor_opt.get_or_insert_with(|| TrieCursor::new(path, TriePtr::default()));

            storage.open_block(block_hash)?;
            cursor.reset(path, storage.root_trieptr());

            let mut node_ptr = storage.root_trieptr();

            for _ in 0..(cursor.path.len() + 1) {
                enum WalkAction {
                    Next(TriePtr),
                    FoundLeaf(TrieLeaf),
                    FollowBackptr(TriePtr),
                    NotFound,
                }

                let cur_block = storage.get_cur_block();
                let mut read_session = TrieReadSession::new(storage, decode_scratch);
                let read = read_session.read_node(&node_ptr)?;
                let action = match cursor.walk_read(&read, &cur_block) {
                    Ok(Some(next_ptr)) => WalkAction::Next(next_ptr),
                    Ok(None) => {
                        if clear_backptr(cursor.ptr().id()) != TrieNodeID::Leaf as u8 {
                            return Err(Error::CorruptionError(
                                "Non-leaf encountered at end of path".to_string(),
                            ));
                        }

                        let leaf = read.as_leaf()?.ok_or_else(|| {
                            Error::CorruptionError("Path reached a non-leaf".to_string())
                        })?;
                        WalkAction::FoundLeaf(leaf.to_owned())
                    }
                    Err(Error::CursorError(CursorError::PathDiverged))
                    | Err(Error::CursorError(CursorError::ChrNotFound)) => WalkAction::NotFound,
                    Err(Error::CursorError(CursorError::BackptrEncountered(ptr))) => {
                        WalkAction::FollowBackptr(ptr)
                    }
                    Err(e) => return Err(e),
                };

                match action {
                    WalkAction::Next(next_ptr) => {
                        node_ptr = next_ptr;
                        continue;
                    }
                    WalkAction::FoundLeaf(leaf) => {
                        return Ok(leaf);
                    }
                    WalkAction::NotFound => {
                        trace!("Path diverged or chr not found -- we're done.");
                        return Err(Error::NotFoundError);
                    }
                    WalkAction::FollowBackptr(ptr) => {
                        let next_node_ptr = Trie::resolve_backptr(storage, &ptr)?;

                        cursor.repair_backptr_finish(&next_node_ptr, storage.get_cur_block());
                        node_ptr = next_node_ptr;
                        continue;
                    }
                }
            }

            trace!("Trie has a cycle");
            Err(Error::CorruptionError("Trie has a cycle".to_string()))
        })
    }

    pub fn format(
        storage: &mut TrieStorageTransaction<T>,
        first_block_hash: &T,
    ) -> Result<(), Error> {
        if storage.readonly() {
            unreachable!("CORRUPTION: constructed read-only TrieStorageTransaction instance");
        }

        storage.format()?;
        storage.extend_to_block(first_block_hash)?;
        let node = TrieNode256::new(&[]);
        let hash = bits::get_node_hash(&node, &[], storage);
        let root_ptr = u32::try_from(storage.root_ptr()).map_err(|_| Error::OverflowError)?;
        let node_type = TrieNodeType::Node256(Box::new(node));
        storage.write_nodetype(root_ptr, &node_type, hash)
    }

    fn do_insert_leaf<S: NodePatching + NodeParking>(
        storage: &mut TrieStorageTransaction<T>,
        block_hash: &T,
        path: &TrieHash,
        leaf_value: &TrieLeaf,
        update_skiplist: bool,
        decode_scratch: &mut S,
    ) -> Result<(), Error> {
        let guard = storage.begin_result_cache_write(block_hash, path);
        let mut value = leaf_value.clone();
        let mut cursor = MARF::walk_cow(storage, block_hash, path, decode_scratch)?;

        if cursor.block_hashes.len() + 1 != cursor.node_ptrs.len() {
            trace!("c.block_hashes = {:?}", &cursor.block_hashes);
            trace!("c.node_ptrs = {:?}", cursor.node_ptrs);
            panic!();
        }

        debug!(
            "MARF Insert in {block_hash}: '{path}' = '{:?}' (...{:?})",
            leaf_value.data, &leaf_value.path
        );

        Trie::add_value(storage, &mut cursor, &mut value, decode_scratch)?;

        if update_skiplist {
            Trie::update_root_hash(storage, &cursor, decode_scratch)?;
        } else {
            Trie::update_root_node_hash(storage, &cursor, decode_scratch)?;
        }
        if let Some(guard) = guard {
            guard.succeed();
        }
        Ok(())
    }

    pub fn insert_leaf<S: NodePatching + NodeParking>(
        storage: &mut TrieStorageTransaction<T>,
        block_hash: &T,
        path: &TrieHash,
        value: &TrieLeaf,
        decode_scratch: &mut S,
    ) -> Result<(), Error> {
        if storage.readonly() {
            unreachable!("CORRUPTION: constructed read-only TrieStorageTransaction instance");
        }
        storage.invalidate_ancestry();
        MARF::do_insert_leaf(storage, block_hash, path, value, true, decode_scratch)
    }

    // like insert_leaf, but don't update the merkle skiplist
    pub fn insert_leaf_in_batch<S: NodePatching + NodeParking>(
        storage: &mut TrieStorageTransaction<T>,
        block_hash: &T,
        path: &TrieHash,
        value: &TrieLeaf,
        decode_scratch: &mut S,
    ) -> Result<(), Error> {
        if storage.readonly() {
            unreachable!("CORRUPTION: constructed read-only TrieStorageTransaction instance");
        }

        storage.invalidate_ancestry();
        MARF::do_insert_leaf(storage, block_hash, path, value, false, decode_scratch)
    }

    /// Instantiate the MARF from a TrieFileStorage instance
    pub fn from_storage(storage: TrieFileStorage<T>) -> MARF<T> {
        MARF {
            storage,
            open_chain_tip: None,
            read_cursor: None,
            nested_read_cursor: None,
            read_state: MarfReadState::new(),
        }
    }

    /// Instantiate the MARF using a TrieFileStorage instance, from the given path on disk.
    /// This will have the side-effect of instantiating a new fork table from the tries encoded on
    /// disk. Performant code should call this method sparingly.
    pub fn from_path(path: &str, open_opts: MARFOpenOpts) -> Result<MARF<T>, Error> {
        let file_storage = TrieFileStorage::open(path, open_opts)?;
        Ok(MARF::from_storage(file_storage))
    }

    /// Instantiate an unconfirmed MARF using a TrieFileStorage instance, from the given path on disk.
    /// This will have the side-effect of instantiating a new fork table from the tries encoded on
    /// disk. Performant code should call this method sparingly.
    pub fn from_path_unconfirmed(path: &str, open_opts: MARFOpenOpts) -> Result<MARF<T>, Error> {
        let file_storage = TrieFileStorage::open_unconfirmed(path, open_opts)?;
        Ok(MARF::from_storage(file_storage))
    }

    pub fn get_by_path(
        storage: &mut TrieStorageConnection<T>,
        block_hash: &T,
        path: &TrieHash,
    ) -> Result<Option<MARFValue>, Error> {
        let mut read_cursor = None;
        let mut nested_read_cursor = None;
        let mut read_state = MarfReadState::new();
        let mut read_ctx = MarfReadCtx::new(
            storage,
            &mut read_cursor,
            &mut nested_read_cursor,
            &mut read_state,
        );
        read_ctx.get_by_path(block_hash, path)
    }

    pub fn get_path(
        storage: &mut TrieStorageConnection<T>,
        block_hash: &T,
        path: &TrieHash,
    ) -> Result<Option<TrieLeaf>, Error> {
        trace!("MARF::get_path({block_hash:?}) {path:?}");
        let mut read_cursor = None;
        let mut nested_read_cursor = None;
        let mut read_state = MarfReadState::new();
        let mut read_ctx = MarfReadCtx::new(
            storage,
            &mut read_cursor,
            &mut nested_read_cursor,
            &mut read_state,
        );
        read_ctx.get_path(block_hash, path)
    }

    /// Load up a MARF value by key, given a handle to the storage connection and a tip to work off
    /// of.
    pub fn get_by_key(
        storage: &mut TrieStorageConnection<T>,
        block_hash: &T,
        key: &str,
    ) -> Result<Option<MARFValue>, Error> {
        let mut read_cursor = None;
        let mut nested_read_cursor = None;
        let mut read_state = MarfReadState::new();
        let mut read_ctx = MarfReadCtx::new(
            storage,
            &mut read_cursor,
            &mut nested_read_cursor,
            &mut read_state,
        );
        read_ctx.get_by_key(block_hash, key)
    }

    /// Load up a MARF value by TrieHash, given a handle to the storage connection and a tip to
    /// work off of.
    pub fn get_by_hash(
        storage: &mut TrieStorageConnection<T>,
        block_hash: &T,
        path: &TrieHash,
    ) -> Result<Option<MARFValue>, Error> {
        MARF::get_by_path(storage, block_hash, path)
    }

    pub fn get_block_height_miner_tip(
        storage: &mut TrieStorageConnection<T>,
        block_hash: &T,
        current_block_hash: &T,
    ) -> Result<Option<u32>, Error> {
        let mut read_cursor = None;
        let mut nested_read_cursor = None;
        let mut read_state = MarfReadState::new();
        let mut read_ctx = MarfReadCtx::new(
            storage,
            &mut read_cursor,
            &mut nested_read_cursor,
            &mut read_state,
        );
        read_ctx.get_block_height_miner_tip(block_hash, current_block_hash)
    }

    pub fn get_block_height(
        storage: &mut TrieStorageConnection<T>,
        block_hash: &T,
        current_block_hash: &T,
    ) -> Result<Option<u32>, Error> {
        MARF::get_block_height_miner_tip(storage, block_hash, current_block_hash)
    }

    pub fn get_block_at_height(
        storage: &mut TrieStorageConnection<T>,
        height: u32,
        current_block_hash: &T,
    ) -> Result<Option<T>, Error> {
        let mut read_cursor = None;
        let mut nested_read_cursor = None;
        let mut read_state = MarfReadState::new();
        let mut read_ctx = MarfReadCtx::new(
            storage,
            &mut read_cursor,
            &mut nested_read_cursor,
            &mut read_state,
        );
        read_ctx.get_block_at_height(height, current_block_hash)
    }

    /// Make an unconfirmed chain tip from an existing chain tip, so that it won't conflict with
    /// the "true" chain tip after the state it represents is later reprocessed and confirmed.
    pub fn make_unconfirmed_chain_tip(chain_tip: &T) -> T {
        let mut bytes = [0u8; 64];
        bytes[0..32].copy_from_slice(chain_tip.as_bytes());
        bytes[32..64].copy_from_slice(chain_tip.as_bytes());

        let h = Sha512Trunc256Sum::from_data(&bytes);
        let mut res_bytes = [0u8; 32];
        res_bytes[0..32].copy_from_slice(h.as_bytes());

        T::from_bytes(res_bytes)
    }

    /// Insert a batch of key/value pairs.  More efficient than inserting them individually, since
    /// the trie root hash will only be calculated once (which is an O(log B) operation).
    fn inner_insert_batch<S: NodePatching + NodeParking>(
        conn: &mut TrieStorageTransaction<T>,
        block_hash: &T,
        keys: &[String],
        values: &[MARFValue],
        decode_scratch: &mut S,
    ) -> Result<(), Error> {
        let leaves: Vec<_> = values
            .iter()
            .map(|value| TrieLeaf::from_value(&[], value.clone()))
            .collect();
        Self::inner_insert_leaf_batch(conn, block_hash, keys, &leaves, decode_scratch, false)
    }

    /// Insert leaves with physical locators while preserving logical root updates.
    fn inner_insert_leaf_batch<S: NodePatching + NodeParking>(
        conn: &mut TrieStorageTransaction<T>,
        block_hash: &T,
        keys: &[String],
        values: &[TrieLeaf],
        decode_scratch: &mut S,
        height_updates: bool,
    ) -> Result<(), Error> {
        if !height_updates && keys.iter().any(|key| reserved_height_key(key)) {
            conn.invalidate_ancestry();
        }
        let failure_guard = conn.result_cache_failure_guard();
        assert_eq!(keys.len(), values.len());

        let (Some(last_key), Some(last_value)) = (keys.last(), values.last()) else {
            // if empty, nothing to do
            failure_guard.succeed();
            return Ok(());
        };

        let (cur_block_hash, cur_block_id) = conn.get_cur_block_and_id();

        let last = keys.len() - 1;
        let mut progress = 0;
        let eta_enabled = keys.len() > 10_000;
        let mut result =
            keys.iter()
                .enumerate()
                .zip(values.iter())
                .try_for_each(|((index, key), value)| {
                    let marf_leaf = value;
                    let path = TrieHash::from_key(key);

                    if eta_enabled {
                        let updated_progress = 100 * index / last;
                        if updated_progress > progress {
                            progress = updated_progress;
                            info!(
                                "Batching insertions in MARF: {}% ({} out of {})",
                                progress, index, last
                            );
                        }
                    }
                    MARF::do_insert_leaf(conn, block_hash, &path, &marf_leaf, false, decode_scratch)
                });

        if result.is_ok() {
            // last insert updates the root with the skiplist hash
            let marf_leaf = last_value;
            let path = TrieHash::from_key(last_key);
            result =
                MARF::do_insert_leaf(conn, block_hash, &path, &marf_leaf, true, decode_scratch);
        }

        // restore
        conn.open_block_maybe_id(&cur_block_hash, cur_block_id)?;

        if result.is_ok() {
            failure_guard.succeed();
        }
        result
    }
}

// instance methods
impl<T: MarfTrieId> MARF<T> {
    pub fn with_conn<F, R>(&mut self, exec: F) -> R
    where
        F: for<'conn> FnOnce(&mut TrieStorageConnection<'conn, T>) -> R,
    {
        let mut conn = self.storage.connection();
        exec(&mut conn)
    }

    pub fn begin_tx(&mut self) -> Result<MarfTransaction<'_, T>, Error> {
        let storage = self.storage.transaction()?;
        Ok(MarfTransaction {
            storage,
            open_chain_tip: &mut self.open_chain_tip,
            read_cursor: &mut self.read_cursor,
            nested_read_cursor: &mut self.nested_read_cursor,
            read_state: &mut self.read_state,
        })
    }

    /// Target the MARF's storage at a given block.
    pub fn open_block(&mut self, block_hash: &T) -> Result<(), Error> {
        <Self as MarfCore<T>>::open_block(self, block_hash, None)
    }

    /// Get the block hash at a given height, if it exists.
    pub fn get_bhh_at_height(&mut self, block_hash: &T, height: u32) -> Result<Option<T>, Error> {
        <Self as MarfCore<T>>::get_block_at_height(self, height, block_hash)
    }

    /// Insert a batch of key/value pairs.
    ///
    /// More efficient than inserting them individually, since the trie root hash will only be
    /// calculated once (which is an `O(log B)` operation).
    pub fn insert_batch(
        &mut self,
        keys: impl AsRef<[String]>,
        values: impl AsRef<[MARFValue]>,
    ) -> Result<(), Error> {
        let keys = keys.as_ref();
        let values = values.as_ref();

        if self.storage.readonly() {
            return Err(Error::ReadOnlyError);
        }
        assert_eq!(keys.len(), values.len());

        let block_hash = match self.open_chain_tip {
            None => Err(Error::WriteNotBegunError),
            Some(WriteChainTip { ref block_hash, .. }) => Ok(block_hash.clone()),
        }?;

        if keys.is_empty() {
            return Ok(());
        }

        let mut tx = self.storage.transaction()?;
        MARF::inner_insert_batch(&mut tx, &block_hash, keys, values, &mut self.read_state)?;
        tx.commit_tx();
        Ok(())
    }

    pub fn insert(&mut self, key: &str, value: MARFValue) -> Result<(), Error> {
        if self.storage.readonly() {
            return Err(Error::ReadOnlyError);
        }
        let marf_leaf = TrieLeaf::from_value(&[], value);
        let path = TrieHash::from_key(key);
        self.insert_raw_tracked(path, marf_leaf, reserved_height_key(key))
    }

    /// Insert the given (key, value) pair into the MARF.  Inserting the same key twice silently
    /// overwrites the existing key.  Succeeds if there are no storage errors.
    /// Must be called after a call to .begin() (will fail otherwise)
    pub fn insert_raw(&mut self, path: TrieHash, marf_leaf: TrieLeaf) -> Result<(), Error> {
        self.insert_raw_tracked(path, marf_leaf, true)
    }

    /// Classify opaque paths conservatively while retaining ordinary single-key insertion behavior.
    fn insert_raw_tracked(
        &mut self,
        path: TrieHash,
        marf_leaf: TrieLeaf,
        opaque: bool,
    ) -> Result<(), Error> {
        if self.storage.readonly() {
            return Err(Error::ReadOnlyError);
        }
        match self.open_chain_tip {
            None => Err(Error::WriteNotBegunError),
            Some(WriteChainTip { ref block_hash, .. }) => {
                let mut tx = self.storage.transaction()?;
                if opaque {
                    tx.invalidate_ancestry();
                }
                let (cur_block_hash, cur_block_id) = tx.get_cur_block_and_id();

                let result = MARF::do_insert_leaf(
                    &mut tx,
                    block_hash,
                    &path,
                    &marf_leaf,
                    true,
                    &mut self.read_state,
                );

                // restore
                tx.open_block_maybe_id(&cur_block_hash, cur_block_id)?;
                tx.commit_tx();

                result
            }
        }
    }

    /// Drop the current trie from the MARF. This rolls back all
    ///   changes in the block, and closes the current chain tip.
    pub fn drop_current(&mut self) {
        if !self.storage.readonly() {
            let mut tx = self
                .storage
                .transaction()
                .expect("BUG: failed to start transaction to drop trie");
            tx.drop_extending_trie();
            self.open_chain_tip.take();
            tx.open_block(&T::sentinel())
                .expect("BUG: should never fail to open the block sentinel");
            tx.commit_tx();
        }
    }

    /// Drop the current trie from the MARF, and roll back all unconfirmed state
    pub fn drop_unconfirmed(&mut self) {
        if !self.storage.readonly() && self.storage.unconfirmed() {
            if let Some(tip) = self.open_chain_tip.take() {
                let mut tx = self
                    .storage
                    .transaction()
                    .expect("BUG: failed to start transaction to drop trie");
                tx.drop_unconfirmed_trie(&tip.block_hash);
                tx.open_block(&T::sentinel())
                    .expect("BUG: should never fail to open the block sentinel");
                tx.commit_tx();
            }
        }
    }

    /// Finish writing the next trie in the MARF.  This persists all changes.
    /// Works for both confirmed and unconfirmed tries
    pub fn commit(&mut self) -> Result<(), Error> {
        if self.storage.readonly() {
            return Err(Error::ReadOnlyError);
        }
        if let Some(_tip) = self.open_chain_tip.take() {
            let mut tx = self.storage.transaction()?;
            tx.flush()?;
            tx.commit_tx();
        }
        Ok(())
    }

    /// Finish writing the next trie in the MARF -- this is used by miners
    ///   to commit the mined block, but write it to the mined_block table,
    ///   rather than out to the marf_data table (this prevents the
    ///   miner's block from getting stepped on after the sortition).
    pub fn commit_mined(&mut self, bhh: &T) -> Result<(), Error> {
        if self.storage.readonly() {
            return Err(Error::ReadOnlyError);
        }
        if self.storage.unconfirmed() {
            return Err(Error::UnconfirmedError);
        }
        if let Some(_tip) = self.open_chain_tip.take() {
            let mut tx = self.storage.transaction()?;
            tx.flush_mined(bhh)?;
            tx.commit_tx();
        }
        Ok(())
    }

    /// Finish writing the next trie in the MARF, but change the hash of the current Trie's
    /// block hash to something other than what we opened it as.  This persists all changes.
    pub fn commit_to(&mut self, real_bhh: &T) -> Result<(), Error> {
        if self.storage.readonly() {
            return Err(Error::ReadOnlyError);
        }
        if self.storage.unconfirmed() {
            return Err(Error::UnconfirmedError);
        }
        if let Some(_tip) = self.open_chain_tip.take() {
            let mut tx = self.storage.transaction()?;
            tx.flush_to(real_bhh)?;
            tx.commit_tx();
        }
        Ok(())
    }

    /// Dump diagnostic information about the MARF's current transient state.
    pub fn dump_diagnostics(&mut self) {
        let (cur_block, cur_block_id) = {
            let conn = self.storage.connection();
            conn.get_cur_block_and_id()
        };
        error!(
            "MARF diagnostics: cur_block={cur_block}, cur_block_id={cur_block_id:?}, \
             readonly={read_only}, unconfirmed={unconfirmed}, \
             has_uncommitted_writes={has_uncommitted_writes}, \
             open_chain_tip={open_tip}",
            read_only = self.storage.readonly(),
            unconfirmed = self.storage.unconfirmed(),
            has_uncommitted_writes = self.storage.has_uncommitted_writes(),
            open_tip = self
                .open_chain_tip
                .as_ref()
                .map(|t| format!("Some({})", t.block_hash))
                .unwrap_or_else(|| "None".to_string()),
        );
    }

    // Comes from the marf.
    pub fn get_block_height_of(
        &mut self,
        bhh: &T,
        current_block_hash: &T,
    ) -> Result<Option<u32>, Error> {
        if Some(bhh) == self.get_open_chain_tip() {
            return Ok(self.get_open_chain_tip_height());
        } else {
            <Self as MarfCore<T>>::get_block_height_miner_tip(self, bhh, current_block_hash)
        }
    }

    /// Get open chain tip
    pub fn get_open_chain_tip(&self) -> Option<&T> {
        self.open_chain_tip.as_ref().map(|x| &x.block_hash)
    }

    /// Get open chain tip block height
    pub fn get_open_chain_tip_height(&self) -> Option<u32> {
        self.open_chain_tip.as_ref().map(|x| x.height)
    }

    /// Attach the immutable source for locator-only leaf commitments and proofs.
    pub fn set_value_extent_resolver(&mut self, resolver: Arc<dyn ValueExtentResolver>) {
        self.storage.set_value_extent_resolver(resolver);
    }

    /// Select a layout already published in the database metadata.
    pub fn set_record_format(&mut self, format: NodeRecordFormat) {
        self.storage.set_record_format(format);
    }

    /// Access internal storage.
    #[cfg(test)]
    pub fn borrow_storage_backend(&mut self) -> TrieStorageConnection<'_, T> {
        self.storage.connection()
    }

    #[cfg(test)]
    pub fn borrow_storage_transaction(&mut self) -> TrieStorageTransaction<'_, T> {
        self.storage.transaction().unwrap()
    }

    /// Make a raw transaction to the underlying storage
    pub fn storage_tx(&mut self) -> Result<Transaction<'_>, db_error> {
        self.storage.sqlite_tx()
    }

    /// Reopen storage read-only
    pub fn reopen_storage_readonly(&self) -> Result<TrieFileStorage<T>, Error> {
        self.storage.reopen_readonly()
    }

    /// Reopen this MARF with readonly storage.
    ///
    /// Returns Err if:
    ///   1) This class is already in the process of writing.
    ///   2) A new underlying SQLite database connection cannot be established.
    pub fn reopen_readonly(&self) -> Result<MARF<T>, Error> {
        if self.open_chain_tip.is_some() {
            error!(
                "MARF at {} is already in the process of writing",
                &self.storage.db_path
            );
            return Err(Error::InProgressError);
        }

        let ro_storage = self.storage.reopen_readonly()?;
        Ok(MARF {
            storage: ro_storage,
            open_chain_tip: None,
            read_cursor: None,
            nested_read_cursor: None,
            read_state: MarfReadState::new(),
        })
    }

    /// Build a read-only storage connection which can be used for reads without modifying the
    ///  calling MARF struct (i.e., the tip pointer is only changed in the connection)
    ///  but reusing self's existing SQLite Connection (avoiding the overhead of
    ///   `reopen_readonly`).
    pub fn reopen_connection(&self) -> Result<ReopenedTrieStorageConnection<'_, T>, Error> {
        if self.open_chain_tip.is_some() {
            error!(
                "MARF at {} is already in the process of writing",
                &self.storage.db_path
            );
            return Err(Error::InProgressError);
        }
        self.storage.reopen_connection()
    }

    /// Get the root trie hash at a particular block
    pub fn get_root_hash_at(&mut self, block_hash: &T) -> Result<TrieHash, Error> {
        self.storage.connection().get_root_hash_at(block_hash)
    }

    /// Convert to the inner sqlite connection
    pub fn into_sqlite_conn(self) -> Connection {
        self.storage.into_sqlite_conn()
    }

    /// Get the underlying storage DB path
    pub fn get_db_path(&self) -> &str {
        &self.storage.db_path
    }
}

// Leaf traversal

impl<T: MarfTrieId> MARF<T> {
    /// Walk all leaves in the trie at `block_hash`, yielding full paths and values.
    ///
    /// Follows backpointers to resolve nodes living in earlier blocks, so the
    /// returned set represents the complete state visible at `block_hash`.
    pub(crate) fn for_each_leaf<F>(
        storage: &mut TrieStorageConnection<T>,
        block_hash: &T,
        mut handle_leaf: F,
    ) -> Result<u64, Error>
    where
        F: FnMut(TrieHash, MARFValue) -> Result<(), Error>,
    {
        storage.check_historical_read_allowed(block_hash)?;

        let (original_block_hash, original_block_id) = storage.get_cur_block_and_id();
        let mut decode_scratch = MarfReadState::new();
        let result =
            Self::inner_each_leaf(storage, block_hash, &mut decode_scratch, &mut handle_leaf);

        storage
            .open_block_maybe_id(&original_block_hash, original_block_id)
            .inspect_err(|e| {
                warn!("Failed to re-open {original_block_hash} {original_block_id:?}: {e:?}");
            })?;

        let (restored_block_hash, _) = storage.get_cur_block_and_id();
        assert_eq!(
            restored_block_hash, original_block_hash,
            "for_each_leaf: open block changed after traversal"
        );

        result
    }

    fn inner_each_leaf<S: NodePatching, F>(
        storage: &mut TrieStorageConnection<T>,
        block_hash: &T,
        decode_scratch: &mut S,
        handle_leaf: &mut F,
    ) -> Result<u64, Error>
    where
        F: FnMut(TrieHash, MARFValue) -> Result<(), Error>,
    {
        storage.open_block(block_hash)?;
        let (cur_block, cur_id) = storage.get_cur_block_and_id();
        let value_resolver = storage.value_extent_resolver();
        let root_read = Trie::read_root(storage, decode_scratch)?;

        let mut leaf_count = 0u64;
        let mut stack: Vec<(TriePtr, Vec<u8>, T, Option<u32>)> = Vec::new();

        if Self::process_leaf_walk_node(
            &root_read,
            value_resolver.as_deref(),
            vec![],
            cur_block,
            cur_id,
            &mut stack,
            handle_leaf,
        )? {
            leaf_count += 1;
        }

        let walk_start = Instant::now();
        let mut last_walk_log = Instant::now();
        let mut nodes_visited: u64 = 0;

        while let Some((ptr, prefix, return_block, return_block_id)) = stack.pop() {
            nodes_visited += 1;
            if last_walk_log.elapsed().as_secs() >= 30 {
                info!(
                    "for_each_leaf: {nodes_visited} nodes visited, {leaf_count} leaves found, stack {}, {:?} elapsed",
                    stack.len(),
                    walk_start.elapsed()
                );
                last_walk_log = Instant::now();
            }

            let (cur_block_hash, _) = storage.get_cur_block_and_id();
            if cur_block_hash != return_block {
                storage.open_block_maybe_id(&return_block, return_block_id)?;
            }

            let mut read_session = TrieReadSession::new(storage, decode_scratch);
            let resolved_node = Self::read_node_for_ptr(&mut read_session, &ptr)?;
            if Self::process_leaf_walk_node(
                &resolved_node.node,
                value_resolver.as_deref(),
                prefix,
                resolved_node.block_hash,
                resolved_node.block_id,
                &mut stack,
                handle_leaf,
            )? {
                leaf_count += 1;
            }
        }

        Ok(leaf_count)
    }

    /// Process one node during the leaf-walk DFS.
    ///
    /// Returns:
    ///  - `true` if `node` was a leaf and was emitted via `handle_leaf`.
    ///  - `false` if `node` was internal and its children were queued.
    ///
    /// `prefix` is the path accumulated before this node.
    ///
    /// `block_hash` and `block_id` identify the block where queued child pointers
    /// should be resolved. Inline children stay in the currently-open block, while
    /// children of a node reached through a backpointer use the backpointer target
    /// returned by `read_node_for_ptr`.
    fn process_leaf_walk_node<F>(
        node: &ReadTrieNode<'_>,
        value_resolver: Option<&dyn ValueExtentResolver>,
        prefix: Vec<u8>,
        block_hash: T,
        block_id: Option<u32>,
        stack: &mut Vec<(TriePtr, Vec<u8>, T, Option<u32>)>,
        handle_leaf: &mut F,
    ) -> Result<bool, Error>
    where
        F: FnMut(TrieHash, MARFValue) -> Result<(), Error>,
    {
        let mut full_prefix = prefix;
        full_prefix.extend_from_slice(node.path_bytes()?);

        if let Some(leaf) = node.as_leaf()? {
            if full_prefix.len() != TRIEHASH_ENCODED_SIZE {
                return Err(Error::CorruptionError(
                    "Leaf path length invalid".to_string(),
                ));
            }
            let path = TrieHash::from_bytes(&full_prefix)
                .ok_or_else(|| Error::CorruptionError("Failed to decode leaf path".to_string()))?;
            let value = match leaf.data {
                Some(value) => value.clone(),
                None => value_resolver
                    .ok_or_else(|| Error::CorruptionError("Leaf walk needs value resolver".into()))?
                    .commitment(leaf.extent.ok_or_else(|| {
                        Error::CorruptionError("Leaf lacks value locator".into())
                    })?)?,
            };
            handle_leaf(path, value)?;
            Ok(true)
        } else {
            for ptr in node.ptrs()?.iter() {
                if ptr.id() != TrieNodeID::Empty as u8 {
                    let mut child_prefix = full_prefix.clone();
                    child_prefix.push(ptr.chr());
                    stack.push((*ptr, child_prefix, block_hash.clone(), block_id));
                }
            }
            Ok(false)
        }
    }

    /// Read a node referenced by `ptr`, following backpointers when necessary.
    fn read_node_for_ptr<'a, S: NodePatching, R: TrieReadStorage<T>>(
        read_session: &'a mut TrieReadSession<'_, T, S, R>,
        ptr: &TriePtr,
    ) -> Result<ResolvedReadNode<'a, T>, Error> {
        if is_backptr(ptr.id()) {
            let back_block_id = ptr.back_block();
            let back_block_hash = read_session
                .storage()
                .get_block_from_local_id(back_block_id)?;
            read_session
                .storage()
                .open_block_known_id(&back_block_hash, back_block_id)?;
            let backptr = ptr.from_backptr();
            let read = read_session.read_node(&backptr)?;
            Ok(ResolvedReadNode {
                node: read,
                block_hash: back_block_hash,
                block_id: Some(back_block_id),
            })
        } else {
            let (cur_block_hash, cur_block_id) = read_session.storage().get_cur_block_and_id();
            let read = read_session.read_node(ptr)?;
            Ok(ResolvedReadNode {
                node: read,
                block_hash: cur_block_hash,
                block_id: cur_block_id,
            })
        }
    }
}
