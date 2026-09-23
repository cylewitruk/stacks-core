//! Immutable Clarity value records addressed directly by physical MARF leaf locators.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use clarity::vm::database::{DataStoreValue, StoredValue, StoredValueResult};
use clarity::vm::errors::{VmExecutionError, VmInternalError};
use clarity::vm::types::codec::packed::{
    PackedByteOwner, PackedValueError, PackedValueRef, SharedPackedValue,
};
use clarity::vm::types::{TypeSignature, Value};
use extent_ptrhash::Base as PtrHashBase;
use memmap2::{Mmap, MmapOptions};
use rand::RngCore;
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use stacks_common::types::StacksEpochId;
use stacks_common::util::hash::{hex_bytes, to_hex};

use super::binary_value_store::{self, EncodedRecord};
use crate::chainstate::stacks::index::inline_value::{InlineValue, InlineValueBytes};

mod dedup_cache;
#[cfg(feature = "dedup-io-diagnostics")]
mod io_probe;
mod read_ahead;
use crate::chainstate::stacks::index::{
    Error as IndexError, FileMapping, MARFValue, ValueExtent, ValueExtentResolver,
};
use dedup_cache::DedupCache;

/// Extent file format discriminator; existing generations are never overwritten or truncated.
const FILE_MAGIC: &[u8; 8] = b"CLREXT01";
/// Record envelope discriminator.
const RECORD_MAGIC: &[u8; 8] = b"CLRVAL01";
/// File header length, including generation identity and reserved bytes.
const FILE_HEADER_LEN: u64 = 48;
/// Record header: magic, logical MARF value, payload/descriptor lengths, integrity hash.
const RECORD_HEADER_LEN: usize = 88;
/// Preserve record placement accepted by earlier chunk-mapped readers.
const RECORD_REGION_BYTES: u64 = 64 * 1024 * 1024;
/// Bound before allocating or mapping a supplied record.
const MAX_RECORD_BYTES: u64 = 32 * 1024 * 1024;

/// One generation of append-only values with a stable mapped prefix and retained EOF view.
#[derive(Debug)]
pub struct ValueExtentStore {
    /// File used for coordinated appends and map creation.
    file: File,
    /// Identity stored in every referencing leaf.
    store_id: [u8; 16],
    /// Whether this handle may append records.
    writable: bool,
    /// Published prefix snapshot; its reserved address range survives appends.
    mapping: Arc<ValueExtentBytes>,
    /// Contiguous alternate view for records ending in the partial EOF page.
    tail: Option<ValueExtentTail>,
    /// Published file length, including bytes not in the complete-page prefix.
    published_len: u64,
    /// Appended records accessible before the block publishes a new mapping.
    pending: HashMap<u64, Arc<ValueExtentBytes>>,
    /// Defer mapping growth until the active block commits.
    block_active: bool,
    /// Appended bytes that must be synced before this handle publishes references.
    needs_sync: bool,
    /// Count successful barriers in storage lifecycle tests.
    #[cfg(test)]
    sync_count: usize,
    /// Fail the next durability barrier in storage lifecycle tests.
    #[cfg(test)]
    fail_sync_once: bool,
    /// Committed hash-to-extent results shared by this store's related reopens.
    dedup_cache: DedupCache,
    /// Existing extents observed in this block, admitted only after successful SQLite commit.
    dedup_candidates: DedupCache,
    /// File length before beginning the SQLite write transaction; excludes its new appends.
    dedup_start: Option<u64>,
    /// Optional bounded speculative reader; never supplies authoritative index results.
    read_ahead: Option<read_ahead::ReadAhead>,
    /// Pending reader path, consumed only by the first sufficiently large miss batch.
    read_ahead_path: Option<PathBuf>,
    /// Immutable historical dedup base; new mappings remain in the writer SQLite transaction.
    ptrhash: Option<Arc<PtrHashBase>>,
}

/// Immutable storage for a file snapshot or a newly appended record.
#[derive(Debug)]
pub enum ValueExtentBytes {
    /// Fixed-length view over a shared, extensible file mapping.
    Mapped {
        /// Shared reservation or conventional fallback owning the bytes.
        mapping: FileMapping,
        /// Immutable length exposed by this particular snapshot.
        length: usize,
    },
    /// Contiguous EOF window retained independently across block publications.
    Tail(Mmap),
    /// Encoded record retained until the next block publication.
    Pending(Vec<u8>),
}

impl AsRef<[u8]> for ValueExtentBytes {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Mapped { mapping, length } => &mapping[..*length],
            Self::Tail(mapping) => mapping,
            Self::Pending(bytes) => bytes,
        }
    }
}

/// A cached EOF window covering any bounded record that reaches the partial page.
#[derive(Debug)]
struct ValueExtentTail {
    /// Absolute file offset corresponding to byte zero of this owner.
    offset: u64,
    /// Immutable mapping retained by records crossing the prefix boundary.
    bytes: Arc<ValueExtentBytes>,
}

/// A checked record borrowing its immutable mapping.
#[derive(Debug)]
pub struct ExtentValueRecord<O: PackedByteOwner> {
    /// Owner retained by projected packed values.
    mapping: Arc<O>,
    /// Binary V1 envelope and value payload within the owner.
    record: Range<usize>,
    /// Independently versioned reconstruction descriptor within the owner.
    descriptor: Range<usize>,
}

/// A verified, encoded immutable record ready for ordered append.
pub struct PreparedValueExtent {
    /// Canonical commitment used by write-side indexing.
    hash: MARFValue,
    /// Shared encoded bytes retained directly for pre-publication reads.
    record: Arc<ValueExtentBytes>,
}

impl PreparedValueExtent {
    /// Verify a source commitment and pack its canonical bytes exactly once.
    pub fn from_canonical(expected: MARFValue, canonical: &str) -> Result<Self, VmExecutionError> {
        if MARFValue::from_value(canonical) != expected {
            return Err(storage_error("source content hash mismatch"));
        }
        let encoded = binary_value_store::encode_migrated(canonical)?;
        Self::new(expected, encoded)
    }

    /// Build a bounded record from an admitted encoding.
    fn new(hash: MARFValue, encoded: EncodedRecord) -> Result<Self, VmExecutionError> {
        let record = encode_record(&hash, &encoded)?;
        if record.len() as u64 > MAX_RECORD_BYTES {
            return Err(storage_error("extent record exceeds size bound"));
        }
        Ok(Self {
            hash,
            record: Arc::new(ValueExtentBytes::Pending(record)),
        })
    }
}

impl ValueExtentStore {
    /// Create the generation-local write-side content index; ordinary reads never consult it.
    pub fn initialize_index(db: &Connection) -> Result<(), VmExecutionError> {
        db.execute_batch("CREATE TABLE IF NOT EXISTS clarity_extent_index (hash BLOB PRIMARY KEY CHECK(length(hash)=40), offset INTEGER NOT NULL CHECK(offset>=48), length INTEGER NOT NULL CHECK(length>0)) WITHOUT ROWID")
            .map_err(|error| storage_error(&error.to_string()))
    }

    /// Reuse committed content and duplicate batch entries within an existing SQLite write transaction.
    pub fn append_indexed(
        &mut self,
        db: &Connection,
        values: &[DataStoreValue],
    ) -> Result<Vec<(MARFValue, ValueExtent)>, VmExecutionError> {
        self.append_indexed_encoded(db, values, None)
    }

