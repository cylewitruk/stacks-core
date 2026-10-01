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

//! Canonical Clarity stable-ID storage and publication.

#[cfg(test)]
use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::ops::{Deref, DerefMut, Range};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::process;
use std::sync::Arc;

use clarity::vm::database::DataStoreValue;
use clarity::vm::errors::VmExecutionError;
use extent_ptrhash::stable::Base as StablePtrHashBase;
use memmap2::{Mmap, MmapOptions};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use stable_value_format::files::{GenerationFile, GenerationPaths};
use stable_value_format::{
    decode_value_record, next_value_slot, DescriptorDirectoryRow, DescriptorId, FileHeader,
    FileKind, ValueDirectoryRow, ValueId, DESCRIPTOR_ROW_BYTES, FILE_HEADER_BYTES,
    MAX_DESCRIPTOR_BYTES, MAX_PARTITION_BYTES, MAX_RECORD_BYTES, VALUE_ROW_BYTES,
};

use super::binary_value_store::{self, EncodedRecord};
use super::value_extents::canonical_from_encoded_parts;
use crate::chainstate::stacks::index::{FileMapping, MARFValue};

#[cfg(test)]
thread_local! {
    static POSITIONED_PARTITION_READS: Cell<u64> = const { Cell::new(0) };
}

/// Append-only file view with a whole-file prefix or bounded mmap windows.
struct MappedGenerationFile {
    file: GenerationFile,
    /// Stable mapped prefix for the large, frequently indexed value directory.
    mapping: Option<FileMapping>,
    windows: VecDeque<(u64, Mmap)>,
}

impl MappedGenerationFile {
    /// Wrap one append-only generation file without mapping its complete range.
    fn new(file: GenerationFile) -> Self {
        Self {
            file,
            mapping: None,
            windows: VecDeque::new(),
        }
    }

    /// Prefer one demand-paged mapping for random fixed-width directory reads.
    fn new_value_directory(file: GenerationFile) -> Self {
        // SAFETY: published rows are immutable and the directory only appends.
        let mapping = unsafe { FileMapping::map(file.file()).ok() };
        Self {
            file,
            mapping,
            windows: VecDeque::new(),
        }
    }

    /// Copy one checked range from a cached mapping, including a window-crossing range.
    fn read_at(&mut self, offset: u64, length: usize) -> io::Result<Vec<u8>> {
        const WINDOW: u64 = 64 * 1024 * 1024;
        let end = offset.checked_add(length as u64).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "mapped generation offset overflow",
            )
        })?;
        if length == 0
            || length > MAX_DESCRIPTOR_BYTES as usize
            || offset < FILE_HEADER_BYTES as u64
            || end > self.file.len()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "generation range outside mapped file",
            ));
        }
        if let Some(mapping) = self.mapping.as_mut() {
            if end > mapping.len() as u64 {
                // SAFETY: completed directory rows only append to the same file.
                let _ = unsafe { mapping.refresh(self.file.file()) };
            }
            if end <= mapping.len() as u64 {
                stacks_profiler::diagnostics::count("v5_directory_map_hits", 1);
                return Ok(mapping[offset as usize..end as usize].to_vec());
            }
        }
        stacks_profiler::diagnostics::count("v5_directory_window_reads", 1);
        let base = offset / WINDOW * WINDOW;
        if let Some((_, mapping)) = self
            .windows
            .iter()
            .rev()
            .find(|(start, mapping)| *start == base && end - base <= mapping.len() as u64)
        {
            let start = (offset - base) as usize;
            return Ok(mapping[start..start + length].to_vec());
        }
        // Extend this window to cover a bounded descriptor crossing its boundary.
        let map_length = (self.file.len() - base).min(WINDOW.max(end - base)) as usize;
        // SAFETY: existing generation bytes are immutable; append only extends the file.
        let mapping = unsafe {
            MmapOptions::new()
                .offset(base)
                .len(map_length)
                .map(self.file.file())?
        };
        let start = (offset - base) as usize;
        let row = mapping[start..start + length].to_vec();
        self.windows.push_back((base, mapping));
        if self.windows.len() > 4 {
            self.windows.pop_front();
        }
        Ok(row)
    }

    /// Decode one descriptor row without a per-lookup file seek.
    fn descriptor_row(
        &mut self,
        id: DescriptorId,
        segment_length: u64,
    ) -> io::Result<DescriptorDirectoryRow> {
        let bytes = self.read_at(
            DescriptorDirectoryRow::directory_offset(id),
            DESCRIPTOR_ROW_BYTES,
        )?;
        DescriptorDirectoryRow::decode(&bytes, segment_length)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }
}

impl Deref for MappedGenerationFile {
    type Target = GenerationFile;

    fn deref(&self) -> &Self::Target {
        &self.file
    }
}

impl DerefMut for MappedGenerationFile {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.file
    }
}

/// One append-only partition with a retained, incrementally mapped prefix.
struct PartitionView {
    file: GenerationFile,
    mapping: Option<FileMapping>,
    tail: Option<(u64, Arc<Mmap>)>,
    /// Current block's appended records until a published mapping covers them.
    pending: HashMap<u32, Arc<StablePartitionBytes>>,
}

/// Immutable file view retained by one stable-ID value after the store advances.
#[derive(Debug)]
pub enum StablePartitionBytes {
    /// Mapped complete pages with a frozen readable length.
    Mapped { mapping: FileMapping, length: usize },
    /// Separately mapped incomplete EOF window.
    Tail(Arc<Mmap>),
    /// Positional fallback for an unpublished or unmappable record.
    Pending(Vec<u8>),
}

impl AsRef<[u8]> for StablePartitionBytes {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Mapped { mapping, length } => &mapping[..*length],
            Self::Tail(bytes) => bytes,
            Self::Pending(bytes) => bytes,
        }
    }
}

impl PartitionView {
    /// Map available complete pages and retain the current partial EOF page.
    fn new(file: GenerationFile) -> Result<Self, VmExecutionError> {
        // SAFETY: generation partitions append only; previously written bytes never change.
        let mapping = unsafe { FileMapping::map_with_capacity(file.file(), MAX_PARTITION_BYTES) }
            .map_err(store_error)?;
        let mut view = Self {
            file,
            mapping: Some(mapping),
            tail: None,
            pending: HashMap::new(),
        };
        view.refresh_tail()?;
        Ok(view)
    }

    /// Read a checked range from the stable prefix or the current unpublished tail.
    fn read_at(&mut self, offset: u64, length: usize) -> Result<Vec<u8>, VmExecutionError> {
        let end = offset
            .checked_add(length as u64)
            .ok_or_else(|| message("Partition read overflow"))?;
        if offset < FILE_HEADER_BYTES as u64 || end > self.file.len() {
            return Err(message("Partition read outside file"));
        }
        if let Some(mapping) = &self.mapping {
            if let (Ok(start), Ok(end)) = (usize::try_from(offset), usize::try_from(end)) {
                if let Some(bytes) = mapping.get(start..end) {
                    return Ok(bytes.to_vec());
                }
            }
        }
        if let Some((base, tail)) = &self.tail {
            if let (Some(start), Some(end)) = (offset.checked_sub(*base), end.checked_sub(*base)) {
                if let (Ok(start), Ok(end)) = (usize::try_from(start), usize::try_from(end)) {
                    if let Some(bytes) = tail.get(start..end) {
                        return Ok(bytes.to_vec());
                    }
                }
            }
        }
        if let Ok(offset) = u32::try_from(offset) {
            if let Some(bytes) = self.pending.get(&offset) {
                if let Some(prefix) = bytes.as_ref().as_ref().get(..length) {
                    return Ok(prefix.to_vec());
                }
            }
        }
        #[cfg(test)]
        POSITIONED_PARTITION_READS.with(|count| count.set(count.get() + 1));
        stacks_profiler::diagnostics::count("v5_partition_positioned_reads", 1);
        self.file.read_at(offset, length).map_err(store_error)
    }

    /// Return one range together with its immutable mapping owner.
    fn read_owned(
        &mut self,
        offset: u64,
        length: usize,
    ) -> Result<(Arc<StablePartitionBytes>, Range<usize>), VmExecutionError> {
        let end = offset
            .checked_add(length as u64)
            .ok_or_else(|| message("Partition read overflow"))?;
        if offset < FILE_HEADER_BYTES as u64 || end > self.file.len() {
            return Err(message("Partition read outside file"));
        }
        if let Some(mapping) = &self.mapping {
            if let (Ok(start), Ok(end)) = (usize::try_from(offset), usize::try_from(end)) {
                if mapping.get(start..end).is_some() {
                    return Ok((
                        Arc::new(StablePartitionBytes::Mapped {
                            mapping: mapping.clone(),
                            length: mapping.len(),
                        }),
                        start..end,
                    ));
                }
            }
        }
        if let Some((base, tail)) = &self.tail {
            if let (Some(start), Some(end)) = (offset.checked_sub(*base), end.checked_sub(*base)) {
                if let (Ok(start), Ok(end)) = (usize::try_from(start), usize::try_from(end)) {
                    if tail.get(start..end).is_some() {
                        return Ok((
                            Arc::new(StablePartitionBytes::Tail(Arc::clone(tail))),
                            start..end,
                        ));
                    }
                }
            }
        }
        if let Ok(offset) = u32::try_from(offset) {
            if let Some(bytes) = self.pending.get(&offset) {
                if bytes.as_ref().as_ref().len() >= length {
                    return Ok((Arc::clone(bytes), 0..length));
                }
            }
        }
        #[cfg(test)]
        POSITIONED_PARTITION_READS.with(|count| count.set(count.get() + 1));
        stacks_profiler::diagnostics::count("v5_partition_positioned_reads", 1);
        let bytes = self.file.read_at(offset, length).map_err(store_error)?;
        Ok((Arc::new(StablePartitionBytes::Pending(bytes)), 0..length))
    }

    /// Sync newly appended bytes and extend the existing virtual mapping when possible.
    fn sync_and_refresh(&mut self) -> Result<(), VmExecutionError> {
        self.file.sync_all().map_err(store_error)?;
        if let Some(mapping) = &mut self.mapping {
            // SAFETY: the same append-only file backs this mapping and its old prefix is immutable.
            unsafe { mapping.refresh(self.file.file()) }.map_err(store_error)?;
        } else {
            return Err(message("Published partition mapping disappeared"));
        }
        self.refresh_tail()?;
        self.pending.clear();
        Ok(())
    }

    /// Retain a bounded contiguous view for records crossing the incomplete EOF page.
    fn refresh_tail(&mut self) -> Result<(), VmExecutionError> {
        let mapping = self
            .mapping
            .as_ref()
            .ok_or_else(|| message("Missing partition mapping"))?;
        let prefix = mapping.len() as u64;
        if prefix >= self.file.len() {
            self.tail = None;
            return Ok(());
        }
        let offset = prefix.saturating_sub(u64::from(MAX_RECORD_BYTES));
        let length = usize::try_from(self.file.len() - offset).map_err(store_error)?;
        // SAFETY: the mapped bytes are an immutable prefix of an append-only partition.
        let tail = unsafe {
            MmapOptions::new()
                .offset(offset)
                .len(length)
                .map(self.file.file())
        }
        .map_err(store_error)?;
        self.tail = Some((offset, Arc::new(tail)));
        Ok(())
    }

    /// Return the observed file length, including the pending tail.
    fn len(&self) -> u64 {
        self.file.len()
    }
}

/// One exact-value result of a stable-ID lookup.
pub struct StableValueRecord {
    /// Retained owner for the existing versioned Binary V1 payload.
    pub owner: Arc<StablePartitionBytes>,
    /// Exact Binary V1 payload range within the owner.
    pub record: Range<usize>,
    /// Exact optional reconstruction descriptor bytes.
    pub descriptor: Arc<Vec<u8>>,
    /// Reconstructed original Clarity MARF commitment.
    pub commitment: MARFValue,
}

/// Byte-bounded FIFO cache for immutable descriptor data, including large bypasses.
struct DescriptorCache {
    /// Descriptors retained by stable ID.
    entries: HashMap<u32, Arc<Vec<u8>>>,
    /// Insertion order for byte-bounded eviction.
    order: VecDeque<u32>,
    /// Current payload bytes held by entries.
    bytes: usize,
    /// Maximum descriptor payload bytes retained.
    limit: usize,
}

impl DescriptorCache {
    /// Start with a bounded budget; no descriptor is needed for correctness from cache.
    fn new(limit: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            limit,
        }
    }

    /// Return immutable bytes already verified against their directory entry.
    fn get(&self, id: u32) -> Option<Arc<Vec<u8>>> {
        self.entries.get(&id).cloned()
    }

    /// Admit only values fitting both the byte budget and the entry-count bound.
    fn admit(&mut self, id: u32, bytes: Arc<Vec<u8>>) {
        if bytes.len() > self.limit || self.limit == 0 {
            return;
        }
        if self.entries.contains_key(&id) {
            return;
        }
        while self.bytes.saturating_add(bytes.len()) > self.limit || self.entries.len() >= 4096 {
            let Some(evicted) = self.order.pop_front() else {
                return;
            };
            if let Some(old) = self.entries.remove(&evicted) {
                self.bytes -= old.len();
            }
        }
        self.bytes += bytes.len();
        self.entries.insert(id, bytes);
        self.order.push_back(id);
    }
}

/// Bounded FIFO of immutable stable-ID directory rows used by repeated reads.
struct ValueRowCache {
    rows: HashMap<u32, ValueDirectoryRow>,
    order: VecDeque<u32>,
}

impl ValueRowCache {
    /// Start empty; the cache never determines whether an ID exists.
    fn new() -> Self {
        Self {
            rows: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    /// Return a previously checked immutable row.
    fn get(&self, id: ValueId) -> Option<ValueDirectoryRow> {
        self.rows.get(&id.get()).copied()
    }

    /// Retain one checked row while holding at most 65,536 entries.
    fn admit(&mut self, id: ValueId, row: ValueDirectoryRow) {
        const CAPACITY: usize = 65_536;
        if self.rows.contains_key(&id.get()) {
            return;
        }
        if self.rows.len() == CAPACITY {
            if let Some(old) = self.order.pop_front() {
                self.rows.remove(&old);
            }
        }
        self.rows.insert(id.get(), row);
        self.order.push_back(id.get());
    }

    /// Forget rows observed in a transaction that may have rolled back.
    fn clear(&mut self) {
        self.rows.clear();
        self.order.clear();
    }
}

/// Bounded prefix hints for previously committed stable values.
struct StableDedupCache {
    entries: HashMap<u32, Option<u32>>,
    order: VecDeque<u32>,
}

impl StableDedupCache {
    /// Start with no hints; every hit still needs full-commitment verification.
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    /// Return a candidate ID unless this prefix is absent or ambiguous.
    fn get(&self, hash: &[u8; 40]) -> Option<u32> {
        self.entries.get(&Self::prefix(hash)).copied().flatten()
    }

    /// Retain a verified hint or mark a prefix shared by distinct IDs.
    fn admit(&mut self, hash: &[u8; 40], id: u32) {
        self.admit_prefix(Self::prefix(hash), Some(id));
    }

    /// Merge one candidate, preserving an ambiguous prefix.
    fn admit_prefix(&mut self, prefix: u32, id: Option<u32>) {
        const CAPACITY: usize = 65_536;
        if let Some(existing) = self.entries.get_mut(&prefix) {
            if *existing != id {
                *existing = None;
            }
            return;
        }
        if self.entries.len() == CAPACITY {
            if let Some(old) = self.order.pop_front() {
                self.entries.remove(&old);
            }
        }
        self.entries.insert(prefix, id);
        self.order.push_back(prefix);
    }

    /// Invalidate an ambiguous prefix after a full-commitment mismatch.
    fn mark_multiple(&mut self, hash: &[u8; 40]) {
        if let Some(entry) = self.entries.get_mut(&Self::prefix(hash)) {
            *entry = None;
        }
    }

    /// Drop hints admitted within an uncommitted block.
    fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
    }

    /// Interpret the first four commitment bytes consistently on every host.
    fn prefix(hash: &[u8; 40]) -> u32 {
        u32::from_le_bytes(hash[..4].try_into().expect("fixed commitment prefix"))
    }
}

/// Exact write-side hits from descriptors known committed at recovery time.
struct ReverseDescriptorCache {
    /// Immutable descriptor bytes mapped to their stable IDs.
    entries: HashMap<Arc<[u8]>, u32>,
    /// Insertion order for bounded eviction.
    order: VecDeque<Arc<[u8]>>,
    /// Descriptor payload bytes retained in the cache.
    bytes: usize,
    /// Maximum payload bytes retained.
    limit: usize,
}

impl ReverseDescriptorCache {
    /// Create an empty cache; correctness never depends on admission.
    fn new(limit: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            limit,
        }
    }

