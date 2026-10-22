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

//! The forest: blocks, forks, lookups, writes, ancestry.
//!
//! A [`MARF`] owns a SQLite connection and an `Inner` (storage state plus
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

pub use self::operations::MarfConnection;
#[cfg(any(test, feature = "testing"))]
pub use self::options::fault_injection_marf_compression;
pub use self::options::test_override_marf_compression;
use crate::MarfTrieId;
use crate::storage::Store;
use crate::trie::Trie;

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
    /// Production value: [`crate::storage::PATCH_CACHE_CAPACITY`]. An experimental
    /// control for benchmarks; consumers leave it at the default.
    pub patch_cache_capacity: usize,
}
/// The trie being built for one block.
struct Building<T> {
    trie: Trie<T>,
    height: u32,
}

/// Everything about a MARF except its SQLite connection.
struct Inner<T: MarfTrieId> {
    store: Store<T>,
    building: Option<Building<T>>,
    /// Block most recently targeted by `open_block`.
    current: T,
}

/// Open storage without a MARF wrapper (kept for API compatibility).
pub struct TrieFileStorage<T: MarfTrieId> {
    db: Connection,
    store: Store<T>,
}
/// Merklized Adaptive-Radix Forest -- a collection of Merklized Adaptive-Radix Tries.
pub struct MARF<T: MarfTrieId> {
    db: Connection,
    inner: Inner<T>,
}

/// A MARF with an open SQLite transaction. Dropping it without `commit`
/// rolls the transaction back (the in-memory trie, if any, is kept).
pub struct MarfTransaction<'a, T: MarfTrieId> {
    /// `None` once committed, rolled back or handed out.
    tx: Option<Transaction<'a>>,
    inner: &'a mut Inner<T>,
}

/// A connection plus mutable MARF state: where all operations are implemented.
pub struct View<'a, T: MarfTrieId> {
    db: &'a Connection,
    inner: &'a mut Inner<T>,
}

/// A read-only view that shares a MARF's SQLite connection but has its own
/// caches and `open_block` state.
pub struct ReopenedMarf<'a, T: MarfTrieId> {
    db: &'a Connection,
    inner: Inner<T>,
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

mod operations;
mod options;
