//! Experimental direct-addressed hashes with commit-published immutable slots. DO NOT RELEASE.
//!
//! A durable dense prefix is mapped; a bounded committed tail is recovered from the MARF.
//! Sidecar checkpoints precede SQL publication. Gaps and unconfirmed IDs retain fallback.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection, OptionalExtension};

use super::ancestry::{self, Ancestry};
use super::blob_layout::{BlobHeader, MAX_READER_PREFIX_LEN};
use super::record::NodeRecordFormat;
use super::{trie_sql, ClarityMarfTrieId, Error, FileMapping, MarfTrieId};
use crate::types::chainstate::{StacksBlockId, TrieHash};
use crate::util::hash::to_hex;

/// Fixed header; one unused ID-zero slot follows it.
const HEADER: usize = 4096;
/// Exact hashes plus a compact fork-specific ancestry extension.
const SLOT: usize = 64 + ancestry::ENCODED_SIZE;
/// Existing v2 files retain their original record width and publication rules.
const LEGACY_SLOT: usize = 64;
/// Version two has a growing dense prefix and no immutable maximum in its header.
const MAGIC: &[u8; 8] = b"MARFDH03";
/// Previous hash-only sidecar format.
const LEGACY_MAGIC: &[u8; 8] = b"MARFDH02";
/// At most 256 slots (20 KiB in v3) per durable checkpoint.
const CHECKPOINT_SLOTS: u32 = 256;

/// Immutable slots newer than the durable prefix, shared by transaction snapshots.
#[derive(Clone, Default)]
struct Tail {
    /// ID immediately before the first record.
    base: u32,
    /// Contiguous records; never populated from an unsynced sidecar suffix.
    slots: Arc<Vec<[u8; SLOT]>>,
}

impl Tail {
    /// Last contained ID, or the base for an empty tail.
    fn end(&self) -> u32 {
        self.base + self.slots.len() as u32
    }

    /// Copy an exact tail record if this tail contains it.
    fn slot(&self, id: u32) -> Option<[u8; SLOT]> {
        self.slots
            .get(id.checked_sub(self.base)?.checked_sub(1)? as usize)
            .copied()
    }

    /// Restrict a shared tail to a snapshot's durable and live bounds.
    fn bounded(&self, durable: u32, live: u32) -> Option<Self> {
        if durable == live {
            return Some(Self {
                base: durable,
                slots: Arc::default(),
            });
        }
        if self.base == durable && self.end() == live {
            return Some(self.clone());
        }
        if self.base > durable || self.end() < live {
            return None;
        }
        Some(Self {
            base: durable,
            slots: Arc::new(
                self.slots[(durable - self.base) as usize..(live - self.base) as usize].to_vec(),
            ),
        })
    }
}

/// Transaction-local publication candidate, admitted to shared memory only after SQL commit.
struct Pending {
    /// Current SQL durable boundary.
    durable: u32,
    /// Current SQL live boundary.
    live: u32,
    /// Records beyond the durable boundary.
    tail: Tail,
}

/// Whether the optional committed-tail metadata exists in this SQL snapshot.
fn has_live_table(db: &Connection) -> Result<bool, Error> {
    Ok(db.prepare_cached("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='marf_direct_hash_live')")?.query_row([], |r| r.get(0))?)
}