    /// Find an exact descriptor without a SQLite query.
    fn get(&self, bytes: &[u8]) -> Option<u32> {
        self.entries.get(bytes).copied()
    }

    /// Retain one verified committed mapping within both cache bounds.
    fn admit(&mut self, bytes: &[u8], id: u32) {
        if self.limit == 0 || bytes.len() > self.limit || self.entries.contains_key(bytes) {
            return;
        }
        while self.bytes.saturating_add(bytes.len()) > self.limit || self.entries.len() >= 4096 {
            let Some(old) = self.order.pop_front() else {
                return;
            };
            if self.entries.remove(&old).is_some() {
                self.bytes -= old.len();
            }
        }
        let key: Arc<[u8]> = Arc::from(bytes);
        self.bytes += key.len();
        self.entries.insert(Arc::clone(&key), id);
        self.order.push_back(key);
    }
}

/// Bounded exact descriptor hits scoped to one offline SQLite write transaction.
///
/// Create a fresh instance for each batch and discard it on rollback. Normal
/// runtime writes never consult this cache, so savepoints cannot leak pending IDs.
pub struct PhysicalDescriptorBatch<'batch, 'connection> {
    /// SQL transaction that bounds all pending descriptor IDs in this cache.
    transaction: &'batch Transaction<'connection>,
    /// Exact descriptor bytes mapped to IDs allocated or found in this transaction.
    entries: HashMap<Box<[u8]>, u32>,
    /// Descriptor payload bytes currently retained.
    bytes: usize,
    /// Maximum descriptor payload bytes retained.
    limit: usize,
    /// Exact cache hits observed by focused converter tests.
    #[cfg(test)]
    hits: u64,
}

impl<'batch, 'connection> PhysicalDescriptorBatch<'batch, 'connection> {
    /// Set the maximum descriptor payload bytes retained by this batch.
    pub fn new(transaction: &'batch Transaction<'connection>, limit: usize) -> Self {
        Self {
            transaction,
            entries: HashMap::new(),
            bytes: 0,
            limit,
            #[cfg(test)]
            hits: 0,
        }
    }

    /// Resolve an exact descriptor, querying SQLite only on cache misses.
    fn resolve(
        &mut self,
        store: &mut StableValueStore,
        db: &Connection,
        descriptor: &[u8],
    ) -> Result<u32, VmExecutionError> {
        if descriptor.is_empty() {
            return Ok(0);
        }
        if let Some(id) = self.entries.get(descriptor) {
            #[cfg(test)]
            {
                self.hits += 1;
            }
            return Ok(*id);
        }
        let id = store.intern_descriptor_bytes(db, descriptor)?;
        if self.limit != 0 && descriptor.len() <= self.limit {
            if self.entries.len() >= 4096
                || self
                    .bytes
                    .checked_add(descriptor.len())
                    .is_none_or(|bytes| bytes > self.limit)
            {
                self.entries.clear();
                self.bytes = 0;
            }
            self.bytes += descriptor.len();
            self.entries.insert(descriptor.into(), id);
        }
        Ok(id)
    }
}

/// One deduplicated value staged for a transaction-batched append.
struct PendingIndexedValue {
    /// Reserved ID, not yet published to SQLite.
    id: ValueId,
    /// Canonical MARF commitment.
    commitment: MARFValue,
    /// Packed bytes reused from inline admission when available.
    encoded: EncodedRecord,
    /// Interned reconstruction descriptor ID.
    descriptor_id: u32,
}

/// A source record supplied to the offline, all-or-nothing generation builder.
pub struct PhysicalValue<'a> {
    /// Full commitment checked against the reconstructed source value before writing.
    pub commitment: &'a MARFValue,
    /// Packed payload without an outer physical header.
    pub payload: &'a [u8],
    /// Reconstruction descriptor, or empty for opaque strings.
    pub descriptor: &'a [u8],
}

/// Borrowed bytes shared by runtime and offline partition batching.
struct PendingRecord<'a> {
    /// Reserved stable row number.
    id: ValueId,
    /// Full source commitment.
    commitment: &'a MARFValue,
    /// Encoded bytes to append.
    payload: &'a [u8],
    /// Already interned reconstruction descriptor.
    descriptor_id: u32,
}

/// Append-only generation with monotonic IDs and transactional reverse indexes.
pub struct StableValueStore {
    paths: GenerationPaths,
    /// Whether this handle can append or recover unpublished data.
    writable: bool,
    store_id: [u8; 16],
    value_directory: MappedGenerationFile,
    descriptor_directory: MappedGenerationFile,
    value_partition: PartitionView,
    descriptor_segment: MappedGenerationFile,
    old_partitions: Vec<(u16, PartitionView)>,
    old_segments: Vec<(u32, MappedGenerationFile)>,
    partition_number: u16,
    descriptor_segment_number: u32,
    next_value_id: u64,
    next_descriptor_id: u64,
    id_limit: u32,
    dirty: bool,
    descriptor_dirty: bool,
    directory_entries_dirty: bool,
    ptrhash: Option<Arc<StablePtrHashBase>>,
    ptrhash_checked: bool,
    /// Bounded descriptor reads shared by typed projections from this handle.
    descriptor_cache: DescriptorCache,
    /// Checked immutable directory rows retained across transaction reads.
    value_row_cache: ValueRowCache,
    /// Verified previous-block hints and candidates awaiting block commit.
    dedup_cache: StableDedupCache,
    dedup_candidates: StableDedupCache,
    dedup_start: Option<u64>,
    /// Exact committed descriptor hits, never populated from pending inserts.
    reverse_descriptor_cache: ReverseDescriptorCache,
    /// Highest descriptor ID verified committed when this handle recovered.
    committed_descriptor_high: u64,
    /// One-shot file-sync failure used to exercise unpublished recovery.
    #[cfg(test)]
    fail_sync_at: Option<u8>,
    /// One-shot storage-full failure before one append stage.
    #[cfg(test)]
    fail_append_at: Option<u8>,
}

impl StableValueStore {
    /// Create a fresh, unpublished generation inside a new directory.
    pub fn create(root: &Path, store_id: [u8; 16]) -> Result<Self, VmExecutionError> {
        fs::create_dir(root).map_err(store_error)?;
        let paths = GenerationPaths {
            root: root.to_path_buf(),
        };
        let header = |kind, number| FileHeader {
            kind,
            number,
            store_id,
        };
        let value_directory = GenerationFile::create(
            &paths.value_directory(),
            header(FileKind::ValueDirectory, 0),
        )
        .map_err(store_error)?;
        let descriptor_directory = GenerationFile::create(
            &paths.descriptor_directory(),
            header(FileKind::DescriptorDirectory, 0),
        )
        .map_err(store_error)?;
        let value_partition =
            create_value_partition(&paths.value_partition(0), header(FileKind::ValueData, 0))?;
        let descriptor_segment = GenerationFile::create(
            &paths.descriptor_segment(0),
            header(FileKind::DescriptorData, 0),
        )
        .map_err(store_error)?;
        File::open(root)
            .and_then(|dir| dir.sync_all())
            .map_err(store_error)?;
        // Activation can publish the generation name only after its parent entry is durable.
        File::open(
            root.parent()
                .ok_or_else(|| message("Missing generation parent"))?,
        )
        .and_then(|dir| dir.sync_all())
        .map_err(store_error)?;
        Ok(Self {
            paths,
            writable: true,
            store_id,
            value_directory: MappedGenerationFile::new_value_directory(value_directory),
            descriptor_directory: MappedGenerationFile::new(descriptor_directory),
            value_partition: PartitionView::new(value_partition)?,
            descriptor_segment: MappedGenerationFile::new(descriptor_segment),
            old_partitions: Vec::new(),
            old_segments: Vec::new(),
            partition_number: 0,
            descriptor_segment_number: 0,
            next_value_id: 1,
            next_descriptor_id: 1,
            id_limit: u32::MAX,
            dirty: true,
            descriptor_dirty: true,
            directory_entries_dirty: false,
            ptrhash: None,
            ptrhash_checked: false,
            descriptor_cache: DescriptorCache::new(8 * 1024 * 1024),
            value_row_cache: ValueRowCache::new(),
            dedup_cache: StableDedupCache::new(),
            dedup_candidates: StableDedupCache::new(),
            dedup_start: None,
            reverse_descriptor_cache: ReverseDescriptorCache::new(4 * 1024 * 1024),
            committed_descriptor_high: 0,
            #[cfg(test)]
            fail_sync_at: None,
            #[cfg(test)]
            fail_append_at: None,
        })
    }

    /// Open a completed file set while preserving all allocated and orphaned IDs.
    pub fn open(root: &Path, store_id: [u8; 16]) -> Result<Self, VmExecutionError> {
        Self::open_with_access(root, store_id, true)
    }

    /// Open committed values without writable file handles or tail recovery.
    pub fn open_readonly(
        root: &Path,
        store_id: [u8; 16],
        db: &Connection,
    ) -> Result<Self, VmExecutionError> {
        let mut store = Self::open_with_access(root, store_id, false)?;
        store.verify_committed_high(db)?;
        store.load_ptrhash(db)?;
        store.committed_descriptor_high = committed_high(db)?.1;
        Ok(store)
    }

    /// Open a generation with explicit filesystem access and no recovery side effects.
    fn open_with_access(
        root: &Path,
        store_id: [u8; 16],
        writable: bool,
    ) -> Result<Self, VmExecutionError> {
        let paths = GenerationPaths {
            root: root.to_path_buf(),
        };
        let header = |kind, number| FileHeader {
            kind,
            number,
            store_id,
        };
        let value_directory = GenerationFile::open(
            &paths.value_directory(),
            header(FileKind::ValueDirectory, 0),
            writable,
        )
        .map_err(store_error)?;
        let descriptor_directory = GenerationFile::open(
            &paths.descriptor_directory(),
            header(FileKind::DescriptorDirectory, 0),
            writable,
        )
        .map_err(store_error)?;
        let next_value_id = next_id(value_directory.len(), VALUE_ROW_BYTES)?;
        let next_descriptor_id = next_id(descriptor_directory.len(), DESCRIPTOR_ROW_BYTES)?;
        let partition_number = last_existing_u16(|number| paths.value_partition(number))?;
        let descriptor_segment_number =
            last_existing_u32(|number| paths.descriptor_segment(number))?;
        let value_partition = GenerationFile::open(
            &paths.value_partition(partition_number),
            header(FileKind::ValueData, u32::from(partition_number)),
            writable,
        )
        .map_err(store_error)?;
        let descriptor_segment = GenerationFile::open(
            &paths.descriptor_segment(descriptor_segment_number),
            header(FileKind::DescriptorData, descriptor_segment_number),
            writable,
        )
        .map_err(store_error)?;
        Ok(Self {
            paths,
            writable,
            store_id,
            value_directory: MappedGenerationFile::new_value_directory(value_directory),
            descriptor_directory: MappedGenerationFile::new(descriptor_directory),
            value_partition: PartitionView::new(value_partition)?,
            descriptor_segment: MappedGenerationFile::new(descriptor_segment),
            old_partitions: Vec::new(),
            old_segments: Vec::new(),
            partition_number,
            descriptor_segment_number,
            next_value_id,
            next_descriptor_id,
            id_limit: u32::MAX,
            dirty: false,
            descriptor_dirty: false,
            directory_entries_dirty: false,
            ptrhash: None,
            ptrhash_checked: false,
            descriptor_cache: DescriptorCache::new(8 * 1024 * 1024),
            value_row_cache: ValueRowCache::new(),
            dedup_cache: StableDedupCache::new(),
            dedup_candidates: StableDedupCache::new(),
            dedup_start: None,
            reverse_descriptor_cache: ReverseDescriptorCache::new(4 * 1024 * 1024),
            committed_descriptor_high: 0,
            #[cfg(test)]
            fail_sync_at: None,
            #[cfg(test)]
            fail_append_at: None,
        })
    }

