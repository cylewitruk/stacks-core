//! Persistence of committed tries: the `marf.sqlite` database and the
//! optional `marf.sqlite.blobs` flat file.
//!
//! The schema and blob layout are the original implementation's (see
//! `docs/marf-rewrite/ANALYSIS.md`). A [`Store`] holds everything about a
//! MARF's storage except the SQLite connection itself; every function takes
//! the connection (or transaction) it should use, so the same code serves
//! plain reads, reads inside a write transaction, and read-only views.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{fs, io};

use rusqlite::{params, Connection, DatabaseName, OpenFlags, OptionalExtension};
use stacks_common::types::chainstate::TrieHash;
use stacks_common::types::sqlite::NO_PARAMS;
use stacks_common::util::db::{
    sql_pragma, sqlite_open, table_exists, tx_begin_immediate, SQLITE_MARF_PAGE_SIZE,
    SQLITE_MMAP_SIZE,
};

use memmap2::Mmap;

use crate::codec::{self, Record, RecordKind, HASH_LEN, MAX_NODE_LEN, MAX_PATCH_DEPTH};
use crate::node::{Branch, Kind, Node, Path as NodePath, Ptr};
use crate::trie::{Fetched, ROOT_OFFSET};
use crate::{corrupt, Error, MarfTrieId};

pub use crate::marf::{TrieFileStorage, TrieHashCalculationMode};
/// The original's name for a connection-plus-state handle.
pub type TrieStorageConnection<'a, T> = crate::marf::View<'a, T>;

// ---------------------------------------------------------------------------
// schema

const SQL_MARF_DATA_TABLE: &str = "
CREATE TABLE IF NOT EXISTS marf_data (
   block_id INTEGER PRIMARY KEY,
   block_hash TEXT UNIQUE NOT NULL,
   -- the trie itself.
   -- if not used, then set to a zero-byte entry.
   data BLOB NOT NULL,
   unconfirmed INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS block_hash_marf_data ON marf_data(block_hash);
CREATE INDEX IF NOT EXISTS unconfirmed_marf_data ON marf_data(unconfirmed);
";
const SQL_MARF_MINED_TABLE: &str = "
CREATE TABLE IF NOT EXISTS mined_blocks (
   block_id INTEGER PRIMARY KEY,
   block_hash TEXT UNIQUE NOT NULL,
   data BLOB NOT NULL
);

CREATE INDEX IF NOT EXISTS block_hash_mined_blocks ON mined_blocks(block_hash);
";
const SQL_EXTENSION_LOCKS_TABLE: &str = "
CREATE TABLE IF NOT EXISTS block_extension_locks (block_hash TEXT PRIMARY KEY);
";
const SQL_MARF_DATA_TABLE_SCHEMA_2: &str = "
-- pointer to a .blobs file with the externally-stored blob data.
-- if not used, then set to 1.
CREATE TABLE IF NOT EXISTS schema_version (
    version INTEGER DEFAULT 1 NOT NULL
);
-- migrated_version tracks the schema-2 external-blob data migration.
CREATE TABLE IF NOT EXISTS migrated_version (
    version INTEGER DEFAULT 1 NOT NULL
);
ALTER TABLE marf_data ADD COLUMN external_offset INTEGER DEFAULT 0 NOT NULL;
ALTER TABLE marf_data ADD COLUMN external_length INTEGER DEFAULT 0 NOT NULL;
CREATE INDEX IF NOT EXISTS index_external_offset ON marf_data(external_offset);

INSERT OR REPLACE INTO schema_version (version) VALUES (2);
INSERT OR REPLACE INTO migrated_version (version) VALUES (1);
";
const SQL_MARF_DATA_TABLE_SCHEMA_3: &str = "
CREATE TABLE IF NOT EXISTS marf_squash_info (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    archival_marf_root_hash BLOB NOT NULL,
    squash_root_node_hash BLOB,
    squash_height INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS marf_squashed_blocks (
    height INTEGER PRIMARY KEY,
    block_hash BLOB NOT NULL UNIQUE,
    marf_root_hash BLOB NOT NULL
);
UPDATE schema_version SET version = 3;
";

pub const SQL_MARF_EXTERNAL_BLOBS_SCHEMA_VERSION: u64 = 2;
pub const SQL_MARF_SCHEMA_VERSION: u64 = 3;
/// Schema version the optimized reference assigns to databases published in
/// its type-first record format (format 6). Not readable here yet.
pub const SQL_MARF_TYPE_FIRST_SCHEMA_VERSION: u64 = 4;

fn schema_version(conn: &Connection) -> u64 {
    conn.query_row("SELECT version FROM schema_version", NO_PARAMS, |r| {
        r.get::<_, i64>(0)
    })
    .map(|v| v as u64)
    .unwrap_or(1)
}

fn migrated_version(conn: &Connection) -> u64 {
    conn.query_row("SELECT version FROM migrated_version", NO_PARAMS, |r| {
        r.get::<_, i64>(0)
    })
    .map(|v| v as u64)
    .unwrap_or(1)
}

fn u64_sql(x: u64) -> Result<i64, Error> {
    i64::try_from(x).map_err(|_| Error::OverflowError)
}

// ---------------------------------------------------------------------------
// blob file

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

impl Clone for Blobs {
    fn clone(&self) -> Blobs {
        Blobs {
            file: self.file.clone(),
            path: self.path.clone(),
            map: None,
        }
    }
}

impl Blobs {
    /// `<db_path>.blobs`.
    pub fn path_for(db_path: &Path) -> PathBuf {
        let mut name = db_path.as_os_str().to_owned();
        name.push(".blobs");
        PathBuf::from(name)
    }

    /// Does a blobs file exist next to `db_path`?
    pub fn exists(db_path: impl AsRef<Path>) -> Result<bool, Error> {
        let db_path = db_path.as_ref();
        if db_path == Path::new(":memory:") {
            return Ok(false);
        }
        match fs::metadata(Self::path_for(db_path)) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    fn open(db_path: &Path, readonly: bool) -> Result<Blobs, Error> {
        let path = Self::path_for(db_path);
        let file = OpenOptions::new()
            .read(true)
            .write(!readonly)
            .create(!readonly)
            .open(&path)?;
        Ok(Blobs {
            file: Arc::new(file),
            path,
            map: None,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Make sure the map covers `[0, end)`. `Ok(false)` if the file is shorter.
    fn ensure_mapped(&mut self, end: u64) -> io::Result<bool> {
        if let Some(m) = &self.map {
            if m.len() as u64 >= end {
                return Ok(true);
            }
        }
        let len = self.file.metadata()?.len();
        if len < end {
            return Ok(false);
        }
        if len == 0 {
            return Ok(true);
        }
        let len = usize::try_from(len)
            .map_err(|_| io::Error::new(io::ErrorKind::Unsupported, "blobs file too large to map"))?;
        // SAFETY: the mapped bytes are only ever appended to by this crate
        // (and the original implementation), never rewritten or truncated
        // while a MARF is open; the map is a read-only view of them.
        let map = unsafe { memmap2::MmapOptions::new().len(len).map(&*self.file)? };
        self.map = Some(map);
        Ok(true)
    }

    /// The bytes `[offset, offset + len)`; an error if the file is shorter.
    pub fn slice(&mut self, offset: u64, len: u64) -> Result<&[u8], Error> {
        let end = offset.checked_add(len).ok_or(Error::OverflowError)?;
        if !self.ensure_mapped(end)? {
            return Err(corrupt!(
                "blobs file shorter than {end} bytes (read of {len} at {offset})"
            ));
        }
        if len == 0 {
            return Ok(&[]);
        }
        let map = self.map.as_ref().expect("mapped");
        Ok(&map[offset as usize..end as usize])
    }

    /// Append-style positioned write (the caller picks `offset` = file length).
    pub fn write_at(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        write_all_at(&self.file, data, offset)?;
        self.file.sync_data()
    }

    /// [`Blobs::write_at`] without the sync, for callers that write a blob in
    /// several pieces and call [`Blobs::sync`] once at the end.
    pub fn write_at_unsynced(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        write_all_at(&self.file, data, offset)
    }

    pub fn sync(&self) -> io::Result<()> {
        self.file.sync_data()
    }
}

#[cfg(unix)]
pub(crate) fn write_all_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.write_all_at(buf, offset)
}

#[cfg(unix)]
pub(crate) fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buf, offset)
}

#[cfg(windows)]
pub(crate) fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let n = file.seek_read(buf, offset)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "blobs file shorter than the record",
            ));
        }
        buf = &mut buf[n..];
        offset += n as u64;
    }
    Ok(())
}