/// Read both boundaries together from the caller's SQL snapshot.
fn publication(db: &Connection) -> Result<Option<(Vec<u8>, u32, u32)>, Error> {
    let sql = if has_live_table(db)? {
        "SELECT d.token,d.max_id,MAX(d.max_id,COALESCE(l.max_id,d.max_id)) FROM marf_direct_hash_index d LEFT JOIN marf_direct_hash_live l ON l.singleton=d.singleton AND l.token=d.token WHERE d.singleton=1 AND d.valid=1"
    } else {
        "SELECT token,max_id,max_id FROM marf_direct_hash_index WHERE singleton=1 AND valid=1"
    };
    Ok(db
        .prepare_cached(sql)?
        .query_row([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?)
}

/// Install backward-compatible tail metadata inside the existing writer transaction.
fn ensure_live_table(db: &Connection) -> Result<(), Error> {
    if has_live_table(db)? {
        return Ok(());
    }
    db.execute_batch("CREATE TABLE marf_direct_hash_live(singleton INTEGER PRIMARY KEY CHECK(singleton=1),token BLOB NOT NULL CHECK(length(token)=16),max_id INTEGER NOT NULL);
        CREATE TRIGGER direct_hash_live_update BEFORE UPDATE ON marf_data WHEN OLD.block_id<=(SELECT l.max_id FROM marf_direct_hash_live l JOIN marf_direct_hash_index d ON l.token=d.token WHERE d.singleton=1 AND d.valid=1) OR NEW.block_id<=(SELECT l.max_id FROM marf_direct_hash_live l JOIN marf_direct_hash_index d ON l.token=d.token WHERE d.singleton=1 AND d.valid=1) BEGIN SELECT RAISE(ABORT,'immutable live hash prefix: reset/rebuild before mutation'); END;
        CREATE TRIGGER direct_hash_live_delete BEFORE DELETE ON marf_data WHEN OLD.block_id<=(SELECT l.max_id FROM marf_direct_hash_live l JOIN marf_direct_hash_index d ON l.token=d.token WHERE d.singleton=1 AND d.valid=1) BEGIN SELECT RAISE(ABORT,'immutable live hash prefix: reset/rebuild before deletion'); END;
        CREATE TRIGGER direct_hash_live_insert BEFORE INSERT ON marf_data WHEN NEW.block_id>0 AND NEW.block_id<=(SELECT l.max_id FROM marf_direct_hash_live l JOIN marf_direct_hash_index d ON l.token=d.token WHERE d.singleton=1 AND d.valid=1) BEGIN SELECT RAISE(ABORT,'immutable live hash prefix: cannot reuse committed ID'); END;")?;
    Ok(())
}

/// Reconstruct only the bounded tail from authoritative SQL and trie root envelopes.
fn recover_tail(
    db: &Connection,
    path: &Path,
    durable: u32,
    live: u32,
    slot_size: usize,
    token: &[u8],
) -> Result<Tail, Error> {
    if live < durable || live - durable >= CHECKPOINT_SLOTS {
        return Err(io::Error::other("invalid direct hash checkpoint bounds").into());
    }
    let mut tail = Tail {
        base: durable,
        slots: Arc::default(),
    };
    if durable == live {
        return Ok(tail);
    }
    #[cfg(feature = "commit-residency-diagnostics")]
    let _recovery = stacks_profiler::diagnostic_span!("Direct hash: Recover committed tail");
    let format = NodeRecordFormat::from_database(db)?;
    let size = format.reader_prefix_len();
    let mut blobs = None;
    let mut ancestry_file = (slot_size == SLOT)
        .then(|| File::open(sidecar_path(path, token)))
        .transpose()?;
    let mut statement = db.prepare_cached("SELECT block_id,block_hash,external_offset,external_length,substr(data,1,?3),unconfirmed FROM marf_data WHERE block_id>?1 AND block_id<=?2 ORDER BY block_id")?;
    let mut rows = statement.query(params![durable, live, size])?;
    while let Some(row) = rows.next()? {
        let id: u32 = row.get(0)?;
        if id != tail.end() + 1 || row.get::<_, bool>(5)? {
            return Err(io::Error::other("noncontiguous committed hash tail").into());
        }
        let block: StacksBlockId = row.get(1)?;
        let offset: u64 = row.get(2)?;
        let length: u64 = row.get(3)?;
        let mut prefix = [0; MAX_READER_PREFIX_LEN];
        if length > 0 {
            if length < size as u64 {
                return Err(io::Error::other("truncated external trie").into());
            }
            if blobs.is_none() {
                blobs = Some(File::open(format!("{}.blobs", path.display()))?);
            }
            let file = blobs.as_mut().expect("opened above");
            file.seek(SeekFrom::Start(offset))?;
            file.read_exact(&mut prefix[..size])?;
        } else {
            let bytes: Vec<u8> = row.get(4)?;
            prefix[..size].copy_from_slice(
                bytes
                    .get(..size)
                    .ok_or_else(|| io::Error::other("truncated inline trie"))?,
            );
        }
        let header = BlobHeader::<StacksBlockId>::parse_format(format, &prefix[..size])?;
        let mut slot = [0; SLOT];
        slot[..32].copy_from_slice(block.as_bytes());
        slot[32..64].copy_from_slice(header.root_hash.as_ref());
        if let Some(file) = &mut ancestry_file {
            if let Some(links) =
                recover_links(db, file, durable, &tail, id, &header.parent_hash, None)?
            {
                slot[64..].copy_from_slice(&links.encode());
            }
        }
        Arc::make_mut(&mut tail.slots).push(slot);
    }
    if tail.end() != live {
        return Err(io::Error::other("missing committed hash tail").into());
    }
    Ok(tail)
}

/// Immutable read view; the partial mapped-page suffix is copied once per publication.
#[derive(Clone)]
struct View {
    /// Shared complete-page mapping, extended only after SQL publication.
    map: FileMapping,
    /// Copied published bytes after the mapped prefix, at most one host page.
    tail: Arc<[u8]>,
    /// Start of the copied suffix; independent of later shared mapping extension.
    tail_start: usize,
    /// Largest slot this particular SQLite snapshot may read.
    max_id: u32,
    /// Width fixed by this immutable file generation.
    slot_size: usize,
}

impl View {
    /// Create a read view bounded by committed SQL metadata, never physical EOF.
    fn load(
        file: &mut File,
        max_id: u32,
        previous: Option<&Self>,
        slot_size: usize,
    ) -> Result<Self, Error> {
        let end = slot_end_for(max_id, slot_size);
        if end > file.metadata()?.len() {
            return Err(io::Error::other("truncated direct hash prefix").into());
        }
        let map = if let Some(previous) = previous {
            let mut map = previous.map.clone();
            // SAFETY: Only previously unpublished pages become readable; published slots are immutable.
            unsafe { map.refresh_prefix(file, end)? };
            map
        } else {
            // SAFETY: Committed prefix bytes are immutable and never truncated by this implementation.
            unsafe { FileMapping::map_prefix(file, end)? }
        };
        let tail_start = map.len();
        let mut tail = vec![0; end as usize - tail_start];
        file.seek(SeekFrom::Start(tail_start as u64))?;
        file.read_exact(&mut tail)?;
        Ok(Self {
            map,
            tail: tail.into(),
            tail_start,
            max_id,
            slot_size,
        })
    }

    /// Copy a fixed record while respecting the view's publication and original tail boundary.
    fn slot(&self, id: u32) -> Option<[u8; SLOT]> {
        if id == 0 || id > self.max_id {
            return None;
        }
        let start = HEADER + id as usize * self.slot_size;
        let mut slot = [0; SLOT];
        let prefix = self.tail_start.saturating_sub(start).min(self.slot_size);
        if prefix > 0 {
            slot[..prefix].copy_from_slice(self.map.get(start..start + prefix)?);
        }
        if prefix < self.slot_size {
            let local = (start + prefix).checked_sub(self.tail_start)?;
            slot[prefix..self.slot_size]
                .copy_from_slice(self.tail.get(local..local + self.slot_size - prefix)?);
        }
        Some(slot)
    }
}

/// Serialized file extension and most recent committed mapping, shared by related reopens.
struct FileState {
    /// Positioned under this mutex; read-only opens cannot publish.
    file: File,
    /// Latest committed view, retained across transactions and related reopens.
    view: View,
    /// Latest committed in-memory suffix; never contains speculative writes.
    tail: Tail,
}

/// One immutable file generation; resets and rebuilds always use a different filename.
struct HashFile {
    /// Initial boundary used only to attribute later online hits in diagnostic builds.
    #[cfg(feature = "commit-residency-diagnostics")]
    initial_max: u32,
    /// Publication identity in both the file header and SQLite.
    token: Vec<u8>,
    /// Record width selected by the immutable file header.
    slot_size: usize,
    /// Only transaction begin and append take this lock; lookups use their local view.
    state: Mutex<FileState>,
}

/// Managed transaction identity and lifetime, without mutation-counter checks.
struct Session {
    /// Connection identity; a different connection must acquire its own snapshot.
    connection: usize,
    /// Revoked on explicit reset and all transaction exits, including implicit rollback.
    active: AtomicBool,
    /// Generation owner for post-commit admission.
    file: Arc<HashFile>,
    /// Path used when reconciling a savepoint rollback.
    path: PathBuf,
    /// Speculative suffix; dropped on rollback, including implicit rollback.
    pending: Mutex<Option<Pending>>,
}

/// Optional dense-prefix index, with shared mapping and transaction-local read boundaries.
#[derive(Clone)]
pub struct DirectHashIndex {
    /// Database pathname used to acquire a fresh generation after reset or rebuild.
    db_path: PathBuf,
    /// Shared generation and append state.
    file: Arc<HashFile>,
    /// Snapshot's immutable read view, absent if the generation no longer matches.
    view: Option<View>,
    /// Related clones can use the same gate only on the same connection.
    session: Option<Arc<Session>>,
    /// Committed non-durable records visible in this transaction snapshot.
    tail: Tail,
}

/// Revokes direct reads when a managed transaction finishes.
pub struct DirectHashGuard(Arc<Session>);

impl DirectHashGuard {
    /// Reconcile savepoint rollback before committing the surrounding SQL transaction.
    pub fn prepare_commit(&self, db: &Connection) -> Result<(), Error> {
        let mut pending = self
            .0
            .pending
            .lock()
            .map_err(|_| io::Error::other("direct hash pending lock poisoned"))?;
        if pending.is_none() {
            return Ok(());
        }
        let Some((token, durable, live)) = publication(db)? else {
            *pending = None;
            return Ok(());
        };
        if token != self.0.file.token || !self.0.active.load(Ordering::Relaxed) {
            *pending = None;
            return Ok(());
        }
        let item = pending.as_mut().expect("checked above");
        if (item.durable, item.live) != (durable, live) {
            *item = Pending {
                durable,
                live,
                tail: recover_tail(
                    db,
                    &self.0.path,
                    durable,
                    live,
                    self.0.file.slot_size,
                    &token,
                )?,
            };
        }
        Ok(())
    }

    /// Admit prepared records only after the authoritative SQL commit succeeded.
    pub fn succeed(self) {
        self.0.active.store(false, Ordering::Relaxed);
        if let Ok(mut pending) = self.0.pending.lock() {
            if let Some(item) = pending.take() {
                if let Ok(mut state) = self.0.file.state.lock() {
                    if item.live >= state.tail.end() {
                        state.tail = item.tail;
                    }
                }
            }
        }
    }
}

impl Drop for DirectHashGuard {
    fn drop(&mut self) {
        self.0.active.store(false, Ordering::Relaxed);
    }
}

/// Identity of a live SQLite connection, never dereferenced by this index.
fn identity(db: &Connection) -> usize {
    // SAFETY: Obtaining the borrowed connection's handle neither mutates nor retains it.
    unsafe { db.handle() as usize }
}

/// Byte length through the given slot, including the unused slot zero.
fn slot_end(id: u32) -> u64 {
    slot_end_for(id, SLOT)
}

/// Committed end for either supported record width.
fn slot_end_for(id: u32, slot_size: usize) -> u64 {
    HEADER as u64 + (u64::from(id) + 1) * slot_size as u64
}

/// Token-addressed filename; old generations are retained for existing readers.
fn sidecar_path(db_path: &Path, token: &[u8]) -> PathBuf {
    PathBuf::from(format!("{}.hashes-{}", db_path.display(), to_hex(token)))
}

impl DirectHashIndex {
    /// Load an optional v2 publication, falling back for missing, old, or truncated files.
    pub fn open(db: &Connection, db_path: &Path) -> Result<Option<Self>, Error> {
        let exists: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='marf_direct_hash_index')", [], |r| r.get(0))?;
        if !exists {
            return Ok(None);
        }
        let row = db
            .query_row(
                "SELECT token,max_id FROM marf_direct_hash_index WHERE singleton=1 AND valid=1",
                [],
                |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, u32>(1)?)),
            )
            .optional()?;
        let Some((token, max_id)) = row else {
            return Ok(None);
        };
        if token.len() != 16 {
            return Ok(None);
        }
        let path = sidecar_path(db_path, &token);
        let mut file = match OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .or_else(|_| File::open(&path))
        {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if file.metadata()?.len() < 24 {
            return Ok(None);
        }
        let mut header = [0; 24];
        file.read_exact(&mut header)?;
        let slot_size = if &header[..8] == MAGIC {
            SLOT
        } else if &header[..8] == LEGACY_MAGIC {
            LEGACY_SLOT
        } else {
            return Ok(None);
        };
        if header[8..24] != token || file.metadata()?.len() < slot_end_for(max_id, slot_size) {
            return Ok(None);
        }
        let view = View::load(&mut file, max_id, None, slot_size)?;
        Ok(Some(Self {
            db_path: db_path.to_path_buf(),
            file: Arc::new(HashFile {
                #[cfg(feature = "commit-residency-diagnostics")]
                initial_max: max_id,
                token,
                slot_size,
                state: Mutex::new(FileState {
                    file,
                    view,
                    tail: Tail {
                        base: max_id,
                        slots: Arc::default(),
                    },
                }),
            }),
            view: None,
            session: None,
            tail: Tail::default(),
        }))
    }

    /// Bind to a newly started managed transaction, before any writes or nested savepoints.
    pub fn begin(&mut self, db: &Connection) -> Result<DirectHashGuard, Error> {
        #[cfg(feature = "commit-residency-diagnostics")]
        let _span = stacks_profiler::diagnostic_span!("Direct hash: Publication check");
        let publication = publication(db)?;
        self.view = None;
        self.tail = Tail::default();
        if let Some((token, max_id, live)) = publication {
            if token != self.file.token {
                if let Some(replacement) = Self::open(db, &self.db_path)? {
                    self.file = replacement.file;
                }
            }
            if token == self.file.token {
                let mut state = self
                    .file
                    .state
                    .lock()
                    .map_err(|_| io::Error::other("direct hash lock poisoned"))?;
                if max_id > state.view.max_id {
                    let previous = state.view.clone();
                    state.view = View::load(
                        &mut state.file,
                        max_id,
                        Some(&previous),
                        self.file.slot_size,
                    )?;
                }
                let mut view = state.view.clone();
                view.max_id = max_id;
                self.view = Some(view);
                self.tail = match state.tail.bounded(max_id, live) {
                    Some(tail) => {
                        #[cfg(feature = "commit-residency-diagnostics")]
                        let _hit =
                            stacks_profiler::diagnostic_span!("Direct hash: Shared tail view hit");
                        tail
                    }
                    None => {
                        recover_tail(db, &self.db_path, max_id, live, self.file.slot_size, &token)?
                    }
                };
                if live >= state.tail.end() {
                    state.tail = self.tail.clone();
                }
            }
        }
        let session = Arc::new(Session {
            connection: identity(db),
            active: AtomicBool::new(true),
            file: self.file.clone(),
            path: self.db_path.clone(),
            pending: Mutex::new(None),
        });
        self.session = Some(session.clone());
        Ok(DirectHashGuard(session))
    }

    /// Revoke same-connection clones before a whole-store reset changes local ID meaning.
    pub fn reset(&mut self, db: &Connection) -> Result<(), Error> {
        if let Some(session) = &self.session {
            session.active.store(false, Ordering::Relaxed);
        }
        self.view = None;
        let token: [u8; 16] = rand::random();
        let path = sidecar_path(&self.db_path, &token);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;
        let mut bytes = vec![0; slot_end(0) as usize];
        bytes[..8].copy_from_slice(MAGIC);
        bytes[8..24].copy_from_slice(&token);
        file.write_all(&bytes)?;
        file.sync_all()?;
        File::open(
            self.db_path
                .parent()
                .ok_or_else(|| io::Error::other("database has no parent"))?,
        )?
        .sync_all()?;
        db.execute(
            "UPDATE marf_direct_hash_index SET token=?1,max_id=0,valid=1 WHERE singleton=1",
            [token.as_slice()],
        )?;
        let exclusions: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='marf_ancestry_exclusions')", [], |r| r.get(0))?;
        if exclusions {
            db.execute("DELETE FROM marf_ancestry_exclusions", [])?;
        }
        // Keep the old owner until the next begin; rollback may restore its publication.

        Ok(())
    }

    /// Buffer a confirmed slot, checkpointing batches before publishing the durable boundary.
    /// The caller must hold SQLite's writer lock; new slots remain unreadable until a later begin.
    pub fn append<T: MarfTrieId>(
        &self,
        db: &Connection,
        id: u32,
        block: &T,
        root: &TrieHash,
    ) -> Result<(), Error> {
        self.append_record(db, id, block, root, None)
    }

    /// Publish ancestry links together with hashes using the existing durable batch.
    pub fn append_with_parent<T: MarfTrieId>(
        &self,
        db: &Connection,
        id: u32,
        block: &T,
        root: &TrieHash,
        parent: &T,
    ) -> Result<(), Error> {
        self.append_record(db, id, block, root, Some(parent))
    }

    /// Build one pending record without exposing it to transaction readers.
    fn append_record<T: MarfTrieId>(
        &self,
        db: &Connection,
        id: u32,
        block: &T,
        root: &TrieHash,
        parent: Option<&T>,
    ) -> Result<(), Error> {
        #[cfg(feature = "commit-residency-diagnostics")]
        let _span = stacks_profiler::diagnostic_span!("Direct hash: Append publication");
        let Some(session) = &self.session else {
            return Ok(());
        };
        if !session.active.load(Ordering::Relaxed)
            || session.connection != identity(db)
            || db.is_autocommit()
            || self.view.is_none()
        {
            return Ok(());
        }
        ensure_live_table(db)?;
        let Some((token, durable, max_id)) = publication(db)? else {
            return Ok(());
        };
        if token != self.file.token || max_id.checked_add(1) != Some(id) {
            return Ok(());
        }
        let mut pending = session
            .pending
            .lock()
            .map_err(|_| io::Error::other("direct hash pending lock poisoned"))?;
        if pending
            .as_ref()
            .is_none_or(|p| (p.durable, p.live) != (durable, max_id))
        {
            let tail = self
                .tail
                .bounded(durable, max_id)
                .map(Ok)
                .unwrap_or_else(|| {
                    recover_tail(
                        db,
                        &self.db_path,
                        durable,
                        max_id,
                        self.file.slot_size,
                        &token,
                    )
                })?;
            *pending = Some(Pending {
                durable,
                live: max_id,
                tail,
            });
        }
        let item = pending.as_mut().expect("initialized above");
        let mut slot = [0; SLOT];
        slot[..32].copy_from_slice(block.as_bytes());
        slot[32..64].copy_from_slice(root.as_ref());
        if self.file.slot_size == SLOT {
            if let Some(parent) = parent {
                let mut state = self
                    .file
                    .state
                    .lock()
                    .map_err(|_| io::Error::other("direct hash lock poisoned"))?;
                if let Some(links) = recover_links(
                    db,
                    &mut state.file,
                    durable,
                    &item.tail,
                    id,
                    parent,
                    self.view.as_ref(),
                )? {
                    slot[64..].copy_from_slice(&links.encode());
                }
            }
        }
        Arc::make_mut(&mut item.tail.slots).push(slot);
        item.live = id;
        if id - durable >= CHECKPOINT_SLOTS {
            #[cfg(feature = "commit-residency-diagnostics")]
            let _checkpoint =
                stacks_profiler::diagnostic_span!("Direct hash: Durable batch checkpoint");
            let mut state = self
                .file
                .state
                .lock()
                .map_err(|_| io::Error::other("direct hash lock poisoned"))?;
            state
                .file
                .seek(SeekFrom::Start(slot_end_for(durable, self.file.slot_size)))?;
            for slot in item.tail.slots.iter() {
                state.file.write_all(&slot[..self.file.slot_size])?;
            }
            state.file.sync_data()?;
            db.execute("UPDATE marf_direct_hash_index SET max_id=?1 WHERE singleton=1 AND token=?2 AND max_id=?3 AND valid=1", params![id,self.file.token,durable])?;
            item.durable = id;
            item.tail = Tail {
                base: id,
                slots: Arc::default(),
            };
        }
        db.execute("INSERT INTO marf_direct_hash_live(singleton,token,max_id) VALUES(1,?1,?2) ON CONFLICT(singleton) DO UPDATE SET token=excluded.token,max_id=excluded.max_id",params![self.file.token,id])?;
        Ok(())
    }

    /// Read only within this managed transaction's committed immutable prefix.
    fn slot(&self, db: &Connection, id: u32) -> Option<[u8; SLOT]> {
        let session = self.session.as_ref()?;
        if !session.active.load(Ordering::Relaxed)
            || session.connection != identity(db)
            || db.is_autocommit()
        {
            return None;
        }
        self.view.as_ref()?.slot(id).or_else(|| self.tail.slot(id))
    }

    /// Return a direct hash, with SQL fallback for newer, unconfirmed, or unindexed IDs.
    pub fn block_hash<T: MarfTrieId>(
        index: Option<&Self>,
        db: &Connection,
        id: u32,
    ) -> Result<T, Error> {
        #[cfg(feature = "commit-residency-diagnostics")]
        let _span = stacks_profiler::diagnostic_span!("Direct hash: Block lookup");
        if let Some(slot) = index.and_then(|i| i.slot(db, id)) {
            #[cfg(feature = "commit-residency-diagnostics")]
            let _hit = stacks_profiler::diagnostic_span!("Direct hash: Block hit");
            #[cfg(feature = "commit-residency-diagnostics")]
            if index.is_some_and(|i| id > i.file.initial_max) {
                let _online = stacks_profiler::diagnostic_span!("Direct hash: Online block hit");
            }
            return Ok(T::from_bytes(slot[..32].try_into().expect("fixed slot")));
        }
        trie_sql::get_block_hash(db, id)
    }

    /// Return the immutable slot at an older height on this exact committed fork.
    fn ancestor_slot<T: MarfTrieId>(
        &self,
        db: &Connection,
        id: u32,
        block: &T,
        height: u32,
        target: u32,
    ) -> Option<[u8; SLOT]> {
        if self.file.slot_size != SLOT {
            return None;
        }
        let slot = self.slot(db, id)?;
        if &slot[..32] != block.as_bytes() {
            return None;
        }
        let links = Ancestry::decode(id, &slot[64..])?;
        if links.height != height {
            return None;
        }
        let found = ancestry::ancestor(id, target, |at| {
            let record = self.slot(db, at)?;
            Ancestry::decode(at, &record[64..])
        })?;
        self.slot(db, found)
    }

    /// Resolve an older height on this exact committed block's fork.
    pub fn ancestor_hash<T: MarfTrieId>(
        &self,
        db: &Connection,
        id: u32,
        block: &T,
        height: u32,
        target: u32,
    ) -> Option<T> {
        let record = self.ancestor_slot(db, id, block, height, target)?;
        Some(T::from_bytes(record[..32].try_into().expect("fixed hash")))
    }

    /// Read the committed root from the same fork-verified ancestor slot.
    pub fn ancestor_root<T: MarfTrieId>(
        &self,
        db: &Connection,
        id: u32,
        block: &T,
        height: u32,
        target: u32,
    ) -> Option<TrieHash> {
        let record = self.ancestor_slot(db, id, block, height, target)?;
        Some(TrieHash(record[32..64].try_into().expect("fixed root")))
    }

    /// Return the direct root only when its exact block identity also matches.
    pub fn root_hash<T: MarfTrieId>(
        &self,
        db: &Connection,
        id: u32,
        block: &T,
    ) -> Result<Option<TrieHash>, Error> {
        #[cfg(feature = "commit-residency-diagnostics")]
        let _span = stacks_profiler::diagnostic_span!("Direct hash: Root lookup");
        let Some(slot) = self.slot(db, id) else {
            return Ok(None);
        };
        if &slot[..32] != block.as_bytes() {
            return Ok(None);
        }
        #[cfg(feature = "commit-residency-diagnostics")]
        let _hit = stacks_profiler::diagnostic_span!("Direct hash: Root hit");
        #[cfg(feature = "commit-residency-diagnostics")]
        if id > self.file.initial_max {
            let _online = stacks_profiler::diagnostic_span!("Direct hash: Online root hit");
        }
        Ok(Some(TrieHash(slot[32..64].try_into().expect("fixed slot"))))
    }
}

