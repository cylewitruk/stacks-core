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

//! Persistence of committed tries: the `marf.sqlite` database and the
//! optional `marf.sqlite.blobs` flat file.
//!
//! The schema and blob layout are the original implementation's (see
//! `docs/marf-rewrite/ANALYSIS.md`). A [`Store`] holds everything about a
//! MARF's storage except the SQLite connection itself; every function takes
//! the connection (or transaction) it should use, so the same code serves
//! plain reads, reads inside a write transaction, and read-only views.

mod archive;
mod blobs;
mod blocks;
mod read;
mod schema;
#[cfg(test)]
mod tests;
mod write;

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use memmap2::Mmap;
use rusqlite::{Connection, OpenFlags};

pub use self::read::descend_branch;
pub use self::schema::{
    SQL_MARF_EXTERNAL_BLOBS_SCHEMA_VERSION, SQL_MARF_SCHEMA_VERSION,
    SQL_MARF_TYPE_FIRST_SCHEMA_VERSION, open,
};
pub use self::write::clear_locks;
pub use crate::archive::PruneInfo;
use crate::codec::Record;
pub use crate::marf::{TrieFileStorage, TrieHashCalculationMode};
use crate::node::{Branch, Ptr};
use crate::observe::{Noop, Observer};
use crate::sqlite::{
    NO_PARAMS, SQLITE_MARF_PAGE_SIZE, SQLITE_MMAP_SIZE, sql_pragma, sqlite_open, table_exists,
};
use crate::{Error, TrieHash, TrieId};

/// The original's name for a connection-plus-state handle.
pub type TrieStorageConnection<'a, T, O = Noop> = crate::marf::View<'a, T, O>;

fn u64_sql(x: u64) -> Result<i64, Error> {
    i64::try_from(x).map_err(|_| Error::OverflowError)
}
/// The external `.blobs` file: tries appended back to back.
///
/// Reads go through a memory map that is created lazily and re-created only
/// when a read reaches past its end (the file only grows, and bytes once
/// written are never rewritten, so an existing map stays valid). Writes use
/// positioned I/O so no cursor state is shared between handles. A clone
/// starts without a map and maps on first use.
pub struct Blobs {
    file: Arc<File>,
    path: PathBuf,
    map: Option<Mmap>,
}

/// Where a block's trie bytes live.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Location {
    /// `marf_data.data` (internal blobs, and every unconfirmed trie).
    Sqlite { unconfirmed: bool },
    /// Byte range in the external blobs file.
    External { offset: u64, len: u64 },
}

/// Storage state for one MARF, minus the SQLite connection.
pub struct Store<T, O: Observer = Noop> {
    observer: std::marker::PhantomData<O>,
    db_path: PathBuf,
    blobs: Option<Blobs>,
    readonly: bool,
    unconfirmed: bool,
    compress: bool,
    prune: Option<PruneInfo>,
    /// Confirmed block hash <-> storage id.
    ids: HashMap<T, u32>,
    hashes: HashMap<u32, T>,
    locations: HashMap<u32, Location>,
    /// Reusable buffers so steady-state reads allocate nothing: record bytes
    /// of a SQLite-stored trie, patch diffs collected along a patch chain,
    /// and the branch a patch chain (or an owned decode) resolves into.
    scratch: Scratch,
    /// Whether the optimized reference's optional direct-hash sidecar has
    /// been checked for (and retired) on this store's first write.
    sidecar_retired: bool,
    /// Capacity of `scratch.patched`; 0 disables lookup and retention.
    patch_cache_capacity: usize,
}

#[derive(Default)]
struct Scratch {
    bytes: BytesScratch,
    /// Patch diffs collected along one patch chain.
    diffs: Vec<Ptr>,
    /// Nodes stored as patch chains, resolved, by `(block_id, offset)`.
    /// Confirmed tries never change, so entries stay valid until the
    /// caches are dropped (see [`Store::invalidate`]); the map is emptied
    /// when it reaches [`PATCH_CACHE_CAPACITY`].
    patched: HashMap<(u32, u64), Cached>,
    /// Where a plain record is decoded for callers that need a `Branch`.
    decoded: Option<Branch>,
}

/// A resolved patch chain: the node, the hash stored at its location, and
/// the chain's depth.
struct Cached {
    node: Branch,
    hash: TrieHash,
    depth: u32,
}

/// Resolved patch chains kept per store. A chain costs a base decode plus up
/// to [`crate::codec::MAX_PATCH_DEPTH`] patch applications, and the same few hot nodes
/// (the root and its children) are patched in block after block.
pub const PATCH_CACHE_CAPACITY: usize = 4096;

#[derive(Default)]
struct BytesScratch {
    buf: Vec<u8>,
    /// Which `(block_id, offset)` `buf` currently holds, within one read.
    key: Option<(u32, u64)>,
}