#[cfg(windows)]
pub(crate) fn write_all_at(file: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let n = file.seek_write(buf, offset)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "seek_write wrote nothing",
            ));
        }
        buf = &buf[n..];
        offset += n as u64;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// store

pub use crate::pruning::PruneInfo;

/// Where a block's trie bytes live.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Location {
    /// `marf_data.data` (internal blobs, and every unconfirmed trie).
    Sqlite { unconfirmed: bool },
    /// Byte range in the external blobs file.
    External { offset: u64, len: u64 },
}

/// Storage state for one MARF, minus the SQLite connection.
pub struct Store<T> {
    pub db_path: PathBuf,
    pub blobs: Option<Blobs>,
    pub readonly: bool,
    pub unconfirmed: bool,
    pub compress: bool,
    pub prune: Option<PruneInfo>,
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
/// to [`MAX_PATCH_DEPTH`] patch applications, and the same few hot nodes
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

impl<'a> StoreNode<'a> {
    pub fn is_leaf(&self) -> Result<bool, Error> {
        match self {
            StoreNode::Record(r) => Ok(r.kind()? == RecordKind::Leaf),
            StoreNode::Patched(_) => Ok(false),
        }
    }

    /// The node's compressed path.
    pub fn path(&self) -> Result<&'a [u8], Error> {
        match self {
            StoreNode::Record(r) => match r.kind()? {
                RecordKind::Leaf => Ok(r.leaf()?.path),
                RecordKind::Branch(_) => r.branch()?.path(),
                RecordKind::Patch => Err(corrupt!("unresolved patch")),
            },
            StoreNode::Patched(b) => Ok(b.path.as_slice()),
        }
    }

    /// The child at `chr` of a branch (`None` for a leaf or an empty slot).
    pub fn child(&self, chr: u8) -> Result<Option<Ptr>, Error> {
        match self {
            StoreNode::Record(r) => match r.kind()? {
                RecordKind::Leaf => Ok(None),
                RecordKind::Branch(_) => r.branch()?.child(chr),
                RecordKind::Patch => Err(corrupt!("unresolved patch")),
            },
            StoreNode::Patched(b) => Ok(b.child(chr).copied()),
        }
    }

    /// One step of a key walk: for a record, [`Record::descend`]; for a
    /// resolved patch chain, the equivalent over the branch.
    pub fn descend(&self, key: &[u8], depth: usize) -> Result<codec::Descent<'a>, Error> {
        match self {
            StoreNode::Record(r) => r.descend(key, depth),
            StoreNode::Patched(b) => Ok(descend_branch(b, key, depth)?),
        }
    }

    /// The leaf's value, if this is a leaf.
    pub fn leaf_value(&self) -> Result<Option<crate::MARFValue>, Error> {
        match self {
            StoreNode::Record(r) if r.kind()? == RecordKind::Leaf => Ok(Some(r.leaf()?.value())),
            _ => Ok(None),
        }
    }

    /// Visit every occupied child slot in slot order.
    pub fn for_each_child(&self, mut visit: impl FnMut(Ptr)) -> Result<(), Error> {
        match self {
            StoreNode::Record(r) => match r.kind()? {
                RecordKind::Leaf => Ok(()),
                RecordKind::Branch(_) => r.branch()?.for_each_slot(|_, p| visit(p)).map(|_| ()),
                RecordKind::Patch => Err(corrupt!("unresolved patch")),
            },
            StoreNode::Patched(b) => {
                b.children.iter().for_each(|p| visit(*p));
                Ok(())
            }
        }
    }

    /// An owned copy.
    pub fn to_node(&self) -> Result<Node, Error> {
        match self {
            StoreNode::Record(r) => match r.decode()?.0 {
                codec::Decoded::Node(n) => Ok(n),
                codec::Decoded::Patch(_) => Err(corrupt!("unresolved patch")),
            },
            StoreNode::Patched(b) => Ok(Node::Branch((*b).clone())),
        }
    }
}

/// [`Record::descend`] for an in-memory branch.
pub fn descend_branch<'a>(b: &Branch, key: &[u8], depth: usize) -> Result<codec::Descent<'a>, Error> {
    let rest = &key[depth..];
    if !rest.starts_with(b.path.as_slice()) {
        return Ok(codec::Descent::Mismatch);
    }
    let Some(&chr) = rest.get(b.path.len()) else {
        return Err(corrupt!("Path reached a non-leaf"));
    };
    Ok(codec::Descent::Child {
        child: b.child(chr).copied(),
        consumed: b.path.len() + 1,
    })
}

/// The result of resolving a node location.
pub struct Resolved<'a> {
    pub node: StoreNode<'a>,
    /// The hash stored at the location (of the outermost patch, if any).
    pub hash: TrieHash,
    /// Number of patches followed.
    pub depth: u32,
}