    /// Recover incomplete unpublished directory tails under the SQLite writer lock.
    ///
    /// Complete rows, including rows orphaned by rollback, remain allocated so
    /// their IDs are never reused. A committed ID without a complete row is fatal.
    pub fn recover_open(
        root: &Path,
        store_id: [u8; 16],
        db: &Connection,
    ) -> Result<Self, VmExecutionError> {
        if !has_partial_directory_rows(root, store_id)? {
            let mut store = Self::open(root, store_id)?;
            store.verify_committed_high(db)?;
            store.load_ptrhash(db)?;
            if db.is_autocommit() {
                store.committed_descriptor_high = committed_high(db)?.1;
            }
            return Ok(store);
        }
        if !db.is_autocommit() {
            return Err(message(
                "Stable directory recovery requires its own SQLite transaction",
            ));
        }
        db.execute_batch("BEGIN IMMEDIATE").map_err(store_error)?;
        let result = (|| {
            let (value_high, descriptor_high) = committed_high(db)?;
            recover_directory_tail(
                &root.join("value-directory.dat"),
                FileHeader {
                    kind: FileKind::ValueDirectory,
                    number: 0,
                    store_id,
                },
                VALUE_ROW_BYTES,
                value_high,
            )?;
            recover_directory_tail(
                &root.join("descriptor-directory.dat"),
                FileHeader {
                    kind: FileKind::DescriptorDirectory,
                    number: 0,
                    store_id,
                },
                DESCRIPTOR_ROW_BYTES,
                descriptor_high,
            )?;
            let mut store = Self::open(root, store_id)?;
            store.verify_committed_high(db)?;
            store.load_ptrhash(db)?;
            store.committed_descriptor_high = descriptor_high;
            Ok(store)
        })();
        match result {
            Ok(store) => {
                db.execute_batch("COMMIT").map_err(store_error)?;
                Ok(store)
            }
            Err(error) => {
                let _ = db.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    /// Bind an activated immutable base to this exact stable-value generation.
    fn load_ptrhash(&mut self, db: &Connection) -> Result<(), VmExecutionError> {
        let db_path = Path::new(db.path().unwrap_or(""));
        self.ptrhash =
            StablePtrHashBase::registered(db, db_path, &self.store_id, self.next_value_id - 1)
                .map_err(store_error)?;
        self.ptrhash_checked = true;
        Ok(())
    }

    /// Ensure committed reverse-index IDs resolve to bounded backing records.
    fn verify_committed_high(&mut self, db: &Connection) -> Result<(), VmExecutionError> {
        let (value_high, descriptor_high) = committed_high(db)?;
        if value_high != 0 {
            let id = ValueId::new(
                u32::try_from(value_high).map_err(|_| message("Committed value ID overflow"))?,
            )
            .map_err(store_error)?;
            let row = self.read_value_row(id)?;
            self.open_value_partition(row.partition)?
                .read_at(u64::from(row.offset), 32)?;
        }
        if descriptor_high != 0 {
            let id = DescriptorId::new(
                u32::try_from(descriptor_high)
                    .map_err(|_| message("Committed descriptor ID overflow"))?,
            )
            .map_err(store_error)?;
            let (segment, _) = id.segment();
            let length = self.open_descriptor_segment(segment)?.len();
            self.descriptor_directory
                .descriptor_row(id, length)
                .map_err(store_error)?;
        }
        Ok(())
    }

    /// Create exact write-side indexes; ordinary value reads use files only.
    pub fn initialize_index(db: &Connection) -> Result<(), VmExecutionError> {
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS clarity_stable_value_index(\
                 hash BLOB PRIMARY KEY CHECK(length(hash)=40),\
                 value_id INTEGER NOT NULL UNIQUE CHECK(value_id>0)) WITHOUT ROWID;\
             CREATE TABLE IF NOT EXISTS clarity_stable_descriptor_index(\
                 descriptor BLOB PRIMARY KEY,\
                 descriptor_id INTEGER NOT NULL UNIQUE CHECK(descriptor_id>0)) WITHOUT ROWID",
        )
        .map_err(store_error)
    }

    /// Limit IDs in fault tests without changing the on-disk width.
    #[cfg(test)]
    pub fn set_test_id_limit(&mut self, limit: u32) {
        self.id_limit = limit;
    }

    /// Return the owning generation UUID for activation metadata.
    pub fn store_id(&self) -> [u8; 16] {
        self.store_id
    }

    /// Append a deduplicated batch inside a caller-owned SQLite transaction.
    pub fn append_indexed(
        &mut self,
        db: &Connection,
        values: &[DataStoreValue],
    ) -> Result<Vec<(MARFValue, ValueId)>, VmExecutionError> {
        self.append_indexed_encoded(db, values, None)
    }

    /// Preserve one physical source record under a new ID without admitting it to dedup.
    ///
    /// The offline converter must rebuild committed reverse-index membership separately;
    /// complete source records can include abandoned or otherwise unindexed values.
    pub fn append_physical_unindexed(
        &mut self,
        db: &Connection,
        commitment: &MARFValue,
        payload: &[u8],
        descriptor: &[u8],
    ) -> Result<ValueId, VmExecutionError> {
        self.append_physical_inner(db, commitment, payload, descriptor, None)
    }

    /// Append physical records using a descriptor cache owned by this SQL transaction.
    pub fn append_physical_unindexed_cached(
        &mut self,
        commitment: &MARFValue,
        payload: &[u8],
        descriptor: &[u8],
        batch: &mut PhysicalDescriptorBatch<'_, '_>,
    ) -> Result<ValueId, VmExecutionError> {
        let transaction = batch.transaction;
        self.append_physical_inner(transaction, commitment, payload, descriptor, Some(batch))
    }

    /// Append verified source records in bulk without sync or reverse-index publication.
    ///
    /// The caller bounds batch bytes, owns the offline generation exclusively, and discards
    /// it on any error. Runtime publication must use the transactional indexed append path.
    pub fn append_physical_batch(
        &mut self,
        values: &[PhysicalValue<'_>],
        descriptors: &mut PhysicalDescriptorBatch<'_, '_>,
    ) -> Result<Vec<ValueId>, VmExecutionError> {
        if !self.writable || descriptors.transaction.is_autocommit() {
            return Err(message(
                "Physical batch requires a writable store and SQL transaction",
            ));
        }
        if values.is_empty() {
            return Ok(Vec::new());
        }
        self.refresh_if_changed(true)?;
        self.next_value_id
            .checked_add(values.len() as u64 - 1)
            .filter(|last| *last <= u64::from(self.id_limit))
            .ok_or_else(|| message("Stable value ID exhausted"))?;
        // Validate every record and all partition capacity before any append or interning.
        let mut partition = self.partition_number;
        let mut end = self.value_partition.len();
        for value in values {
            let canonical = canonical_from_encoded_parts(value.payload, value.descriptor)?;
            if MARFValue::from_value(&canonical) != *value.commitment {
                return Err(message("Physical value commitment mismatch"));
            }
            let length = u32::try_from(
                32usize
                    .checked_add(value.payload.len())
                    .ok_or_else(|| message("Record length overflow"))?,
            )
            .map_err(store_error)?;
            let (next, offset) = next_value_slot(partition, end, length).map_err(store_error)?;
            partition = next;
            end = u64::from(offset) + u64::from(length);
        }
        let db = descriptors.transaction;
        let mut pending = Vec::with_capacity(values.len());
        for (index, value) in values.iter().enumerate() {
            pending.push(PendingRecord {
                id: ValueId::new((self.next_value_id + index as u64) as u32)
                    .map_err(store_error)?,
                commitment: value.commitment,
                payload: value.payload,
                descriptor_id: descriptors.resolve(self, db, value.descriptor)?,
            });
        }
        // Offline conversion never exposes pending reads, so do not accumulate record owners.
        let rows = self.append_record_views(&pending, false)?;
        #[cfg(test)]
        self.inject_storage_full(3)?;
        self.value_directory
            .append_value_row_batch(&rows)
            .map_err(store_error)?;
        self.next_value_id += pending.len() as u64;
        self.dirty = true;
        Ok(rows.into_iter().map(|(id, _)| id).collect())
    }

    /// Verify one source record before publishing either its ID or its locator.
    fn append_physical_inner(
        &mut self,
        db: &Connection,
        commitment: &MARFValue,
        payload: &[u8],
        descriptor: &[u8],
        batch: Option<&mut PhysicalDescriptorBatch<'_, '_>>,
    ) -> Result<ValueId, VmExecutionError> {
        if !self.writable {
            return Err(message("Read-only stable value store"));
        }
        if db.is_autocommit() {
            return Err(message("Physical value append needs a SQLite transaction"));
        }
        self.refresh_if_changed(true)?;
        let id = self.next_value()?;
        let canonical = canonical_from_encoded_parts(payload, descriptor)?;
        if MARFValue::from_value(&canonical) != *commitment {
            return Err(message("Physical value commitment mismatch"));
        }
        let descriptor_id = match batch {
            Some(batch) => batch.resolve(self, db, descriptor)?,
            None => self.intern_descriptor_bytes(db, descriptor)?,
        };
        let mut row = self.append_record(commitment, payload)?;
        row.descriptor_id = descriptor_id;
        #[cfg(test)]
        self.inject_storage_full(3)?;
        self.value_directory
            .append_value_row(id, row)
            .map_err(store_error)?;
        self.next_value_id += 1;
        self.dirty = true;
        Ok(id)
    }

    /// Reuse already packed inline candidates while assigning external IDs.
    pub fn append_indexed_encoded(
        &mut self,
        db: &Connection,
        values: &[DataStoreValue],
        mut encoded: Option<&mut [Option<EncodedRecord>]>,
    ) -> Result<Vec<(MARFValue, ValueId)>, VmExecutionError> {
        if !self.writable {
            return Err(message("Read-only stable value store"));
        }
        let _diagnostic =
            stacks_profiler::diagnostics::wall_clock_sampled("V5: Dedup and encode", 32);
        if encoded
            .as_ref()
            .is_some_and(|records| records.len() != values.len())
        {
            return Err(message("Prepared stable value count mismatch"));
        }
        if db.is_autocommit() {
            return Err(message("Stable value append needs a SQLite transaction"));
        }
        {
            let _refresh =
                stacks_profiler::diagnostics::wall_clock_sampled("V5: Refresh directories", 32);
            self.refresh_if_changed(true)?;
        }
        if !self.ptrhash_checked {
            self.load_ptrhash(db)?;
        }
        let ptrhash = self.ptrhash.clone();
        let mut lookup = db
            .prepare_cached(if ptrhash.is_some() {
                "SELECT value_id FROM clarity_stable_value_delta WHERE hash=?1"
            } else {
                "SELECT value_id FROM clarity_stable_value_index WHERE hash=?1"
            })
            .map_err(store_error)?;
        let mut insert = db
            .prepare_cached(if ptrhash.is_some() {
                "INSERT INTO clarity_stable_value_delta(hash,value_id) VALUES(?1,?2)"
            } else {
                "INSERT INTO clarity_stable_value_index(hash,value_id) VALUES(?1,?2)"
            })
            .map_err(store_error)?;
        let mut results = Vec::with_capacity(values.len());
        let mut batch_ids = HashMap::with_capacity(values.len());
        let mut pending = Vec::new();
        for (index, value) in values.iter().enumerate() {
            let commitment = MARFValue::from_value(value.canonical());
            if let Some(id) = batch_ids.get(&commitment.0).copied() {
                stacks_profiler::diagnostics::count("v5_batch_hits", 1);
                results.push((commitment, id));
                continue;
            }
            stacks_profiler::diagnostics::count("v5_unique_values", 1);
            if let Some(id) = self.cached_value(&commitment)? {
                stacks_profiler::diagnostics::count("v5_dedup_cache_hits", 1);
                batch_ids.insert(commitment.0, id);
                results.push((commitment, id));
                continue;
            }
            stacks_profiler::diagnostics::count("v5_dedup_cache_misses", 1);
            if let Some(base) = ptrhash.as_ref() {
                let candidate = {
                    let _lookup =
                        stacks_profiler::diagnostics::wall_clock_sampled("V5: PtrHash lookup", 256);
                    base.candidate(&commitment.0).map_err(store_error)?
                };
                stacks_profiler::diagnostics::count("v5_ptrhash_queries", 1);
                if let Some(raw) = candidate {
                    let id = ValueId::new(raw).map_err(store_error)?;
                    let matches = {
                        let _verify = stacks_profiler::diagnostics::wall_clock_sampled(
                            "V5: PtrHash verify",
                            256,
                        );
                        self.commitment(id)? == commitment
                    };
                    if matches {
                        stacks_profiler::diagnostics::count("v5_ptrhash_hits", 1);
                        self.stage_dedup_candidate(&commitment, id);
                        batch_ids.insert(commitment.0, id);
                        results.push((commitment, id));
                        continue;
                    }
                    stacks_profiler::diagnostics::count("v5_ptrhash_collisions", 1);
                } else {
                    stacks_profiler::diagnostics::count("v5_ptrhash_negatives", 1);
                }
            }
            let delta = {
                let _lookup =
                    stacks_profiler::diagnostics::wall_clock_sampled("V5: SQLite lookup", 256);
                lookup
                    .query_row([commitment.as_bytes()], |row| row.get::<_, u32>(0))
                    .optional()
                    .map_err(store_error)?
            };
            stacks_profiler::diagnostics::count("v5_sqlite_queries", 1);
            if let Some(raw) = delta {
                stacks_profiler::diagnostics::count("v5_sqlite_hits", 1);
                let id = ValueId::new(raw).map_err(store_error)?;
                if self.commitment(id)? != commitment {
                    return Err(message("Stable value reverse index disagrees with record"));
                }
                self.stage_dedup_candidate(&commitment, id);
                batch_ids.insert(commitment.0, id);
                results.push((commitment, id));
                continue;
            }
            stacks_profiler::diagnostics::count("v5_sqlite_misses", 1);
            let raw = self
                .next_value_id
                .checked_add(pending.len() as u64)
                .filter(|raw| *raw <= u64::from(self.id_limit))
                .ok_or_else(|| message("Stable value ID exhausted"))?;
            let id = ValueId::new(raw as u32).map_err(store_error)?;
            let encoded = {
                let _encode =
                    stacks_profiler::diagnostics::wall_clock_sampled("V5: Packed encoding", 256);
                match encoded
                    .as_deref_mut()
                    .and_then(|records| records[index].take())
                {
                    Some(record) => record,
                    None => binary_value_store::encode_entry(value)?,
                }
            };
            pending.push(PendingIndexedValue {
                id,
                commitment: commitment.clone(),
                encoded,
                descriptor_id: 0,
            });
            batch_ids.insert(commitment.0, id);
            results.push((commitment, id));
        }
        if !pending.is_empty() {
            self.validate_partition_capacity(
                pending.iter().map(|value| value.encoded.record().len()),
            )?;
            // Resolve the entire batch's ID capacity before persisting descriptors.
            for value in &mut pending {
                let _intern =
                    stacks_profiler::diagnostics::wall_clock_sampled("V5: Descriptor intern", 256);
                value.descriptor_id = self.intern_descriptor(db, &value.encoded)?;
            }
            let rows = {
                let _append = stacks_profiler::diagnostics::wall_clock_sampled(
                    "V5: Append bytes and row",
                    32,
                );
                self.append_records_batch(&pending)?
            };
            #[cfg(test)]
            crash_at("value_bytes");
            #[cfg(test)]
            self.inject_storage_full(3)?;
            self.value_directory
                .append_value_row_batch(&rows)
                .map_err(store_error)?;
            #[cfg(test)]
            crash_at("value_row");
            self.next_value_id += pending.len() as u64;
            self.dirty = true;
            let _index = stacks_profiler::diagnostics::wall_clock_sampled("V5: SQLite insert", 32);
            for value in &pending {
                insert
                    .execute(params![value.commitment.as_bytes(), value.id.get()])
                    .map_err(store_error)?;
                #[cfg(test)]
                crash_at("value_index");
            }
        }
        Ok(results)
    }

    /// Verify a prefix hint against the stored full commitment.
    fn cached_value(
        &mut self,
        commitment: &MARFValue,
    ) -> Result<Option<ValueId>, VmExecutionError> {
        let Some(raw) = self
            .dedup_start
            .and_then(|_| self.dedup_cache.get(&commitment.0))
        else {
            return Ok(None);
        };
        let id = ValueId::new(raw).map_err(store_error)?;
        if self.commitment(id)? == *commitment {
            Ok(Some(id))
        } else {
            self.dedup_cache.mark_multiple(&commitment.0);
            stacks_profiler::diagnostics::count("v5_dedup_prefix_collisions", 1);
            Ok(None)
        }
    }

    /// Stage only an ID allocated before this block, so savepoint rollback is safe.
    fn stage_dedup_candidate(&mut self, commitment: &MARFValue, id: ValueId) {
        if self
            .dedup_start
            .is_some_and(|start| u64::from(id.get()) < start)
        {
            self.dedup_candidates.admit(&commitment.0, id.get());
        }
    }

    /// Read a value by stable ID and verify its generation-relative directory row.
    pub fn read(&mut self, id: ValueId) -> Result<StableValueRecord, VmExecutionError> {
        let _read = stacks_profiler::diagnostics::wall_clock_sampled("V5: Read value", 256);
        stacks_profiler::diagnostics::count("v5_value_reads", 1);
        let row = self.read_value_row(id)?;
        let partition = self.open_value_partition(row.partition)?;
        let (owner, range) =
            partition.read_owned(u64::from(row.offset), row.record_length as usize)?;
        let (digest, _) =
            decode_value_record(&owner.as_ref().as_ref()[range.clone()]).map_err(store_error)?;
        let mut commitment = [0; 40];
        commitment[..32].copy_from_slice(&digest);
        let descriptor = if row.descriptor_id == 0 {
            Arc::new(Vec::new())
        } else {
            let id = DescriptorId::new(row.descriptor_id).map_err(store_error)?;
            self.read_descriptor(id)?
        };
        Ok(StableValueRecord {
            owner,
            record: range.start + 32..range.end,
            descriptor,
            commitment: MARFValue(commitment),
        })
    }

    /// Fetch only the commitment bytes, avoiding payload and descriptor decoding.
    pub fn commitment(&mut self, id: ValueId) -> Result<MARFValue, VmExecutionError> {
        let _read = stacks_profiler::diagnostics::wall_clock_sampled("V5: Read commitment", 256);
        stacks_profiler::diagnostics::count("v5_commitment_reads", 1);
        let row = self.read_value_row(id)?;
        let partition = self.open_value_partition(row.partition)?;
        let digest = partition
            .read_at(u64::from(row.offset), 32)
            .map_err(store_error)?;
        let mut bytes = [0; 40];
        bytes[..32].copy_from_slice(&digest);
        Ok(MARFValue(bytes))
    }

    /// Synchronize all newly referenced files before a MARF root may commit.
    pub fn sync_unpublished(&mut self) -> Result<(), VmExecutionError> {
        if !self.dirty {
            return Ok(());
        }
        #[cfg(test)]
        self.inject_sync_failure(0)?;
        if self.descriptor_dirty {
            self.descriptor_segment.sync_all().map_err(store_error)?;
        }
        #[cfg(test)]
        self.inject_sync_failure(1)?;
        if self.descriptor_dirty {
            self.descriptor_directory.sync_all().map_err(store_error)?;
        }
        #[cfg(test)]
        self.inject_sync_failure(2)?;
        self.value_partition.sync_and_refresh()?;
        #[cfg(test)]
        self.inject_sync_failure(3)?;
        self.value_directory.sync_all().map_err(store_error)?;
        #[cfg(test)]
        self.inject_sync_failure(4)?;
        if self.directory_entries_dirty {
            File::open(&self.paths.root)
                .and_then(|dir| dir.sync_all())
                .map_err(store_error)?;
        }
        self.dirty = false;
        self.descriptor_dirty = false;
        self.directory_entries_dirty = false;
        Ok(())
    }

    /// Scope dedup candidate admission to values committed before this block.
    pub fn begin_block(&mut self) -> Result<(), VmExecutionError> {
        self.refresh_if_changed(false)?;
        self.dedup_candidates.clear();
        self.dedup_start = Some(self.next_value_id);
        Ok(())
    }

    /// Admit verified old-value hints only after the MARF block commits.
    pub fn commit_dedup_cache(&mut self) {
        for (&prefix, &id) in &self.dedup_candidates.entries {
            self.dedup_cache.admit_prefix(prefix, id);
        }
        self.dedup_candidates.clear();
        self.dedup_start = None;
    }

    /// Release block-local read copies after a transaction rolls back.
    pub fn discard_unpublished_cache(&mut self) {
        self.value_partition.pending.clear();
        for (_, partition) in &mut self.old_partitions {
            partition.pending.clear();
        }
        self.value_row_cache.clear();
        self.dedup_cache.clear();
        self.dedup_candidates.clear();
        self.dedup_start = None;
    }

    /// Fail once before the selected durability step without publishing references.
    #[cfg(test)]
    fn inject_sync_failure(&mut self, stage: u8) -> Result<(), VmExecutionError> {
        if self.fail_sync_at == Some(stage) {
            self.fail_sync_at = None;
            return Err(message("Injected stable value sync failure"));
        }
        Ok(())
    }

    /// Fail once with a storage-full error before the selected append step.
    #[cfg(test)]
    fn inject_storage_full(&mut self, stage: u8) -> Result<(), VmExecutionError> {
        if self.fail_append_at == Some(stage) {
            self.fail_append_at = None;
            return Err(store_error(io::Error::from(io::ErrorKind::StorageFull)));
        }
        Ok(())
    }

    /// Reopen complete external appends before assigning an ID in this SQLite writer transaction.
    pub fn refresh_if_changed(&mut self, require_complete: bool) -> Result<(), VmExecutionError> {
        let value_length = fs::metadata(self.paths.value_directory())
            .map_err(store_error)?
            .len();
        let descriptor_length = fs::metadata(self.paths.descriptor_directory())
            .map_err(store_error)?
            .len();
        if value_length == self.value_directory.len()
            && descriptor_length == self.descriptor_directory.len()
        {
            return Ok(());
        }
        // A reader may race an uncommitted append. The previous complete view remains valid.
        let value_body = value_length
            .checked_sub(FILE_HEADER_BYTES as u64)
            .ok_or_else(|| message("Truncated value directory"))?;
        let descriptor_body = descriptor_length
            .checked_sub(FILE_HEADER_BYTES as u64)
            .ok_or_else(|| message("Truncated descriptor directory"))?;
        if value_body % VALUE_ROW_BYTES as u64 != 0
            || descriptor_body % DESCRIPTOR_ROW_BYTES as u64 != 0
        {
            return if require_complete {
                Err(message("Partial stable directory row requires recovery"))
            } else {
                Ok(())
            };
        }
        let base = self.ptrhash.clone();
        let checked = self.ptrhash_checked;
        let committed_descriptor_high = self.committed_descriptor_high;
        *self = Self::open_with_access(&self.paths.root, self.store_id, self.writable)?;
        self.ptrhash = base;
        self.ptrhash_checked = checked;
        self.committed_descriptor_high = committed_descriptor_high;
        Ok(())
    }

    /// Allocate a value ID before writing any record bytes.
    fn next_value(&self) -> Result<ValueId, VmExecutionError> {
        if self.next_value_id > u64::from(self.id_limit) {
            return Err(message("Stable value ID exhausted"));
        }
        ValueId::new(self.next_value_id as u32).map_err(store_error)
    }

    /// Lookup or persist an exact descriptor in the caller's transaction.
    fn intern_descriptor(
        &mut self,
        db: &Connection,
        encoded: &EncodedRecord,
    ) -> Result<u32, VmExecutionError> {
        self.intern_descriptor_bytes(db, encoded.shape().unwrap_or_default())
    }

    /// Intern exact descriptor bytes for both online writes and offline conversion.
    fn intern_descriptor_bytes(
        &mut self,
        db: &Connection,
        bytes: &[u8],
    ) -> Result<u32, VmExecutionError> {
        if bytes.is_empty() {
            return Ok(0);
        }
        if let Some(id) = self.reverse_descriptor_cache.get(bytes) {
            return Ok(id);
        }
        if let Some(raw) = db
            .query_row(
                "SELECT descriptor_id FROM clarity_stable_descriptor_index WHERE descriptor=?1",
                [bytes],
                |row| row.get::<_, u32>(0),
            )
            .optional()
            .map_err(store_error)?
        {
            let id = DescriptorId::new(raw).map_err(store_error)?;
            if self.read_descriptor(id)?.as_slice() != bytes {
                return Err(message("Descriptor reverse index disagrees with bytes"));
            }
            if u64::from(raw) <= self.committed_descriptor_high {
                self.reverse_descriptor_cache.admit(bytes, raw);
            }
            return Ok(raw);
        }
        if self.next_descriptor_id > u64::from(self.id_limit) {
            return Err(message("Descriptor ID exhausted"));
        }
        let id = DescriptorId::new(self.next_descriptor_id as u32).map_err(store_error)?;
        let (segment, _) = id.segment();
        if segment != self.descriptor_segment_number {
            self.descriptor_segment.sync_all().map_err(store_error)?;
            self.descriptor_segment = MappedGenerationFile::new(
                GenerationFile::create(
                    &self.paths.descriptor_segment(segment),
                    FileHeader {
                        kind: FileKind::DescriptorData,
                        number: segment,
                        store_id: self.store_id,
                    },
                )
                .map_err(store_error)?,
            );
            self.descriptor_segment_number = segment;
            self.directory_entries_dirty = true;
        }
        #[cfg(test)]
        self.inject_storage_full(0)?;
        let row = self
            .descriptor_segment
            .append_descriptor(id, bytes)
            .map_err(store_error)?;
        #[cfg(test)]
        crash_at("descriptor_bytes");
        #[cfg(test)]
        self.inject_storage_full(1)?;
        self.descriptor_directory
            .append_descriptor_row(id, row)
            .map_err(store_error)?;
        #[cfg(test)]
        crash_at("descriptor_row");
        self.next_descriptor_id += 1;
        self.dirty = true;
        self.descriptor_dirty = true;
        db.execute(
            "INSERT INTO clarity_stable_descriptor_index(descriptor,descriptor_id) VALUES(?1,?2)",
            params![bytes, id.get()],
        )
        .map_err(store_error)?;
        Ok(id.get())
    }

    /// Check an entire runtime batch's partition capacity before persisting descriptors or values.
    fn validate_partition_capacity(
        &self,
        lengths: impl IntoIterator<Item = usize>,
    ) -> Result<(), VmExecutionError> {
        let mut partition = self.partition_number;
        let mut end = self.value_partition.len();
        for payload_length in lengths {
            let length = u32::try_from(
                32usize
                    .checked_add(payload_length)
                    .ok_or_else(|| message("Record length overflow"))?,
            )
            .map_err(store_error)?;
            let (next, offset) = next_value_slot(partition, end, length).map_err(store_error)?;
            partition = next;
            end = u64::from(offset) + u64::from(length);
        }
        Ok(())
    }

    /// Append one compact value, rolling to a new partition before overflow.
    fn append_record(
        &mut self,
        commitment: &MARFValue,
        payload: &[u8],
    ) -> Result<ValueDirectoryRow, VmExecutionError> {
        let length = u32::try_from(
            32usize
                .checked_add(payload.len())
                .ok_or_else(|| message("Record length overflow"))?,
        )
        .map_err(store_error)?;
        let (partition, _) =
            next_value_slot(self.partition_number, self.value_partition.len(), length)
                .map_err(store_error)?;
        if partition != self.partition_number {
            self.advance_value_partition(partition)?;
        }
        #[cfg(test)]
        self.inject_storage_full(2)?;
        let row = self
            .value_partition
            .file
            .append_value(commitment.0, payload)
            .map_err(store_error)?;
        #[cfg(test)]
        crash_at("value_bytes");
        Ok(row)
    }

    /// Publish a new partition only after its predecessor's bytes are durable.
    fn advance_value_partition(&mut self, partition: u16) -> Result<(), VmExecutionError> {
        self.value_partition.sync_and_refresh()?;
        let next = create_value_partition(
            &self.paths.value_partition(partition),
            FileHeader {
                kind: FileKind::ValueData,
                number: u32::from(partition),
                store_id: self.store_id,
            },
        )?;
        self.value_partition = PartitionView::new(next)?;
        self.partition_number = partition;
        self.directory_entries_dirty = true;
        Ok(())
    }

    /// Write consecutive new records in partition-sized groups for one transaction.
    fn append_records_batch(
        &mut self,
        pending: &[PendingIndexedValue],
    ) -> Result<Vec<(ValueId, ValueDirectoryRow)>, VmExecutionError> {
        let records: Vec<_> = pending
            .iter()
            .map(|value| PendingRecord {
                id: value.id,
                commitment: &value.commitment,
                payload: value.encoded.record(),
                descriptor_id: value.descriptor_id,
            })
            .collect();
        self.append_record_views(&records, true)
    }

    /// Group writes by partition, optionally retaining runtime read-your-writes owners.
    fn append_record_views(
        &mut self,
        pending: &[PendingRecord<'_>],
        retain_pending: bool,
    ) -> Result<Vec<(ValueId, ValueDirectoryRow)>, VmExecutionError> {
        let mut rows = Vec::with_capacity(pending.len());
        let mut cursor = 0;
        while cursor < pending.len() {
            let mut next_length = self.value_partition.len();
            let mut end = cursor;
            for value in &pending[cursor..] {
                let length = u32::try_from(
                    32usize
                        .checked_add(value.payload.len())
                        .ok_or_else(|| message("Record length overflow"))?,
                )
                .map_err(store_error)?;
                let (partition, _) = next_value_slot(self.partition_number, next_length, length)
                    .map_err(store_error)?;
                if partition != self.partition_number {
                    break;
                }
                next_length += u64::from(length);
                end += 1;
            }
            if end == cursor {
                let first = &pending[cursor];
                let length = u32::try_from(
                    32usize
                        .checked_add(first.payload.len())
                        .ok_or_else(|| message("Record length overflow"))?,
                )
                .map_err(store_error)?;
                let (partition, _) =
                    next_value_slot(self.partition_number, self.value_partition.len(), length)
                        .map_err(store_error)?;
                self.advance_value_partition(partition)?;
                continue;
            }
            let values: Vec<_> = pending[cursor..end]
                .iter()
                .map(|value| (value.commitment.0, value.payload))
                .collect();
            #[cfg(test)]
            self.inject_storage_full(2)?;
            let appended = self
                .value_partition
                .file
                .append_value_batch(&values)
                .map_err(store_error)?;
            for (value, (mut row, record)) in pending[cursor..end].iter().zip(appended) {
                row.descriptor_id = value.descriptor_id;
                if retain_pending {
                    self.value_partition
                        .pending
                        .insert(row.offset, Arc::new(StablePartitionBytes::Pending(record)));
                }
                rows.push((value.id, row));
            }
            cursor = end;
        }
        Ok(rows)
    }

    /// Read the fixed-width value row with the referenced partition's length.
    fn read_value_row(&mut self, id: ValueId) -> Result<ValueDirectoryRow, VmExecutionError> {
        let _read = stacks_profiler::diagnostics::wall_clock_sampled("V5: Directory row", 256);
        stacks_profiler::diagnostics::count("v5_directory_reads", 1);
        if let Some(row) = self.value_row_cache.get(id) {
            stacks_profiler::diagnostics::count("v5_directory_row_cache_hits", 1);
            return Ok(row);
        }
        stacks_profiler::diagnostics::count("v5_directory_row_cache_misses", 1);
        let bytes = self
            .value_directory
            .read_at(ValueDirectoryRow::directory_offset(id), VALUE_ROW_BYTES)
            .map_err(store_error)?;
        let partition = u16::from_le_bytes(bytes[..2].try_into().unwrap());
        let length = self.open_value_partition(partition)?.len();
        let row = ValueDirectoryRow::decode(&bytes, length).map_err(store_error)?;
        self.value_row_cache.admit(id, row);
        Ok(row)
    }

    /// Open a previously published or current value partition by number.
    fn open_value_partition(
        &mut self,
        partition: u16,
    ) -> Result<&mut PartitionView, VmExecutionError> {
        if partition == self.partition_number {
            return Ok(&mut self.value_partition);
        }
        if let Some(index) = self
            .old_partitions
            .iter()
            .position(|(number, _)| *number == partition)
        {
            let entry = self.old_partitions.remove(index);
            self.old_partitions.push(entry);
        } else {
            let file = GenerationFile::open(
                &self.paths.value_partition(partition),
                FileHeader {
                    kind: FileKind::ValueData,
                    number: u32::from(partition),
                    store_id: self.store_id,
                },
                false,
            )
            .map_err(store_error)?;
            if self.old_partitions.len() == 8 {
                self.old_partitions.remove(0);
            }
            self.old_partitions
                .push((partition, PartitionView::new(file)?));
        }
        Ok(&mut self
            .old_partitions
            .last_mut()
            .expect("inserted partition")
            .1)
    }

    /// Open a descriptor data segment by its deterministic ID-derived number.
    fn open_descriptor_segment(
        &mut self,
        segment: u32,
    ) -> Result<&mut MappedGenerationFile, VmExecutionError> {
        if segment == self.descriptor_segment_number {
            return Ok(&mut self.descriptor_segment);
        }
        if let Some(index) = self
            .old_segments
            .iter()
            .position(|(number, _)| *number == segment)
        {
            let entry = self.old_segments.remove(index);
            self.old_segments.push(entry);
        } else {
            let file = GenerationFile::open(
                &self.paths.descriptor_segment(segment),
                FileHeader {
                    kind: FileKind::DescriptorData,
                    number: segment,
                    store_id: self.store_id,
                },
                false,
            )
            .map_err(store_error)?;
            if self.old_segments.len() == 8 {
                self.old_segments.remove(0);
            }
            self.old_segments
                .push((segment, MappedGenerationFile::new(file)));
        }
        Ok(&mut self.old_segments.last_mut().expect("inserted segment").1)
    }

    /// Verify an existing descriptor by ID before accepting a reverse-index hit.
    fn read_descriptor(&mut self, id: DescriptorId) -> Result<Arc<Vec<u8>>, VmExecutionError> {
        if let Some(bytes) = self.descriptor_cache.get(id.get()) {
            return Ok(bytes);
        }
        let (segment, _) = id.segment();
        let length = self.open_descriptor_segment(segment)?.len();
        let row = self
            .descriptor_directory
            .descriptor_row(id, length)
            .map_err(store_error)?;
        let bytes = self
            .open_descriptor_segment(segment)?
            .read_at(u64::from(row.offset), row.length as usize)
            .map_err(store_error)?;
        let bytes = Arc::new(bytes);
        self.descriptor_cache.admit(id.get(), Arc::clone(&bytes));
        Ok(bytes)
    }
}

/// Convert a directory length to the next never-assigned row ID.
fn next_id(length: u64, stride: usize) -> Result<u64, VmExecutionError> {
    let body = length
        .checked_sub(FILE_HEADER_BYTES as u64)
        .ok_or_else(|| message("Truncated stable directory"))?;
    if body % stride as u64 != 0 {
        return Err(message("Partial stable directory row"));
    }
    Ok(body / stride as u64 + 1)
}

/// Read the highest IDs whose reverse mappings survived the SQLite commit.
fn committed_high(db: &Connection) -> Result<(u64, u64), VmExecutionError> {
    let mut value: u64 = db
        .query_row(
            "SELECT COALESCE(MAX(value_id),0) FROM clarity_stable_value_index",
            [],
            |row| row.get(0),
        )
        .map_err(store_error)?;
    let streamed_base_present: bool = db
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='clarity_stable_value_high' AND type='table')",
            [],
            |row| row.get(0),
        )
        .map_err(store_error)?;
    if streamed_base_present {
        let base: u64 = db
            .query_row(
                "SELECT value_id FROM clarity_stable_value_high WHERE singleton=1",
                [],
                |row| row.get(0),
            )
            .map_err(store_error)?;
        value = value.max(base);
    }
    let delta_present: bool = db
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='clarity_stable_value_delta' AND type='table')",
            [],
            |row| row.get(0),
        )
        .map_err(store_error)?;
    if delta_present {
        let delta: u64 = db
            .query_row(
                "SELECT COALESCE(MAX(value_id),0) FROM clarity_stable_value_delta",
                [],
                |row| row.get(0),
            )
            .map_err(store_error)?;
        value = value.max(delta);
    }
    let descriptor = db
        .query_row(
            "SELECT COALESCE(MAX(descriptor_id),0) FROM clarity_stable_descriptor_index",
            [],
            |row| row.get(0),
        )
        .map_err(store_error)?;
    Ok((value, descriptor))
}