    /// Append deduplicated values, reusing any records already encoded for inline selection.
    pub fn append_indexed_encoded(
        &mut self,
        db: &Connection,
        values: &[DataStoreValue],
        mut encoded: Option<&mut [Option<EncodedRecord>]>,
    ) -> Result<Vec<(MARFValue, ValueExtent)>, VmExecutionError> {
        if encoded
            .as_ref()
            .is_some_and(|records| records.len() != values.len())
        {
            return Err(storage_error("prepared record count mismatch"));
        }
        let _writeback_diagnostic =
            stacks_profiler::diagnostic_span!("Writeback: Dedup and encode");
        if !self.writable || db.is_autocommit() {
            return Err(storage_error(
                "deduplicated append requires a writable transaction",
            ));
        }
        let mut positions = HashMap::new();
        let mut requests = Vec::with_capacity(values.len());
        let mut hashes = Vec::new();
        let mut extents = Vec::new();
        let mut missing_slots = Vec::new();
        let mut prepared = Vec::new();
        let mut unique_values = Vec::new();
        for (index, value) in values.iter().enumerate() {
            let hash = {
                let _phase = stacks_profiler::diagnostic_span!("Writeback: Value commitment");
                MARFValue::from_value(value.canonical())
            };
            if let Some(&slot) = positions.get(&hash.0) {
                stacks_profiler::diagnostics::count("dedup_batch_hits", 1);
                requests.push(slot);
                continue;
            }
            let slot = hashes.len();
            positions.insert(hash.0, slot);
            requests.push(slot);
            let cached = self.cached_extent(&hash)?;
            stacks_profiler::diagnostics::count(
                if cached.is_some() {
                    "dedup_cache_hits"
                } else {
                    "dedup_cache_misses"
                },
                1,
            );
            hashes.push(hash);
            extents.push(cached);
            unique_values.push((index, value));
        }
        if let Some(base) = self.ptrhash.clone() {
            #[cfg(feature = "commit-residency-diagnostics")]
            if extent_ptrhash::diagnostics::enabled() {
                static FIRST: std::sync::atomic::AtomicBool =
                    std::sync::atomic::AtomicBool::new(true);
                if FIRST.swap(false, std::sync::atomic::Ordering::Relaxed) {
                    base.report_residency("first-measured");
                    if let Ok((pages, resident)) =
                        extent_ptrhash::diagnostics::residency(self.mapping.as_ref().as_ref())
                    {
                        eprintln!(
                            "VALUE_RESIDENCY {}",
                            serde_json::json!({"phase":"first-measured","pages":pages,"resident_pages":resident,"page_bytes":extent_ptrhash::diagnostics::page_size()})
                        );
                    }
                }
            }

            let _phase = stacks_profiler::diagnostic_span!("Writeback: Dedup PtrHash lookup");
            for (slot, hash) in hashes.iter().enumerate() {
                if extents[slot].is_some() {
                    continue;
                }
                stacks_profiler::diagnostics::count("dedup_ptrhash_queries", 1);
                let Some(location) = base
                    .candidate(&hash.0)
                    .map_err(|error| storage_error(&error.to_string()))?
                else {
                    stacks_profiler::diagnostics::count("dedup_ptrhash_negatives", 1);
                    continue;
                };
                let extent = ValueExtent {
                    store_id: self.store_id,
                    offset: location.offset,
                    length: location.length,
                };
                #[cfg(feature = "commit-residency-diagnostics")]
                extent_ptrhash::diagnostics::header(extent.offset);
                #[cfg(feature = "commit-residency-diagnostics")]
                let _header = stacks_profiler::diagnostic_span!("PtrHash: Header verification");
                let record = self.read_at(extent)?;
                if record.commitment() != *hash {
                    stacks_profiler::diagnostics::count("dedup_ptrhash_fingerprint_collisions", 1);
                    continue;
                }
                stacks_profiler::diagnostics::count("dedup_ptrhash_hits", 1);
                stacks_profiler::diagnostics::count("dedup_index_hits", 1);
                if self.dedup_start.is_some_and(|start| extent.offset < start) {
                    self.dedup_candidates.insert(hash.0, extent);
                }
                extents[slot] = Some(extent);
            }
        }
        let misses: Vec<usize> = extents
            .iter()
            .enumerate()
            .filter_map(|(i, extent)| extent.is_none().then_some(i))
            .collect();
        if misses.len() >= 8 {
            if let Some(path) = self.read_ahead_path.take() {
                self.read_ahead = read_ahead::ReadAhead::open(path).ok();
                if self.read_ahead.is_none() {
                    stacks_profiler::diagnostics::count("dedup_prefetch_unavailable", 1);
                }
            }
        }
        let mut lookup = db
            .prepare_cached(if self.ptrhash.is_some() {
                "SELECT offset,length FROM clarity_extent_delta WHERE hash=?1"
            } else {
                "SELECT offset,length FROM clarity_extent_index WHERE hash=?1"
            })
            .map_err(|error| storage_error(&error.to_string()))?;
        for window in misses.chunks(read_ahead::WINDOW * 2) {
            let split = window.len() / 2;
            let mut keys = [[0; 40]; read_ahead::WINDOW];
            let mut flight = if window.len() >= 8 {
                self.read_ahead.as_ref().and_then(|reader| {
                    for (key, &slot) in keys.iter_mut().zip(&window[split..]) {
                        *key = hashes[slot].0;
                    }
                    reader.start(&keys[..window.len() - split])
                })
            } else {
                None
            };
            for (i, &slot) in window.iter().enumerate() {
                if i == split {
                    if let Some(flight) = flight.take() {
                        let _wait =
                            stacks_profiler::diagnostic_span!("Writeback: Dedup read-ahead wait");
                        let stats = flight.finish();
                        stacks_profiler::diagnostics::count(
                            "dedup_prefetch_queries",
                            stats.queries,
                        );
                        stacks_profiler::diagnostics::count("dedup_prefetch_errors", stats.errors);
                        stacks_profiler::diagnostics::count(
                            "dedup_prefetch_worker_ns",
                            stats.nanos,
                        );
                        #[cfg(feature = "dedup-io-diagnostics")]
                        stacks_profiler::diagnostics::maximum(
                            "dedup_prefetch_pager_peak_bytes",
                            stats.pager_bytes,
                        );
                    }
                }
                let hash = &hashes[slot];
                let location: Option<(u64, u64)> = {
                    let _lookup =
                        stacks_profiler::diagnostic_span!("Writeback: Dedup SQLite lookup");
                    #[cfg(feature = "dedup-io-diagnostics")]
                    let _io = io_probe::QueryProbe::begin(db);
                    lookup
                        .query_row([hash.0.as_slice()], |row| Ok((row.get(0)?, row.get(1)?)))
                        .optional()
                        .map_err(|error| storage_error(&error.to_string()))?
                };
                if let Some((offset, length)) = location {
                    stacks_profiler::diagnostics::count("dedup_index_hits", 1);
                    let extent = ValueExtent {
                        store_id: self.store_id,
                        offset,
                        length,
                    };
                    self.read(extent, hash)?;
                    if self.dedup_start.is_some_and(|start| offset < start) {
                        self.dedup_candidates.insert(hash.0, extent);
                    }
                    extents[slot] = Some(extent);
                } else {
                    stacks_profiler::diagnostics::count("dedup_index_misses", 1);
                    let _encode = stacks_profiler::diagnostic_span!("Writeback: Packed encoding");
                    missing_slots.push(slot);
                    prepared.push(PreparedValueExtent::new(
                        hash.clone(),
                        match encoded
                            .as_deref_mut()
                            .and_then(|records| records[unique_values[slot].0].take())
                        {
                            Some(record) => record,
                            None => binary_value_store::encode_entry(unique_values[slot].1)?,
                        },
                    )?);
                }
            }
        }
        let appended = self.append_prepared(&prepared)?;
        let _index = stacks_profiler::diagnostic_span!("Writeback: Dedup SQLite insert");
        let mut insert = db
            .prepare_cached(if self.ptrhash.is_some() {
                "INSERT INTO clarity_extent_delta VALUES(?1,?2,?3)"
            } else {
                "INSERT INTO clarity_extent_index VALUES(?1,?2,?3)"
            })
            .map_err(|error| storage_error(&error.to_string()))?;
        for (slot, (hash, extent)) in missing_slots.into_iter().zip(appended) {
            insert
                .execute(params![hash.0.as_slice(), extent.offset, extent.length])
                .map_err(|error| storage_error(&error.to_string()))?;
            extents[slot] = Some(extent);
        }
        requests
            .into_iter()
            .map(|slot| {
                Ok((
                    hashes[slot].clone(),
                    extents[slot].ok_or_else(|| storage_error("missing deduplicated extent"))?,
                ))
            })
            .collect()
    }

    /// Verify a prefix hint against its full stored commitment, without hashing the payload.
    fn cached_extent(&mut self, hash: &MARFValue) -> Result<Option<ValueExtent>, VmExecutionError> {
        let Some(extent) = self.dedup_start.and_then(|_| self.dedup_cache.get(&hash.0)) else {
            return Ok(None);
        };
        // Envelope failures remain storage errors. Only a valid different commitment is a collision.
        let record = self.read_at(extent)?;
        if record.commitment() == *hash {
            Ok(Some(extent))
        } else {
            self.dedup_cache.mark_multiple(&hash.0);
            stacks_profiler::diagnostics::count("dedup_prefix_collisions", 1);
            Ok(None)
        }
    }