/// Where block `block_id`'s trie bytes live (cached in `locations`).
fn location_in(
    locations: &mut HashMap<u32, Location>,
    external: bool,
    conn: &Connection,
    block_id: u32,
) -> Result<Location, Error> {
    if let Some(loc) = locations.get(&block_id) {
        return Ok(*loc);
    }
    let (offset, len, unconfirmed): (i64, i64, i64) = conn
        .prepare_cached(
            "SELECT external_offset, external_length, unconfirmed FROM marf_data WHERE block_id = ?1",
        )?
        .query_row(params![block_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?
        .ok_or(Error::NotFoundError)?;
    let loc = if external && len != 0 {
        Location::External {
            offset: offset as u64,
            len: len as u64,
        }
    } else {
        Location::Sqlite {
            unconfirmed: unconfirmed != 0,
        }
    };
    locations.insert(block_id, loc);
    Ok(loc)
}

/// Bytes of block `block_id`'s trie from `offset` for up to `max` bytes
/// (fewer at the end of the trie). Borrows the blobs map or the scratch
/// buffer; takes the fields it needs so callers can hold other parts of the
/// store at the same time.
fn trie_bytes<'s>(
    blobs: &'s mut Option<Blobs>,
    scratch: &'s mut BytesScratch,
    conn: &Connection,
    loc: Location,
    block_id: u32,
    offset: u64,
    max: usize,
) -> Result<&'s [u8], Error> {
    match loc {
        Location::External { offset: base, len } => {
            if offset >= len {
                return Err(corrupt!(
                    "offset {offset} beyond trie of {len} bytes (block {block_id})"
                ));
            }
            let want = (max as u64).min(len - offset);
            let blobs = blobs.as_mut().expect("external location implies blobs");
            blobs.slice(base + offset, want)
        }
        // A per-node incremental blob read, like the original: a walk through
        // a trie touches a handful of its nodes and lookups cross many tries,
        // so caching whole tries here only thrashes. The record is not
        // length-prefixed; read the hash and type byte first and then only
        // the bound that type allows, so a leaf costs ~100 bytes of copying
        // rather than the Node256 bound.
        Location::Sqlite { .. } => {
            if scratch.key != Some((block_id, offset)) {
                let blob = conn.blob_open(
                    DatabaseName::Main,
                    "marf_data",
                    "data",
                    i64::from(block_id),
                    true,
                )?;
                let len = blob.len();
                let off = usize::try_from(offset).map_err(|_| Error::OverflowError)?;
                if off >= len {
                    return Err(corrupt!(
                        "offset {offset} beyond trie of {len} bytes (block {block_id})"
                    ));
                }
                let avail = len - off;
                let head = max.min(avail).min(HASH_LEN + 1);
                scratch.buf.resize(head, 0);
                let n = blob.read_at(&mut scratch.buf, off)?;
                scratch.buf.truncate(n);
                if n == HASH_LEN + 1 && max > HASH_LEN + 1 {
                    let want = max.min(avail).min(codec::max_record_len(scratch.buf[HASH_LEN]));
                    if want > n {
                        scratch.buf.resize(want, 0);
                        let m = blob.read_at(&mut scratch.buf[n..], off + n)?;
                        scratch.buf.truncate(n + m);
                    }
                }
                scratch.key = Some((block_id, offset));
            }
            Ok(&scratch.buf)
        }
    }
}

/// Parse the record at `ptr` in block `block_id` (located at `loc`).
fn record_in<'s>(
    blobs: &'s mut Option<Blobs>,
    scratch: &'s mut BytesScratch,
    conn: &Connection,
    loc: Location,
    block_id: u32,
    ptr: &Ptr,
) -> Result<Record<'s>, Error> {
    let bytes = trie_bytes(blobs, scratch, conn, loc, block_id, ptr.ptr, MAX_NODE_LEN)?;
    Record::parse(bytes, ptr.kind_id()).map_err(|e| match e {
        Error::CorruptionError(m) => Error::CorruptionError(format!(
            "{m} (node at {} in block {block_id})",
            ptr.ptr
        )),
        e => e,
    })
}