/// Check identity and row framing before deciding whether a recovery lock is needed.
fn has_partial_directory_rows(root: &Path, store_id: [u8; 16]) -> Result<bool, VmExecutionError> {
    for (name, kind, stride) in [
        (
            "value-directory.dat",
            FileKind::ValueDirectory,
            VALUE_ROW_BYTES,
        ),
        (
            "descriptor-directory.dat",
            FileKind::DescriptorDirectory,
            DESCRIPTOR_ROW_BYTES,
        ),
    ] {
        let file = GenerationFile::open(
            &root.join(name),
            FileHeader {
                kind,
                number: 0,
                store_id,
            },
            false,
        )
        .map_err(store_error)?;
        if (file.len() - FILE_HEADER_BYTES as u64) % stride as u64 != 0 {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Trim only bytes after the final complete row, preserving every full ID.
fn recover_directory_tail(
    path: &Path,
    header: FileHeader,
    stride: usize,
    committed_high: u64,
) -> Result<(), VmExecutionError> {
    let file = GenerationFile::open(path, header, false).map_err(store_error)?;
    let body = file.len() - FILE_HEADER_BYTES as u64;
    let complete = body / stride as u64;
    if committed_high > complete {
        return Err(message("Committed stable ID has no complete directory row"));
    }
    let complete_length = FILE_HEADER_BYTES as u64 + complete * stride as u64;
    if complete_length == file.len() {
        return Ok(());
    }
    drop(file);
    let writable = OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(store_error)?;
    writable.set_len(complete_length).map_err(store_error)?;
    writable.sync_all().map_err(store_error)
}

/// Locate the final contiguous u16-numbered file in a generation.
fn last_existing_u16(mut path: impl FnMut(u16) -> PathBuf) -> Result<u16, VmExecutionError> {
    let mut number = 0u16;
    if !path(number).exists() {
        return Err(message("Missing values partition zero"));
    }
    while let Some(next) = number.checked_add(1) {
        if !path(next).exists() {
            break;
        }
        number = next;
    }
    Ok(number)
}

/// Locate the final contiguous u32-numbered descriptor segment.
fn last_existing_u32(mut path: impl FnMut(u32) -> PathBuf) -> Result<u32, VmExecutionError> {
    let mut number = 0u32;
    if !path(number).exists() {
        return Err(message("Missing descriptor segment zero"));
    }
    while let Some(next) = number.checked_add(1) {
        if !path(next).exists() {
            break;
        }
        number = next;
    }
    Ok(number)
}

/// Adapt file and SQL failures to the existing Clarity storage error type.
fn store_error(error: impl std::fmt::Display) -> VmExecutionError {
    message(&error.to_string())
}

/// Exit a dedicated crash-test process without unwinding or syncing its files.
#[cfg(test)]
fn crash_at(point: &str) {
    if matches!(std::env::var("STACKSLIB_STABLE_CRASH_POINT"), Ok(value) if value == point) {
        process::exit(73);
    }
}

/// Create a partition with all record space reserved before it receives a value.
fn create_value_partition(
    path: &Path,
    header: FileHeader,
) -> Result<GenerationFile, VmExecutionError> {
    let file = GenerationFile::create(path, header).map_err(store_error)?;
    if let Err(error) = file.preallocate_value_partition(value_partition_reservation()) {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(store_error(error));
    }
    Ok(file)
}

/// Production partitions reserve the full 4 GiB; unit tests use a small reservation.
fn value_partition_reservation() -> u64 {
    if cfg!(test) {
        1024 * 1024
    } else {
        MAX_PARTITION_BYTES
    }
}

/// Construct one Clarity storage error without committing partial references.
fn message(text: &str) -> VmExecutionError {
    use clarity::vm::errors::VmInternalError;
    VmInternalError::DBError(text.into()).into()
}

#[cfg(test)]
mod tests {
    use super::super::value_extents::StableMappedValueRecord;
    use super::super::value_extents::ValueBackend;

    use super::*;
    #[cfg(not(feature = "direct-value-eager"))]
    use clarity::vm::database::StoredValue;
    use clarity::vm::database::TypedValueData;
    #[cfg(not(feature = "direct-value-eager"))]
    use clarity::vm::types::TypeSignature;
    use clarity::vm::types::Value;
    #[cfg(not(feature = "direct-value-eager"))]
    use stacks_common::types::StacksEpochId;
    use std::io::Write;
    use std::process::Command;

    /// A cached directory window must not hide later appended ID rows.
    #[test]
    fn mapped_directory_sees_append_after_cached_eof() {
        let dir = tempfile::tempdir().unwrap();
        let file = GenerationFile::create(
            &dir.path().join("value-directory.dat"),
            FileHeader {
                kind: FileKind::ValueDirectory,
                number: 0,
                store_id: [4; 16],
            },
        )
        .unwrap();
        let mut view = MappedGenerationFile::new(file);
        let row = ValueDirectoryRow {
            partition: 0,
            offset: FILE_HEADER_BYTES as u32,
            record_length: 33,
            descriptor_id: 0,
        };
        let first = ValueId::new(1).unwrap();
        view.append_value_row(first, row).unwrap();
        assert_eq!(
            view.read_at(ValueDirectoryRow::directory_offset(first), VALUE_ROW_BYTES)
                .unwrap(),
            row.encode()
        );
        assert_eq!(view.windows.len(), 1, "directory row should be mmap-backed");
        let second = ValueId::new(2).unwrap();
        view.append_value_row(second, row).unwrap();
        assert_eq!(
            view.read_at(ValueDirectoryRow::directory_offset(second), VALUE_ROW_BYTES)
                .unwrap(),
            row.encode()
        );
    }
    /// Widely separated sparse IDs retain only four mapped directory windows.
    #[test]
    fn mapped_directory_windows_stay_bounded_above_four_gib() {
        use std::io::{Seek, SeekFrom};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("value-directory.dat");
        let header = FileHeader {
            kind: FileKind::ValueDirectory,
            number: 0,
            store_id: [9; 16],
        };
        drop(GenerationFile::create(&path, header).unwrap());
        let row = ValueDirectoryRow {
            partition: 0,
            offset: FILE_HEADER_BYTES as u32,
            record_length: 33,
            descriptor_id: 0,
        };
        let ids = [1, 5_000_000, 10_000_000, 15_000_000, 306_783_379]
            .map(|raw| ValueId::new(raw).unwrap());
        let mut file = OpenOptions::new().write(true).open(&path).unwrap();
        for id in ids {
            file.seek(SeekFrom::Start(ValueDirectoryRow::directory_offset(id)))
                .unwrap();
            file.write_all(&row.encode()).unwrap();
        }
        drop(file);
        let mut view =
            MappedGenerationFile::new(GenerationFile::open(&path, header, false).unwrap());
        for id in ids {
            assert_eq!(
                view.read_at(ValueDirectoryRow::directory_offset(id), VALUE_ROW_BYTES)
                    .unwrap(),
                row.encode()
            );
            assert!(view.windows.len() <= 4);
        }
        assert!(ValueDirectoryRow::directory_offset(ids[4]) > u32::MAX as u64);
        assert_eq!(view.windows.len(), 4);
        assert_eq!(
            view.read_at(ValueDirectoryRow::directory_offset(ids[0]), VALUE_ROW_BYTES)
                .unwrap(),
            row.encode()
        );
        assert_eq!(view.windows.len(), 4);
    }

    /// A 14-byte row may straddle the 64 MiB boundary without a read syscall.
    #[test]
    fn mapped_directory_row_crosses_window_boundary() {
        use std::io::{Seek, SeekFrom};

        const WINDOW: u64 = 64 * 1024 * 1024;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("value-directory.dat");
        let header = FileHeader {
            kind: FileKind::ValueDirectory,
            number: 0,
            store_id: [4; 16],
        };
        let file = GenerationFile::create(&path, header).unwrap();
        file.file().set_len(WINDOW + 32).unwrap();
        drop(file);
        let expected = [0x5a; VALUE_ROW_BYTES];
        let mut writer = OpenOptions::new().write(true).open(&path).unwrap();
        writer.seek(SeekFrom::Start(WINDOW - 7)).unwrap();
        writer.write_all(&expected).unwrap();
        drop(writer);
        let mut view =
            MappedGenerationFile::new(GenerationFile::open(&path, header, false).unwrap());
        assert_eq!(view.read_at(WINDOW - 7, VALUE_ROW_BYTES).unwrap(), expected);
        assert_eq!(view.windows.len(), 1);
    }

    /// Offline batching checks the entire input before mutation and keeps memory bounded.
    #[test]
    fn physical_batch_validates_before_append_and_reopens() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("generation");
        let mut db = Connection::open_in_memory().unwrap();
        StableValueStore::initialize_index(&db).unwrap();
        let mut store = StableValueStore::create(&root, [17; 16]).unwrap();
        let value = DataStoreValue::Typed(
            TypedValueData::prepare(Value::buff_from(vec![7; 64]).unwrap()).unwrap(),
        );
        let commitment = MARFValue::from_value(value.canonical());
        let bad = MARFValue([1; 40]);
        let encoded = binary_value_store::encode_entry(&value).unwrap();
        let source = |hash| PhysicalValue {
            commitment: hash,
            payload: encoded.record(),
            descriptor: encoded.shape().unwrap(),
        };
        let transaction = db.transaction().unwrap();
        let mut descriptors = PhysicalDescriptorBatch::new(&transaction, 1024);
        let unchanged = (
            store.next_value_id,
            store.next_descriptor_id,
            store.value_partition.len(),
            store.value_directory.len(),
        );
        assert!(store
            .append_physical_batch(&[source(&commitment), source(&bad)], &mut descriptors)
            .is_err());
        assert_eq!(
            (
                store.next_value_id,
                store.next_descriptor_id,
                store.value_partition.len(),
                store.value_directory.len()
            ),
            unchanged
        );
        store.set_test_id_limit(1);
        assert!(store
            .append_physical_batch(
                &[source(&commitment), source(&commitment)],
                &mut descriptors
            )
            .is_err());
        assert_eq!(
            (
                store.next_value_id,
                store.next_descriptor_id,
                store.value_partition.len(),
                store.value_directory.len()
            ),
            unchanged
        );
        store.set_test_id_limit(u32::MAX);
        let mut ids = Vec::new();
        for _ in 0..3 {
            ids.extend(
                store
                    .append_physical_batch(
                        &[source(&commitment), source(&commitment)],
                        &mut descriptors,
                    )
                    .unwrap(),
            );
            assert!(store.value_partition.pending.is_empty());
        }
        assert_eq!(
            ids.iter().map(|id| id.get()).collect::<Vec<_>>(),
            [1, 2, 3, 4, 5, 6]
        );
        assert_eq!(descriptors.entries.len(), 1);
        store.sync_unpublished().unwrap();
        drop(descriptors);
        transaction.commit().unwrap();
        drop(store);
        let mut reopened = StableValueStore::open_readonly(&root, [17; 16], &db).unwrap();
        for id in ids {
            assert_eq!(reopened.commitment(id).unwrap(), commitment);
        }
        let indexed: u64 = db
            .query_row(
                "SELECT COUNT(*) FROM clarity_stable_value_index",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            indexed, 0,
            "offline records do not publish reverse-index membership"
        );
    }

    /// Offline conversion keeps duplicate physical records without inventing dedup membership.
    #[test]
    fn physical_extent_conversion_preserves_records_and_commitments() {
        let dir = tempfile::tempdir().unwrap();
        let value = DataStoreValue::Typed(
            TypedValueData::prepare(Value::buff_from(vec![7; 64]).unwrap()).unwrap(),
        );
        let canonical = value.canonical().to_owned();
        let commitment = MARFValue::from_value(&canonical);
        let encoded = binary_value_store::encode_entry(&value).unwrap();
        let payload = encoded.record();
        let descriptor = encoded.shape().unwrap();

        let root = dir.path().join("generation");
        let mut db = Connection::open_in_memory().unwrap();
        StableValueStore::initialize_index(&db).unwrap();
        let mut target = StableValueStore::create(&root, [7; 16]).unwrap();
        let bad = MARFValue([1; 40]);
        let transaction = db.transaction().unwrap();
        let mut ids = Vec::new();
        let mut descriptors = PhysicalDescriptorBatch::new(&transaction, 1024);
        for _ in 0..2 {
            if ids.is_empty() {
                let before = target.next_value_id;
                assert!(target
                    .append_physical_unindexed(&transaction, &bad, payload, descriptor)
                    .is_err());
                assert_eq!(target.next_value_id, before);
            }
            ids.push(
                target
                    .append_physical_unindexed_cached(
                        &commitment,
                        payload,
                        descriptor,
                        &mut descriptors,
                    )
                    .unwrap(),
            );
        }
        assert_eq!((ids[0].get(), ids[1].get()), (1, 2));
        assert_eq!(descriptors.hits, 1);
        assert_eq!(descriptors.entries.len(), 1);
        assert_eq!(target.read_value_row(ids[0]).unwrap().descriptor_id, 1);
        assert_eq!(target.read_value_row(ids[1]).unwrap().descriptor_id, 1);
        let indexed: i64 = transaction
            .query_row(
                "SELECT COUNT(*) FROM clarity_stable_value_index",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(indexed, 0);
        target.sync_unpublished().unwrap();
        drop(descriptors);
        transaction.commit().unwrap();
        drop(target);
        let mut reopened = StableValueStore::open_readonly(&root, [7; 16], &db).unwrap();
        assert!(reopened.append_indexed(&db, &[value]).is_err());
        POSITIONED_PARTITION_READS.with(|count| count.set(0));
        for id in ids {
            let value = reopened.read(id).unwrap();
            assert!(!matches!(
                value.owner.as_ref(),
                StablePartitionBytes::Pending(_)
            ));
            let record = StableMappedValueRecord::from_stable_parts(value);
            assert_eq!(record.canonical().unwrap(), canonical);
        }
        assert_eq!(POSITIONED_PARTITION_READS.with(Cell::get), 0);
        assert!(!reopened.value_directory.windows.is_empty());
        assert!(!reopened.descriptor_directory.windows.is_empty());
        assert!(!reopened.descriptor_segment.windows.is_empty());
        assert!(reopened.value_partition.mapping.is_some());
    }

    /// A transaction-local descriptor hit cannot survive its rolled-back transaction.
    #[test]
    fn physical_descriptor_batch_discards_aborted_ids() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("generation");
        let mut db = Connection::open_in_memory().unwrap();
        StableValueStore::initialize_index(&db).unwrap();
        let mut store = StableValueStore::create(&root, [8; 16]).unwrap();
        let value = DataStoreValue::Typed(
            TypedValueData::prepare(Value::buff_from(vec![3; 64]).unwrap()).unwrap(),
        );
        let commitment = MARFValue::from_value(value.canonical());
        let encoded = binary_value_store::encode_entry(&value).unwrap();
        let descriptor = encoded.shape().unwrap();

        let first = {
            let transaction = db.transaction().unwrap();
            let mut batch = PhysicalDescriptorBatch::new(&transaction, 1024);
            let id = store
                .append_physical_unindexed_cached(
                    &commitment,
                    encoded.record(),
                    descriptor,
                    &mut batch,
                )
                .unwrap();
            store.sync_unpublished().unwrap();
            drop(batch);
            transaction.rollback().unwrap();
            id
        };
        drop(store);

        let mut reopened = StableValueStore::recover_open(&root, [8; 16], &db).unwrap();
        let transaction = db.transaction().unwrap();
        let mut batch = PhysicalDescriptorBatch::new(&transaction, 1024);
        let second = reopened
            .append_physical_unindexed_cached(&commitment, encoded.record(), descriptor, &mut batch)
            .unwrap();
        assert_eq!((first.get(), second.get()), (1, 2));
        assert_eq!(reopened.read_value_row(second).unwrap().descriptor_id, 2);
        reopened.sync_unpublished().unwrap();
        drop(batch);
        transaction.commit().unwrap();
        assert_eq!(reopened.read(second).unwrap().commitment, commitment);
    }

    /// Rebuild a small generation in ID order without changing logical identities.
    fn relocate_fixture(
        source: &mut StableValueStore,
        target_root: &Path,
    ) -> Result<StableValueStore, VmExecutionError> {
        let mut target = StableValueStore::create(target_root, source.store_id)?;
        for raw in 1..source.next_descriptor_id {
            let id =
                DescriptorId::new(u32::try_from(raw).map_err(store_error)?).map_err(store_error)?;
            let (segment, _) = id.segment();
            if segment != target.descriptor_segment_number {
                target.descriptor_segment.sync_all().map_err(store_error)?;
                target.descriptor_segment = MappedGenerationFile::new(
                    GenerationFile::create(
                        &target.paths.descriptor_segment(segment),
                        FileHeader {
                            kind: FileKind::DescriptorData,
                            number: segment,
                            store_id: target.store_id,
                        },
                    )
                    .map_err(store_error)?,
                );
                target.descriptor_segment_number = segment;
                target.directory_entries_dirty = true;
            }
            let bytes = source.read_descriptor(id)?;
            let row = target
                .descriptor_segment
                .append_descriptor(id, bytes.as_slice())
                .map_err(store_error)?;
            target
                .descriptor_directory
                .append_descriptor_row(id, row)
                .map_err(store_error)?;
            target.next_descriptor_id += 1;
        }
        for raw in 1..source.next_value_id {
            let id = ValueId::new(u32::try_from(raw).map_err(store_error)?).map_err(store_error)?;
            let source_row = source.read_value_row(id)?;
            let value = source.read(id)?;
            let payload = &value.owner.as_ref().as_ref()[value.record];
            let mut row = target.append_record(&value.commitment, payload)?;
            row.descriptor_id = source_row.descriptor_id;
            target
                .value_directory
                .append_value_row(id, row)
                .map_err(store_error)?;
            target.next_value_id += 1;
        }
        target.sync_unpublished()?;
        Ok(target)
    }

    /// A partition keeps its mapped prefix stable as later values become durable.
    #[test]
    fn partition_mapping_extends_without_replacing_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let file = GenerationFile::create(
            &dir.path().join("values-00000.dat"),
            FileHeader {
                kind: FileKind::ValueData,
                number: 0,
                store_id: [6; 16],
            },
        )
        .unwrap();
        let mut view = PartitionView::new(file).unwrap();
        let mut commitment = [0; 40];
        commitment[..32].fill(5);
        let payload = vec![1; 256 * 1024];
        let first = view.file.append_value(commitment, &payload).unwrap();
        view.sync_and_refresh().unwrap();
        if matches!(&view.mapping, Some(FileMapping::Stable(_))) {
            assert!(
                view.tail.is_some(),
                "the incomplete EOF page needs its own view"
            );
        }
        let first_bytes = view
            .read_at(u64::from(first.offset), first.record_length as usize)
            .unwrap();
        assert_eq!(&first_bytes[32..], payload);
        let (retained, retained_range) = view
            .read_owned(u64::from(first.offset), first.record_length as usize)
            .unwrap();
        assert!(!matches!(
            retained.as_ref(),
            StablePartitionBytes::Pending(_)
        ));
        let retained_pointer = retained.as_ref().as_ref()[retained_range.clone()].as_ptr();
        #[cfg(all(unix, target_pointer_width = "64"))]
        let stable_base = matches!(&view.mapping, Some(FileMapping::Stable(_)))
            .then(|| view.mapping.as_ref().unwrap().as_ptr());
        let second = view.file.append_value(commitment, &payload).unwrap();
        assert_eq!(
            view.read_at(u64::from(second.offset), second.record_length as usize)
                .unwrap()[32..],
            payload
        );
        view.sync_and_refresh().unwrap();
        assert_eq!(
            view.read_at(u64::from(first.offset), first.record_length as usize)
                .unwrap(),
            first_bytes
        );
        #[cfg(all(unix, target_pointer_width = "64"))]
        if let Some(base) = stable_base {
            assert_eq!(view.mapping.as_ref().unwrap().as_ptr(), base);
        }
        drop(view);
        assert_eq!(
            retained.as_ref().as_ref()[retained_range.clone()],
            first_bytes
        );
        assert_eq!(
            retained.as_ref().as_ref()[retained_range].as_ptr(),
            retained_pointer
        );
    }

    /// Duplicate values in one transaction batch share an ID across the whole batch.
    #[test]
    fn duplicate_batch_values_keep_one_committed_id() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("generation");
        let db = Connection::open_in_memory().unwrap();
        StableValueStore::initialize_index(&db).unwrap();
        let mut store = StableValueStore::create(&root, [7; 16]).unwrap();
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        let values = ["same", "other", "same", "same", "other"]
            .map(|value| DataStoreValue::Canonical(value.into()));
        let ids = store.append_indexed(&db, &values).unwrap();
        assert_eq!(
            ids.iter().map(|(_, id)| id.get()).collect::<Vec<_>>(),
            [1, 2, 1, 1, 2]
        );
        store.sync_unpublished().unwrap();
        db.execute_batch("COMMIT").unwrap();
        let rows: u32 = db
            .query_row(
                "SELECT COUNT(*) FROM clarity_stable_value_index",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rows, 2);
        drop(store);
        let mut reopened = StableValueStore::open_readonly(&root, [7; 16], &db).unwrap();
        assert!(reopened.append_indexed(&db, &values).is_err());
        assert_eq!(reopened.read(ids[0].1).unwrap().commitment, ids[2].0);
        assert_eq!(reopened.read(ids[1].1).unwrap().commitment, ids[4].0);
    }

    /// Exact reverse indexes survive reopen; rolled-back rows keep their IDs reserved.
    #[test]
    fn dedup_rollback_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("generation");
        let db = Connection::open_in_memory().unwrap();
        StableValueStore::initialize_index(&db).unwrap();
        let mut store = StableValueStore::create(&root, [7; 16]).unwrap();
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        let first = store
            .append_indexed(&db, &[DataStoreValue::Canonical("hello".into())])
            .unwrap();
        assert_eq!(first[0].1.get(), 1);
        assert_eq!(
            store
                .append_indexed(&db, &[DataStoreValue::Canonical("hello".into())])
                .unwrap()[0]
                .1,
            first[0].1
        );
        store.sync_unpublished().unwrap();
        db.execute_batch("COMMIT").unwrap();
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        let orphan = store
            .append_indexed(&db, &[DataStoreValue::Canonical("rollback".into())])
            .unwrap();
        assert_eq!(orphan[0].1.get(), 2);
        db.execute_batch("ROLLBACK").unwrap();
        drop(store);
        let mut reopened = StableValueStore::open(&root, [7; 16]).unwrap();
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        let next = reopened
            .append_indexed(&db, &[DataStoreValue::Canonical("next".into())])
            .unwrap();
        assert_eq!(next[0].1.get(), 3);
        assert_eq!(reopened.read(first[0].1).unwrap().commitment, first[0].0);
        assert_eq!(reopened.read(next[0].1).unwrap().commitment, next[0].0);
        db.execute_batch("COMMIT").unwrap();
    }

    /// A small injected ID bound fails before appending a new record.
    #[test]
    fn exhausted_id_leaves_directory_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let db = Connection::open_in_memory().unwrap();
        StableValueStore::initialize_index(&db).unwrap();
        let mut store = StableValueStore::create(&dir.path().join("generation"), [3; 16]).unwrap();
        store.set_test_id_limit(1);
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        store
            .append_indexed(&db, &[DataStoreValue::Canonical("one".into())])
            .unwrap();
        let before = store.value_directory.len();
        assert!(store
            .append_indexed(&db, &[DataStoreValue::Canonical("two".into())])
            .is_err());
        assert_eq!(store.value_directory.len(), before);
        db.execute_batch("ROLLBACK").unwrap();
    }

    /// An exhausted batch leaves both value and descriptor files unchanged.
    #[test]
    fn exhausted_batch_does_not_intern_descriptors() {
        let dir = tempfile::tempdir().unwrap();
        let db = Connection::open_in_memory().unwrap();
        StableValueStore::initialize_index(&db).unwrap();
        let mut store = StableValueStore::create(&dir.path().join("generation"), [3; 16]).unwrap();
        store.set_test_id_limit(1);
        let typed = |byte| {
            DataStoreValue::Typed(
                TypedValueData::prepare(Value::buff_from(vec![byte; 96]).unwrap()).unwrap(),
            )
        };
        let value_length = store.value_directory.len();
        let descriptor_length = store.descriptor_directory.len();
        let partition_length = store.value_partition.len();
        let segment_length = store.descriptor_segment.len();
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        assert!(store.append_indexed(&db, &[typed(1), typed(2)]).is_err());
        assert_eq!(store.value_directory.len(), value_length);
        assert_eq!(store.descriptor_directory.len(), descriptor_length);
        assert_eq!(store.value_partition.len(), partition_length);
        assert_eq!(store.descriptor_segment.len(), segment_length);
        let descriptors: u32 = db
            .query_row(
                "SELECT COUNT(*) FROM clarity_stable_descriptor_index",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(descriptors, 0);
        db.execute_batch("ROLLBACK").unwrap();

        // The fixture must require a descriptor when one value fits.
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        store.append_indexed(&db, &[typed(1)]).unwrap();
        assert!(store.descriptor_directory.len() > descriptor_length);
        db.execute_batch("ROLLBACK").unwrap();
    }

    /// Exhaustion on either the first or a later record leaves all batch storage untouched.
    #[test]
    fn exhausted_partition_batch_does_not_intern_or_append() {
        for first_fits in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let db = Connection::open_in_memory().unwrap();
            StableValueStore::initialize_index(&db).unwrap();
            let mut store =
                StableValueStore::create(&dir.path().join("generation"), [19; 16]).unwrap();
            let typed = |byte| {
                DataStoreValue::Typed(
                    TypedValueData::prepare(Value::buff_from(vec![byte; 96]).unwrap()).unwrap(),
                )
            };
            let values = [typed(1), typed(2)];
            let encoded = binary_value_store::encode_entry(&values[0]).unwrap();
            assert!(!encoded.shape().unwrap().is_empty());
            let record_length = 32 + encoded.record().len() as u64;
            let partition_length = MAX_PARTITION_BYTES - if first_fits { record_length } else { 0 };
            let path = store.paths.value_partition(u16::MAX);
            let header = FileHeader {
                kind: FileKind::ValueData,
                number: u32::from(u16::MAX),
                store_id: store.store_id,
            };
            // Sparse length exercises the real u32 offset boundary without allocating 4 GiB.
            let file = GenerationFile::create(&path, header).unwrap();
            file.file().set_len(partition_length).unwrap();
            drop(file);
            store.value_partition =
                PartitionView::new(GenerationFile::open(&path, header, true).unwrap()).unwrap();
            store.partition_number = u16::MAX;
            let before = (
                store.value_directory.len(),
                store.descriptor_directory.len(),
                store.descriptor_segment.len(),
            );
            db.execute_batch("BEGIN IMMEDIATE").unwrap();
            let error = store.append_indexed(&db, &values).unwrap_err();
            assert!(error.to_string().contains("Exhausted"), "{error}");
            assert_eq!(store.value_partition.len(), partition_length);
            assert_eq!(fs::metadata(path).unwrap().len(), partition_length);
            assert_eq!(
                (
                    store.value_directory.len(),
                    store.descriptor_directory.len(),
                    store.descriptor_segment.len(),
                ),
                before
            );
            let rows: u32 = db
                .query_row(
                    "SELECT (SELECT COUNT(*) FROM clarity_stable_descriptor_index) + \
                     (SELECT COUNT(*) FROM clarity_stable_value_index)",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(rows, 0);
            db.execute_batch("ROLLBACK").unwrap();
        }
    }

    /// Restart discards only torn, unpublished row bytes and keeps complete IDs.
    #[test]
    fn recover_torn_directory_tails() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("generation");
        let db = Connection::open_in_memory().unwrap();
        StableValueStore::initialize_index(&db).unwrap();
        let mut store = StableValueStore::create(&root, [4; 16]).unwrap();
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        let first = store
            .append_indexed(&db, &[DataStoreValue::Canonical("first".into())])
            .unwrap();
        store.sync_unpublished().unwrap();
        db.execute_batch("COMMIT").unwrap();
        drop(store);
        for path in [
            root.join("value-directory.dat"),
            root.join("descriptor-directory.dat"),
        ] {
            OpenOptions::new()
                .append(true)
                .open(path)
                .unwrap()
                .write_all(&[0xaa, 0xbb])
                .unwrap();
        }
        let mut recovered = StableValueStore::recover_open(&root, [4; 16], &db).unwrap();
        assert_eq!(recovered.read(first[0].1).unwrap().commitment, first[0].0);
        assert_eq!(recovered.next_value_id, 2);
        assert_eq!(recovered.next_descriptor_id, 1);
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        let second = recovered
            .append_indexed(&db, &[DataStoreValue::Canonical("second".into())])
            .unwrap();
        assert_eq!(second[0].1.get(), 2);
        db.execute_batch("COMMIT").unwrap();
    }

    /// An indexed commitment with missing backing bytes is never repaired by reuse.
    #[test]
    fn committed_id_without_row_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("generation");
        let db = Connection::open_in_memory().unwrap();
        StableValueStore::initialize_index(&db).unwrap();
        let store = StableValueStore::create(&root, [5; 16]).unwrap();
        drop(store);
        db.execute(
            "INSERT INTO clarity_stable_value_index VALUES(?1,1)",
            [[8u8; 40].as_slice()],
        )
        .unwrap();
        let path = root.join("value-directory.dat");
        assert!(StableValueStore::recover_open(&root, [5; 16], &db).is_err());
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&[0xaa])
            .unwrap();
        assert!(StableValueStore::recover_open(&root, [5; 16], &db).is_err());
        assert_eq!(
            fs::metadata(path).unwrap().len(),
            FILE_HEADER_BYTES as u64 + 1
        );
    }

    /// A typed projection retains mapped value bytes after append and store drop.
    #[cfg(not(feature = "direct-value-eager"))]
    #[test]
    fn typed_projection_retains_partition_mapping() {
        let dir = tempfile::tempdir().unwrap();
        let db = Connection::open_in_memory().unwrap();
        StableValueStore::initialize_index(&db).unwrap();
        let mut store = StableValueStore::create(&dir.path().join("generation"), [3; 16]).unwrap();
        let value = Value::buff_from(vec![17; 4096]).unwrap();
        let expected = TypeSignature::type_of(&value).unwrap();
        let entry = DataStoreValue::Typed(TypedValueData::prepare(value.clone()).unwrap());
        assert!(binary_value_store::encode_entry(&entry)
            .unwrap()
            .shape()
            .is_some());
        let canonical = entry.canonical().to_owned();
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        let id = store.append_indexed(&db, &[entry]).unwrap()[0].1;
        store.sync_unpublished().unwrap();
        db.execute_batch("COMMIT").unwrap();
        let retained = store.read(id).unwrap();
        let repeated = store.read(id).unwrap();
        assert!(Arc::ptr_eq(&retained.descriptor, &repeated.descriptor));
        let record = StableMappedValueRecord::from_stable_parts(retained);
        assert_eq!(record.canonical().unwrap(), canonical);
        let stored = record.stored(&expected, &StacksEpochId::latest()).unwrap();
        let StoredValue::Packed(packed) = stored.value else {
            panic!("expected mapped packed value");
        };
        let address = packed.as_view().as_sequence_bytes().unwrap().as_ptr();
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        store
            .append_indexed(&db, &[DataStoreValue::Canonical("later".into())])
            .unwrap();
        store.sync_unpublished().unwrap();
        db.execute_batch("COMMIT").unwrap();
        drop(record);
        drop(store);
        assert_eq!(
            packed.as_view().as_sequence_bytes().unwrap().as_ptr(),
            address
        );
        assert_eq!(packed.to_owned_value().unwrap(), value);
    }

    /// Historical base hits and new transactional delta rows retain exact IDs.
    #[test]
    fn ptrhash_base_and_delta_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("marf.sqlite");
        let db = Connection::open(&db_path).unwrap();
        StableValueStore::initialize_index(&db).unwrap();
        let root = dir.path().join("marf.sqlite.stable-v1");
        let store_id = [6; 16];
        let mut store = StableValueStore::create(&root, store_id).unwrap();
        db.execute_batch("CREATE TABLE clarity_stable_format(singleton INTEGER,version INTEGER,path TEXT,store_id BLOB)").unwrap();
        db.execute(
            "INSERT INTO clarity_stable_format VALUES(1,1,'marf.sqlite.stable-v1',?1)",
            [store_id.as_slice()],
        )
        .unwrap();
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        let original = store
            .append_indexed(&db, &[DataStoreValue::Canonical("base-value".into())])
            .unwrap()[0]
            .1;
        store.sync_unpublished().unwrap();
        db.execute_batch("COMMIT").unwrap();
        drop(store);
        extent_ptrhash::stable::build_and_activate(&db_path, &dir.path().join("stable-base"))
            .unwrap();
        let mut store = StableValueStore::recover_open(&root, store_id, &db).unwrap();
        assert!(store.ptrhash.is_some());
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        let reused = store
            .append_indexed(&db, &[DataStoreValue::Canonical("base-value".into())])
            .unwrap()[0]
            .1;
        assert_eq!(reused, original);
        let new_id = store
            .append_indexed(&db, &[DataStoreValue::Canonical("delta-value".into())])
            .unwrap()[0]
            .1;
        assert_eq!(new_id.get(), 2);
        assert_eq!(
            store
                .append_indexed(&db, &[DataStoreValue::Canonical("delta-value".into())])
                .unwrap()[0]
                .1,
            new_id
        );
        store.sync_unpublished().unwrap();
        db.execute_batch("COMMIT").unwrap();
        let main_count: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM clarity_stable_value_index",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let delta_count: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM clarity_stable_value_delta",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!((main_count, delta_count), (1, 1));
        drop(store);
        let mut direct = StableValueStore::open(&root, store_id).unwrap();
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        assert_eq!(
            direct
                .append_indexed(&db, &[DataStoreValue::Canonical("base-value".into())])
                .unwrap()[0]
                .1,
            original,
        );
        assert!(direct.ptrhash.is_some());
        db.execute_batch("ROLLBACK").unwrap();
        drop(direct);
        let mut reopened = StableValueStore::recover_open(&root, store_id, &db).unwrap();
        assert_eq!(
            reopened.commitment(original).unwrap(),
            MARFValue::from_value("base-value")
        );
        assert_eq!(
            reopened.commitment(new_id).unwrap(),
            MARFValue::from_value("delta-value")
        );
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        let abandoned = reopened
            .append_indexed(&db, &[DataStoreValue::Canonical("abandoned".into())])
            .unwrap()[0]
            .1;
        db.execute_batch("ROLLBACK").unwrap();
        drop(reopened);
        let mut recovered = StableValueStore::recover_open(&root, store_id, &db).unwrap();
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        let replacement = recovered
            .append_indexed(&db, &[DataStoreValue::Canonical("abandoned".into())])
            .unwrap()[0]
            .1;
        assert!(
            replacement.get() > abandoned.get(),
            "rolled-back IDs remain reserved"
        );
        recovered.sync_unpublished().unwrap();
        db.execute_batch("COMMIT").unwrap();
    }

    /// Descriptor cache remains byte bounded and bypasses oversized entries.
    #[test]
    fn descriptor_cache_evicts_without_changing_ids() {
        let mut cache = DescriptorCache::new(5);
        cache.admit(1, Arc::new(vec![1, 2, 3]));
        cache.admit(2, Arc::new(vec![4, 5]));
        assert!(cache.get(1).is_some());
        cache.admit(3, Arc::new(vec![6, 7, 8]));
        assert!(cache.get(1).is_none());
        assert_eq!(cache.get(2).unwrap().as_slice(), [4, 5]);
        assert_eq!(cache.get(3).unwrap().as_slice(), [6, 7, 8]);
        cache.admit(4, Arc::new(vec![0; 6]));
        assert!(cache.get(4).is_none());
        assert_eq!(cache.bytes, 5);
    }

    /// Exact reverse hits are bounded; oversized descriptors bypass admission.
    #[test]
    fn reverse_descriptor_cache_is_bounded() {
        let mut cache = ReverseDescriptorCache::new(5);
        cache.admit(&[1, 2, 3], 1);
        cache.admit(&[4, 5], 2);
        assert_eq!(cache.get(&[1, 2, 3]), Some(1));
        cache.admit(&[6, 7, 8], 3);
        assert_eq!(cache.get(&[1, 2, 3]), None);
        assert_eq!(cache.get(&[4, 5]), Some(2));
        assert_eq!(cache.get(&[6, 7, 8]), Some(3));
        cache.admit(&[0; 6], 4);
        assert_eq!(cache.get(&[0; 6]), None);
        assert_eq!(cache.bytes, 5);
    }

    /// An aborted descriptor never becomes a positive reverse-cache hit.
    #[test]
    fn reverse_descriptor_cache_respects_rollback() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("generation");
        let db = Connection::open_in_memory().unwrap();
        StableValueStore::initialize_index(&db).unwrap();
        let mut store = StableValueStore::create(&root, [11; 16]).unwrap();
        let old = binary_value_store::encode_entry(&DataStoreValue::Typed(
            TypedValueData::prepare(Value::buff_from(vec![7; 64]).unwrap()).unwrap(),
        ))
        .unwrap();
        let pending = binary_value_store::encode_entry(&DataStoreValue::Typed(
            TypedValueData::prepare(Value::string_ascii_from_bytes(b"pending".to_vec()).unwrap())
                .unwrap(),
        ))
        .unwrap();
        assert_ne!(old.shape(), pending.shape());
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        let committed_id = store.intern_descriptor(&db, &old).unwrap();
        store.sync_unpublished().unwrap();
        db.execute_batch("COMMIT").unwrap();
        drop(store);

        let mut reopened = StableValueStore::recover_open(&root, [11; 16], &db).unwrap();
        assert_eq!(reopened.committed_descriptor_high, u64::from(committed_id));
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        assert_eq!(reopened.intern_descriptor(&db, &old).unwrap(), committed_id);
        assert_eq!(
            reopened.reverse_descriptor_cache.get(old.shape().unwrap()),
            Some(committed_id)
        );
        let aborted_id = reopened.intern_descriptor(&db, &pending).unwrap();
        assert!(aborted_id > committed_id);
        assert_eq!(
            reopened
                .reverse_descriptor_cache
                .get(pending.shape().unwrap()),
            None
        );
        db.execute_batch("ROLLBACK").unwrap();

        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        db.execute_batch("SAVEPOINT descriptor_probe").unwrap();
        let saved_id = reopened.intern_descriptor(&db, &pending).unwrap();
        assert!(saved_id > aborted_id);
        db.execute_batch("ROLLBACK TO descriptor_probe; RELEASE descriptor_probe")
            .unwrap();
        let next_id = reopened.intern_descriptor(&db, &pending).unwrap();
        assert!(next_id > saved_id);
        assert_eq!(
            reopened
                .reverse_descriptor_cache
                .get(pending.shape().unwrap()),
            None
        );
        reopened.sync_unpublished().unwrap();
        db.execute_batch("COMMIT").unwrap();
    }

    /// A failed file barrier leaves no committed reference and never reuses its ID.
    #[test]
    fn sync_failure_before_each_publication_step_recovers() {
        for stage in 0..5 {
            let dir = tempfile::tempdir().unwrap();
            let db_path = dir.path().join("marf.sqlite");
            let root = dir.path().join("generation");
            let db = Connection::open(&db_path).unwrap();
            StableValueStore::initialize_index(&db).unwrap();
            let mut store = StableValueStore::create(&root, [stage + 1; 16]).unwrap();
            let value = || {
                DataStoreValue::Typed(
                    TypedValueData::prepare(Value::buff_from(vec![9; 128]).unwrap()).unwrap(),
                )
            };
            let original = value();
            let expected = MARFValue::from_value(original.canonical());
            db.execute_batch("BEGIN IMMEDIATE").unwrap();
            let orphan = store.append_indexed(&db, &[original]).unwrap()[0].1;
            store.fail_sync_at = Some(stage);
            assert!(store.sync_unpublished().is_err(), "sync stage {stage}");
            assert!(store.dirty);
            let observer = Connection::open(&db_path).unwrap();
            let visible: u64 = observer
                .query_row(
                    "SELECT COUNT(*) FROM clarity_stable_value_index",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(visible, 0, "sync stage {stage}");
            db.execute_batch("ROLLBACK").unwrap();
            drop(store);

            let mut recovered =
                StableValueStore::recover_open(&root, [stage + 1; 16], &db).unwrap();
            db.execute_batch("BEGIN IMMEDIATE").unwrap();
            let replacement = recovered.append_indexed(&db, &[value()]).unwrap()[0].1;
            assert!(replacement.get() > orphan.get(), "sync stage {stage}");
            recovered.sync_unpublished().unwrap();
            db.execute_batch("COMMIT").unwrap();
            drop(recovered);
            let mut final_store =
                StableValueStore::recover_open(&root, [stage + 1; 16], &db).unwrap();
            assert_eq!(final_store.read(replacement).unwrap().commitment, expected);
        }
    }

    /// Storage-full failures before each append step leave no published value reference.
    #[test]
    fn storage_full_before_append_steps_recovers() {
        for stage in 0..4 {
            let dir = tempfile::tempdir().unwrap();
            let db_path = dir.path().join("marf.sqlite");
            let root = dir.path().join("generation");
            let db = Connection::open(&db_path).unwrap();
            StableValueStore::initialize_index(&db).unwrap();
            let mut store = StableValueStore::create(&root, [stage + 31; 16]).unwrap();
            let value = || {
                DataStoreValue::Typed(
                    TypedValueData::prepare(Value::buff_from(vec![stage; 128]).unwrap()).unwrap(),
                )
            };
            let expected = MARFValue::from_value(value().canonical());
            db.execute_batch("BEGIN IMMEDIATE").unwrap();
            store.fail_append_at = Some(stage);
            let error = store.append_indexed(&db, &[value()]).unwrap_err();
            assert!(
                error.to_string().contains("storage") || error.to_string().contains("space"),
                "stage {stage}: {error}"
            );
            db.execute_batch("ROLLBACK").unwrap();
            drop(store);
            let mut recovered =
                StableValueStore::recover_open(&root, [stage + 31; 16], &db).unwrap();
            let visible: u64 = db
                .query_row(
                    "SELECT COUNT(*) FROM clarity_stable_value_index",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(visible, 0, "stage {stage}");
            db.execute_batch("BEGIN IMMEDIATE").unwrap();
            let id = recovered.append_indexed(&db, &[value()]).unwrap()[0].1;
            recovered.sync_unpublished().unwrap();
            db.execute_batch("COMMIT").unwrap();
            drop(recovered);
            let mut reopened =
                StableValueStore::recover_open(&root, [stage + 31; 16], &db).unwrap();
            assert_eq!(
                reopened.read(id).unwrap().commitment,
                expected,
                "stage {stage}"
            );
        }
    }

    /// A complete replacement generation switches atomically while old owners stay readable.
    #[test]
    fn relocated_generation_keeps_ids_and_old_reader() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("marf.sqlite");
        let old_root = dir.path().join("generation-old");
        let new_root = dir.path().join("generation-new");
        let db = Connection::open(&db_path).unwrap();
        StableValueStore::initialize_index(&db).unwrap();
        let store_id = [15; 16];
        let mut old = StableValueStore::create(&old_root, store_id).unwrap();
        db.execute_batch(
            "CREATE TABLE clarity_stable_format(\
                 singleton INTEGER PRIMARY KEY CHECK(singleton=1),\
                 version INTEGER NOT NULL CHECK(version=1),\
                 path TEXT NOT NULL,\
                 store_id BLOB NOT NULL CHECK(length(store_id)=16))",
        )
        .unwrap();
        db.execute(
            "INSERT INTO clarity_stable_format VALUES(1,1,'generation-old',?1)",
            [store_id.as_slice()],
        )
        .unwrap();
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        let first_value = DataStoreValue::Typed(
            TypedValueData::prepare(Value::buff_from(vec![7; 96]).unwrap()).unwrap(),
        );
        let first_text = first_value.canonical().to_owned();
        let first = old.append_indexed(&db, &[first_value]).unwrap()[0].1;
        let second = old
            .append_indexed(&db, &[DataStoreValue::Canonical("second".into())])
            .unwrap()[0]
            .1;
        old.sync_unpublished().unwrap();
        db.execute_batch("COMMIT").unwrap();
        let retained = old.read(first).unwrap();
        assert!(!retained.descriptor.is_empty());
        assert!(!matches!(
            retained.owner.as_ref(),
            StablePartitionBytes::Pending(_)
        ));

        let mut relocated = relocate_fixture(&mut old, &new_root).unwrap();
        assert_eq!(
            relocated.commitment(first).unwrap(),
            old.commitment(first).unwrap()
        );
        assert_eq!(
            relocated.commitment(second).unwrap(),
            old.commitment(second).unwrap()
        );
        let observer = Connection::open(&db_path).unwrap();
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        db.execute(
            "UPDATE clarity_stable_format SET path='generation-new' WHERE singleton=1",
            [],
        )
        .unwrap();
        let visible: String = observer
            .query_row(
                "SELECT path FROM clarity_stable_format WHERE singleton=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(visible, "generation-old");
        db.execute_batch("COMMIT").unwrap();
        drop(relocated);
        drop(old);
        fs::remove_dir_all(&old_root).unwrap();

        assert_eq!(
            StableMappedValueRecord::from_stable_parts(retained)
                .canonical()
                .unwrap(),
            first_text
        );
        let mut active = ValueBackend::open_registered(&db, &db_path)
            .unwrap()
            .unwrap();
        assert!(active.is_stable());
        assert_eq!(
            active.read_id(first).unwrap().canonical().unwrap(),
            first_text
        );
        assert_eq!(
            active.read_id(second).unwrap().canonical().unwrap(),
            "second"
        );
    }

    /// Dedicated child for a crash before or after generation activation commits.
    #[test]
    #[ignore = "launched by process_crash_generation_switch"]
    fn stable_generation_crash_child() {
        let Ok(root) = std::env::var("STACKSLIB_STABLE_SWITCH_ROOT") else {
            return;
        };
        let db_path = Path::new(&root).join("marf.sqlite");
        let db = Connection::open(db_path).unwrap();
        db.execute_batch(
            "BEGIN IMMEDIATE;
             UPDATE clarity_stable_format SET path='generation-new' WHERE singleton=1",
        )
        .unwrap();
        crash_at("before_generation_commit");
        db.execute_batch("COMMIT").unwrap();
        crash_at("after_generation_commit");
        panic!("generation switch did not reach its requested exit point");
    }

    /// A crash around SQLite activation selects one complete value generation.
    #[test]
    fn process_crash_generation_switch() {
        const CHILD: &str =
            "clarity_vm::database::stable_value_store::tests::stable_generation_crash_child";
        for (point, expected_path) in [
            ("before_generation_commit", "generation-old"),
            ("after_generation_commit", "generation-new"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let db_path = dir.path().join("marf.sqlite");
            let old_root = dir.path().join("generation-old");
            let new_root = dir.path().join("generation-new");
            let db = Connection::open(&db_path).unwrap();
            StableValueStore::initialize_index(&db).unwrap();
            let store_id = [41; 16];
            let mut old = StableValueStore::create(&old_root, store_id).unwrap();
            db.execute_batch(
                "CREATE TABLE clarity_stable_format(
                    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                    version INTEGER NOT NULL CHECK(version=1),
                    path TEXT NOT NULL,
                    store_id BLOB NOT NULL CHECK(length(store_id)=16))",
            )
            .unwrap();
            db.execute(
                "INSERT INTO clarity_stable_format VALUES(1,1,'generation-old',?1)",
                [store_id.as_slice()],
            )
            .unwrap();
            db.execute_batch("BEGIN IMMEDIATE").unwrap();
            let value = DataStoreValue::Canonical("generation-crash-value".into());
            let commitment = MARFValue::from_value(value.canonical());
            let id = old.append_indexed(&db, &[value]).unwrap()[0].1;
            old.sync_unpublished().unwrap();
            db.execute_batch("COMMIT").unwrap();
            let new = relocate_fixture(&mut old, &new_root).unwrap();
            assert_eq!(new.next_value_id, old.next_value_id);
            drop(new);
            drop(old);
            drop(db);

            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--ignored", "--exact", CHILD, "--nocapture"])
                .env("STACKSLIB_STABLE_SWITCH_ROOT", dir.path())
                .env("STACKSLIB_STABLE_CRASH_POINT", point)
                .output()
                .unwrap();
            assert_eq!(
                output.status.code(),
                Some(73),
                "{point}: {}",
                String::from_utf8_lossy(&output.stderr)
            );

            let db = Connection::open(&db_path).unwrap();
            let selected: String = db
                .query_row(
                    "SELECT path FROM clarity_stable_format WHERE singleton=1",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(selected, expected_path, "{point}");
            let mut active = ValueBackend::open_registered(&db, &db_path)
                .unwrap()
                .unwrap();
            let canonical = active.read_id(id).unwrap().canonical().unwrap();
            assert_eq!(canonical, "generation-crash-value", "{point}");
            assert_eq!(MARFValue::from_value(&canonical), commitment, "{point}");
        }
    }

    /// Dedicated child entry point for process-exit durability tests.
    #[test]
    #[ignore = "launched by process_crash_publication_matrix"]
    fn stable_crash_child() {
        let Ok(root) = std::env::var("STACKSLIB_STABLE_CRASH_ROOT") else {
            return;
        };
        let db_path = Path::new(&root).join("marf.sqlite");
        let generation = Path::new(&root).join("generation");
        let db = Connection::open(db_path).unwrap();
        let mut store = StableValueStore::recover_open(&generation, [23; 16], &db).unwrap();
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        let value = DataStoreValue::Typed(
            TypedValueData::prepare(Value::buff_from(vec![6; 96]).unwrap()).unwrap(),
        );
        store.append_indexed(&db, &[value]).unwrap();
        crash_at("after_append");
        store.sync_unpublished().unwrap();
        crash_at("after_sync");
        db.execute_batch("COMMIT").unwrap();
        crash_at("after_commit");
        panic!("crash test did not reach its requested exit point");
    }

    /// Process exit at each append/publication point leaves a recoverable generation.
    #[test]
    fn process_crash_publication_matrix() {
        const CHILD: &str = "clarity_vm::database::stable_value_store::tests::stable_crash_child";
        for point in [
            "descriptor_bytes",
            "descriptor_row",
            "value_bytes",
            "value_row",
            "value_index",
            "after_append",
            "after_sync",
            "after_commit",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("generation");
            let db_path = dir.path().join("marf.sqlite");
            let db = Connection::open(&db_path).unwrap();
            StableValueStore::initialize_index(&db).unwrap();
            drop(StableValueStore::create(&root, [23; 16]).unwrap());
            drop(db);
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--ignored", "--exact", CHILD, "--nocapture"])
                .env("STACKSLIB_STABLE_CRASH_ROOT", dir.path())
                .env("STACKSLIB_STABLE_CRASH_POINT", point)
                .output()
                .unwrap();
            assert_eq!(
                output.status.code(),
                Some(73),
                "{point}: {}",
                String::from_utf8_lossy(&output.stderr)
            );

            let db = Connection::open(&db_path).unwrap();
            let visible: u64 = db
                .query_row(
                    "SELECT COUNT(*) FROM clarity_stable_value_index",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(visible, u64::from(point == "after_commit"), "{point}");
            let mut recovered = StableValueStore::recover_open(&root, [23; 16], &db).unwrap();
            if visible == 0 {
                let reserved = recovered.next_value_id;
                db.execute_batch("BEGIN IMMEDIATE").unwrap();
                let value = DataStoreValue::Typed(
                    TypedValueData::prepare(Value::buff_from(vec![6; 96]).unwrap()).unwrap(),
                );
                let id = recovered.append_indexed(&db, &[value]).unwrap()[0].1;
                assert_eq!(u64::from(id.get()), reserved, "{point}");
                recovered.sync_unpublished().unwrap();
                db.execute_batch("COMMIT").unwrap();
            } else {
                assert_eq!(recovered.next_value_id, 2, "{point}");
                assert_eq!(
                    recovered.commitment(ValueId::new(1).unwrap()).unwrap(),
                    MARFValue::from_value(
                        DataStoreValue::Typed(
                            TypedValueData::prepare(Value::buff_from(vec![6; 96]).unwrap())
                                .unwrap(),
                        )
                        .canonical()
                    ),
                    "{point}"
                );
            }
        }
    }

    /// A newly written value is read from retained bytes before publication.
    #[test]
    fn current_block_value_reads_avoid_positioned_io() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("generation");
        let mut store = StableValueStore::create(&root, [5; 16]).unwrap();
        let source = DataStoreValue::Canonical("u7".into());
        let commitment = MARFValue::from_value(source.canonical());
        let encoded = binary_value_store::encode_entry(&source).unwrap();
        let payload = encoded.record().to_vec();
        let id = ValueId::new(1).unwrap();
        let rows = store
            .append_records_batch(&[PendingIndexedValue {
                id,
                commitment: commitment.clone(),
                encoded,
                descriptor_id: 0,
            }])
            .unwrap();
        store.value_directory.append_value_row_batch(&rows).unwrap();
        store.next_value_id += 1;
        POSITIONED_PARTITION_READS.with(|count| count.set(0));
        assert_eq!(store.commitment(id).unwrap(), commitment);
        let value = store.read(id).unwrap();
        assert!(matches!(
            value.owner.as_ref(),
            StablePartitionBytes::Pending(_)
        ));
        assert_eq!(&value.owner.as_ref().as_ref()[value.record], payload);
        assert_eq!(POSITIONED_PARTITION_READS.with(Cell::get), 0);
        store.sync_unpublished().unwrap();
        assert!(store.value_partition.pending.is_empty());
        assert_eq!(store.read(id).unwrap().commitment, commitment);
    }

    /// Repeated immutable IDs hit the row cache without retaining an unbounded history.
    #[test]
    fn value_row_cache_is_bounded() {
        let mut cache = ValueRowCache::new();
        let row = ValueDirectoryRow {
            partition: 0,
            offset: FILE_HEADER_BYTES as u32,
            record_length: 33,
            descriptor_id: 0,
        };
        let first = ValueId::new(1).unwrap();
        cache.admit(first, row);
        assert_eq!(cache.get(first), Some(row));
        for raw in 2..=65_537 {
            cache.admit(ValueId::new(raw).unwrap(), row);
        }
        assert_eq!(cache.rows.len(), 65_536);
        assert_eq!(cache.get(first), None);
        assert_eq!(cache.get(ValueId::new(65_537).unwrap()), Some(row));
    }

    /// Prefix collisions never return an unverified stable ID.
    #[test]
    fn stable_dedup_cache_is_bounded_and_marks_collisions() {
        let mut cache = StableDedupCache::new();
        let mut first = [0; 40];
        first[4] = 1;
        let mut collision = first;
        collision[4] = 2;
        cache.admit(&first, 7);
        assert_eq!(cache.get(&collision), Some(7));
        cache.mark_multiple(&collision);
        assert_eq!(cache.get(&first), None);
        for raw in 1..=65_537u32 {
            let mut hash = [0; 40];
            hash[..4].copy_from_slice(&raw.to_le_bytes());
            cache.admit(&hash, raw);
        }
        assert_eq!(cache.entries.len(), 65_536);
    }

    /// Hints enter the live cache only after commit and are dropped on rollback.
    #[test]
    fn stable_dedup_cache_commit_and_discard() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("generation");
        let db = Connection::open_in_memory().unwrap();
        StableValueStore::initialize_index(&db).unwrap();
        let mut store = StableValueStore::create(&root, [7; 16]).unwrap();
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        let value = || DataStoreValue::Canonical("cached".into());
        let (hash, id) = store.append_indexed(&db, &[value()]).unwrap()[0].clone();
        store.sync_unpublished().unwrap();
        db.execute_batch("COMMIT").unwrap();

        store.begin_block().unwrap();
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        assert_eq!(store.append_indexed(&db, &[value()]).unwrap()[0].1, id);
        assert_eq!(store.dedup_candidates.get(&hash.0), Some(id.get()));
        assert_eq!(store.dedup_cache.get(&hash.0), None);
        db.execute_batch("ROLLBACK").unwrap();
        store.discard_unpublished_cache();
        assert_eq!(store.dedup_cache.get(&hash.0), None);

        store.begin_block().unwrap();
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        assert_eq!(store.append_indexed(&db, &[value()]).unwrap()[0].1, id);
        db.execute_batch("COMMIT").unwrap();
        store.commit_dedup_cache();
        assert_eq!(store.dedup_cache.get(&hash.0), Some(id.get()));
        store.begin_block().unwrap();
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        assert_eq!(store.append_indexed(&db, &[value()]).unwrap()[0].1, id);
        db.execute_batch("COMMIT").unwrap();
        store.commit_dedup_cache();
        store.discard_unpublished_cache();
        assert_eq!(store.dedup_cache.get(&hash.0), None);
    }
}