/// Whether low-level writes made the reserved mapping namespace unsuitable for shortcuts.
fn ancestry_excluded(db: &Connection, id: u32) -> Result<bool, Error> {
    let exists: bool = db.prepare_cached("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='marf_ancestry_exclusions')")?.query_row([], |r| r.get(0))?;
    if !exists {
        return Ok(false);
    }
    Ok(db
        .prepare_cached("SELECT EXISTS(SELECT 1 FROM marf_ancestry_exclusions WHERE block_id=?1)")?
        .query_row([id], |r| r.get(0))?)
}

/// Persist conservative invalidation with the same SQL transaction as the block.
pub fn exclude_ancestry(db: &Connection, id: u32) -> Result<(), Error> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS marf_ancestry_exclusions(block_id INTEGER PRIMARY KEY)",
    )?;
    db.execute(
        "INSERT OR IGNORE INTO marf_ancestry_exclusions VALUES(?1)",
        [id],
    )?;
    Ok(())
}

/// Read only authoritative durable bytes or an explicitly recovered/constructed tail.
fn read_links(
    file: &mut File,
    durable: u32,
    tail: &Tail,
    id: u32,
    view: Option<&View>,
) -> Option<Ancestry> {
    if let Some(slot) = tail.slot(id).or_else(|| view.and_then(|v| v.slot(id))) {
        return Ancestry::decode(id, &slot[64..]);
    }
    if id == 0 || id > durable {
        return None;
    }
    let mut bytes = [0; ancestry::ENCODED_SIZE];
    file.seek(SeekFrom::Start(
        HEADER as u64 + u64::from(id) * SLOT as u64 + 64,
    ))
    .ok()?;
    file.read_exact(&mut bytes).ok()?;
    Ancestry::decode(id, &bytes)
}