/// Open or create the database at `db_path`.
pub fn open<T: MarfTrieId>(
    db_path: impl AsRef<Path>,
    external_blobs: bool,
    compress: bool,
    readonly: bool,
    unconfirmed: bool,
    force_migrate: bool,
) -> Result<(Connection, Store<T>), Error> {
    let db_path = db_path.as_ref();
    let in_memory = db_path == Path::new(":memory:");
    let mut create = false;
    let flags = if in_memory {
        create = true;
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
    } else {
        match fs::metadata(db_path) {
            Ok(_) if readonly => OpenFlags::SQLITE_OPEN_READ_ONLY,
            Ok(_) => OpenFlags::SQLITE_OPEN_READ_WRITE,
            Err(e) if e.kind() == io::ErrorKind::NotFound && !readonly => {
                create = true;
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(Error::NotFoundError),
            Err(e) => return Err(e.into()),
        }
    };
    let mut conn = sqlite_open(db_path, flags, false)?;
    sql_pragma(&conn, "mmap_size", &SQLITE_MMAP_SIZE)?;
    sql_pragma(&conn, "page_size", &SQLITE_MARF_PAGE_SIZE)?;

    if create {
        let tx = tx_begin_immediate(&mut conn)?;
        tx.execute_batch(SQL_MARF_DATA_TABLE)?;
        tx.execute_batch(SQL_MARF_MINED_TABLE)?;
        tx.execute_batch(SQL_EXTENSION_LOCKS_TABLE)?;
        tx.commit()?;
    }

    let blobs = if external_blobs && !in_memory {
        Some(Blobs::open(db_path, readonly)?)
    } else {
        None
    };

    if readonly {
        let v = schema_version(&conn);
        if v != SQL_MARF_SCHEMA_VERSION {
            return Err(corrupt!(
                "MARF schema version {v} is not compatible with read-only open (expected {SQL_MARF_SCHEMA_VERSION})"
            ));
        }
    } else {
        let first_version = migrate(&mut conn)?;
        if let Some(blobs) = blobs.as_ref() {
            if first_version < SQL_MARF_EXTERNAL_BLOBS_SCHEMA_VERSION || force_migrate {
                export_trie_blobs::<T>(&conn, blobs)?;
            }
        }
        if detect_partial_migration(&conn)? {
            return Err(corrupt!(
                "PARTIAL MIGRATION DETECTED! This is an irrecoverable error. You will need to restart your node from genesis."
            ));
        }
    }

    let prune = read_prune_info(&conn)?;
    let store = Store {
        db_path: db_path.to_path_buf(),
        blobs,
        readonly,
        unconfirmed,
        compress,
        prune,
        ids: HashMap::new(),
        hashes: HashMap::new(),
        locations: HashMap::new(),
        scratch: Scratch::default(),
        sidecar_retired: false,
    };
    Ok((conn, store))
}

/// Bring the schema up to date; returns the version found.
fn migrate(conn: &mut Connection) -> Result<u64, Error> {
    let first = schema_version(conn);
    loop {
        match schema_version(conn) {
            1 => {
                let tx = tx_begin_immediate(conn)?;
                tx.execute_batch(SQL_MARF_DATA_TABLE_SCHEMA_2)?;
                tx.commit()?;
            }
            2 => {
                let tx = tx_begin_immediate(conn)?;
                tx.execute_batch(SQL_MARF_DATA_TABLE_SCHEMA_3)?;
                tx.commit()?;
            }
            SQL_MARF_SCHEMA_VERSION => break,
            SQL_MARF_TYPE_FIRST_SCHEMA_VERSION => {
                return Err(corrupt!(
                    "MARF database uses schema {x} (optimized record format 6: packed \
                     branches and stable-ID values), which this implementation does not \
                     read yet; see docs/marf-rewrite/COMPAT.md",
                    x = SQL_MARF_TYPE_FIRST_SCHEMA_VERSION
                ))
            }
            x => {
                return Err(corrupt!(
                    "Unable to migrate MARF data table: unrecognized schema {x}"
                ))
            }
        }
    }
    if first >= SQL_MARF_EXTERNAL_BLOBS_SCHEMA_VERSION
        && migrated_version(conn) != SQL_MARF_SCHEMA_VERSION
        && !detect_partial_migration(conn)?
    {
        set_migrated(conn)?;
    }
    Ok(first)
}

fn set_migrated(conn: &Connection) -> Result<(), Error> {
    conn.execute(
        "UPDATE migrated_version SET version = ?1",
        params![u64_sql(SQL_MARF_SCHEMA_VERSION)?],
    )?;
    Ok(())
}

/// Either every confirmed trie has been moved to the blobs file, or none has.
fn detect_partial_migration(conn: &Connection) -> Result<bool, Error> {
    if migrated_version(conn) == schema_version(conn) {
        return Ok(false);
    }
    let migrated: i64 = conn.query_row(
        "SELECT COUNT(*) FROM marf_data WHERE external_offset = 0 AND external_length = 0 AND unconfirmed = 0",
        NO_PARAMS,
        |r| r.get(0),
    )?;
    let not_migrated: i64 = conn.query_row(
        "SELECT COUNT(*) FROM marf_data WHERE external_offset != 0 AND external_length != 0 AND unconfirmed = 0",
        NO_PARAMS,
        |r| r.get(0),
    )?;
    Ok(migrated > 0 && not_migrated > 0)
}

/// Move every confirmed trie out of `marf_data.data` into the blobs file
/// (the schema-1 to schema-2 data migration).
fn export_trie_blobs<T: MarfTrieId>(conn: &Connection, blobs: &Blobs) -> Result<(), Error> {
    if detect_partial_migration(conn)? {
        return Err(corrupt!(
            "PARTIAL MIGRATION DETECTED! This is an irrecoverable error."
        ));
    }
    let rows: Vec<(u32, T)> = {
        let mut stmt = conn.prepare(
            "SELECT block_id, block_hash FROM marf_data WHERE unconfirmed = 0 ORDER BY block_id",
        )?;
        let rows = stmt.query_map(NO_PARAMS, |r| Ok((r.get::<_, u32>(0)?, r.get::<_, T>(1)?)))?;
        rows.collect::<Result<_, _>>()?
    };
    let mut offset = external_blobs_length(conn)?;
    for (block_id, bhh) in rows {
        let data: Vec<u8> = conn.query_row(
            "SELECT data FROM marf_data WHERE block_id = ?1",
            params![block_id],
            |r| r.get(0),
        )?;
        if data.is_empty() {
            continue;
        }
        blobs.write_at(offset, &data)?;
        conn.execute(
            "UPDATE marf_data SET block_hash = ?1, data = ?2, unconfirmed = ?3, external_offset = ?4, external_length = ?5 WHERE block_id = ?6",
            params![bhh, &[] as &[u8], 0, u64_sql(offset)?, u64_sql(data.len() as u64)?, block_id],
        )?;
        offset += data.len() as u64;
    }
    conn.execute("VACUUM", NO_PARAMS)?;
    set_migrated(conn)
}

pub(crate) fn read_prune_info(conn: &Connection) -> Result<Option<PruneInfo>, Error> {
    if !table_exists(conn, "marf_squash_info")? {
        return Ok(None);
    }
    let row: Option<(Vec<u8>, Option<Vec<u8>>, i64)> = conn
        .query_row(
            "SELECT archival_marf_root_hash, squash_root_node_hash, squash_height FROM marf_squash_info WHERE id = 1",
            NO_PARAMS,
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((archival, squash_root, height)) = row else {
        return Ok(None);
    };
    let hash = |bytes: &[u8]| {
        TrieHash::from_bytes(bytes).ok_or_else(|| corrupt!("Invalid squash root hash length"))
    };
    let squash_root = squash_root.ok_or_else(|| {
        corrupt!("marf_squash_info row exists but prune_root_node_hash is NULL (squash build did not finalize)")
    })?;
    Ok(Some(PruneInfo {
        archival_marf_root_hash: hash(&archival)?,
        prune_root_node_hash: hash(&squash_root)?,
        prune_height: u32::try_from(height).map_err(|_| corrupt!("Invalid squash height"))?,
    }))
}

fn external_blobs_length(conn: &Connection) -> Result<u64, Error> {
    let len: Option<i64> = conn
        .query_row(
            "SELECT (external_offset + external_length) AS blobs_length FROM marf_data ORDER BY external_offset DESC LIMIT 1",
            NO_PARAMS,
            |r| r.get(0),
        )
        .optional()?;
    Ok(len.unwrap_or(0) as u64)
}

/// See [`Store::node`]; takes the store's fields apart so the result can
/// borrow the record bytes or the cached branch while the caller keeps the
/// rest of the store.
#[allow(clippy::too_many_arguments)]
fn resolve_in<'s>(
    blobs: &'s mut Option<Blobs>,
    bytes: &'s mut BytesScratch,
    diffs: &mut Vec<Ptr>,
    patched: &'s mut HashMap<(u32, u64), Cached>,
    locations: &mut HashMap<u32, Location>,
    conn: &Connection,
    block_id: u32,
    ptr: &Ptr,
) -> Result<Resolved<'s>, Error> {
    let external = blobs.is_some();
    let cur_block = block_id;
    let mut block = block_id;
    let mut ptr = ptr.without_back();
    let key = (block_id, ptr.ptr);
    if patched.len() >= PATCH_CACHE_CAPACITY {
        patched.clear();
    }
    // (two lookups on a hit: a borrow returned from one branch of an `if`
    // would hold `patched` for the rest of the function)
    if patched.contains_key(&key) {
        let c = &patched[&key];
        return Ok(Resolved {
            node: StoreNode::Patched(&c.node),
            hash: c.hash,
            depth: c.depth,
        });
    }
    bytes.key = None;
    diffs.clear();
    // patches met on the way down: (block read from, base pointer, diff range)
    let mut chain: [(u32, Ptr, usize, usize); MAX_PATCH_DEPTH as usize] =
        [(0, Ptr::default(), 0, 0); MAX_PATCH_DEPTH as usize];
    let mut depth = 0usize;
    let mut hash: Option<TrieHash> = None;
    let mut cacheable = true;
    loop {
        let loc = location_in(locations, external, conn, block)?;
        cacheable &= !matches!(loc, Location::Sqlite { unconfirmed: true });
        let rec = record_in(blobs, bytes, conn, loc, block, &ptr)?;
        if hash.is_none() {
            hash = Some(rec.hash());
        }
        if !rec.is_patch() {
            break;
        }
        if depth == MAX_PATCH_DEPTH as usize {
            return Err(Error::NodeTooDeep);
        }
        let patch = rec.patch()?;
        let start = diffs.len();
        patch.for_each_diff(|d| diffs.push(d))?;
        chain[depth] = (block, patch.base, start, diffs.len());
        depth += 1;
        block = patch.base.back_block;
        ptr = patch.base.without_back();
    }
    let hash = hash.expect("set");
    let loc = location_in(locations, external, conn, block)?;
    let rec = record_in(blobs, bytes, conn, loc, block, &ptr)?;
    if depth == 0 {
        return Ok(Resolved {
            node: StoreNode::Record(rec),
            hash,
            depth: 0,
        });
    }
    let view = rec
        .branch()
        .map_err(|_| corrupt!("patch base at {} in block {block} is not a branch", ptr.ptr))?;
    let mut out = Branch::new(view.kind(), NodePath::EMPTY);
    view.fill(&mut out)?;
    // apply base-first: the innermost patch (closest to the base) first
    for (patch_block, base, start, end) in chain[..depth].iter().rev() {
        codec::apply_patch(&mut out, *base, &diffs[*start..*end], *patch_block, cur_block)?;
    }
    let depth = depth as u32;
    // an unconfirmed trie is rewritten in place, so its nodes go to a slot
    // that the next such read overwrites instead of the cache proper
    let key = if cacheable { key } else { UNCONFIRMED_SLOT };
    patched.insert(
        key,
        Cached {
            node: out,
            hash,
            depth,
        },
    );
    let c = &patched[&key];
    Ok(Resolved {
        node: StoreNode::Patched(&c.node),
        hash,
        depth,
    })
}