    /// Open the generation explicitly registered by a database; stray files never activate it.
    pub fn open_registered(
        db: &Connection,
        db_path: &Path,
        writable: bool,
    ) -> Result<Option<Self>, VmExecutionError> {
        let enabled: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='clarity_extent_format')", [], |row| row.get(0))
            .map_err(|error| storage_error(&error.to_string()))?;
        if !enabled {
            return Ok(None);
        }
        let (version, identity): (i64, Vec<u8>) = db
            .query_row(
                "SELECT version,store_id FROM clarity_extent_format WHERE singleton=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|error| storage_error(&error.to_string()))?;
        if version != 1 {
            return Err(storage_error("unsupported extent format"));
        }
        let indexed: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='clarity_extent_index')", [], |row| row.get(0)).map_err(|error| storage_error(&error.to_string()))?;
        if !indexed {
            return Err(storage_error(
                "extent generation lacks its write-side index",
            ));
        }
        let value_path = db_path.with_file_name(format!(
            "{}.values",
            db_path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| storage_error("invalid extent database path"))?
        ));
        let mut store = Self::open_existing(&value_path, writable)?;
        if identity.as_slice() != store.store_id() {
            return Err(storage_error(
                "extent generation disagrees with database marker",
            ));
        }
        store.ptrhash = PtrHashBase::registered(db, &store.store_id, store.published_len)
            .map_err(|error| storage_error(&error.to_string()))?;
        let enabled = writable && store.ptrhash.is_none();
        #[cfg(feature = "dedup-io-diagnostics")]
        let enabled =
            enabled && std::env::var_os("STACKS_DEDUP_READ_AHEAD").is_none_or(|v| v != "0");
        if enabled {
            store.read_ahead_path = Some(db_path.to_path_buf());
        }
        Ok(Some(store))
    }

    /// Open an existing generation, or create and durably initialize an empty writable one.
    pub fn open(path: &Path, writable: bool) -> Result<Self, VmExecutionError> {
        Self::open_inner(path, writable, writable)
    }

    /// Open an activated store without creating a replacement for a missing generation.
    pub fn open_existing(path: &Path, writable: bool) -> Result<Self, VmExecutionError> {
        Self::open_inner(path, writable, false)
    }

    /// Read the persistent identity used by the database completion marker.
    pub fn store_id(&self) -> [u8; 16] {
        self.store_id
    }

    /// Initialize or validate a generation under the file's append lock.
    fn open_inner(path: &Path, writable: bool, create: bool) -> Result<Self, VmExecutionError> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(writable)
            .create(create)
            .open(path)
            .map_err(io_error)?;
        if writable {
            file.lock().map_err(io_error)?;
        }
        let result = (|| {
            if file.metadata().map_err(io_error)?.len() == 0 && create {
                let mut header = [0u8; FILE_HEADER_LEN as usize];
                header[..8].copy_from_slice(FILE_MAGIC);
                rand::thread_rng().fill_bytes(&mut header[8..24]);
                file.write_all(&header).map_err(io_error)?;
                file.sync_all().map_err(io_error)?;
                if let Some(parent) = path.parent() {
                    File::open(parent)
                        .and_then(|directory| directory.sync_all())
                        .map_err(io_error)?;
                }
            }
            file.seek(SeekFrom::Start(0)).map_err(io_error)?;
            let mut header = [0; FILE_HEADER_LEN as usize];
            file.read_exact(&mut header).map_err(io_error)?;
            if &header[..8] != FILE_MAGIC || header[24..].iter().any(|byte| *byte != 0) {
                return Err(storage_error("invalid extent file header"));
            }
            let mut id = [0; 16];
            id.copy_from_slice(&header[8..24]);
            Ok(id)
        })();
        if writable {
            file.unlock().map_err(io_error)?;
        }
        let store_id = result?;
        // SAFETY: this generation only appends; its existing bytes are immutable.
        let mapping = unsafe { FileMapping::map(&file) }.map_err(io_error)?;
        let (mapping, tail, published_len) = Self::snapshot(&file, mapping)?;
        Ok(Self {
            file,
            store_id,
            writable,
            mapping,
            tail,
            published_len,
            pending: HashMap::new(),
            block_active: false,
            needs_sync: false,
            #[cfg(test)]
            sync_count: 0,
            #[cfg(test)]
            fail_sync_once: false,
            dedup_cache: DedupCache::new(65_536, store_id),
            dedup_candidates: DedupCache::new(65_536, store_id),
            dedup_start: None,
            read_ahead: None,
            read_ahead_path: None,
            ptrhash: None,
        })
    }

    /// Freeze the exposed prefix length and cache a contiguous EOF window without reading it.
    fn snapshot(
        file: &File,
        mapping: FileMapping,
    ) -> Result<(Arc<ValueExtentBytes>, Option<ValueExtentTail>, u64), VmExecutionError> {
        let file_len = file.metadata().map_err(io_error)?.len();
        let prefix_len = mapping.len().min(
            usize::try_from(file_len)
                .map_err(|_| storage_error("extent file exceeds address space"))?,
        );
        let tail = if (prefix_len as u64) < file_len {
            // Any valid record ending after the prefix starts within MAX_RECORD_BYTES of it.
            let offset = (prefix_len as u64).saturating_sub(MAX_RECORD_BYTES);
            let length = usize::try_from(file_len - offset)
                .map_err(|_| storage_error("extent tail exceeds address space"))?;
            // SAFETY: the checked window is already in the immutable append-only file. Its
            // mapping stays alive for retained records, including after a newer tail is mapped.
            let mapping = unsafe { MmapOptions::new().offset(offset).len(length).map(file) }
                .map_err(io_error)?;
            Some(ValueExtentTail {
                offset,
                bytes: Arc::new(ValueExtentBytes::Tail(mapping)),
            })
        } else {
            None
        };
        Ok((
            Arc::new(ValueExtentBytes::Mapped {
                mapping,
                length: prefix_len,
            }),
            tail,
            file_len,
        ))
    }

    /// Retain subsequent appends in memory until the block publishes its file snapshot.
    pub fn begin_block(&mut self) -> Result<(), VmExecutionError> {
        // A different top-level writer may have published a block since this handle opened.
        self.publish_block()?;
        self.block_active = true;
        self.dedup_candidates.clear();
        self.dedup_start = Some(self.published_len);
        Ok(())
    }

    /// Durably publish this block's values before its trie references can commit.
    pub fn publish_block(&mut self) -> Result<(), VmExecutionError> {
        if self.needs_sync {
            self.file.lock().map_err(io_error)?;
            let result = self.sync_dirty_locked();
            let unlock = self.file.unlock().map_err(io_error);
            result?;
            unlock?;
        }
        self.refresh_mapping()?;
        self.pending.clear();
        self.block_active = false;
        Ok(())
    }

    /// Refresh externally appended bytes without publishing this handle's pending block.
    pub fn refresh_mapping(&mut self) -> Result<(), VmExecutionError> {
        let length = self.file.metadata().map_err(io_error)?.len();
        if length != self.published_len {
            let ValueExtentBytes::Mapped { mapping, .. } = self.mapping.as_ref() else {
                unreachable!("published prefix must be a file mapping");
            };
            let mut mapping = mapping.clone();
            // SAFETY: this is the same append-only file. Refresh maps only the unexposed suffix;
            // retained snapshots own their prefix or the old conventional fallback independently.
            unsafe { mapping.refresh(&self.file) }.map_err(io_error)?;
            let (mapping, tail, published_len) = Self::snapshot(&self.file, mapping)?;
            self.mapping = mapping;
            self.tail = tail;
            self.published_len = published_len;
        }
        Ok(())
    }

    /// Admit existing index rows only after the associated SQLite transaction commits.
    pub fn commit_dedup_cache(&mut self) {
        for entry in self.dedup_candidates.entries() {
            self.dedup_cache.admit(entry);
        }
        self.dedup_candidates.clear();
        self.dedup_start = None;
    }

    /// Discard unreachable records after a block rollback; retained values remain valid.
    pub fn discard_block(&mut self) {
        self.dedup_candidates.clear();
        self.dedup_start = None;
        self.pending.clear();
        self.block_active = false;
    }

    /// Append a batch; standalone calls are durable before returning their locators.
    /// Failed or rolled-back batches may leave unreachable records, but never reuse their offsets.
    pub fn append(
        &mut self,
        values: &[DataStoreValue],
    ) -> Result<Vec<(MARFValue, ValueExtent)>, VmExecutionError> {
        let prepared = values
            .iter()
            .map(|value| {
                let hash = MARFValue::from_value(value.canonical());
                PreparedValueExtent::new(hash, binary_value_store::encode_entry(value)?)
            })
            .collect::<Result<Vec<_>, VmExecutionError>>()?;
        self.append_prepared(&prepared)
    }

    /// Append prepared records under the cross-process lock; defer block-owned sync.
    pub fn append_prepared(
        &mut self,
        values: &[PreparedValueExtent],
    ) -> Result<Vec<(MARFValue, ValueExtent)>, VmExecutionError> {
        if !self.writable {
            return Err(storage_error("read-only extent store"));
        }
        if values.is_empty() {
            return Ok(Vec::new());
        }
        let _lock = stacks_profiler::diagnostic_span!("Writeback: Append lock wait");
        self.file.lock().map_err(io_error)?;
        drop(_lock);
        stacks_profiler::diagnostics::count("append_batches", 1);
        stacks_profiler::diagnostics::count("appended_records", values.len() as u64);
        let result = self.append_locked(values).and_then(|result| {
            if !self.block_active {
                self.sync_dirty_locked()?;
            }
            Ok(result)
        });
        let unlock = self.file.unlock().map_err(io_error);
        let result = result?;
        unlock?;
        if self.block_active {
            for ((_, extent), value) in result.iter().zip(values) {
                self.pending
                    .insert(extent.offset, Arc::clone(&value.record));
            }
        } else {
            // Standalone appends, including migration batches, are publication boundaries.
            self.publish_block()?;
        }
        Ok(result)
    }

    /// Buffer sequential writes while holding the append lock.
    fn append_locked(
        &mut self,
        values: &[PreparedValueExtent],
    ) -> Result<Vec<(MARFValue, ValueExtent)>, VmExecutionError> {
        let _write = stacks_profiler::diagnostic_span!("Writeback: Append bytes");
        let mut result = Vec::with_capacity(values.len());
        let mut offset = self.file.seek(SeekFrom::End(0)).map_err(io_error)?;
        let mut writer = BufWriter::with_capacity(1024 * 1024, &mut self.file);
        // A partial write still leaves an unsynced suffix; only a successful publication
        // may clear this marker. Offsets are never reused after rollback.
        self.needs_sync = true;
        for value in values {
            let record = value.record.as_ref().as_ref();
            let length = record.len() as u64;
            stacks_profiler::diagnostics::count("appended_bytes", length);
            if offset % RECORD_REGION_BYTES + length > RECORD_REGION_BYTES {
                offset = offset
                    .checked_add(RECORD_REGION_BYTES - offset % RECORD_REGION_BYTES)
                    .ok_or_else(|| storage_error("extent offset overflow"))?;
                writer.seek(SeekFrom::Start(offset)).map_err(io_error)?;
            }
            writer.write_all(record).map_err(io_error)?;
            result.push((
                value.hash.clone(),
                ValueExtent {
                    store_id: self.store_id,
                    offset,
                    length,
                },
            ));
            offset = offset
                .checked_add(length)
                .ok_or_else(|| storage_error("extent offset overflow"))?;
        }
        writer.flush().map_err(io_error)?;
        drop(writer);
        Ok(result)
    }

    /// Sync all appends through the current file frontier while holding its append lock.
    fn sync_dirty_locked(&mut self) -> Result<(), VmExecutionError> {
        if !self.needs_sync {
            return Ok(());
        }
        let _sync = stacks_profiler::diagnostic_span!("Writeback: Value file sync");
        stacks_profiler::diagnostics::count("value_sync_calls", 1);
        #[cfg(test)]
        if std::mem::take(&mut self.fail_sync_once) {
            return Err(storage_error("injected value sync failure"));
        }
        self.file.sync_data().map_err(io_error)?;
        self.needs_sync = false;
        #[cfg(test)]
        {
            self.sync_count += 1;
        }
        Ok(())
    }

    /// Read a directly addressed record, checking its generation, bounds and hash label.
    pub fn read(
        &self,
        extent: ValueExtent,
        expected: &MARFValue,
    ) -> Result<MappedValueRecord, VmExecutionError> {
        self.read_checked(extent, Some(expected))
    }

    /// Fetch an immutable value by its physical locator without reconstructing its commitment.
    pub fn read_at(&self, extent: ValueExtent) -> Result<MappedValueRecord, VmExecutionError> {
        self.read_checked(extent, None)
    }

    /// Validate the record envelope and retain its mapping.
    #[stacks_profiler::profile(name = "Clarity value extent fetch")]
    fn read_checked(
        &self,
        extent: ValueExtent,
        expected: Option<&MARFValue>,
    ) -> Result<MappedValueRecord, VmExecutionError> {
        if extent.store_id != self.store_id
            || extent.offset < FILE_HEADER_LEN
            || !(RECORD_HEADER_LEN as u64..=MAX_RECORD_BYTES).contains(&extent.length)
        {
            return Err(storage_error("invalid extent identity or bounds"));
        }
        let end = extent
            .offset
            .checked_add(extent.length)
            .ok_or_else(|| storage_error("extent bounds overflow"))?;
        let (mapping, local, required) = if end <= self.mapping.as_ref().as_ref().len() as u64 {
            (
                Arc::clone(&self.mapping),
                usize::try_from(extent.offset)
                    .map_err(|_| storage_error("extent offset overflow"))?,
                usize::try_from(end).map_err(|_| storage_error("extent address overflow"))?,
            )
        } else if end <= self.published_len {
            let tail = self
                .tail
                .as_ref()
                .ok_or_else(|| storage_error("missing extent tail"))?;
            let local = extent
                .offset
                .checked_sub(tail.offset)
                .ok_or_else(|| storage_error("extent exceeds tail window"))?;
            let end = end - tail.offset;
            (
                Arc::clone(&tail.bytes),
                usize::try_from(local).map_err(|_| storage_error("extent offset overflow"))?,
                usize::try_from(end).map_err(|_| storage_error("extent address overflow"))?,
            )
        } else {
            let mapping = self
                .pending
                .get(&extent.offset)
                .ok_or_else(|| storage_error("extent exceeds published file snapshot"))?;
            let length = usize::try_from(extent.length)
                .map_err(|_| storage_error("extent length overflow"))?;
            if length != mapping.as_ref().as_ref().len() {
                return Err(storage_error("invalid pending extent length"));
            }
            (Arc::clone(mapping), 0, length)
        };
        let bytes = mapping
            .as_ref()
            .as_ref()
            .get(local..required)
            .ok_or_else(|| storage_error("unmapped extent range"))?;
        if &bytes[..8] != RECORD_MAGIC
            || expected.is_some_and(|value| &bytes[8..48] != value.as_bytes())
        {
            return Err(storage_error("extent does not match leaf commitment"));
        }
        let record_len =
            u32::from_le_bytes(bytes[48..52].try_into().expect("checked header")) as usize;
        let descriptor_len =
            u32::from_le_bytes(bytes[52..56].try_into().expect("checked header")) as usize;
        if record_len
            .checked_add(descriptor_len)
            .and_then(|len| len.checked_add(RECORD_HEADER_LEN))
            != Some(bytes.len())
        {
            return Err(storage_error("invalid extent payload lengths"));
        }
        let payload = local + RECORD_HEADER_LEN;
        Ok(MappedValueRecord {
            mapping,
            record: payload..payload + record_len,
            descriptor: payload + record_len..required,
        })
    }
}