/// Reconstruct admitted ancestry from authoritative parent identities, never unsynced file suffixes.
fn recover_links<T: MarfTrieId>(
    db: &Connection,
    file: &mut File,
    durable: u32,
    tail: &Tail,
    id: u32,
    parent: &T,
    view: Option<&View>,
) -> Result<Option<Ancestry>, Error> {
    if ancestry_excluded(db, id)? || trie_sql::read_squash_info(db)?.is_some() {
        return Ok(None);
    }
    if parent == &T::sentinel() {
        return Ok(Some(Ancestry::genesis()));
    }
    let parent_id = match trie_sql::get_block_identifier(db, parent) {
        Ok(id) => id,
        Err(Error::NotFoundError) => return Ok(None),
        Err(Error::SQLError(rusqlite::Error::QueryReturnedNoRows)) => return Ok(None),
        Err(e) => return Err(e),
    };
    Ok(Ancestry::child(id, parent_id, |at| {
        read_links(file, durable, tail, at, view)
    }))
}

/// Build and durably publish a new dense confirmed-prefix generation on an offline database.
/// Stops at the first hole or unconfirmed row; old generations remain available to old snapshots.
pub fn build(db_path: &Path) -> Result<(u32, u64), Box<dyn std::error::Error>> {
    let mut db = Connection::open(db_path)?;
    db.execute_batch("PRAGMA cache_size=-16384; PRAGMA synchronous=FULL")?;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS marf_ancestry_exclusions(block_id INTEGER PRIMARY KEY)",
    )?;
    let token: [u8; 16] = rand::random();
    let path = sidecar_path(db_path, &token);
    let mut writer = BufWriter::with_capacity(
        1024 * 1024,
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?,
    );
    let mut header = [0; HEADER];
    header[..8].copy_from_slice(MAGIC);
    header[8..24].copy_from_slice(&token);
    writer.write_all(&header)?;
    writer.write_all(&[0; SLOT])?;
    let format = NodeRecordFormat::from_database(&tx)?;
    let mut blobs = File::open(format!("{}.blobs", db_path.display()))?;
    let mut max_id = 0u32;
    let mut links: Vec<Option<Ancestry>> = vec![None];
    let mut previous_block = StacksBlockId::sentinel();
    let squashed = trie_sql::read_squash_info(&tx)?.is_some();
    {
        let mut statement = tx.prepare("SELECT m.block_id,m.block_hash,m.external_offset,m.external_length,m.data,m.unconfirmed,e.block_id IS NOT NULL FROM marf_data m LEFT JOIN marf_ancestry_exclusions e ON e.block_id=m.block_id ORDER BY m.block_id")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let id: u32 = row.get(0)?;
            if max_id.checked_add(1) != Some(id) || row.get::<_, bool>(5)? {
                break;
            }
            let block: StacksBlockId = row.get(1)?;
            let offset: u64 = row.get(2)?;
            let length: u64 = row.get(3)?;
            let mut prefix = [0; MAX_READER_PREFIX_LEN];
            let size = format.reader_prefix_len();
            if length > 0 {
                if length < size as u64 {
                    return Err("truncated external trie".into());
                }
                blobs.seek(SeekFrom::Start(offset))?;
                blobs.read_exact(&mut prefix[..size])?;
            } else {
                let data: Vec<u8> = row.get(4)?;
                prefix[..size].copy_from_slice(data.get(..size).ok_or("truncated inline trie")?);
            }
            let header = BlobHeader::<StacksBlockId>::parse_format(format, &prefix[..size])?;
            let record = if squashed || row.get::<_, bool>(6)? {
                None
            } else if header.parent_hash == StacksBlockId::sentinel() {
                Some(Ancestry::genesis())
            } else {
                let parent_id = if header.parent_hash == previous_block {
                    Some(max_id)
                } else {
                    match trie_sql::get_block_identifier(&tx, &header.parent_hash) {
                        Ok(id) => Some(id),
                        Err(
                            Error::NotFoundError
                            | Error::SQLError(rusqlite::Error::QueryReturnedNoRows),
                        ) => None,
                        Err(error) => return Err(error.into()),
                    }
                };
                parent_id.and_then(|parent| {
                    Ancestry::child(id, parent, |at| links.get(at as usize).copied().flatten())
                })
            };
            writer.write_all(block.as_ref())?;
            writer.write_all(header.root_hash.as_ref())?;
            writer.write_all(
                &record
                    .map(Ancestry::encode)
                    .unwrap_or([0; ancestry::ENCODED_SIZE]),
            )?;
            links.push(record);
            previous_block = block;
            max_id = id;
            if max_id % 250_000 == 0 {
                eprintln!("direct hashes: {max_id}");
            }
        }
    }
    writer.flush()?;
    writer.get_ref().sync_all()?;
    File::open(db_path.parent().ok_or("database has no parent")?)?.sync_all()?;
    tx.execute_batch("CREATE TABLE IF NOT EXISTS marf_direct_hash_index(singleton INTEGER PRIMARY KEY CHECK(singleton=1),token BLOB NOT NULL CHECK(length(token)=16),max_id INTEGER NOT NULL,valid INTEGER NOT NULL CHECK(valid IN (0,1)));
    DROP TRIGGER IF EXISTS direct_hash_update;
    DROP TRIGGER IF EXISTS direct_hash_delete;
    DROP TRIGGER IF EXISTS direct_hash_insert;
    CREATE TRIGGER direct_hash_update BEFORE UPDATE ON marf_data WHEN OLD.block_id<=(SELECT max_id FROM marf_direct_hash_index WHERE singleton=1) OR NEW.block_id<=(SELECT max_id FROM marf_direct_hash_index WHERE singleton=1) BEGIN SELECT RAISE(ABORT,'immutable direct hash prefix: reset/rebuild index before historical mutation'); END;
    CREATE TRIGGER direct_hash_delete BEFORE DELETE ON marf_data WHEN OLD.block_id<=(SELECT max_id FROM marf_direct_hash_index WHERE singleton=1) BEGIN SELECT RAISE(ABORT,'immutable direct hash prefix: reset index before deletion'); END;
    CREATE TRIGGER direct_hash_insert BEFORE INSERT ON marf_data WHEN NEW.block_id>0 AND NEW.block_id<=(SELECT max_id FROM marf_direct_hash_index WHERE singleton=1) BEGIN SELECT RAISE(ABORT,'immutable direct hash prefix: cannot reuse published ID'); END;")?;
    tx.execute(
        "INSERT OR REPLACE INTO marf_direct_hash_index VALUES(1,?1,?2,1)",
        params![token.as_slice(), max_id],
    )?;
    tx.commit()?;
    Ok((max_id, fs::metadata(path)?.len()))
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::chainstate::stacks::index::marf::{MARFOpenOpts, MarfConnection, MARF};
    use crate::chainstate::stacks::index::{ClarityMarfTrieId, MARFValue};

    /// Compare sealing and fork histories against a store without the optional index.
    #[test]
    fn direct_hash_marf_sealing_forks_and_reopens() {
        sealing_forks_and_reopens(NodeRecordFormat::Legacy);
    }

    /// Clarity's type-first roots work for building, sealing and committed-tail recovery.
    #[test]
    fn direct_hash_type_first_sealing_forks_and_reopens() {
        sealing_forks_and_reopens(NodeRecordFormat::TypeFirstV1);
    }

    /// Compact raw mapping leaves preserve indexed and unindexed fork roots.
    #[test]
    fn direct_hash_compact_raw_sealing_forks_and_reopens() {
        sealing_forks_and_reopens(NodeRecordFormat::TypeFirstV2);
    }

    /// Named and opaque-path overrides retain exact roots and lookups through descendants/rebuild.
    #[test]
    fn ancestry_reserved_overrides_use_authoritative_trie() {
        use crate::chainstate::stacks::index::marf::{MarfCore, BLOCK_HEIGHT_TO_HASH_MAPPING_KEY};
        use crate::chainstate::stacks::index::TrieLeaf;
        for raw in [false, true] {
            let dirs = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
            let paths: Vec<_> = dirs.iter().map(|d| d.path().join("marf.sqlite")).collect();
            let opts = MARFOpenOpts {
                external_blobs: true,
                mmap: true,
                ..MARFOpenOpts::default()
            };
            let mut stores: Vec<_> = paths
                .iter()
                .map(|p| {
                    MARF::<StacksBlockId>::from_path(p.to_str().unwrap(), opts.clone()).unwrap()
                })
                .collect();
            for i in 1..=12u8 {
                for marf in &mut stores {
                    let parent = if i == 1 {
                        StacksBlockId::sentinel()
                    } else {
                        StacksBlockId([i - 1; 32])
                    };
                    marf.begin(&parent, &StacksBlockId([i; 32])).unwrap();
                    marf.insert("ordinary", MARFValue::from(u32::from(i)))
                        .unwrap();
                    marf.commit().unwrap();
                }
            }
            drop(stores);
            build(&paths[1]).unwrap();
            let mut stores: Vec<_> = paths
                .iter()
                .map(|p| {
                    MARF::<StacksBlockId>::from_path(p.to_str().unwrap(), opts.clone()).unwrap()
                })
                .collect();
            for i in 13..=14u8 {
                for marf in &mut stores {
                    let tip = StacksBlockId([i; 32]);
                    marf.begin(&StacksBlockId([i - 1; 32]), &tip).unwrap();
                    if i == 13 {
                        let key = format!("{BLOCK_HEIGHT_TO_HASH_MAPPING_KEY}::4");
                        let value = MARFValue::from(StacksBlockId([8; 32]));
                        if raw {
                            marf.insert_raw(
                                TrieHash::from_key(&key),
                                TrieLeaf::from_value(&[], value),
                            )
                            .unwrap();
                        } else {
                            marf.insert(&key, value).unwrap();
                        }
                    }
                    {
                        let mut tx = marf.begin_tx().unwrap();
                        assert_eq!(
                            MarfCore::get_block_at_height(&mut tx, 4, &tip).unwrap(),
                            Some(StacksBlockId([8; 32]))
                        );
                    }
                    marf.seal().unwrap();
                    marf.commit().unwrap();
                }
                let expected = stores[0].get_root_hash_at(&StacksBlockId([i; 32])).unwrap();
                assert_eq!(
                    stores[1].get_root_hash_at(&StacksBlockId([i; 32])).unwrap(),
                    expected
                );
            }
            drop(stores);
            build(&paths[1]).unwrap();
            let mut db = Connection::open(&paths[1]).unwrap();
            let mut index = DirectHashIndex::open(&db, &paths[1]).unwrap().unwrap();
            let tx = db.transaction().unwrap();
            let _guard = index.begin(&tx).unwrap();
            let id = trie_sql::get_block_identifier(&tx, &StacksBlockId([14; 32])).unwrap();
            assert!(index
                .ancestor_hash(&tx, id, &StacksBlockId([14; 32]), 13, 4)
                .is_none());
        }
    }

    /// Exercise equivalent histories with and without an index in the selected layout.
    fn sealing_forks_and_reopens(format: NodeRecordFormat) {
        let dirs = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
        let paths = dirs
            .iter()
            .map(|d| d.path().join("marf.sqlite"))
            .collect::<Vec<_>>();
        let opts = MARFOpenOpts {
            external_blobs: true,
            mmap: true,
            ..MARFOpenOpts::default()
        };
        let mut stores = paths
            .iter()
            .map(|p| MARF::<StacksBlockId>::from_path(p.to_str().unwrap(), opts.clone()).unwrap())
            .collect::<Vec<_>>();
        for marf in &mut stores {
            if format.is_type_first() {
                format.publish(marf.sqlite_conn()).unwrap();
            }
            marf.set_record_format(format);
        }
        for i in 1..=20u8 {
            for m in &mut stores {
                let parent = if i == 1 {
                    StacksBlockId::sentinel()
                } else {
                    StacksBlockId([i - 1; 32])
                };
                m.begin(&parent, &StacksBlockId([i; 32])).unwrap();
                m.insert(
                    &format!("key-{i}"),
                    MARFValue::from_value(&format!("value-{i}")),
                )
                .unwrap();
                m.seal().unwrap();
                m.commit().unwrap();
            }
        }
        drop(stores);
        build(&paths[1]).unwrap();
        let mut stores = paths
            .iter()
            .map(|p| MARF::<StacksBlockId>::from_path(p.to_str().unwrap(), opts.clone()).unwrap())
            .collect::<Vec<_>>();
        for (id, parent) in [(21, 20), (22, 21), (23, 15), (24, 23)] {
            for m in &mut stores {
                m.begin(&StacksBlockId([parent; 32]), &StacksBlockId([id; 32]))
                    .unwrap();
                m.insert("changed", MARFValue::from_value("fork value"))
                    .unwrap();
                m.seal().unwrap();
                m.commit().unwrap();
                let mut reopened = m.reopen_readonly().unwrap();
                assert_eq!(
                    reopened.get(&StacksBlockId([id; 32]), "changed").unwrap(),
                    Some(MARFValue::from_value("fork value"))
                );
            }
            let read_root = |p: &Path| {
                let db = Connection::open(p).unwrap();
                let offset: u64 = db
                    .query_row(
                        "SELECT external_offset FROM marf_data WHERE block_hash=?1",
                        [StacksBlockId([id; 32])],
                        |r| r.get(0),
                    )
                    .unwrap();
                let mut file = File::open(format!("{}.blobs", p.display())).unwrap();
                file.seek(SeekFrom::Start(
                    offset + 36 + u64::from(format.is_type_first()),
                ))
                .unwrap();
                let mut hash = [0; 32];
                file.read_exact(&mut hash).unwrap();
                hash
            };
            assert_eq!(read_root(&paths[0]), read_root(&paths[1]));
            let db = Connection::open(&paths[1]).unwrap();
            let (published, actual): (u32,u32) = db.query_row("SELECT max_id,(SELECT MAX(block_id) FROM marf_data) FROM marf_direct_hash_index WHERE singleton=1", [], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
            assert!(published <= actual && actual - published < CHECKPOINT_SLOTS);
            let live: u32 = db
                .query_row("SELECT max_id FROM marf_direct_hash_live", [], |r| r.get(0))
                .unwrap();
            assert_eq!(
                live, actual,
                "every new trie must enter the recoverable committed tail"
            );
            let mut index = DirectHashIndex::open(&db, &paths[1]).unwrap().unwrap();
            db.execute_batch("BEGIN").unwrap();
            let _guard = index.begin(&db).unwrap();
            let height = match id {
                21 => 20,
                22 => 21,
                23 => 15,
                24 => 16,
                _ => unreachable!(),
            };
            assert_eq!(
                index.ancestor_hash(&db, actual, &StacksBlockId([id; 32]), height, 0),
                Some(StacksBlockId([1; 32]))
            );
            assert_eq!(
                index
                    .root_hash(&db, actual, &StacksBlockId([id; 32]))
                    .unwrap(),
                Some(TrieHash(read_root(&paths[1])))
            );
        }
    }
}

#[cfg(test)]
mod online_tests {
    use super::*;

    /// Small real SQL fixture containing valid inline legacy root envelopes.
    fn fixture(count: u32) -> (tempfile::TempDir, PathBuf, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("marf.sqlite");
        let db = Connection::open(&path).unwrap();
        db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE marf_data(block_id INTEGER PRIMARY KEY,block_hash TEXT UNIQUE,unconfirmed INTEGER,external_offset INTEGER,external_length INTEGER,data BLOB)").unwrap();
        File::create(format!("{}.blobs", path.display())).unwrap();
        for id in 1..=count {
            insert(&db, id, 0);
        }
        build(&path).unwrap();
        (dir, path, db)
    }

    /// Insert one canonical envelope using the same auto-ID allocation as runtime writes.
    fn insert(db: &Connection, value: u32, unconfirmed: u8) -> u32 {
        let mut data = vec![0; 68];
        data[36..].fill((value as u8).wrapping_add(10));
        let mut block = [0; 32];
        block[..4].copy_from_slice(&value.to_le_bytes());
        db.execute("INSERT INTO marf_data(block_hash,unconfirmed,external_offset,external_length,data) VALUES(?1,?2,0,0,?3)",params![StacksBlockId(block),unconfirmed,data]).unwrap();
        db.last_insert_rowid() as u32
    }

    /// Construct deterministic identities including IDs above 255.
    fn block(value: u32) -> StacksBlockId {
        let mut bytes = [0; 32];
        bytes[..4].copy_from_slice(&value.to_le_bytes());
        StacksBlockId(bytes)
    }

    /// Publish the record inserted by `insert` without changing the current read snapshot.
    fn append(index: &DirectHashIndex, db: &Connection, id: u32, value: u32) {
        index
            .append(
                db,
                id,
                &block(value),
                &TrieHash([(value as u8).wrapping_add(10); 32]),
            )
            .unwrap();
    }

    /// Existing v2 generations continue to serve and batch-append their 64-byte records.
    #[test]
    fn ancestry_v2_hash_only_generation_remains_compatible() {
        let (_dir, path, mut db) = fixture(3);
        let token: Vec<u8> = db
            .query_row("SELECT token FROM marf_direct_hash_index", [], |r| r.get(0))
            .unwrap();
        let filename = sidecar_path(&path, &token);
        let original = fs::read(&filename).unwrap();
        let mut legacy = original[..HEADER].to_vec();
        legacy[..8].copy_from_slice(LEGACY_MAGIC);
        for slot in original[HEADER..].chunks_exact(SLOT) {
            legacy.extend_from_slice(&slot[..LEGACY_SLOT]);
        }
        fs::write(&filename, legacy).unwrap();
        let mut index = DirectHashIndex::open(&db, &path).unwrap().unwrap();
        assert_eq!(index.file.slot_size, LEGACY_SLOT);
        for value in 4..=270 {
            commit_slot(&mut index, &mut db, value);
        }
        drop(index);
        let mut reopened = DirectHashIndex::open(&db, &path).unwrap().unwrap();
        let tx = db.transaction().unwrap();
        let _guard = reopened.begin(&tx).unwrap();
        for id in 1..=270 {
            assert_eq!(
                DirectHashIndex::block_hash::<StacksBlockId>(Some(&reopened), &tx, id).unwrap(),
                block(id)
            );
            assert!(reopened
                .ancestor_hash(&tx, id, &block(id), id - 1, 0)
                .is_none());
        }
    }

    /// Online links are fork-specific and recover correctly across durable checkpoints.
    #[test]
    fn ancestry_online_forks_checkpoint_and_reopen() {
        let (_dir, path, mut db) = fixture(0);
        let mut index = DirectHashIndex::open(&db, &path).unwrap().unwrap();
        let mut parents = vec![0u32];
        let mut heights = vec![0u32];
        for value in 1..=520u32 {
            let parent = if value == 1 {
                0
            } else if value % 101 == 0 {
                value / 2
            } else {
                value - 1
            };
            let parent_hash = if parent == 0 {
                StacksBlockId::sentinel()
            } else {
                block(parent)
            };
            let height = if parent == 0 {
                0
            } else {
                heights[parent as usize] + 1
            };
            let tx = db.transaction().unwrap();
            let guard = index.begin(&tx).unwrap();
            let id = insert(&tx, value, 0);
            let mut data: Vec<u8> = tx
                .query_row("SELECT data FROM marf_data WHERE block_id=?1", [id], |r| {
                    r.get(0)
                })
                .unwrap();
            data[..32].copy_from_slice(parent_hash.as_bytes());
            tx.execute(
                "UPDATE marf_data SET data=?1 WHERE block_id=?2",
                params![data, id],
            )
            .unwrap();
            index
                .append_with_parent(
                    &tx,
                    id,
                    &block(value),
                    &TrieHash([(value as u8).wrapping_add(10); 32]),
                    &parent_hash,
                )
                .unwrap();
            assert!(
                index
                    .ancestor_hash(&tx, id, &block(value), height, 0)
                    .is_none(),
                "speculative slots must be invisible"
            );
            assert!(index
                .ancestor_root(&tx, id, &block(value), height, 0)
                .is_none());
            guard.prepare_commit(&tx).unwrap();
            tx.commit().unwrap();
            guard.succeed();
            parents.push(parent);
            heights.push(height);
            if value % 127 == 0 {
                index = DirectHashIndex::open(&db, &path).unwrap().unwrap();
            }
            let tx = db.transaction().unwrap();
            let _guard = index.begin(&tx).unwrap();
            for target in [0, height / 2, height] {
                let mut expected = value;
                while heights[expected as usize] > target {
                    expected = parents[expected as usize];
                }
                assert_eq!(
                    index.ancestor_hash(&tx, id, &block(value), height, target),
                    Some(block(expected))
                );
                assert_eq!(
                    index.ancestor_root(&tx, id, &block(value), height, target),
                    Some(TrieHash([(expected as u8).wrapping_add(10); 32]))
                );
            }
            assert!(index
                .ancestor_hash(&tx, id, &block(value), height + 1, 0)
                .is_none());
            assert!(index
                .ancestor_hash(&tx, id, &block(value + 1), height, 0)
                .is_none());
            assert!(index
                .ancestor_root(&tx, id, &block(value + 1), height, 0)
                .is_none());
        }
        // Rebuild derives the same ancestry from authoritative parent headers.
        drop(index);
        build(&path).unwrap();
        let mut index = DirectHashIndex::open(&db, &path).unwrap().unwrap();
        let tx = db.transaction().unwrap();
        let _guard = index.begin(&tx).unwrap();
        for value in 1..=520 {
            assert_eq!(
                index.ancestor_hash(&tx, value, &block(value), heights[value as usize], 0),
                Some(block(1))
            );
        }
    }

    /// Commit exposes online slots only in subsequent transactions and related reopens.
    #[test]
    fn online_commit_reopen_and_tail_growth() {
        let (_dir, path, mut db) = fixture(1);
        let mut index = DirectHashIndex::open(&db, &path).unwrap().unwrap();
        let mut reopened = index.clone();
        for value in 2..=700u32 {
            let tx = db
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .unwrap();
            let guard = index.begin(&tx).unwrap();
            assert!(index.slot(&tx, value - 1).is_some());
            let id = insert(&tx, value, 0);
            // Use a root that avoids debug arithmetic overflow in the test helper.
            index
                .append(
                    &tx,
                    id,
                    &block(value),
                    &TrieHash([(value as u8).wrapping_add(10); 32]),
                )
                .unwrap();
            assert!(index.slot(&tx, id).is_none());
            guard.prepare_commit(&tx).unwrap();
            tx.commit().unwrap();
            guard.succeed();
        }
        let mut db2 = Connection::open(&path).unwrap();
        let tx = db2.transaction().unwrap();
        let _guard = reopened.begin(&tx).unwrap();
        assert!(Arc::ptr_eq(&index.file, &reopened.file));
        assert_eq!(
            reopened.root_hash(&tx, 700, &block(700)).unwrap(),
            Some(TrieHash([(700u32 as u8).wrapping_add(10); 32]))
        );
        assert_eq!(reopened.view.as_ref().unwrap().max_id, 513);
        assert_eq!(reopened.tail.end(), 700);
        #[cfg(all(unix, target_pointer_width = "64"))]
        std::assert_matches!(reopened.view.as_ref().unwrap().map, FileMapping::Stable(_));
    }

    /// Rolled-back IDs are reused, but their old bytes are never exposed or cached.
    #[test]
    fn online_rollback_suffix_reuse_and_crash_reopen() {
        let (_dir, path, mut db) = fixture(1);
        let mut index = DirectHashIndex::open(&db, &path).unwrap().unwrap();
        {
            let tx = db.transaction().unwrap();
            let _guard = index.begin(&tx).unwrap();
            let id = insert(&tx, 2, 0);
            append(&index, &tx, id, 2);
            assert!(index.slot(&tx, 2).is_none());
        }
        // Reopening simulates a process dying after sidecar sync but before SQL commit.
        drop(index);
        let mut index = DirectHashIndex::open(&db, &path).unwrap().unwrap();
        {
            let tx = db.transaction().unwrap();
            let _guard = index.begin(&tx).unwrap();
            assert!(index.slot(&tx, 2).is_none());
            let id = insert(&tx, 3, 0);
            assert_eq!(id, 2);
            append(&index, &tx, id, 3);
            tx.commit().unwrap();
        }
        let tx = db.transaction().unwrap();
        let _guard = index.begin(&tx).unwrap();
        assert_eq!(
            index.root_hash(&tx, 2, &block(3)).unwrap(),
            Some(TrieHash([13; 32]))
        );
        assert!(index.root_hash(&tx, 2, &block(2)).unwrap().is_none());
    }

    /// Older WAL readers keep their bound while another connection publishes new slots.
    #[test]
    fn online_reader_snapshot_survives_other_writer_and_rebuild() {
        let (_dir, path, mut reader) = fixture(1);
        let mut index = DirectHashIndex::open(&reader, &path).unwrap().unwrap();
        let tx = reader.transaction().unwrap();
        let guard = index.begin(&tx).unwrap();
        let mut writer = Connection::open(&path).unwrap();
        let mut writing = index.clone();
        {
            let tx = writer.transaction().unwrap();
            let _guard = writing.begin(&tx).unwrap();
            let id = insert(&tx, 2, 0);
            append(&writing, &tx, id, 2);
            tx.commit().unwrap();
        }
        assert!(index.slot(&tx, 2).is_none());
        assert!(index.slot(&writer, 1).is_none());
        let old_file = index.file.clone();
        build(&path).unwrap();
        assert!(index.slot(&tx, 1).is_some());
        tx.commit().unwrap();
        drop(guard);
        let tx = reader.transaction().unwrap();
        let _guard = index.begin(&tx).unwrap();
        assert!(!Arc::ptr_eq(&index.file, &old_file));
        assert!(index.slot(&tx, 2).is_some());
    }

    /// Reset changes generations; rollback restores the preceding generation and committed slots.
    #[test]
    fn online_reset_commit_and_rollback() {
        let (_dir, path, mut db) = fixture(2);
        let mut index = DirectHashIndex::open(&db, &path).unwrap().unwrap();
        let token = index.file.token.clone();
        {
            let tx = db.transaction().unwrap();
            let _guard = index.begin(&tx).unwrap();
            let clone = index.clone();
            index.reset(&tx).unwrap();
            tx.execute("DELETE FROM marf_data", []).unwrap();
            assert!(clone.slot(&tx, 1).is_none());
        }
        {
            let tx = db.transaction().unwrap();
            let _guard = index.begin(&tx).unwrap();
            assert_eq!(index.file.token, token);
            assert!(index.slot(&tx, 2).is_some());
            index.reset(&tx).unwrap();
            tx.execute("DELETE FROM marf_data", []).unwrap();
            tx.commit().unwrap();
        }
        {
            let tx = db.transaction().unwrap();
            let _guard = index.begin(&tx).unwrap();
            assert_ne!(index.file.token, token);
            assert!(index.slot(&tx, 1).is_none());
            let id = insert(&tx, 7, 0);
            assert_eq!(id, 1);
            append(&index, &tx, id, 7);
            tx.commit().unwrap();
        }
        let tx = db.transaction().unwrap();
        let _guard = index.begin(&tx).unwrap();
        assert_eq!(
            index.root_hash(&tx, 1, &block(7)).unwrap(),
            Some(TrieHash([17; 32]))
        );
    }

    /// Generic stores retain only their dense confirmed prefix; unconfirmed rows remain mutable.
    #[test]
    fn online_gap_unconfirmed_and_immutable_prefix() {
        let (_dir, path, mut db) = fixture(1);
        insert(&db, 2, 1);
        insert(&db, 3, 0);
        assert_eq!(build(&path).unwrap().0, 1);
        let mut index = DirectHashIndex::open(&db, &path).unwrap().unwrap();
        let tx = db.transaction().unwrap();
        let _guard = index.begin(&tx).unwrap();
        assert!(index.slot(&tx, 2).is_none());
        assert_eq!(
            DirectHashIndex::block_hash::<StacksBlockId>(Some(&index), &tx, 2).unwrap(),
            block(2)
        );
        let id = insert(&tx, 4, 0);
        append(&index, &tx, id, 4);
        assert!(tx
            .execute("DELETE FROM marf_data WHERE block_id=1", [])
            .is_err());
        assert!(tx
            .execute(
                "UPDATE marf_data SET block_hash='changed' WHERE block_id=1",
                []
            )
            .is_err());
        tx.execute("DELETE FROM marf_data WHERE block_id=2", [])
            .unwrap();
        assert!(index.slot(&tx, 1).is_some());
        tx.commit().unwrap();
        assert_eq!(build(&path).unwrap().0, 1);
    }

    /// Commit a fixture slot through the same admission hooks as storage transactions.
    fn commit_slot(index: &mut DirectHashIndex, db: &mut Connection, value: u32) {
        let tx = db.transaction().unwrap();
        let guard = index.begin(&tx).unwrap();
        let id = insert(&tx, value, 0);
        append(index, &tx, id, value);
        guard.prepare_commit(&tx).unwrap();
        tx.commit().unwrap();
        guard.succeed();
    }

    /// Live records survive reopen using authoritative data even if unsynced suffix bytes vanish.
    #[test]
    fn batched_durable_boundary_recovery_and_shared_admission() {
        let (_dir, path, mut db) = fixture(1);
        let mut index = DirectHashIndex::open(&db, &path).unwrap().unwrap();
        let file = sidecar_path(&path, &index.file.token);
        for value in 2..=256 {
            commit_slot(&mut index, &mut db, value);
        }
        assert_eq!(fs::metadata(&file).unwrap().len(), slot_end(1));
        assert_eq!(index.file.state.lock().unwrap().tail.slots.len(), 255);
        let cached = index.file.state.lock().unwrap().tail.slots.clone();
        {
            let tx = db.transaction().unwrap();
            let _guard = index.begin(&tx).unwrap();
            assert!(Arc::ptr_eq(&cached, &index.tail.slots));
            assert_eq!(
                index.root_hash(&tx, 256, &block(256)).unwrap(),
                Some(TrieHash([10; 32]))
            );
        }
        // Garbage from an interrupted future checkpoint must never be used for the live tail.
        {
            let mut f = OpenOptions::new().write(true).open(&file).unwrap();
            f.seek(SeekFrom::End(0)).unwrap();
            f.write_all(&vec![0xee; 255 * SLOT]).unwrap();
        }
        drop(index);
        let mut index = DirectHashIndex::open(&db, &path).unwrap().unwrap();
        {
            let tx = db.transaction().unwrap();
            let _guard = index.begin(&tx).unwrap();
            assert_eq!(
                index.root_hash(&tx, 256, &block(256)).unwrap(),
                Some(TrieHash([10; 32]))
            );
            assert_eq!(index.view.as_ref().unwrap().max_id, 1);
        }
        commit_slot(&mut index, &mut db, 257);
        let (_, durable, live) = publication(&db).unwrap().unwrap();
        assert_eq!((durable, live), (257, 257));
        assert!(index.file.state.lock().unwrap().tail.slots.is_empty());
        drop(index);
        let mut index = DirectHashIndex::open(&db, &path).unwrap().unwrap();
        let tx = db.transaction().unwrap();
        let _guard = index.begin(&tx).unwrap();
        assert_eq!(
            index.root_hash(&tx, 257, &block(257)).unwrap(),
            Some(TrieHash([11; 32]))
        );
    }

    /// A synced checkpoint whose SQL transaction rolls back cannot expose abandoned slot IDs.
    #[test]
    fn batched_checkpoint_rollback_and_id_reuse() {
        let (_dir, path, mut db) = fixture(1);
        let mut index = DirectHashIndex::open(&db, &path).unwrap().unwrap();
        for value in 2..=256 {
            commit_slot(&mut index, &mut db, value);
        }
        {
            let tx = db.transaction().unwrap();
            let _guard = index.begin(&tx).unwrap();
            let id = insert(&tx, 257, 0);
            append(&index, &tx, id, 257);
            assert_eq!(publication(&tx).unwrap().unwrap().1, 257);
            // implicit rollback after the physical write+sync
        }
        assert_eq!(publication(&db).unwrap().unwrap().1, 1);
        drop(index);
        let mut index = DirectHashIndex::open(&db, &path).unwrap().unwrap();
        commit_slot(&mut index, &mut db, 999);
        let tx = db.transaction().unwrap();
        let _guard = index.begin(&tx).unwrap();
        assert_eq!(
            index.root_hash(&tx, 257, &block(999)).unwrap(),
            Some(TrieHash([(999u32 as u8).wrapping_add(10); 32]))
        );
        assert!(index.root_hash(&tx, 257, &block(257)).unwrap().is_none());
    }

    /// Savepoint rollback is reconciled before admission, including a rolled-back checkpoint.
    #[test]
    fn batched_savepoint_checkpoint_rollback_before_commit() {
        let (_dir, path, mut db) = fixture(1);
        let mut index = DirectHashIndex::open(&db, &path).unwrap().unwrap();
        for value in 2..=256 {
            commit_slot(&mut index, &mut db, value);
        }
        let tx = db.transaction().unwrap();
        let guard = index.begin(&tx).unwrap();
        tx.execute_batch("SAVEPOINT suffix").unwrap();
        let id = insert(&tx, 257, 0);
        append(&index, &tx, id, 257);
        tx.execute_batch("ROLLBACK TO suffix; RELEASE suffix")
            .unwrap();
        guard.prepare_commit(&tx).unwrap();
        tx.commit().unwrap();
        guard.succeed();
        assert_eq!(index.file.state.lock().unwrap().tail.end(), 256);
        commit_slot(&mut index, &mut db, 999);
        let tx = db.transaction().unwrap();
        let _guard = index.begin(&tx).unwrap();
        assert_eq!(
            index.root_hash(&tx, 257, &block(999)).unwrap(),
            Some(TrieHash([(999u32 as u8).wrapping_add(10); 32]))
        );
    }

    /// Independent writers' committed tails are recovered; older reader snapshots remain bounded.
    #[test]
    fn batched_independent_writer_and_snapshot_boundaries() {
        let (_dir, path, mut db) = fixture(1);
        let mut reader = DirectHashIndex::open(&db, &path).unwrap().unwrap();
        let tx = db.transaction().unwrap();
        let old = reader.begin(&tx).unwrap();
        let mut writer_db = Connection::open(&path).unwrap();
        let mut writer = DirectHashIndex::open(&writer_db, &path).unwrap().unwrap();
        commit_slot(&mut writer, &mut writer_db, 2);
        assert!(reader.slot(&tx, 2).is_none());
        tx.commit().unwrap();
        drop(old);
        let tx = db.transaction().unwrap();
        let _guard = reader.begin(&tx).unwrap();
        assert_eq!(
            reader.root_hash(&tx, 2, &block(2)).unwrap(),
            Some(TrieHash([12; 32]))
        );
        assert!(writer_db
            .execute("DELETE FROM marf_data WHERE block_id=2", [])
            .is_err());
        assert!(writer_db
            .execute(
                "UPDATE marf_data SET block_hash='changed' WHERE block_id=2",
                []
            )
            .is_err());
    }

    /// Missing/old/truncated generations fall back without exposing partial slot bytes.
    #[test]
    fn online_missing_truncated_and_old_format_recovery() {
        let (_dir, path, db) = fixture(2);
        let index = DirectHashIndex::open(&db, &path).unwrap().unwrap();
        let file = sidecar_path(&path, &index.file.token);
        drop(index);
        let bytes = fs::read(&file).unwrap();
        fs::write(&file, &bytes[..HEADER]).unwrap();
        assert!(DirectHashIndex::open(&db, &path).unwrap().is_none());
        let mut old = bytes.clone();
        old[..8].copy_from_slice(b"MARFDH01");
        fs::write(&file, old).unwrap();
        assert!(DirectHashIndex::open(&db, &path).unwrap().is_none());
        fs::remove_file(file).unwrap();
        assert!(DirectHashIndex::open(&db, &path).unwrap().is_none());
    }
}