/// Cache key reserved for nodes of the unconfirmed trie (never a real location).
const UNCONFIRMED_SLOT: (u32, u64) = (u32::MAX, u64::MAX);

impl<T: MarfTrieId> Store<T> {
    /// A read-only store over the same files with fresh caches.
    pub fn reopen_readonly(&self) -> Store<T> {
        Store {
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
        let conn = sqlite_open(&self.db_path, OpenFlags::SQLITE_OPEN_READ_ONLY, false)?;
        sql_pragma(&conn, "mmap_size", &SQLITE_MMAP_SIZE)?;
        sql_pragma(&conn, "page_size", &SQLITE_MARF_PAGE_SIZE)?;
        Ok(conn)
    }

    pub fn prune_height(&self) -> Option<u32> {
        self.prune.as_ref().map(|s| s.prune_height)
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

    // -- block identity ----------------------------------------------------

    /// Storage id of a block, confirmed or unconfirmed.
    pub fn block_id(&mut self, conn: &Connection, bhh: &T) -> Result<u32, Error> {
        if let Some(id) = self.ids.get(bhh) {
            return Ok(*id);
        }
        let row: Option<(u32, i64)> = conn
            .prepare_cached("SELECT block_id, unconfirmed FROM marf_data WHERE block_hash = ?1")?
            .query_row(params![bhh], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        match row {
            None => Err(Error::NotFoundError),
            Some((id, unconfirmed)) => {
                if unconfirmed == 0 {
                    self.ids.insert(bhh.clone(), id);
                    self.hashes.insert(id, bhh.clone());
                }
                Ok(id)
            }
        }
    }

    /// Block identifier for storage id `id`.
    pub fn block_hash(&mut self, conn: &Connection, id: u32) -> Result<&T, Error> {
        if !self.hashes.contains_key(&id) {
            let row: Option<(T, i64)> = conn
                .prepare_cached(
                    "SELECT block_hash, unconfirmed FROM marf_data WHERE block_id = ?1",
                )?
                .query_row(params![id], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?;
            let Some((bhh, unconfirmed)) = row else {
                return Err(Error::NotFoundError);
            };
            if unconfirmed == 0 {
                self.ids.insert(bhh.clone(), id);
            }
            // Unconfirmed rows are cached by id only; their hash never changes
            // while the row exists, and a dropped row is never read again.
            self.hashes.insert(id, bhh);
        }
        Ok(self.hashes.get(&id).expect("just inserted"))
    }

    pub fn confirmed_block_id(&self, conn: &Connection, bhh: &T) -> Result<Option<u32>, Error> {
        if let Some(id) = self.ids.get(bhh) {
            return Ok(Some(*id));
        }
        Ok(conn
            .prepare_cached(
                "SELECT block_id FROM marf_data WHERE block_hash = ?1 AND unconfirmed = 0",
            )?
            .query_row(params![bhh], |r| r.get(0))
            .optional()?)
    }

    pub fn unconfirmed_block_id(&self, conn: &Connection, bhh: &T) -> Result<Option<u32>, Error> {
        Ok(conn
            .prepare_cached(
                "SELECT block_id FROM marf_data WHERE block_hash = ?1 AND unconfirmed = 1",
            )?
            .query_row(params![bhh], |r| r.get(0))
            .optional()?)
    }

    pub fn has_block(&self, conn: &Connection, bhh: &T) -> Result<bool, Error> {
        Ok(self.confirmed_block_id(conn, bhh)?.is_some()
            || self.unconfirmed_block_id(conn, bhh)?.is_some())
    }

    /// Highest confirmed storage id (0 when empty).
    pub fn num_blocks(&self, conn: &Connection) -> Result<u32, Error> {
        Ok(conn
            .prepare_cached("SELECT IFNULL(MAX(block_id), 0) FROM marf_data WHERE unconfirmed = 0")?
            .query_row(NO_PARAMS, |r| r.get(0))?)
    }

    pub fn latest_confirmed_block(&self, conn: &Connection) -> Result<T, Error> {
        Ok(conn.query_row(
            "SELECT block_hash FROM marf_data WHERE unconfirmed = 0 ORDER BY block_id DESC LIMIT 1",
            NO_PARAMS,
            |r| r.get(0),
        )?)
    }

    // -- node reads ----------------------------------------------------------

    fn location(&mut self, conn: &Connection, block_id: u32) -> Result<Location, Error> {
        location_in(&mut self.locations, self.blobs.is_some(), conn, block_id)
    }

    /// Bytes of block `block_id`'s trie from `offset`, at most `max`.
    fn bytes(&mut self, conn: &Connection, block_id: u32, offset: u64, max: usize) -> Result<&[u8], Error> {
        let loc = self.location(conn, block_id)?;
        self.scratch.bytes.key = None;
        trie_bytes(&mut self.blobs, &mut self.scratch.bytes, conn, loc, block_id, offset, max)
    }

    /// Read the whole trie blob of `block_id`.
    pub fn read_trie_blob(&mut self, conn: &Connection, block_id: u32) -> Result<Vec<u8>, Error> {
        match self.location(conn, block_id)? {
            Location::External { offset, len } => {
                let blobs = self.blobs.as_mut().expect("external location implies blobs");
                Ok(blobs.slice(offset, len)?.to_vec())
            }
            Location::Sqlite { .. } => Ok(conn.query_row(
                "SELECT data FROM marf_data WHERE block_id = ?1",
                params![block_id],
                |r| r.get(0),
            )?),
        }
    }

    /// The hash stored in front of the node at `offset`.
    pub fn read_hash(
        &mut self,
        conn: &Connection,
        block_id: u32,
        offset: u64,
    ) -> Result<TrieHash, Error> {
        let bytes = self.bytes(conn, block_id, offset, HASH_LEN)?;
        TrieHash::from_bytes(bytes).ok_or_else(|| corrupt!("Failed to read hash in full"))
    }

    pub fn read_root_hash(&mut self, conn: &Connection, block_id: u32) -> Result<TrieHash, Error> {
        self.read_hash(conn, block_id, ROOT_OFFSET)
    }

    /// The parent block id recorded in the trie blob header of `block_id`.
    pub fn read_parent(&mut self, conn: &Connection, block_id: u32) -> Result<T, Error> {
        let bytes = self.bytes(conn, block_id, 0, 32)?;
        let buf: [u8; 32] = bytes
            .try_into()
            .map_err(|_| corrupt!("Failed to read parent hash in full"))?;
        Ok(T::from_bytes(buf))
    }

    pub fn confirmed_blocks(&self, conn: &Connection) -> Result<Vec<(u32, T)>, Error> {
        let mut stmt = conn.prepare(
            "SELECT block_id, block_hash FROM marf_data WHERE unconfirmed = 0 ORDER BY block_id",
        )?;
        let rows = stmt.query_map(NO_PARAMS, |r| Ok((r.get::<_, u32>(0)?, r.get::<_, T>(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Insert an empty placeholder row for `bhh`; returns its storage id.
    pub fn insert_placeholder(&mut self, conn: &Connection, bhh: &T) -> Result<u32, Error> {
        let id = conn
            .prepare_cached(
                "INSERT INTO marf_data (block_hash, data, unconfirmed, external_offset, external_length) VALUES (?1, ?2, 0, 0, 0)",
            )?
            .insert(params![bhh, &[] as &[u8]])?;
        let id = u32::try_from(id).map_err(|_| Error::OverflowError)?;
        self.ids.insert(bhh.clone(), id);
        self.hashes.insert(id, bhh.clone());
        Ok(id)
    }

    /// Write a pruned MARF's shared trie blob at the end of the blobs file
    /// and point every confirmed `marf_data` row at it. `write(blobs, base)`
    /// produces the blob at file offset `base`, in as many positioned writes
    /// as it likes, and returns the blob's length; the file is synced once
    /// afterwards. Returns `base`.
    pub fn write_shared_blob_with(
        &mut self,
        conn: &Connection,
        write: impl FnOnce(&Blobs, u64) -> Result<u64, Error>,
    ) -> Result<u64, Error> {
        let blobs = self
            .blobs
            .as_ref()
            .ok_or_else(|| corrupt!("prune destination requires external blobs"))?;
        let base = external_blobs_length(conn)?;
        let len = write(blobs, base)?;
        blobs.sync()?;
        conn.execute(
            "UPDATE marf_data SET external_offset = ?1, external_length = ?2 WHERE unconfirmed = 0",
            params![u64_sql(base)?, u64_sql(len)?],
        )?;
        self.locations.clear();
        self.scratch.patched.clear();
        Ok(base)
    }

    /// Write the squash side tables.
    pub fn write_prune_metadata(
        &mut self,
        conn: &Connection,
        archival_root: &TrieHash,
        prune_root_node_hash: &TrieHash,
        prune_height: u32,
        blocks: &[(u32, T, TrieHash)],
    ) -> Result<(), Error> {
        conn.execute(
            "INSERT OR REPLACE INTO marf_squash_info (id, archival_marf_root_hash, squash_root_node_hash, squash_height) VALUES (1, ?1, ?2, ?3)",
            params![archival_root.as_bytes().to_vec(), prune_root_node_hash.as_bytes().to_vec(), i64::from(prune_height)],
        )?;
        let mut stmt = conn.prepare("INSERT INTO marf_squashed_blocks (height, block_hash, marf_root_hash) VALUES (?1, ?2, ?3)")?;
        for (height, bhh, root) in blocks {
            stmt.execute(params![i64::from(*height), bhh.as_bytes(), root.as_bytes()])?;
        }
        self.prune = Some(PruneInfo {
            archival_marf_root_hash: *archival_root,
            prune_root_node_hash: *prune_root_node_hash,
            prune_height,
        });
        Ok(())
    }

    /// All `(height, block_hash, archival_root_hash)` rows of the squash side table.
    pub fn pruned_blocks(&self, conn: &Connection) -> Result<Vec<(u32, T, TrieHash)>, Error> {
        let mut stmt = conn.prepare(
            "SELECT height, block_hash, marf_root_hash FROM marf_squashed_blocks ORDER BY height",
        )?;
        let rows = stmt.query_map(NO_PARAMS, |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Vec<u8>>(1)?,
                r.get::<_, Vec<u8>>(2)?,
            ))
        })?;
        let mut out = vec![];
        for row in rows {
            let (h, bh, root) = row?;
            let h = u32::try_from(h).map_err(|_| corrupt!("Invalid squash block height"))?;
            let arr: [u8; 32] = bh
                .as_slice()
                .try_into()
                .map_err(|_| corrupt!("Invalid squash block_hash length"))?;
            let root = TrieHash::from_bytes(&root)
                .ok_or_else(|| corrupt!("Invalid squash root hash length"))?;
            out.push((h, T::from_bytes(arr), root));
        }
        Ok(out)
    }

    /// Read the record at `ptr` in `block_id`: either a node or a patch.
    /// Resolve the node at `ptr` in `block_id` without materializing it:
    /// a plain record is borrowed from storage; a patch chain is followed to
    /// its base (at most [`MAX_PATCH_DEPTH`] hops) and applied into the
    /// store's scratch branch. Nothing is allocated once the scratch buffers
    /// have grown to their working size.
    pub fn node(&mut self, conn: &Connection, block_id: u32, ptr: &Ptr) -> Result<Resolved<'_>, Error> {
        resolve_in(
            &mut self.blobs,
            &mut self.scratch.bytes,
            &mut self.scratch.diffs,
            &mut self.scratch.patched,
            &mut self.locations,
            conn,
            block_id,
            ptr,
        )
    }

    /// Like [`Store::node`], but always as a `Branch` (decoded into scratch
    /// when the record is a plain branch); `None` for a leaf.
    pub fn branch(&mut self, conn: &Connection, block_id: u32, ptr: &Ptr) -> Result<Option<(&Branch, u32)>, Error> {
        let r = resolve_in(
            &mut self.blobs,
            &mut self.scratch.bytes,
            &mut self.scratch.diffs,
            &mut self.scratch.patched,
            &mut self.locations,
            conn,
            block_id,
            ptr,
        )?;
        match r.node {
            StoreNode::Patched(b) => Ok(Some((b, r.depth))),
            StoreNode::Record(rec) => match rec.kind()? {
                RecordKind::Leaf => Ok(None),
                RecordKind::Patch => Err(corrupt!("unresolved patch")),
                RecordKind::Branch(_) => {
                    let out = self
                        .scratch
                        .decoded
                        .get_or_insert_with(|| Branch::new(Kind::Node256, NodePath::EMPTY));
                    rec.branch()?.fill(out)?;
                    Ok(Some((out, 0)))
                }
            },
        }
    }

    /// Read the node at `ptr` in `block_id` as an owned value, resolving
    /// patch chains. Returns the node, its stored hash, and the patch depth.
    pub fn read_node(
        &mut self,
        conn: &Connection,
        block_id: u32,
        ptr: &Ptr,
    ) -> Result<(Node, TrieHash, u32), Error> {
        let r = self.node(conn, block_id, ptr)?;
        Ok((r.node.to_node()?, r.hash, r.depth))
    }

    /// Fetch the target of a back-pointer, for copy-on-write.
    pub fn fetch(&mut self, conn: &Connection, ptr: &Ptr) -> Result<Fetched, Error> {
        let (node, _, depth) = self.read_node(conn, ptr.back_block, ptr)?;
        Ok(Fetched { node, depth })
    }

    // -- writes ----------------------------------------------------------------

    /// Persist a confirmed trie under `bhh`; returns its storage id.
    pub fn write_confirmed(
        &mut self,
        conn: &Connection,
        bhh: &T,
        blob: &[u8],
    ) -> Result<u32, Error> {
        self.retire_direct_hash_sidecar(conn)?;
        let block_id: i64 = match self.blobs.as_ref() {
            Some(blobs) => {
                let offset = external_blobs_length(conn)?;
                blobs.write_at(offset, blob)?;
                conn.prepare_cached(
                    "INSERT INTO marf_data (block_hash, data, unconfirmed, external_offset, external_length) VALUES (?1, ?2, 0, ?3, ?4)",
                )?
                .insert(params![bhh, &[] as &[u8], u64_sql(offset)?, u64_sql(blob.len() as u64)?])?
            }
            None => conn
                .prepare_cached(
                    "INSERT INTO marf_data (block_hash, data, unconfirmed, external_offset, external_length) VALUES (?1, ?2, 0, 0, 0)",
                )?
                .insert(params![bhh, blob])?,
        };
        let block_id = u32::try_from(block_id).map_err(|_| Error::OverflowError)?;
        self.ids.insert(bhh.clone(), block_id);
        self.hashes.insert(block_id, bhh.clone());
        self.locations.remove(&block_id);
        Ok(block_id)
    }

    /// Persist (or overwrite) an unconfirmed trie, always inside SQLite.
    pub fn write_unconfirmed(
        &mut self,
        conn: &Connection,
        bhh: &T,
        blob: &[u8],
    ) -> Result<u32, Error> {
        self.retire_direct_hash_sidecar(conn)?;
        if self.confirmed_block_id(conn, bhh)?.is_some() {
            return Err(corrupt!("tried to overwrite confirmed MARF trie {bhh}"));
        }
        let id = match self.unconfirmed_block_id(conn, bhh)? {
            Some(id) => {
                conn.execute(
                    "UPDATE marf_data SET data = ?1 WHERE block_id = ?2",
                    params![blob, id],
                )?;
                id
            }
            None => {
                let id = conn
                    .prepare_cached(
                        "INSERT INTO marf_data (block_hash, data, unconfirmed, external_offset, external_length) VALUES (?1, ?2, 1, 0, 0)",
                    )?
                    .insert(params![bhh, blob])?;
                u32::try_from(id).map_err(|_| Error::OverflowError)?
            }
        };
        self.locations.remove(&id);
        Ok(id)
    }

    /// Persist a mined (never-chained) trie.
    pub fn write_mined(&mut self, conn: &Connection, bhh: &T, blob: &[u8]) -> Result<(), Error> {
        self.retire_direct_hash_sidecar(conn)?;
        let existing: Option<u32> = conn
            .query_row(
                "SELECT block_id FROM mined_blocks WHERE block_hash = ?1",
                params![bhh],
                |r| r.get(0),
            )
            .optional()?;
        match existing {
            Some(id) => conn.execute(
                "UPDATE mined_blocks SET data = ?1 WHERE block_id = ?2",
                params![blob, id],
            )?,
            None => conn.execute(
                "INSERT INTO mined_blocks (block_hash, data) VALUES (?1, ?2)",
                params![bhh, blob],
            )?,
        };
        Ok(())
    }

    pub fn drop_unconfirmed(&mut self, conn: &Connection, bhh: &T) -> Result<(), Error> {
        if let Some(id) = self.unconfirmed_block_id(conn, bhh)? {
            self.hashes.remove(&id);
            self.locations.remove(&id);
        }
        conn.execute(
            "DELETE FROM marf_data WHERE block_hash = ?1 AND unconfirmed = 1",
            params![bhh],
        )?;
        Ok(())
    }

    /// Take the extension lock for `bhh`; `false` if it is already taken (or,
    /// for confirmed tries, already stored).
    pub fn lock(&self, conn: &Connection, bhh: &T, unconfirmed: bool) -> Result<bool, Error> {
        if !unconfirmed {
            let stored = conn
                .query_row(
                    "SELECT 1 FROM marf_data WHERE block_hash = ?1 LIMIT 1",
                    params![bhh],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if stored {
                return Ok(false);
            }
        }
        let locked = conn
            .query_row(
                "SELECT 1 FROM block_extension_locks WHERE block_hash = ?1 LIMIT 1",
                params![bhh],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if locked {
            return Ok(false);
        }
        conn.execute(
            "INSERT INTO block_extension_locks (block_hash) VALUES (?1)",
            params![bhh],
        )?;
        Ok(true)
    }

    pub fn unlock(&self, conn: &Connection, bhh: &T) -> Result<(), Error> {
        conn.execute(
            "DELETE FROM block_extension_locks WHERE block_hash = ?1",
            params![bhh],
        )?;
        Ok(())
    }

    // -- squash side tables -------------------------------------------------------

    pub fn pruned_block_height(&self, conn: &Connection, bhh: &T) -> Result<Option<u32>, Error> {
        if self.prune.is_none() {
            return Ok(None);
        }
        let h: Option<i64> = conn
            .prepare_cached("SELECT height FROM marf_squashed_blocks WHERE block_hash = ?1")?
            .query_row(params![bhh.as_bytes()], |r| r.get(0))
            .optional()?;
        h.map(|h| u32::try_from(h).map_err(|_| corrupt!("Invalid squash block height")))
            .transpose()
    }

    pub fn pruned_block_root_hash(
        &self,
        conn: &Connection,
        bhh: &T,
    ) -> Result<Option<TrieHash>, Error> {
        if self.prune.is_none() {
            return Ok(None);
        }
        let bytes: Option<Vec<u8>> = conn
            .prepare_cached(
                "SELECT marf_root_hash FROM marf_squashed_blocks WHERE block_hash = ?1",
            )?
            .query_row(params![bhh.as_bytes()], |r| r.get(0))
            .optional()?;
        bytes
            .map(|b| {
                TrieHash::from_bytes(&b).ok_or_else(|| corrupt!("Invalid squash root hash length"))
            })
            .transpose()
    }

    pub fn pruned_root_hash_at_height(
        &self,
        conn: &Connection,
        height: u32,
    ) -> Result<Option<TrieHash>, Error> {
        let bytes: Option<Vec<u8>> = conn
            .prepare_cached("SELECT marf_root_hash FROM marf_squashed_blocks WHERE height = ?1")?
            .query_row(params![i64::from(height)], |r| r.get(0))
            .optional()?;
        bytes
            .map(|b| {
                TrieHash::from_bytes(&b).ok_or_else(|| corrupt!("Invalid squash root hash length"))
            })
            .transpose()
    }

    pub fn pruned_block_at_height(
        &self,
        conn: &Connection,
        height: u32,
    ) -> Result<Option<T>, Error> {
        let bytes: Option<Vec<u8>> = conn
            .prepare_cached("SELECT block_hash FROM marf_squashed_blocks WHERE height = ?1")?
            .query_row(params![i64::from(height)], |r| r.get(0))
            .optional()?;
        bytes
            .map(|b| {
                let arr: [u8; 32] = b.as_slice().try_into().map_err(|_| {
                    corrupt!(
                        "Invalid squash block_hash length {} at height {height}",
                        b.len()
                    )
                })?;
                Ok(T::from_bytes(arr))
            })
            .transpose()
    }

    /// Reject trie traversal below the squash height, where blocks share the
    /// squash blob and have no per-block state.
    pub fn check_historical_read_allowed(&self, conn: &Connection, bhh: &T) -> Result<(), Error> {
        let Some(prune_height) = self.prune_height() else {
            return Ok(());
        };
        let Some(block_height) = self.pruned_block_height(conn, bhh)? else {
            return Ok(());
        };
        if block_height < prune_height {
            return Err(Error::HistoricalReadInPrunedRange {
                block_height,
                prune_height,
            });
        }
        Ok(())
    }
}

/// Remove all extension locks (recovery after a crash).
pub fn clear_locks(db_path: &Path) -> Result<(), Error> {
    let conn = sqlite_open(db_path, OpenFlags::SQLITE_OPEN_READ_WRITE, false)?;
    conn.execute("DELETE FROM block_extension_locks", NO_PARAMS)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use stacks_common::types::chainstate::StacksBlockId;

    use super::*;
    use crate::marf::{MARFOpenOpts, MarfConnection, MARF};
    use crate::{ClarityMarfTrieId, MARFValue};

    fn sidecar_valid(marf: &MARF<StacksBlockId>) -> i64 {
        marf.sqlite_conn()
            .query_row(
                "SELECT valid FROM marf_direct_hash_index WHERE id = 1",
                NO_PARAMS,
                |r| r.get(0),
            )
            .unwrap()
    }

    /// The direct-hash sidecar is retired inside the first writing
    /// transaction. If that transaction rolls back, the retirement rolls back
    /// with it, and the next write must issue it again (review finding).
    #[test]
    fn sidecar_retirement_is_redone_after_a_rolled_back_commit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("marf.sqlite");
        let mut marf = MARF::<StacksBlockId>::from_path(&path, MARFOpenOpts::default()).unwrap();
        marf.sqlite_conn()
            .execute_batch(
                "CREATE TABLE marf_direct_hash_index (id INTEGER PRIMARY KEY, valid INTEGER NOT NULL);
                 INSERT INTO marf_direct_hash_index (id, valid) VALUES (1, 1);",
            )
            .unwrap();
        let b1 = StacksBlockId([1u8; 32]);
        let value = MARFValue([7u8; 40]);

        // first commit fails after the retirement: a row for the block
        // appears underneath it, so the trie insert hits the UNIQUE index
        let mut tx = marf.begin_tx().unwrap();
        tx.begin(&StacksBlockId::sentinel(), &b1).unwrap();
        tx.insert("k", value.clone()).unwrap();
        tx.sqlite_tx()
            .execute(
                "INSERT INTO marf_data (block_hash, data, unconfirmed) VALUES (?1, ?2, 0)",
                params![&b1, &[] as &[u8]],
            )
            .unwrap();
        assert!(tx.commit().is_err());
        assert_eq!(sidecar_valid(&marf), 1, "the failed commit rolled the retirement back");

        // the next commit succeeds and must retire the sidecar
        let mut tx = marf.begin_tx().unwrap();
        tx.begin(&StacksBlockId::sentinel(), &b1).unwrap();
        tx.insert("k", value).unwrap();
        tx.commit().unwrap();
        assert_eq!(sidecar_valid(&marf), 0);
    }
}