impl ValueExtentResolver for Mutex<ValueExtentStore> {
    fn inline_commitment(&self, value: &InlineValue) -> Result<MARFValue, IndexError> {
        InlineValueRecord::from_inline(value)
            .commitment()
            .map_err(|error| IndexError::CorruptionError(error.to_string()))
    }
    fn commitment(&self, extent: ValueExtent) -> Result<MARFValue, IndexError> {
        let record = self
            .lock()
            .map_err(|_| IndexError::CorruptionError("Value extent lock poisoned".into()))?
            .read_at(extent)
            .map_err(|error| IndexError::CorruptionError(error.to_string()))?;
        Ok(record.commitment())
    }
}

/// Value record retained by a file snapshot or the current block's appended bytes.
pub type MappedValueRecord = ExtentValueRecord<ValueExtentBytes>;
/// Value record retained entirely in RAM for ephemeral execution.
pub type OwnedValueRecord = ExtentValueRecord<Vec<u8>>;
/// Inline record retaining the trie mapping or its pending/fallback byte owner.
pub type InlineValueRecord = ExtentValueRecord<InlineValueBytes>;

impl InlineValueRecord {
    /// Reuse the extent decoder over the inline record's immutable owner and checked ranges.
    pub fn from_inline(value: &InlineValue) -> Self {
        Self {
            mapping: value.owner(),
            record: value.record_range(),
            descriptor: value.descriptor_range(),
        }
    }

    /// Reconstruct the logical commitment only when a generic MARF/hash/proof read needs it.
    pub fn commitment(&self) -> Result<MARFValue, VmExecutionError> {
        self.canonical()
            .map(|canonical| MARFValue::from_value(&canonical))
    }
}

impl MappedValueRecord {
    /// Copy an eligible record for trie migration, inspecting lengths before touching payload pages.
    pub fn inline_candidate(&self) -> Result<Option<InlineValue>, VmExecutionError> {
        if !InlineValue::fits_inline(self.record.len(), self.descriptor.len()) {
            return Ok(None);
        }
        let bytes = self.mapping.as_ref().as_ref();
        InlineValue::from_parts(&bytes[self.record.clone()], &bytes[self.descriptor.clone()])
            .map(Some)
            .map_err(|error| storage_error(&error.to_string()))
    }

    /// Read the admitted record's original commitment without accessing its payload.
    fn commitment(&self) -> MARFValue {
        // read_checked validated the complete envelope before constructing this view.
        let start = self.record.start - RECORD_HEADER_LEN + RECORD_MAGIC.len();
        MARFValue(
            self.mapping.as_ref().as_ref()[start..start + 40]
                .try_into()
                .expect("validated extent commitment"),
        )
    }
}

impl OwnedValueRecord {
    /// Encode one ephemeral value without creating a SQLite value row.
    pub fn from_value(value: &DataStoreValue) -> Result<Self, VmExecutionError> {
        let encoded = binary_value_store::encode_entry(value)?;
        let mut bytes = encoded.record().to_vec();
        let record_end = bytes.len();
        bytes.extend_from_slice(encoded.shape().unwrap_or_default());
        let end = bytes.len();
        Ok(Self {
            mapping: Arc::new(bytes),
            record: 0..record_end,
            descriptor: record_end..end,
        })
    }
}

impl<O: PackedByteOwner + 'static> ExtentValueRecord<O> {
    /// Reconstruct exact canonical text for generic backing-store consumers.
    pub fn canonical(&self) -> Result<String, VmExecutionError> {
        let record = &self.mapping.as_ref().as_ref()[self.record.clone()];
        let Some((&kind, payload)) = record
            .strip_prefix(&[1])
            .and_then(|record| record.split_first())
        else {
            return Err(storage_error("invalid value extent envelope"));
        };
        match kind {
            0 => String::from_utf8(payload.to_vec())
                .map_err(|_| storage_error("invalid canonical UTF-8")),
            1 => Ok(to_hex(payload)),
            2 => PackedValueRef::parse(payload)
                .and_then(|packed| {
                    packed.reconstruct_consensus(
                        &self.mapping.as_ref().as_ref()[self.descriptor.clone()],
                    )
                })
                .map(|consensus| to_hex(&consensus))
                .map_err(|error| storage_error(&error.to_string())),
            _ => Err(storage_error("unknown value extent kind")),
        }
    }

    /// Return a mapped packed value, with historical schema projection handled by owned fallback.
    pub fn stored(
        &self,
        expected: &TypeSignature,
        epoch: &StacksEpochId,
    ) -> Result<StoredValueResult, VmExecutionError> {
        let record = &self.mapping.as_ref().as_ref()[self.record.clone()];
        if record.starts_with(&[1, 2])
            && PackedValueRef::parse(&record[2..])
                .and_then(|packed| {
                    packed.matches_storage_schema(
                        &self.mapping.as_ref().as_ref()[self.descriptor.clone()],
                        expected,
                    )
                })
                .map_err(|error| storage_error(&error.to_string()))?
        {
            // The encoder/migrator admitted these immutable bytes. The VM supplies the declared
            // storage type; differing historical shapes use canonical projection below.
            match SharedPackedValue::from_encoded_owner(
                Arc::clone(&self.mapping),
                self.record.start + 2..self.record.end,
                expected,
                epoch,
            ) {
                Ok(value) => {
                    return Ok(StoredValueResult {
                        serialized_byte_len: u64::from(value.consensus_byte_len()),
                        #[cfg(not(feature = "direct-value-eager"))]
                        value: StoredValue::Packed(value),
                        #[cfg(feature = "direct-value-eager")]
                        value: StoredValue::Owned(
                            value
                                .to_owned_value()
                                .map_err(|error| storage_error(&error.to_string()))?,
                        ),
                    });
                }
                Err(PackedValueError::Invariant(error)) => {
                    return Err(storage_error(&error.to_string()));
                }
                Err(_) => {}
            }
        }
        let canonical = self.canonical()?;
        let bytes = hex_bytes(&canonical)
            .map_err(|_| storage_error("typed extent is not consensus bytes"))?;
        let value = Value::try_deserialize_bytes_at_epoch(&bytes, expected, epoch)
            .map_err(|error| storage_error(&error.to_string()))?;
        Ok(StoredValueResult {
            serialized_byte_len: bytes.len() as u64,
            value: StoredValue::Owned(value),
        })
    }
}

/// Build an integrity-protected record from already derived canonical encoding.
fn encode_record(hash: &MARFValue, encoded: &EncodedRecord) -> Result<Vec<u8>, VmExecutionError> {
    let record = encoded.record();
    let descriptor = encoded.shape().unwrap_or_default();
    let mut bytes = Vec::with_capacity(RECORD_HEADER_LEN + record.len() + descriptor.len());
    bytes.extend_from_slice(RECORD_MAGIC);
    bytes.extend_from_slice(hash.as_bytes());
    bytes.extend_from_slice(
        &u32::try_from(record.len())
            .map_err(|_| storage_error("record too large"))?
            .to_le_bytes(),
    );
    bytes.extend_from_slice(
        &u32::try_from(descriptor.len())
            .map_err(|_| storage_error("descriptor too large"))?
            .to_le_bytes(),
    );
    let digest = Sha256::new()
        .chain_update(&bytes)
        .chain_update(record)
        .chain_update(descriptor)
        .finalize();
    bytes.extend_from_slice(&digest);
    bytes.extend_from_slice(record);
    bytes.extend_from_slice(descriptor);
    Ok(bytes)
}