/// A node as it sits in storage: a borrowed record, or, for a location that
/// holds a patch chain, the chain's base with the patches applied (resolved
/// into the store's scratch branch).
#[derive(Clone, Copy)]
pub enum StoreNode<'a> {
    Record(Record<'a>),
    Patched(&'a Branch),
}
/// The result of resolving a node location.
pub struct Resolved<'a> {
    pub node: StoreNode<'a>,
    /// The hash stored at the location (of the outermost patch, if any).
    pub hash: TrieHash,
    /// Number of patches followed.
    pub depth: u32,
}

/// Cache key reserved for nodes of the unconfirmed trie (never a real location).
const UNCONFIRMED_SLOT: (u32, u64) = (u32::MAX, u64::MAX);

impl<T: TrieId, O: Observer> Store<T, O> {
    /// A read-only store over the same files with fresh caches.
    pub fn reopen_readonly(&self) -> Store<T, O> {
        Store {
            observer: std::marker::PhantomData,
            db_path: self.db_path.clone(),
            blobs: self.blobs.clone(),
            readonly: true,
            unconfirmed: self.unconfirmed,
            compress: self.compress,
            prune: self.prune.clone(),
            ids: HashMap::new(),
            hashes: HashMap::new(),
            locations: HashMap::new(),
            scratch: Scratch::default(),
            sidecar_retired: true,
            patch_cache_capacity: self.patch_cache_capacity,
        }
    }

    /// The optimized reference may keep a `<db>.hashes-<token>` sidecar
    /// (block id -> block hash, plus ancestry) described by a
    /// `marf_direct_hash_index` row, and extends it as it commits. This
    /// implementation does not maintain that file, so before its first write
    /// to such a database it marks the publication invalid: the reference
    /// then falls back to the SQL tables, which stay authoritative, instead
    /// of trusting a sidecar that stopped at an older block.
    fn retire_direct_hash_sidecar(&mut self, conn: &Connection) -> Result<(), Error> {
        if self.sidecar_retired {
            return Ok(());
        }
        self.sidecar_retired = true;
        if table_exists(conn, "marf_direct_hash_index")? {
            conn.execute("UPDATE marf_direct_hash_index SET valid = 0", NO_PARAMS)?;
        }
        Ok(())
    }

    /// Open a second SQLite connection to the same database, read-only.
    pub fn reopen_readonly_connection(&self) -> Result<Connection, Error> {
        let conn = sqlite_open::<O>(&self.db_path, OpenFlags::SQLITE_OPEN_READ_ONLY, false)?;
        sql_pragma(&conn, "mmap_size", &SQLITE_MMAP_SIZE)?;
        sql_pragma(&conn, "page_size", &SQLITE_MARF_PAGE_SIZE)?;
        Ok(conn)
    }

    pub fn prune_height(&self) -> Option<u32> {
        self.prune.as_ref().map(|s| s.prune_height)
    }

    /// Entries retained in the resolved-patch cache (benchmark
    /// instrumentation). The scratch slot a resolved chain is returned from
    /// when it is not cacheable (unconfirmed trie, or capacity 0) is not an
    /// entry: nothing is ever looked up in it.
    #[cfg(any(test, feature = "testing", feature = "counters"))]
    pub fn patch_cache_len(&self) -> usize {
        self.scratch.patched.len()
            - usize::from(self.scratch.patched.contains_key(&UNCONFIRMED_SLOT))
    }

    /// Forget everything cached from the database: block ids, locations and
    /// resolved patch chains. Called when a transaction rolls back, since
    /// rows written inside it (and their storage ids) may be reused.
    pub fn invalidate(&mut self) {
        self.ids.clear();
        self.hashes.clear();
        self.locations.clear();
        self.scratch.patched.clear();
        // a sidecar retirement issued inside the rolled-back transaction was
        // undone with it; the next write must issue it again
        self.sidecar_retired = self.readonly;
        self.scratch.bytes.key = None;
    }
}

/// Immutable policy for a storage handle and the tries it may write.
#[derive(Clone, Copy, Debug)]
pub struct StoragePolicy {
    /// Writes through this handle are forbidden.
    pub readonly: bool,
    /// This handle builds unconfirmed rather than confirmed tries.
    pub unconfirmed: bool,
    /// Confirmed writes use compressed pointer encoding.
    pub compress: bool,
}

/// Storage policy and archive identity available to the MARF coordinator.
pub trait StorageContext {
    /// The policy established when the handle was opened.
    fn policy(&self) -> StoragePolicy;
    /// The database backing this handle.
    fn database_path(&self) -> &Path;
    /// Completed archive boundary, if this store has been pruned.
    fn prune_info(&self) -> Option<&PruneInfo>;
}

impl<T, O: Observer> StorageContext for Store<T, O> {
    fn policy(&self) -> StoragePolicy {
        StoragePolicy {
            readonly: self.readonly,
            unconfirmed: self.unconfirmed,
            compress: self.compress,
        }
    }
    fn database_path(&self) -> &Path {
        &self.db_path
    }
    fn prune_info(&self) -> Option<&PruneInfo> {
        self.prune.as_ref()
    }
}