/// Translate filesystem errors to backing-store failures.
fn io_error(error: io::Error) -> VmExecutionError {
    storage_error(&error.to_string())
}

/// Construct a storage failure without accepting a partially read value.
fn storage_error(message: &str) -> VmExecutionError {
    VmInternalError::DBError(message.into()).into()
}

#[cfg(test)]
mod tests {
    use clarity::vm::database::TypedValueData;
    use tempfile::tempdir;

    use super::*;
    use std::fs;

    /// Multiple transaction batches require one barrier at publication, and clean blocks none.
    #[test]
    fn block_batches_share_one_durability_barrier() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("values");
        let mut store = ValueExtentStore::open(&path, true).unwrap();
        store.begin_block().unwrap();
        for value in ["first", "second", "third"] {
            let (hash, extent) = store
                .append(&[DataStoreValue::Canonical(value.into())])
                .unwrap()
                .pop()
                .unwrap();
            assert_eq!(store.sync_count, 0);
            assert_eq!(
                store.read(extent, &hash).unwrap().canonical().unwrap(),
                value
            );
        }
        assert!(store.needs_sync);
        store.publish_block().unwrap();
        assert_eq!(store.sync_count, 1);
        assert!(!store.needs_sync);
        store.begin_block().unwrap();
        store.publish_block().unwrap();
        assert_eq!(store.sync_count, 1);
        store
            .append(&[DataStoreValue::Canonical("standalone".into())])
            .unwrap();
        assert_eq!(store.sync_count, 2);
    }

    /// A failed barrier leaves pending owners intact and blocks reference publication.
    #[test]
    fn failed_barrier_can_retry_before_reference_commit() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("values");
        let db_path = dir.path().join("index.sqlite");
        let db = Connection::open(&db_path).unwrap();
        ValueExtentStore::initialize_index(&db).unwrap();
        let mut store = ValueExtentStore::open(&path, true).unwrap();
        store.begin_block().unwrap();
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        let (hash, extent) = store
            .append_indexed(&db, &[DataStoreValue::Canonical("pending".into())])
            .unwrap()
            .pop()
            .unwrap();
        store.fail_sync_once = true;
        assert!(store.publish_block().is_err());
        assert!(store.block_active && store.needs_sync);
        assert_eq!(store.sync_count, 0);
        assert_eq!(
            store.read(extent, &hash).unwrap().canonical().unwrap(),
            "pending"
        );
        let observer = Connection::open(&db_path).unwrap();
        let visible: u64 = observer
            .query_row("SELECT COUNT(*) FROM clarity_extent_index", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(visible, 0);
        store.publish_block().unwrap();
        assert_eq!(store.sync_count, 1);
        db.execute_batch("COMMIT").unwrap();
        let visible: u64 = observer
            .query_row("SELECT COUNT(*) FROM clarity_extent_index", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(visible, 1);
        drop(store);
        let reopened = ValueExtentStore::open_existing(&path, false).unwrap();
        assert_eq!(
            reopened.read(extent, &hash).unwrap().canonical().unwrap(),
            "pending"
        );
    }

    /// Mapping refresh never syncs or retires an active block's pending values.
    #[test]
    fn mapping_refresh_preserves_pending_block() {
        let dir = tempdir().unwrap();
        let mut store = ValueExtentStore::open(&dir.path().join("values"), true).unwrap();
        store.begin_block().unwrap();
        let (hash, extent) = store
            .append(&[DataStoreValue::Canonical("pending".into())])
            .unwrap()
            .pop()
            .unwrap();
        store.refresh_mapping().unwrap();
        assert!(store.block_active && store.needs_sync);
        assert_eq!(store.sync_count, 0);
        assert_eq!(
            store.read(extent, &hash).unwrap().canonical().unwrap(),
            "pending"
        );
        store.publish_block().unwrap();
        assert_eq!(store.sync_count, 1);
    }

    /// Inline generic reads retain exact canonical spelling and the original commitment.
    #[test]
    fn inline_record_reuses_canonical_reconstruction() {
        for canonical in [
            "",
            "metadata",
            "00FF",
            "03",
            "0100000000000000000000000000000001",
        ] {
            let value = DataStoreValue::Canonical(canonical.into());
            let encoded = binary_value_store::encode_entry(&value).unwrap();
            let inline =
                InlineValue::from_parts(encoded.record(), encoded.shape().unwrap_or_default())
                    .unwrap();
            let record = InlineValueRecord::from_inline(&inline);
            assert_eq!(record.canonical().unwrap(), canonical);
            assert_eq!(
                record.commitment().unwrap(),
                MARFValue::from_value(canonical)
            );
        }
    }

    /// A VM projection retains the original mapped payload after every read handle is dropped.
    #[cfg(not(feature = "direct-value-eager"))]
    #[test]
    fn inline_vm_projection_retains_original_mapping() {
        let expected_value = Value::buff_from(vec![3, 5, 7]).unwrap();
        let expected = TypeSignature::type_of(&expected_value).unwrap();
        let value = DataStoreValue::Typed(TypedValueData::prepare(expected_value.clone()).unwrap());
        let encoded = binary_value_store::encode_entry(&value).unwrap();
        let descriptor = encoded.shape().unwrap_or_default();
        assert!(InlineValue::fits_inline(
            encoded.record().len(),
            descriptor.len()
        ));
        let mut file = tempfile::tempfile().unwrap();
        file.set_len(128 * 1024).unwrap();
        file.seek(SeekFrom::Start(64)).unwrap();
        file.write_all(encoded.record()).unwrap();
        file.write_all(descriptor).unwrap();
        file.flush().unwrap();
        // SAFETY: The mapped file remains immutable for the lifetime of every retained view.
        let mapping = unsafe { FileMapping::map(&file).unwrap() };
        let length = encoded.record().len() + descriptor.len();
        let range_start = mapping[64..].as_ptr() as usize;
        let inline = InlineValue::from_mapping(
            mapping.clone(),
            64..64 + length,
            encoded.record().len() as u8,
        )
        .unwrap();
        let record = InlineValueRecord::from_inline(&inline);
        assert_eq!(
            record.commitment().unwrap(),
            MARFValue::from_value(value.canonical())
        );
        let stored = record.stored(&expected, &StacksEpochId::latest()).unwrap();
        let StoredValue::Packed(retained) = stored.value else {
            panic!("expected borrowed inline value")
        };
        let address = retained.as_view().as_sequence_bytes().unwrap().as_ptr() as usize;
        assert!((range_start..range_start + encoded.record().len()).contains(&address));
        drop(record);
        drop(inline);
        drop(mapping);
        drop(file);
        assert_eq!(
            retained.as_view().as_sequence_bytes().unwrap().as_ptr() as usize,
            address
        );
        assert_eq!(retained.to_owned_value().unwrap(), expected_value);
    }

    /// Retained projections survive appends, replacement tail maps, and dropping the store.
    /// Committed content is reused across batches and reopen; rollback never publishes an invalid locator.
    #[test]
    fn indexed_appends_deduplicate_and_survive_reopen() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("values");
        let db = Connection::open_in_memory().unwrap();
        ValueExtentStore::initialize_index(&db).unwrap();
        let mut store = ValueExtentStore::open(&path, true).unwrap();
        let same = || DataStoreValue::Canonical("same".into());
        assert!(store.append_indexed(&db, &[same()]).is_err());
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        let original = store.append_indexed(&db, &[same(), same()]).unwrap();
        assert_eq!(original[0], original[1]);
        db.execute_batch("COMMIT").unwrap();
        let length = fs::metadata(&path).unwrap().len();
        drop(store);
        let mut store = ValueExtentStore::open_existing(&path, true).unwrap();
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        assert_eq!(
            store.append_indexed(&db, &[same()]).unwrap()[0],
            original[0]
        );
        db.execute_batch("COMMIT").unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), length);
        let other = || DataStoreValue::Canonical("other".into());
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        store.append_indexed(&db, &[other()]).unwrap();
        db.execute_batch("ROLLBACK; BEGIN IMMEDIATE").unwrap();
        let located = store.append_indexed(&db, &[other()]).unwrap();
        db.execute_batch("COMMIT").unwrap();
        assert_eq!(
            store
                .read(located[0].1, &located[0].0)
                .unwrap()
                .canonical()
                .unwrap(),
            "other"
        );
        let count: u64 = db
            .query_row("SELECT COUNT(*) FROM clarity_extent_index", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 2);
        db.execute_batch("BEGIN IMMEDIATE; UPDATE clarity_extent_index SET offset=48 WHERE hash != (SELECT hash FROM clarity_extent_index ORDER BY offset LIMIT 1)").unwrap();
        assert!(store
            .append_indexed(&db, &[DataStoreValue::Canonical("other".into())])
            .is_err());
        db.execute_batch("ROLLBACK").unwrap();
    }

    /// A full digest mismatch rejects a candidate even when all fingerprint bytes collide.
    #[test]
    fn ptrhash_fingerprint_collision_does_not_reuse_wrong_value() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("index.sqlite");
        let db = Connection::open(&path).unwrap();
        ValueExtentStore::initialize_index(&db).unwrap();
        let mut store =
            ValueExtentStore::open(&dir.path().join("index.sqlite.values"), true).unwrap();
        db.execute_batch("CREATE TABLE clarity_extent_format(singleton INTEGER PRIMARY KEY,version INTEGER,store_id BLOB)").unwrap();
        db.execute(
            "INSERT INTO clarity_extent_format VALUES(1,1,?1)",
            [store.store_id.as_slice()],
        )
        .unwrap();
        let requested = DataStoreValue::Canonical("collision-request".into());
        let hash = MARFValue::from_value(requested.canonical());
        let mut colliding = hash.clone();
        colliding.0[10] ^= 1;
        // Synthetic envelope with the same partition and fingerprint but a distinct full label.
        let prepared = PreparedValueExtent::new(
            colliding.clone(),
            binary_value_store::encode_entry(&DataStoreValue::Canonical("different".into()))
                .unwrap(),
        )
        .unwrap();
        let old = store.append_prepared(&[prepared]).unwrap()[0].1;
        db.execute(
            "INSERT INTO clarity_extent_index VALUES(?1,?2,?3)",
            params![colliding.0.as_slice(), old.offset, old.length],
        )
        .unwrap();
        drop(store);
        extent_ptrhash::build_and_activate(&path, &dir.path().join("base")).unwrap();
        let mut store = ValueExtentStore::open_registered(&db, &path, true)
            .unwrap()
            .unwrap();
        assert!(store
            .ptrhash
            .as_ref()
            .unwrap()
            .candidate(&hash.0)
            .unwrap()
            .is_some());
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        store.begin_block().unwrap();
        let result = store.append_indexed(&db, &[requested]).unwrap();
        assert_ne!(result[0].1, old);
        assert_eq!(
            store.read(result[0].1, &hash).unwrap().canonical().unwrap(),
            "collision-request"
        );
        db.execute_batch("ROLLBACK").unwrap();
        store.discard_block();
    }

    /// Historical reuse and the mutable delta preserve transactions, savepoints, and reopen lifetime.
    #[test]
    fn ptrhash_preserves_extent_identity_and_transactional_delta() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("index.sqlite");
        let db = Connection::open(&path).unwrap();
        ValueExtentStore::initialize_index(&db).unwrap();
        let mut store =
            ValueExtentStore::open(&dir.path().join("index.sqlite.values"), true).unwrap();
        db.execute_batch("CREATE TABLE clarity_extent_format(singleton INTEGER PRIMARY KEY,version INTEGER,store_id BLOB)").unwrap();
        db.execute(
            "INSERT INTO clarity_extent_format VALUES(1,1,?1)",
            [store.store_id.as_slice()],
        )
        .unwrap();
        let values: Vec<_> = (0..128)
            .map(|i| DataStoreValue::Canonical(format!("historical-{i}")))
            .collect();
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        store.begin_block().unwrap();
        let expected = store.append_indexed(&db, &values).unwrap();
        store.publish_block().unwrap();
        db.execute_batch("COMMIT").unwrap();
        drop(store);
        extent_ptrhash::build_and_activate(&path, &dir.path().join("base")).unwrap();
        let mut store = ValueExtentStore::open_registered(&db, &path, true)
            .unwrap()
            .unwrap();
        assert!(store.read_ahead_path.is_none());
        let other = ValueExtentStore::open_registered(&db, &path, false)
            .unwrap()
            .unwrap();
        assert!(Arc::ptr_eq(
            store.ptrhash.as_ref().unwrap(),
            other.ptrhash.as_ref().unwrap()
        ));
        let before = fs::metadata(dir.path().join("index.sqlite.values"))
            .unwrap()
            .len();
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        store.begin_block().unwrap();
        assert_eq!(store.append_indexed(&db, &values).unwrap(), expected);
        assert_eq!(
            fs::metadata(dir.path().join("index.sqlite.values"))
                .unwrap()
                .len(),
            before
        );
        let new = || DataStoreValue::Canonical("new-value".into());
        db.execute_batch("SAVEPOINT value_write").unwrap();
        let abandoned = store.append_indexed(&db, &[new()]).unwrap();
        assert_eq!(store.append_indexed(&db, &[new()]).unwrap(), abandoned);
        db.execute_batch("ROLLBACK TO value_write; RELEASE value_write")
            .unwrap();
        let restored = store.append_indexed(&db, &[new()]).unwrap();
        assert!(restored[0].1.offset > abandoned[0].1.offset);
        db.execute_batch("ROLLBACK").unwrap();
        store.discard_block();
        assert_eq!(
            db.query_row("SELECT count(*) FROM clarity_extent_delta", [], |r| r
                .get::<_, u64>(0))
                .unwrap(),
            0
        );
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        store.begin_block().unwrap();
        let committed = store.append_indexed(&db, &[new()]).unwrap();
        store.publish_block().unwrap();
        db.execute_batch("COMMIT").unwrap();
        store.commit_dedup_cache();
        drop(store);
        let mut store = ValueExtentStore::open_registered(&db, &path, true)
            .unwrap()
            .unwrap();
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        store.begin_block().unwrap();
        assert_eq!(store.append_indexed(&db, &[new()]).unwrap(), committed);
        assert_eq!(store.append_indexed(&db, &values).unwrap(), expected);
        db.execute_batch("ROLLBACK").unwrap();
        store.discard_block();
        assert_eq!(
            db.query_row("SELECT count(*) FROM clarity_extent_index", [], |r| r
                .get::<_, u64>(0))
                .unwrap(),
            128
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM clarity_extent_delta", [], |r| r
                .get::<_, u64>(0))
                .unwrap(),
            1
        );
    }

    /// Read-ahead preserves ordered extents, new writes, and authoritative corruption errors.
    #[test]
    fn read_ahead_preserves_writer_snapshot_and_extent_results() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("index.sqlite");
        let db = Connection::open(&path).unwrap();
        db.execute_batch("PRAGMA journal_mode=WAL").unwrap();
        ValueExtentStore::initialize_index(&db).unwrap();
        let mut store = ValueExtentStore::open(&dir.path().join("values"), true).unwrap();
        let values: Vec<_> = (0..96)
            .map(|i| DataStoreValue::Canonical(format!("value-{i}")))
            .collect();
        store.begin_block().unwrap();
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        let expected = store.append_indexed(&db, &values).unwrap();
        store.publish_block().unwrap();
        db.execute_batch("COMMIT").unwrap();
        store.commit_dedup_cache();
        store.read_ahead = Some(read_ahead::ReadAhead::open(path).unwrap());
        store.begin_block().unwrap();
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        assert_eq!(store.append_indexed(&db, &values).unwrap(), expected);
        // Worker cannot see this transaction's new rows; writer must still find and reuse them.
        let added: Vec<_> = (0..96)
            .map(|i| DataStoreValue::Canonical(format!("new-{i}")))
            .collect();
        let new = store.append_indexed(&db, &added).unwrap();
        assert_eq!(store.append_indexed(&db, &added).unwrap(), new);
        db.execute_batch("ROLLBACK").unwrap();
        store.discard_block();
        store.begin_block().unwrap();
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        // The worker sees the committed valid row, but the main transaction sees corruption.
        db.execute(
            "UPDATE clarity_extent_index SET offset=49 WHERE hash=?1",
            [expected[0].0 .0.as_slice()],
        )
        .unwrap();
        assert!(store.append_indexed(&db, &values).is_err());
        db.execute_batch("ROLLBACK; PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        store.discard_block();
    }

    /// A first prefix collision falls back rather than returning the wrong value or masking errors.
    #[test]
    fn prefix_cache_verifies_commitments_and_preserves_errors() {
        let dir = tempdir().unwrap();
        let mut store = ValueExtentStore::open(&dir.path().join("values"), true).unwrap();
        let value = DataStoreValue::Canonical("prefix test".into());
        let (hash, extent) = store.append(&[value]).unwrap()[0].clone();
        store.begin_block().unwrap();
        store.dedup_cache.insert(hash.0, extent);
        assert_eq!(store.cached_extent(&hash).unwrap(), Some(extent));
        let mut collision = hash.clone();
        collision.0[4] ^= 1;
        assert_eq!(store.cached_extent(&collision).unwrap(), None);
        assert_eq!(store.cached_extent(&hash).unwrap(), None);
        store.dedup_cache.clear();
        let invalid = ValueExtent {
            offset: u64::MAX,
            ..extent
        };
        store.dedup_cache.insert(hash.0, invalid);
        assert!(store.cached_extent(&hash).is_err());
    }

    /// Only successfully committed, pre-existing locations enter the shared exact cache.
    #[test]
    fn dedup_cache_commit_rollback_and_savepoint_admission() {
        let dir = tempdir().unwrap();
        let db = Connection::open_in_memory().unwrap();
        ValueExtentStore::initialize_index(&db).unwrap();
        let mut store = ValueExtentStore::open(&dir.path().join("values"), true).unwrap();
        let value = || DataStoreValue::Canonical("retained".into());
        let hash = MARFValue::from_value("retained");
        store.begin_block().unwrap();
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        let original = store.append_indexed(&db, &[value()]).unwrap()[0].clone();
        store.append_indexed(&db, &[value()]).unwrap();
        assert_eq!(store.dedup_candidates.entries().count(), 0);
        store.publish_block().unwrap();
        db.execute_batch("COMMIT").unwrap();
        store.commit_dedup_cache();
        assert_eq!(store.dedup_cache.get(&hash.0), None);

        store.begin_block().unwrap();
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        assert_eq!(store.append_indexed(&db, &[value()]).unwrap()[0], original);
        assert_eq!(store.dedup_candidates.entries().count(), 1);
        // Mapping publication precedes SQLite commit and must not publish cache entries.
        store.publish_block().unwrap();
        assert_eq!(store.dedup_cache.get(&hash.0), None);
        db.execute_batch("ROLLBACK").unwrap();
        // Also covers abandonment without an explicit discard callback.
        store.begin_block().unwrap();
        assert_eq!(store.dedup_candidates.entries().count(), 0);
        assert_eq!(store.dedup_cache.get(&hash.0), None);
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        store.append_indexed(&db, &[value()]).unwrap();
        store.publish_block().unwrap();
        db.execute_batch("COMMIT").unwrap();
        store.commit_dedup_cache();
        assert_eq!(store.dedup_cache.get(&hash.0), Some(original.1));

        store.begin_block().unwrap();
        db.execute_batch("BEGIN IMMEDIATE; SAVEPOINT transient")
            .unwrap();
        let temporary = || DataStoreValue::Canonical("temporary".into());
        let temporary_hash = MARFValue::from_value("temporary");
        store.append_indexed(&db, &[temporary()]).unwrap();
        store.append_indexed(&db, &[temporary()]).unwrap();
        db.execute_batch("ROLLBACK TO transient; RELEASE transient")
            .unwrap();
        assert_eq!(store.append_indexed(&db, &[value()]).unwrap()[0], original);
        store.publish_block().unwrap();
        db.execute_batch("COMMIT").unwrap();
        store.commit_dedup_cache();
        assert_eq!(store.dedup_cache.get(&temporary_hash.0), None);
        let count: u64 = db
            .query_row("SELECT count(*) FROM clarity_extent_index", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);
        store.begin_block().unwrap();
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        let restored = store.append_indexed(&db, &[temporary()]).unwrap()[0].clone();
        let retained = store.read(restored.1, &restored.0).unwrap();
        db.execute_batch("ROLLBACK").unwrap();
        store.discard_block();
        assert_eq!(store.dedup_cache.get(&temporary_hash.0), None);
        assert_eq!(retained.canonical().unwrap(), "temporary");
        // Previously committed results remain valid after an unrelated rollback.
        assert_eq!(store.dedup_cache.get(&hash.0), Some(original.1));
    }

    #[test]
    fn mapped_value_outlives_store_and_appends() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("values");
        let mut store = ValueExtentStore::open(&path, true).unwrap();
        let value = Value::buff_from(vec![17; 4096]).unwrap();
        let expected = TypeSignature::type_of(&value).unwrap();
        let entries = [DataStoreValue::Typed(
            TypedValueData::prepare(value.clone()).unwrap(),
        )];
        let (hash, extent) = store.append(&entries).unwrap().pop().unwrap();
        let record = store.read(extent, &hash).unwrap();
        let stored = record.stored(&expected, &StacksEpochId::latest()).unwrap();
        let StoredValue::Packed(packed) = stored.value else {
            panic!("expected mapped packed value");
        };
        let original_address = packed.as_view().as_sequence_bytes().unwrap().as_ptr();
        for _ in 0..10 {
            let (hash, appended) = store.append(&entries).unwrap().pop().unwrap();
            store.read(appended, &hash).unwrap();
        }
        drop(record);
        drop(store);
        assert_eq!(
            packed.as_view().as_sequence_bytes().unwrap().as_ptr(),
            original_address
        );
        assert_eq!(packed.to_owned_value().unwrap(), value);
        let mut reopened = ValueExtentStore::open(&path, false).unwrap();
        assert_eq!(
            reopened.read(extent, &hash).unwrap().canonical().unwrap(),
            entries[0].canonical()
        );
    }

    /// Reads and transaction appends preserve one snapshot until the block publishes.
    #[test]
    fn whole_file_mapping_changes_only_at_block_publication() {
        let dir = tempdir().unwrap();
        let mut store = ValueExtentStore::open(&dir.path().join("values"), true).unwrap();
        let value = Value::buff_from(vec![17; 4096]).unwrap();
        let expected = TypeSignature::type_of(&value).unwrap();
        let entries = [DataStoreValue::Typed(
            TypedValueData::prepare(value.clone()).unwrap(),
        )];
        let (hash, original) = store.append(&entries).unwrap().pop().unwrap();
        let snapshot = Arc::clone(&store.mapping);
        let original_owner = store.read_at(original).unwrap().mapping;
        let StoredValue::Packed(first) = store
            .read(original, &hash)
            .unwrap()
            .stored(&expected, &StacksEpochId::latest())
            .unwrap()
            .value
        else {
            panic!("expected packed value");
        };
        let first_address = first.as_view().as_sequence_bytes().unwrap().as_ptr();
        store.begin_block().unwrap();
        let mut retained = Vec::new();
        let mut locators = Vec::new();
        for _ in 0..3 {
            let (hash, extent) = store.append(&entries).unwrap().pop().unwrap();
            let record = store.read(extent, &hash).unwrap();
            std::assert_matches!(record.mapping.as_ref(), ValueExtentBytes::Pending(_));
            let StoredValue::Packed(value) = record
                .stored(&expected, &StacksEpochId::latest())
                .unwrap()
                .value
            else {
                panic!("expected packed value");
            };
            retained.push(value);
            locators.push(extent);
            assert!(Arc::ptr_eq(&snapshot, &store.mapping));
            assert!(Arc::ptr_eq(
                &original_owner,
                &store.read_at(original).unwrap().mapping
            ));
        }
        store.publish_block().unwrap();
        assert!(!Arc::ptr_eq(&snapshot, &store.mapping));
        assert!(store.pending.is_empty());
        assert_eq!(store.published_len, store.file.metadata().unwrap().len());
        let published = Arc::clone(&store.mapping);
        for extent in locators {
            let record = store.read_at(extent).unwrap();
            assert!(!matches!(
                record.mapping.as_ref(),
                ValueExtentBytes::Pending(_)
            ));
            assert!(Arc::ptr_eq(
                &record.mapping,
                &store.read_at(extent).unwrap().mapping
            ));
        }
        store.begin_block().unwrap();
        store.publish_block().unwrap();
        assert!(
            Arc::ptr_eq(&published, &store.mapping),
            "read-only block remapped"
        );
        drop(store);
        assert_eq!(
            first_address,
            first.as_view().as_sequence_bytes().unwrap().as_ptr()
        );
        assert!(!first.is_materialized());
        for packed in retained {
            assert!(!packed.is_materialized());
            assert_eq!(packed.as_view().as_sequence_bytes().unwrap(), &[17; 4096]);
        }
    }

    /// Rolling back releases pending owners without invalidating retained read results.
    #[test]
    fn pending_value_survives_block_rollback() {
        let dir = tempdir().unwrap();
        let mut store = ValueExtentStore::open(&dir.path().join("values"), true).unwrap();
        let snapshot = Arc::clone(&store.mapping);
        store.begin_block().unwrap();
        let (_, extent) = store
            .append(&[DataStoreValue::Canonical("pending".into())])
            .unwrap()
            .pop()
            .unwrap();
        let retained = store.read_at(extent).unwrap();
        store.discard_block();
        assert!(store.pending.is_empty());
        assert!(Arc::ptr_eq(&snapshot, &store.mapping));
        assert!(store.read_at(extent).is_err());
        assert_eq!(retained.canonical().unwrap(), "pending");
        store.begin_block().unwrap();
        let (_, next) = store
            .append(&[DataStoreValue::Canonical("next".into())])
            .unwrap()
            .pop()
            .unwrap();
        assert!(next.offset > extent.offset);
        store.publish_block().unwrap();
        assert_eq!(store.read_at(next).unwrap().canonical().unwrap(), "next");
    }

    /// Distant records reuse the same whole-file snapshot across legacy chunk boundaries.
    #[test]
    fn whole_file_snapshot_covers_multiple_legacy_chunks() {
        let dir = tempdir().unwrap();
        let mut store = ValueExtentStore::open(&dir.path().join("values"), true).unwrap();
        let boundary = 64 * 1024 * 1024;
        store.file.set_len(boundary + 8).unwrap();
        let (_, extent) = store
            .append(&[DataStoreValue::Canonical("crossing".into())])
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(extent.offset, boundary + 8);
        assert!(extent.offset + extent.length > boundary);
        let record = store.read_at(extent).unwrap();
        assert!(Arc::ptr_eq(
            &record.mapping,
            &store.read_at(extent).unwrap().mapping
        ));
        assert_eq!(
            store.read_at(extent).unwrap().canonical().unwrap(),
            "crossing"
        );
    }

    /// New block views observe growth from independent writers without remapping per read.
    #[test]
    fn block_view_refreshes_external_appends() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("values");
        let mut writer = ValueExtentStore::open(&path, true).unwrap();
        let mut reader = ValueExtentStore::open_existing(&path, false).unwrap();
        let (_, extent) = writer
            .append(&[DataStoreValue::Canonical("external".into())])
            .unwrap()
            .pop()
            .unwrap();
        assert!(reader.read_at(extent).is_err());
        reader.begin_block().unwrap();
        let snapshot = reader.read_at(extent).unwrap().mapping;
        for _ in 0..3 {
            let record = reader.read_at(extent).unwrap();
            assert!(Arc::ptr_eq(&snapshot, &record.mapping));
            assert_eq!(record.canonical().unwrap(), "external");
        }
        reader.discard_block();
    }

    /// Growth preserves the prefix address and a retained snapshot's exposed length.
    #[cfg(all(unix, target_pointer_width = "64"))]
    #[test]
    fn reserved_value_prefix_survives_block_growth() {
        let dir = tempdir().unwrap();
        let mut store = ValueExtentStore::open(&dir.path().join("values"), true).unwrap();
        let value = Value::buff_from(vec![3; 128 * 1024]).unwrap();
        let expected = TypeSignature::type_of(&value).unwrap();
        let entries = [DataStoreValue::Typed(
            TypedValueData::prepare(value).unwrap(),
        )];
        let (_, first) = store.append(&entries).unwrap().pop().unwrap();
        store.append(&entries).unwrap();
        let ValueExtentBytes::Mapped {
            mapping: FileMapping::Stable(_),
            ..
        } = store.mapping.as_ref()
        else {
            panic!("expected reserved mapping");
        };
        let old_prefix = Arc::clone(&store.mapping);
        let old_len = old_prefix.as_ref().as_ref().len();
        let address = old_prefix.as_ref().as_ref().as_ptr();
        let StoredValue::Packed(retained) = store
            .read_at(first)
            .unwrap()
            .stored(&expected, &StacksEpochId::latest())
            .unwrap()
            .value
        else {
            panic!("expected borrowed value");
        };
        let value_address = retained.as_view().as_sequence_bytes().unwrap().as_ptr();
        for _ in 0..3 {
            store.begin_block().unwrap();
            store.append(&entries).unwrap();
            assert_eq!(store.mapping.as_ref().as_ref().as_ptr(), address);
            store.publish_block().unwrap();
            assert_eq!(store.mapping.as_ref().as_ref().as_ptr(), address);
            assert_eq!(old_prefix.as_ref().as_ref().len(), old_len);
        }
        assert!(store.mapping.as_ref().as_ref().len() > old_len);
        drop(store);
        assert_eq!(
            retained.as_view().as_sequence_bytes().unwrap().as_ptr(),
            value_address
        );
        assert_eq!(
            retained.as_view().as_sequence_bytes().unwrap(),
            vec![3; 128 * 1024]
        );
        assert!(!retained.is_materialized());
    }

    /// A large record spanning the final page remains one borrowed slice through later commits.
    #[cfg(all(unix, target_pointer_width = "64"))]
    #[test]
    fn boundary_spanning_value_uses_retained_contiguous_tail() {
        let dir = tempdir().unwrap();
        let mut store = ValueExtentStore::open(&dir.path().join("values"), true).unwrap();
        let value = Value::buff_from(vec![5; 128 * 1024 + 17]).unwrap();
        let expected = TypeSignature::type_of(&value).unwrap();
        let entries = [DataStoreValue::Typed(
            TypedValueData::prepare(value).unwrap(),
        )];
        let (_, extent) = store.append(&entries).unwrap().pop().unwrap();
        let prefix_len = store.mapping.as_ref().as_ref().len();
        assert!(extent.offset < prefix_len as u64);
        assert!(extent.offset + extent.length > prefix_len as u64);
        let tail = Arc::clone(&store.tail.as_ref().unwrap().bytes);
        let record = store.read_at(extent).unwrap();
        assert!(Arc::ptr_eq(&tail, &record.mapping));
        let StoredValue::Packed(retained) = record
            .stored(&expected, &StacksEpochId::latest())
            .unwrap()
            .value
        else {
            panic!("expected borrowed value");
        };
        let address = retained.as_view().as_sequence_bytes().unwrap().as_ptr();
        for _ in 0..3 {
            assert!(Arc::ptr_eq(&tail, &store.read_at(extent).unwrap().mapping));
        }
        // This append remains inside the same partial EOF page, so only the tail view changes.
        store.begin_block().unwrap();
        store
            .append(&[DataStoreValue::Canonical("small".into())])
            .unwrap();
        store.publish_block().unwrap();
        assert_eq!(store.mapping.as_ref().as_ref().len(), prefix_len);
        assert!(!Arc::ptr_eq(&tail, &store.tail.as_ref().unwrap().bytes));
        store.begin_block().unwrap();
        store.append(&entries).unwrap();
        store.publish_block().unwrap();
        let later = store.read_at(extent).unwrap();
        assert!(Arc::ptr_eq(&store.mapping, &later.mapping));
        drop(store);
        assert_eq!(
            retained.as_view().as_sequence_bytes().unwrap().as_ptr(),
            address
        );
        assert_eq!(
            retained.as_view().as_sequence_bytes().unwrap(),
            vec![5; 128 * 1024 + 17]
        );
        assert!(!retained.is_materialized());
    }

    /// Conventional mapping fallback preserves retained values when its snapshot is replaced.
    #[test]
    fn conventional_value_mapping_remains_supported() {
        let dir = tempdir().unwrap();
        let mut store = ValueExtentStore::open(&dir.path().join("values"), true).unwrap();
        let (_, extent) = store
            .append(&[DataStoreValue::Canonical("fallback".into())])
            .unwrap()
            .pop()
            .unwrap();
        // SAFETY: the store retains an immutable, append-only file generation.
        let conventional =
            FileMapping::Conventional(Arc::new(unsafe { Mmap::map(&store.file).unwrap() }));
        let (mapping, tail, length) =
            ValueExtentStore::snapshot(&store.file, conventional).unwrap();
        store.mapping = mapping;
        store.tail = tail;
        store.published_len = length;
        assert!(store.tail.is_none());
        let retained = store.read_at(extent).unwrap();
        store.begin_block().unwrap();
        store
            .append(&[DataStoreValue::Canonical("later".into())])
            .unwrap();
        store.publish_block().unwrap();
        assert!(store.tail.is_none());
        assert!(!Arc::ptr_eq(&retained.mapping, &store.mapping));
        drop(store);
        assert_eq!(retained.canonical().unwrap(), "fallback");
    }

    /// Stored digest bytes are ignored; reads inspect framing without hashing payloads.
    #[test]
    fn obsolete_integrity_digest_is_not_checked() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("values");
        let mut store = ValueExtentStore::open(&path, true).unwrap();
        let (hash, extent) = store
            .append(&[DataStoreValue::Canonical("metadata".into())])
            .unwrap()
            .pop()
            .unwrap();
        drop(store);
        let mut file = OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(extent.offset + 56)).unwrap();
        file.write_all(&[0; 32]).unwrap();
        drop(file);
        let mut store = ValueExtentStore::open_existing(&path, false).unwrap();
        assert_eq!(
            store.read(extent, &hash).unwrap().canonical().unwrap(),
            "metadata"
        );
    }

    /// Locator-only reads and explicit commitment resolution agree with canonical values.
    #[test]
    fn locator_only_commitments_match_admitted_content() {
        let dir = tempdir().unwrap();
        let mut store = ValueExtentStore::open(&dir.path().join("values"), true).unwrap();
        let entries = [
            DataStoreValue::Canonical("metadata".into()),
            DataStoreValue::Typed(
                TypedValueData::prepare(Value::buff_from(vec![7; 4096]).unwrap()).unwrap(),
            ),
        ];
        let locators = store.append(&entries).unwrap();
        for ((expected, extent), entry) in locators.iter().zip(&entries) {
            assert_eq!(
                store.read_at(*extent).unwrap().canonical().unwrap(),
                entry.canonical()
            );
            assert_eq!(*expected, MARFValue::from_value(&entry.canonical()));
        }
        let resolver = Mutex::new(store);
        for (expected, extent) in locators {
            assert_eq!(resolver.commitment(extent).unwrap(), expected);
            let wrong = ValueExtent {
                store_id: [0; 16],
                ..extent
            };
            assert!(resolver.commitment(wrong).is_err());
        }
    }

    /// Commitments are available from pending buffers and every published mapping lifetime.
    #[test]
    fn header_commitments_survive_publication_and_reopen() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("values");
        let mut store = ValueExtentStore::open(&path, true).unwrap();
        store.begin_block().unwrap();
        let (hash, extent) = store
            .append(&[DataStoreValue::Typed(
                TypedValueData::prepare(Value::buff_from(vec![7; 262_144]).unwrap()).unwrap(),
            )])
            .unwrap()
            .pop()
            .unwrap();
        let retained = store.read_at(extent).unwrap();
        assert_eq!(retained.commitment(), hash);
        let resolver = Mutex::new(store);
        assert_eq!(resolver.commitment(extent).unwrap(), hash);
        resolver.lock().unwrap().publish_block().unwrap();
        assert_eq!(resolver.commitment(extent).unwrap(), hash);
        drop(resolver);
        assert_eq!(retained.commitment(), hash);
        let resolver = Mutex::new(ValueExtentStore::open_existing(&path, false).unwrap());
        assert_eq!(resolver.commitment(extent).unwrap(), hash);
        assert!(resolver
            .commitment(ValueExtent {
                length: extent.length + 1,
                ..extent
            })
            .is_err());
        assert!(resolver
            .commitment(ValueExtent {
                offset: u64::MAX,
                ..extent
            })
            .is_err());
    }

    /// Header-only access must not decode a payload, even if a test corrupts its codec marker.
    #[test]
    fn header_commitment_does_not_decode_payload() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("values");
        let mut store = ValueExtentStore::open(&path, true).unwrap();
        let (hash, extent) = store
            .append(&[DataStoreValue::Typed(
                TypedValueData::prepare(Value::buff_from(vec![9; 262_144]).unwrap()).unwrap(),
            )])
            .unwrap()
            .pop()
            .unwrap();
        drop(store);
        let mut file = OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(extent.offset + RECORD_HEADER_LEN as u64))
            .unwrap();
        file.write_all(&[0xff]).unwrap();
        drop(file);
        let resolver = Mutex::new(ValueExtentStore::open_existing(&path, false).unwrap());
        assert_eq!(resolver.commitment(extent).unwrap(), hash);
        assert!(resolver
            .lock()
            .unwrap()
            .read_at(extent)
            .unwrap()
            .canonical()
            .is_err());
        drop(resolver);
        let mut file = OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(extent.offset)).unwrap();
        file.write_all(b"!").unwrap();
        drop(file);
        let resolver = Mutex::new(ValueExtentStore::open_existing(&path, false).unwrap());
        assert!(resolver.commitment(extent).is_err());
    }

    /// Foreign generations, mismatched commitments and invalid bounds cannot yield values.
    #[test]
    fn invalid_locators_fail_closed() {
        let dir = tempdir().unwrap();
        let mut store = ValueExtentStore::open(&dir.path().join("values"), true).unwrap();
        let (hash, extent) = store
            .append(&[DataStoreValue::Canonical("metadata".into())])
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(
            store.read(extent, &hash).unwrap().canonical().unwrap(),
            "metadata"
        );
        assert!(store.read(extent, &MARFValue::from_value("other")).is_err());
        let mut wrong = extent;
        wrong.store_id[0] ^= 1;
        assert!(store.read(wrong, &hash).is_err());
        wrong = extent;
        wrong.offset = u64::MAX;
        assert!(store.read(wrong, &hash).is_err());
        wrong = extent;
        wrong.length += 1;
        assert!(store.read(wrong, &hash).is_err());
    }

    /// Malformed framing is rejected without scanning the record payload.
    #[test]
    fn damaged_header_is_rejected() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("values");
        let mut store = ValueExtentStore::open(&path, true).unwrap();
        let (hash, extent) = store
            .append(&[DataStoreValue::Canonical("metadata".into())])
            .unwrap()
            .pop()
            .unwrap();
        drop(store);
        let mut file = OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(extent.offset)).unwrap();
        file.write_all(b"!").unwrap();
        drop(file);
        let mut reopened = ValueExtentStore::open(&path, false).unwrap();
        assert!(reopened.read(extent, &hash).is_err());
    }
}
