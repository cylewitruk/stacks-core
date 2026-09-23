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

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Cursor, Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::{env, fs, io};

/// Positional read: reads bytes from a file at a given offset without modifying the
/// file cursor. Maps to `pread(2)` on Unix and `seek_read` on Windows.
fn pread(fd: &fs::File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        fd.read_at(buf, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        fd.seek_read(buf, offset)
    }
    #[cfg(not(any(unix, windows)))]
    {
        compile_error!("pread: unsupported platform");
    }
}

/// Positional write: writes bytes to a file at a given offset without modifying the
/// file cursor. Maps to `pwrite(2)` on Unix and `seek_write` on Windows.
fn pwrite(fd: &fs::File, buf: &[u8], offset: u64) -> io::Result<usize> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        fd.write_at(buf, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        fd.seek_write(buf, offset)
    }
    #[cfg(not(any(unix, windows)))]
    {
        compile_error!("pwrite: unsupported platform");
    }
}

/// Positional write_all: writes the entire buffer at the given offset.
/// Loops until all bytes are written (handles short writes).
fn pwrite_all(fd: &fs::File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        let n = pwrite(fd, buf, offset)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "failed to write whole buffer",
            ));
        }
        buf = buf.get(n..).unwrap_or(&[]);
        offset += n as u64;
    }
    Ok(())
}

use rusqlite::Connection;
use stacks_common::types::chainstate::{TrieHash, TRIEHASH_ENCODED_SIZE};

use crate::chainstate::stacks::index::blob_layout::{self, BlobHeader};
use crate::chainstate::stacks::index::inline_value::{self, InlineValue};
use crate::chainstate::stacks::index::mapped_file::FileMapping;
use crate::chainstate::stacks::index::node::TrieNodeType;
use crate::chainstate::stacks::index::node::{clear_ctrl_bits, TrieNodeID, TriePtr};
use crate::chainstate::stacks::index::record::{NodeRecordFormat, RecordContext};
use crate::chainstate::stacks::index::storage::NodeHashReader;
use crate::chainstate::stacks::index::{
    bits, trie_sql, BorrowedNodeBytes, Error, MarfDataEntry, MarfTrieId, NodeDecodeScratch,
    ReadTrieItem, ReadTrieNode,
};
use crate::chainstate::stacks::index::{NodePath, TrieLeaf};
use crate::util_lib::db::sql_vacuum;

/// Reader-thread count for the bulk header fan-out.
///
/// The workers spend nearly all their time blocked on small positioned
/// reads, so the count targets a device queue depth rather than a core
/// count. Always in `16..=32`.
fn header_read_parallelism() -> usize {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    (cores * 2).clamp(16, 32)
}

/// Positioned equivalent of `read_exact`.
///
/// Matches Unix `FileExt::read_exact_at` cursor behavior in non-concurrent
/// use: the file cursor is unchanged after the call. The Windows
/// `FileExt::seek_read` does mutate the cursor, so we save and restore it
/// explicitly via the `Seek` impl on `&File`. This save/read/restore sequence
/// is not atomic with other cursor-using operations on the same file handle.
pub(super) fn read_exact_at(file: &fs::File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.read_exact_at(buf, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        // `Seek` is implemented for `&File`, so we can save and restore the
        // cursor through a local mutable binding without a `&mut File`.
        let mut handle: &fs::File = file;
        let original_pos = handle.stream_position()?;
        let read_result = (|| -> io::Result<()> {
            let mut total = 0;
            while total < buf.len() {
                let read_offset = offset.checked_add(total as u64).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "read_exact_at: offset overflow",
                    )
                })?;
                // `total` is kept within `buf` by the loop invariant
                let unread = buf.get_mut(total..).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "read_exact_at: invalid buffer offset",
                    )
                })?;
                match handle.seek_read(unread, read_offset) {
                    Ok(0) => {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "read_exact_at: short read at end of file",
                        ));
                    }
                    Ok(n) => {
                        total += n;
                    }
                    // Match `read_exact`/`read_exact_at`: an interrupted
                    // read is transient, so retry without advancing `total`.
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => {
                        return Err(e);
                    }
                }
            }
            Ok(())
        })();
        // If the read failed, propagate that error and let any restore
        // error fall on the floor (it would just mask the real failure).
        // If the read succeeded, surface a restore error so callers don't
        // silently see a moved cursor.
        let restore_result = handle.seek(SeekFrom::Start(original_pos)).map(|_| ());
        match (read_result, restore_result) {
            (Err(e), _) => Err(e),
            (Ok(()), Err(e)) => Err(e),
            (Ok(()), Ok(())) => Ok(()),
        }
    }
}

/// Async `posix_fadvise(WILLNEED)` hint over `[offset, offset + len)`.
/// Returns immediately; no-op on non-Linux targets (Windows, macOS).
#[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
fn prefetch_file_range(file: &File, offset: u64, len: u64) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        nix::fcntl::posix_fadvise(
            file.as_raw_fd(),
            offset as i64,
            len as i64,
            nix::fcntl::PosixFadviseAdvice::POSIX_FADV_WILLNEED,
        )
        .map_err(|e| io::Error::from_raw_os_error(e as i32))?;
    }
    Ok(())
}

/// Read the [`BlobHeader`] of every entry in `chunk`.
/// Worker body for [`TrieFile::bulk_read_blob_headers_sorted`].
///
/// Opens its own handle: positioned reads share no cursor state across
/// threads this way, which the Windows `read_exact_at` requires (it
/// restores the handle's cursor non-atomically).
fn read_blob_header_chunk<T: MarfTrieId + Send + Sync>(
    path: &str,
    chunk: &[MarfDataEntry<T>],
    format: NodeRecordFormat,
) -> Result<Vec<(T, BlobHeader<T>)>, Error> {
    let file = File::open(path).map_err(Error::IOError)?;
    let mut buf = vec![0u8; format.reader_prefix_len()];
    let mut headers = Vec::with_capacity(chunk.len());
    for entry in chunk {
        read_exact_at(&file, &mut buf, entry.external_offset).map_err(Error::IOError)?;
        headers.push((
            entry.block_hash.clone(),
            BlobHeader::parse_format(format, &buf)?,
        ));
    }
    Ok(headers)
}

/// Mapping between block IDs and trie offsets
pub type TrieIdOffsets = HashMap<u32, u64>;

/// Handle to a flat file containing Trie blobs, optionally mmap-accelerated for reads.
/// When `mmap` is `Some`, hot read methods (`get_node_hash`, `read_trie_item`,
/// `read_node_type_id`) slice directly into the mapped region instead of using
/// positional I/O. The `Write`/`Seek` trait impls always go through the fd.
pub struct TrieFileDisk {
    record_context: RecordContext,
    fd: File,
    path: String,
    /// Whether mapping is enabled, including deferred conventional maps for empty files.
    mmap_enabled: bool,
    /// Shared blob mapping retained by this handle and its reopened views.
    mmap: Option<FileMapping>,
    /// Cached mapping from block_id → trie file offset. Interior-mutable so that
    /// read methods can populate the cache while taking `&self`.
    trie_offsets: RefCell<TrieIdOffsets>,
}

impl TrieFileDisk {
    /// Refresh mapping coverage after synchronized writes, preserving backed pages.
    fn refresh_mapping(&mut self) -> io::Result<()> {
        if !self.mmap_enabled {
            return Ok(());
        }
        if self.fd.metadata()?.len() == 0 {
            return Ok(());
        } else if let Some(mapping) = &mut self.mmap {
            // SAFETY: Trie blobs are append-only; shared readers retain immutable prefix pages.
            unsafe {
                mapping.refresh(&self.fd)?;
            }
        } else {
            // SAFETY: The first synchronized append establishes an immutable prefix.
            self.mmap = Some(unsafe { FileMapping::map(&self.fd)? });
        }
        Ok(())
    }
}

/// Handle to a flat in-memory buffer containing Trie blobs (used for testing)
pub struct TrieFileRAM {
    record_context: RecordContext,
    fd: Cursor<Vec<u8>>,
    trie_offsets: RefCell<TrieIdOffsets>,
}

/// This is flat-file storage for a MARF's tries.  All tries are stored as contiguous byte arrays
/// within a larger byte array.  The variants differ in how those bytes are backed.  The `RAM`
/// variant stores data in RAM in a byte buffer, and the `Disk` variant stores data in a flat file
/// on disk — optionally with a memory-mapped read overlay for zero-syscall reads.
pub enum TrieFile {
    RAM(TrieFileRAM),
    Disk(TrieFileDisk),
}

/// A mapped node or the already-probed body of its first patch.
pub enum MappedTrieItem<'a> {
    /// Ordinary nodes retain the borrowed, zero-copy representation.
    Node(ReadTrieNode<'a>),
    /// Patch bytes exclude the hash, which has already been decoded.
    Patch {
        hash: TrieHash,
        marker: u8,
        payload: &'a [u8],
    },
}

impl TrieFile {
    /// Configure the database layout and immutable value resolver before reading records.
    pub fn set_record_context(&mut self, context: RecordContext) {
        match self {
            Self::Disk(disk) => disk.record_context = context,
            Self::RAM(ram) => ram.record_context = context,
        }
    }

    /// Physical layout and value source shared by this file handle's readers.
    pub fn record_context(&self) -> &RecordContext {
        match self {
            Self::Disk(disk) => &disk.record_context,
            Self::RAM(ram) => &ram.record_context,
        }
    }

    /// Make a new disk-backed TrieFile (no mmap).
    fn new_disk(path: &str, readonly: bool) -> Result<TrieFile, Error> {
        let fd = OpenOptions::new()
            .read(true)
            .write(!readonly)
            .create(!readonly)
            .open(path)?;
        Ok(TrieFile::Disk(TrieFileDisk {
            record_context: RecordContext::default(),
            fd,
            path: path.to_string(),
            mmap_enabled: false,
            mmap: None,
            trie_offsets: RefCell::new(TrieIdOffsets::new()),
        }))
    }

    /// Make a new RAM-backed TrieFile
    fn new_ram() -> TrieFile {
        TrieFile::RAM(TrieFileRAM {
            record_context: RecordContext::default(),
            fd: Cursor::new(vec![]),
            trie_offsets: RefCell::new(TrieIdOffsets::new()),
        })
    }

    /// Make a new disk-backed TrieFile with mmap-accelerated reads.
    /// Empty files reserve address space where supported; conventional maps wait for a write.
    fn new_mmap(path: &str, readonly: bool) -> Result<TrieFile, Error> {
        let fd = OpenOptions::new()
            .read(true)
            .write(!readonly)
            .create(!readonly)
            .open(path)?;
        let file_len = fd.metadata()?.len();
        let mmap = if file_len > 0 {
            // SAFETY: The .blobs file is append-only and single-writer. Existing data
            // at existing offsets never changes. The mmap is read-only.
            Some(unsafe { FileMapping::map(&fd)? })
        } else {
            // Stable reservations can be shared even before the first write.
            // Conventional mmap cannot map an empty file and remains deferred.
            unsafe { FileMapping::map(&fd).ok() }
        };
        Ok(TrieFile::Disk(TrieFileDisk {
            record_context: RecordContext::default(),
            fd,
            path: path.to_string(),
            mmap_enabled: true,
            mmap,
            trie_offsets: RefCell::new(TrieIdOffsets::new()),
        }))
    }

    /// Open an independent read-only descriptor while retaining the shared mapping.
    pub fn reopen_readonly(&self) -> Result<TrieFile, Error> {
        match self {
            Self::Disk(disk) => {
                #[cfg(unix)]
                {
                    let fd = File::open(&disk.path)?;
                    let source = disk.fd.metadata()?;
                    let reopened = fd.metadata()?;
                    if source.dev() != reopened.dev() || source.ino() != reopened.ino() {
                        return Err(io::Error::other("blob file changed while reopening").into());
                    }
                    Ok(Self::Disk(TrieFileDisk {
                        record_context: disk.record_context.clone(),
                        fd,
                        path: disk.path.clone(),
                        mmap_enabled: disk.mmap_enabled,
                        mmap: disk.mmap.clone(),
                        trie_offsets: RefCell::new(TrieIdOffsets::new()),
                    }))
                }
                #[cfg(not(unix))]
                {
                    // Preserve independent mappings where descriptor identity is unavailable.
                    let mut reopened = if disk.mmap_enabled {
                        Self::new_mmap(&disk.path, true)?
                    } else {
                        Self::new_disk(&disk.path, true)?
                    };
                    reopened.set_record_context(disk.record_context.clone());
                    Ok(reopened)
                }
            }
            Self::RAM(ram) => {
                let mut reopened = Self::new_ram();
                reopened.set_record_context(ram.record_context.clone());
                Ok(reopened)
            }
        }
    }

    /// Does the TrieFile exist at the expected path?
    pub fn exists(path: &str) -> Result<bool, Error> {
        if path == ":memory:" {
            Ok(false)
        } else {
            let blob_path = format!("{}.blobs", path);
            match fs::metadata(&blob_path) {
                Ok(_) => Ok(true),
                Err(e) => {
                    if e.kind() == io::ErrorKind::NotFound {
                        Ok(false)
                    } else {
                        return Err(e.into());
                    }
                }
            }
        }
    }

    /// Durably sync blob data to disk.
    /// No-op for RAM-backed TrieFiles.
    pub fn sync_data(&mut self) -> Result<(), io::Error> {
        if let TrieFile::Disk(ref mut data) = self {
            #[cfg(feature = "commit-residency-diagnostics")]
            let _sync = stacks_profiler::diagnostic_span!("Commit: Blob sync");
            data.fd.sync_data()?;
            #[cfg(feature = "commit-residency-diagnostics")]
            drop(_sync);
            #[cfg(feature = "commit-residency-diagnostics")]
            let _map = stacks_profiler::diagnostic_span!("Commit: Blob mapping");
            data.refresh_mapping()?;
        }
        Ok(())
    }

    /// Async-prefetch the node at `(block_id, in_block_ptr)`: hint the
    /// node's max on-disk size for its type (`node_id`) from its start. The
    /// kernel rounds to its own page size, so a node inside one page warms
    /// just that page, while one straddling a boundary warms both.
    ///
    /// Best-effort: requires the blob offset already in `trie_offsets`,
    /// else no-op. No-op for RAM-backed `TrieFile`s and non-Linux targets.
    pub(super) fn prefetch_node(
        &self,
        block_id: u32,
        in_block_ptr: u64,
        node_id: u8,
        u64_ptr_offsets: bool,
    ) {
        let TrieFile::Disk(disk) = self else {
            return;
        };
        let Some(blob_offset) = disk.trie_offsets.borrow().get(&block_id).copied() else {
            return;
        };
        let Some(abs) = blob_offset.checked_add(in_block_ptr) else {
            return;
        };
        let Ok(len) = bits::get_node_max_byte_len(node_id, u64_ptr_offsets) else {
            return;
        };
        let _ = prefetch_file_range(&disk.fd, abs, len as u64);
    }

    /// Get a copy of the path to this TrieFile.
    /// If in RAM, then the path will be ":memory:"
    pub fn get_path(&self) -> String {
        match self {
            TrieFile::RAM(_) => ":memory:".to_string(),
            TrieFile::Disk(ref disk) => disk.path.clone(),
        }
    }

    /// Instantiate a TrieFile, given the associated DB path.
    /// If path is ':memory:', then it'll be an in-RAM TrieFile.
    /// If `use_mmap` is true, the file will be memory-mapped for reads.
    /// Otherwise, it'll use seek+read I/O on `$db_path.blobs`.
    pub fn from_db_path(path: &str, readonly: bool, use_mmap: bool) -> Result<TrieFile, Error> {
        if path == ":memory:" {
            Ok(TrieFile::new_ram())
        } else {
            let blob_path = format!("{}.blobs", path);
            if use_mmap {
                TrieFile::new_mmap(&blob_path, readonly)
            } else {
                TrieFile::new_disk(&blob_path, readonly)
            }
        }
    }

    /// Append a new trie blob to external storage, and add the offset and length to the trie DB.
    /// Return the trie ID
    pub fn store_trie_blob<T: MarfTrieId>(
        &mut self,
        db: &Connection,
        bhh: &T,
        buffer: &[u8],
    ) -> Result<u32, Error> {
        let offset = self.append_trie_blob(db, buffer)?;
        test_debug!("Stored trie blob {} to offset {}", bhh, offset);
        trie_sql::write_external_trie_blob(db, bhh, offset, buffer.len() as u64)
    }

    /// Read a trie blob in its entirety from the DB
    fn read_trie_blob_from_db(db: &Connection, block_id: u32) -> Result<Vec<u8>, Error> {
        let trie_blob = {
            let mut fd = trie_sql::open_trie_blob_readonly(db, block_id)?;
            let mut trie_blob = vec![];
            fd.read_to_end(&mut trie_blob)
                .inspect_err(|e| error!("Failed to read trie blob {block_id} from DB: {e:}"))?;
            trie_blob
        };
        Ok(trie_blob)
    }

    /// Read a trie blob in its entirety from the blobs file.
    /// Takes `&self` — uses positional reads.
    pub fn read_trie_blob_bytes(&self, db: &Connection, block_id: u32) -> Result<Vec<u8>, Error> {
        let (offset, length) = trie_sql::get_external_trie_offset_length(db, block_id)?;
        let mut buf = vec![0u8; length as usize];
        let n = self
            .read_bytes_at(&mut buf, offset)
            .inspect_err(|e| error!("Failed to read trie blob {block_id}: {e:}"))?;
        if n < length as usize {
            return Err(Error::CorruptionError(format!(
                "Short read for trie blob {block_id}: expected {length} bytes, read {n}"
            )));
        }
        buf.truncate(n);
        Ok(buf)
    }

    /// Vacuum the database and report the size before and after.
    ///
    /// Returns database errors.  Filesystem errors from reporting the file size change are masked.
    fn inner_post_migrate_vacuum(db: &Connection, db_path: &str) -> Result<(), Error> {
        // for fun, report the shrinkage
        let size_before_opt = fs::metadata(db_path)
            .map(|stat| Some(stat.len()))
            .unwrap_or(None);

        info!(
            "Preemptively vacuuming the database file to free up space after copying trie blobs to a separate file"
        );
        sql_vacuum(db)?;

        let size_after_opt = fs::metadata(db_path)
            .map(|stat| Some(stat.len()))
            .unwrap_or(None);

        if let (Some(sz_before), Some(sz_after)) = (size_before_opt, size_after_opt) {
            debug!("Shrank DB from {} to {} bytes", sz_before, sz_after);
        }

        Ok(())
    }

    /// Vacuum the database, and set up and tear down the necessary environment variables to
    /// use same parent directory for scratch space.
    ///
    /// Infallible -- any vacuum errors are masked.
    fn post_migrate_vacuum(db: &Connection, db_path: &str) {
        // set SQLITE_TMPDIR if it isn't set already
        let mut set_sqlite_tmpdir = false;
        let mut old_tmpdir_opt = None;
        if let Some(parent_path) = Path::new(db_path).parent() {
            if env::var("SQLITE_TMPDIR").is_err() {
                debug!(
                    "Sqlite will store temporary migration state in '{}'",
                    parent_path.display()
                );
                env::set_var("SQLITE_TMPDIR", parent_path);
                set_sqlite_tmpdir = true;
            }

            // also set TMPDIR
            old_tmpdir_opt = env::var("TMPDIR").ok();
            env::set_var("TMPDIR", parent_path);
        }

        // don't materialize the error; just warn
        let res = TrieFile::inner_post_migrate_vacuum(db, db_path);
        if let Err(e) = res {
            warn!("Failed to VACUUM the MARF DB post-migration: {:?}", &e);
        }

        if set_sqlite_tmpdir {
            debug!("Unset SQLITE_TMPDIR");
            env::remove_var("SQLITE_TMPDIR");
        }
        if let Some(old_tmpdir) = old_tmpdir_opt {
            debug!("Restore TMPDIR to '{}'", &old_tmpdir);
            env::set_var("TMPDIR", old_tmpdir);
        } else {
            debug!("Unset TMPDIR");
            env::remove_var("TMPDIR");
        }
    }

    /// Copy the trie blobs out of a sqlite3 DB into their own file.
    /// NOTE: this is *not* thread-safe.  Do not call while the DB is being used by another thread.
    pub fn export_trie_blobs<T: MarfTrieId>(
        &mut self,
        db: &Connection,
        db_path: &str,
    ) -> Result<(), Error> {
        if trie_sql::detect_partial_migration(db)? {
            panic!(
                "PARTIAL MIGRATION DETECTED! This is an irrecoverable error. You will need to restart your node from genesis."
            );
        }

        let max_block = trie_sql::count_blocks(db)?;
        info!(
            "Migrate {} blocks to external blob storage at {}",
            max_block,
            &self.get_path()
        );

        for block_id in 0..(max_block + 1) {
            match trie_sql::is_unconfirmed_block(db, block_id) {
                Ok(true) => {
                    test_debug!("Skip block_id {} since it's unconfirmed", block_id);
                    continue;
                }
                Err(Error::NotFoundError) => {
                    test_debug!("Skip block_id {} since it's not a block", block_id);
                    continue;
                }
                Ok(false) => {
                    // get the blob
                    let trie_blob = TrieFile::read_trie_blob_from_db(db, block_id)?;

                    // get the block ID
                    let bhh: T = trie_sql::get_block_hash(db, block_id)?;

                    // append the blob, replacing the current trie blob
                    if block_id % 1000 == 0 {
                        info!(
                            "Migrate block {} ({} of {}) to external blob storage",
                            &bhh, block_id, max_block
                        );
                    }

                    // append directly to file, so we can get the true offset
                    let offset = match self {
                        TrieFile::Disk(ref disk) => disk.fd.metadata()?.len(),
                        TrieFile::RAM(ref ram) => ram.fd.get_ref().len() as u64,
                    };
                    match self {
                        TrieFile::Disk(ref disk) => {
                            pwrite_all(&disk.fd, &trie_blob, offset)?;
                        }
                        TrieFile::RAM(ref mut ram) => {
                            let data = ram.fd.get_mut();
                            let start = offset as usize;
                            let end = start + trie_blob.len();
                            if data.len() < end {
                                data.resize(end, 0);
                            }
                            data.get_mut(start..end)
                                .expect("BUG: just resized to cover range")
                                .copy_from_slice(&trie_blob);
                        }
                    }

                    test_debug!("Stored trie blob {} to offset {}", bhh, offset);
                    trie_sql::update_external_trie_blob(
                        db,
                        &bhh,
                        offset,
                        trie_blob.len() as u64,
                        block_id,
                    )?;
                }
                Err(e) => {
                    test_debug!(
                        "Failed to determine if {} is unconfirmed: {:?}",
                        block_id,
                        &e
                    );
                    return Err(e);
                }
            }
        }

        TrieFile::post_migrate_vacuum(db, db_path);

        debug!("Mark MARF trie migration of '{}' as finished", db_path);
        trie_sql::set_migrated(db).expect("FATAL: failed to mark DB as migrated");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chainstate::stacks::index::blob_layout;

    fn remove_if_exists(path: &str) {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => panic!("failed to remove {path}: {e}"),
        }
    }

    /// Full-page growth preserves the mapped prefix; an incomplete tail remains readable.
    #[cfg(all(unix, target_pointer_width = "64"))]
    #[test]
    fn mmap_append_and_sync_keep_prefix_address() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blobs");
        let page = unsafe { nix::libc::sysconf(nix::libc::_SC_PAGESIZE) } as usize;
        fs::write(&path, vec![1; page]).unwrap();
        let mut file = TrieFile::new_mmap(path.to_str().unwrap(), false).unwrap();
        let original = file.mmap_slice_at(0, page).unwrap().as_ptr();
        let TrieFile::Disk(disk) = &file else {
            panic!("disk expected")
        };
        pwrite_all(&disk.fd, &vec![2; page + 3], page as u64).unwrap();
        file.sync_data().unwrap();
        assert_eq!(file.mmap_slice_at(0, page).unwrap().as_ptr(), original);
        assert_eq!(
            file.mmap_slice_at(page as u64, page).unwrap(),
            vec![2; page]
        );
        assert!(file.mmap_slice_at((2 * page) as u64, 3).is_none());
        let mut tail = [0; 6];
        assert_eq!(
            file.read_bytes_at(&mut tail, (2 * page - 3) as u64)
                .unwrap(),
            6
        );
        assert_eq!(tail, [2; 6]);
        file.sync_data().unwrap();
        assert_eq!(file.mmap_slice_at(0, page).unwrap().as_ptr(), original);
    }

    /// Reopened readers share append visibility and retain the mapping after the writer drops.
    #[cfg(all(unix, target_pointer_width = "64"))]
    #[test]
    fn reopened_mmap_shares_lifetime_and_growth() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blobs");
        let page = unsafe { nix::libc::sysconf(nix::libc::_SC_PAGESIZE) } as usize;
        let mut writer = TrieFile::new_mmap(path.to_str().unwrap(), false).unwrap();
        let reader = writer.reopen_readonly().unwrap();
        let sibling = reader.reopen_readonly().unwrap();
        let TrieFile::Disk(disk) = &writer else {
            panic!("disk expected")
        };
        pwrite_all(&disk.fd, &vec![4; page], 0).unwrap();
        writer.sync_data().unwrap();
        let original = writer.mmap_slice_at(0, page).unwrap().as_ptr();
        let retained = reader.mmap_slice_at(0, page).unwrap();
        assert_eq!(retained.as_ptr(), original);
        let TrieFile::Disk(disk) = &writer else {
            panic!("disk expected")
        };
        pwrite_all(&disk.fd, &vec![5; page + 1], page as u64).unwrap();
        writer.sync_data().unwrap();
        assert_eq!(retained, vec![4; page]);
        assert_eq!(
            sibling.mmap_slice_at(page as u64, page).unwrap(),
            vec![5; page]
        );
        assert!(sibling.mmap_slice_at((page * 2) as u64, 1).is_none());
        let mut tail = [0];
        assert_eq!(
            sibling.read_bytes_at(&mut tail, (page * 2) as u64).unwrap(),
            1
        );
        assert_eq!(tail, [5]);
        drop(writer);
        assert_eq!(reader.mmap_slice_at(0, page).unwrap().as_ptr(), original);
        drop(reader);
        assert_eq!(sibling.mmap_slice_at(0, page).unwrap().as_ptr(), original);
    }

    /// Sharing must not pair an old mapping with a descriptor for a replacement file.
    #[cfg(unix)]
    #[test]
    fn reopened_mmap_rejects_replaced_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blobs");
        let file = TrieFile::new_mmap(path.to_str().unwrap(), false).unwrap();
        fs::rename(&path, dir.path().join("old")).unwrap();
        fs::write(&path, [1]).unwrap();
        assert!(file.reopen_readonly().is_err());
    }

    #[test]
    fn stale_mmap_boundary_reads_fall_back_to_pread() {
        let db_path = "/tmp/stacks-index-file-stale-mmap-boundary.sqlite";
        let blob_path = format!("{db_path}.blobs");
        remove_if_exists(db_path);
        remove_if_exists(&blob_path);
        fs::write(&blob_path, b"abcd").unwrap();

        let trie_file = TrieFile::from_db_path(db_path, false, true).unwrap();

        let TrieFile::Disk(disk) = &trie_file else {
            panic!("expected disk trie file");
        };
        pwrite_all(&disk.fd, b"efgh", 4).unwrap();
        disk.fd.sync_data().unwrap();

        assert!(trie_file.mmap_slice_at(4, 1).is_none());
        assert!(trie_file.mmap_slice_at(2, 4).is_none());

        let mut exact_eof = [0; 4];
        let n = trie_file.read_bytes_at(&mut exact_eof, 4).unwrap();
        assert_eq!(n, 4);
        assert_eq!(&exact_eof, b"efgh");

        let mut straddling = [0; 4];
        let n = trie_file.read_bytes_at(&mut straddling, 2).unwrap();
        assert_eq!(n, 4);
        assert_eq!(&straddling, b"cdef");

        remove_if_exists(&blob_path);
    }
}

impl Write for TrieFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            TrieFile::RAM(ram) => ram.fd.write(buf),
            TrieFile::Disk(disk) => disk.fd.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            TrieFile::RAM(ram) => ram.fd.flush(),
            TrieFile::Disk(disk) => disk.fd.flush(),
        }
    }
}

impl Seek for TrieFile {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        match self {
            TrieFile::RAM(ram) => ram.fd.seek(pos),
            TrieFile::Disk(disk) => disk.fd.seek(pos),
        }
    }
}

/// NodeHashReader for TrieFile
pub struct TrieFileNodeHashReader<'a> {
    db: &'a Connection,
    file: &'a TrieFile,
    block_id: u32,
    trie_offset: Option<u64>,
}

impl<'a> TrieFileNodeHashReader<'a> {
    pub fn new(
        db: &'a Connection,
        file: &'a TrieFile,
        block_id: u32,
        trie_offset: Option<u64>,
    ) -> TrieFileNodeHashReader<'a> {
        TrieFileNodeHashReader {
            db,
            file,
            block_id,
            trie_offset,
        }
    }
}

impl NodeHashReader for TrieFileNodeHashReader<'_> {
    fn read_node_hash<W: Write>(&mut self, ptr: &TriePtr, w: &mut W) -> Result<(), Error> {
        let hash = self
            .file
            .get_node_hash(self.db, self.block_id, ptr, self.trie_offset)?;
        w.write_all(hash.as_ref()).map_err(|e| e.into())
    }
}

impl TrieFile {
    /// Cache a known trie blob offset.
    pub(super) fn cache_trie_offset(&mut self, block_id: u32, offset: u64) -> Result<(), Error> {
        self.validate_header_at(offset)?;
        let offsets_cache = match self {
            TrieFile::RAM(ref mut ram) => &mut ram.trie_offsets,
            TrieFile::Disk(ref mut disk) => &mut disk.trie_offsets,
        };
        offsets_cache.borrow_mut().insert(block_id, offset);
        Ok(())
    }

    /// Determine the file offset in the TrieFile where a serialized trie starts.
    /// The offsets are stored in the given DB, and are cached indefinitely once loaded.
    /// Takes `&self` — the offset cache uses interior mutability (`RefCell`).
    pub fn get_trie_offset(&self, db: &Connection, block_id: u32) -> Result<u64, Error> {
        let cache = match self {
            TrieFile::RAM(ref ram) => &ram.trie_offsets,
            TrieFile::Disk(ref disk) => &disk.trie_offsets,
        };
        if let Some(offset) = cache.borrow().get(&block_id).copied() {
            return Ok(offset);
        }
        let (offset, _length) = trie_sql::get_external_trie_offset_length(db, block_id)?;
        self.validate_header_at(offset)?;
        cache.borrow_mut().insert(block_id, offset);
        Ok(offset)
    }

    /// Validate an immutable trie's version before admitting its offset to the cache.
    fn validate_header_at(&self, offset: u64) -> Result<(), Error> {
        let format = self.record_context().format;
        if format == NodeRecordFormat::Legacy {
            return Ok(());
        }
        let mut bytes = [0u8; blob_layout::ROOT_NODE_OFFSET];
        let count = self.read_bytes_at(&mut bytes, offset)?;
        format.validate_trie_header(&bytes[..count])
    }

    /// Read up to `buf.len()` bytes at a given file offset without modifying any cursor state.
    /// Uses mmap when available and the requested range is covered; otherwise falls back to `pread`.
    ///
    /// The mmap may be stale when another connection (e.g., the chains coordinator) has
    /// appended data that this connection's mmap doesn't cover yet. In that case we
    /// gracefully fall back to `pread`, which always sees the latest file contents.
    fn read_bytes_at(&self, buf: &mut [u8], offset: u64) -> Result<usize, Error> {
        match self {
            TrieFile::Disk(ref disk) => {
                if let Some(ref mmap) = disk.mmap {
                    let start = offset as usize;
                    let end = start.checked_add(buf.len()).ok_or(Error::OverflowError)?;
                    if let Some(src) = mmap.get(start..end) {
                        buf.copy_from_slice(src);
                        return Ok(buf.len());
                    }

                    // Mmap doesn't cover this full range; fall through to pread.
                    //
                    // Reopened views share coverage, but independent storage opens may
                    // still have stale mappings. The partial EOF page is also read here.
                }
                let mut total = 0;
                while total < buf.len() {
                    let read_offset = offset
                        .checked_add(total as u64)
                        .ok_or(Error::OverflowError)?;
                    let dst = buf.get_mut(total..).ok_or(Error::OverflowError)?;
                    match pread(&disk.fd, dst, read_offset) {
                        Ok(0) => break,
                        Ok(n) => total += n,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        Err(e) => return Err(Error::IOError(e)),
                    }
                }
                Ok(total)
            }
            TrieFile::RAM(ref ram) => {
                let data = ram.fd.get_ref();
                let start = offset as usize;
                let bytes = data.get(start..).ok_or(Error::NotFoundError)?;
                let len = buf.len().min(bytes.len());
                let dst = buf.get_mut(..len).ok_or(Error::NotFoundError)?;
                let src = bytes.get(..len).ok_or(Error::NotFoundError)?;
                dst.copy_from_slice(src);
                Ok(len)
            }
        }
    }

    /// Get a slice from the mmap region at the given offset, if mmap is active and covers at
    /// least `min_len` bytes. Returns `None` if the mmap is disabled or stale for the requested
    /// range.
    fn mmap_slice_at(&self, offset: u64, min_len: usize) -> Option<&[u8]> {
        if let TrieFile::Disk(ref disk) = self {
            let mmap = disk.mmap.as_ref()?;
            let start = offset as usize;
            let end = start.checked_add(min_len)?;
            mmap.get(start..end)?;
            mmap.get(start..)
        } else {
            None
        }
    }

    /// Read bytes at a known file position into scratch, then decode.
    /// For mmap: slices directly into the mapped region (zero-copy decode).
    /// For disk: uses `pread` into scratch's node_bytes buffer.
    /// For RAM: slices the in-memory buffer.
    fn read_item_at_offset<'a>(
        &self,
        file_offset: u64,
        ptr: &TriePtr,
        scratch: &'a mut impl NodeDecodeScratch,
    ) -> Result<ReadTrieItem<'a>, Error> {
        let format = self.record_context().format;
        let max_len = format.max_record_len(ptr.id())?;

        // Fast path: mmap slice available — decode directly from it.
        if let Some(bytes) = self.mmap_slice_at(file_offset, max_len) {
            return bits::read_trie_item_from_slice_format(bytes, ptr.id(), format, scratch);
        }
        // Slow path: positional read into scratch's reusable buffer, then decode.
        // Pattern: take buffer → pread → decode (extracts hash + node ID from bytes,
        // copies decoded node into scratch slots) → restore buffer for reuse.
        let mut buf = scratch.take_node_bytes();
        if buf.len() < max_len {
            buf.resize(max_len, 0);
        }
        let buf_len = buf.len();
        let read_buf = buf.get_mut(..max_len).ok_or_else(|| {
            Error::CorruptionError(format!(
                "Trie blob read buffer shorter than requested max length: {} < {}",
                buf_len, max_len
            ))
        })?;
        let n = self.read_bytes_at(read_buf, file_offset)?;
        let result: Result<(Option<TrieHash>, TrieNodeID), Error> = (|| {
            let read_bytes = buf.get(..n).ok_or_else(|| Error::OverflowError)?;
            let record = format.parse(read_bytes)?;
            record.decode_into_scratch(ptr.id(), scratch)?;
            Ok((record.hash, record.logical_type()))
        })();
        scratch.restore_node_bytes(buf);
        let (hash, stored_node_id) = result?;
        if stored_node_id == TrieNodeID::Patch {
            Ok(ReadTrieItem::from_patch(scratch.patch(), hash))
        } else {
            Ok(ReadTrieItem::from_node(ReadTrieNode::from_state_borrowed(
                scratch.get_ref(),
                hash,
            )))
        }
    }

    /// Read hash bytes at a known file position.
    fn read_hash_at(&self, file_offset: u64) -> Result<TrieHash, Error> {
        if self.record_context().format.is_type_first() {
            return self.read_node_type_at(file_offset).map(|(_, hash)| hash);
        }
        if let Some(bytes) = self.mmap_slice_at(file_offset, TRIEHASH_ENCODED_SIZE) {
            let (hash, _) = bits::parse_hash_from_bytes(bytes)?;
            return Ok(hash);
        }
        let mut buf = [0u8; TRIEHASH_ENCODED_SIZE];
        let n = self.read_bytes_at(&mut buf, file_offset)?;
        if n < TRIEHASH_ENCODED_SIZE {
            return Err(Error::CorruptionError(
                "Failed to read hash in full via pread".to_string(),
            ));
        }
        Ok(TrieHash(buf))
    }

    /// Read node type ID and hash at a known file position.
    fn read_node_type_at(&self, file_offset: u64) -> Result<(TrieNodeID, TrieHash), Error> {
        let context = self.record_context();
        if let Some(bytes) = self.mmap_slice_at(file_offset, TRIEHASH_ENCODED_SIZE + 1) {
            let record = context.format.parse(bytes)?;
            // A hashless leaf can straddle the stable mapping's complete-page boundary.
            if record.hash.is_some()
                || bytes.len() >= context.format.max_record_len(TrieNodeID::Leaf as u8)?
            {
                return Ok((record.logical_type(), context.hash(record)?));
            }
        }
        let mut buf = [0u8; 1 + 33 + inline_value::LENGTH_BYTES + inline_value::MAX_BYTES];
        let prefix_len = TRIEHASH_ENCODED_SIZE + 1;
        let count = self.read_bytes_at(&mut buf[..prefix_len], file_offset)?;
        let record = context.format.parse(&buf[..count])?;
        if let Some(hash) = record.hash {
            return Ok((record.logical_type(), hash));
        }
        let count = self.read_bytes_at(&mut buf, file_offset)?;
        let record = context.format.parse(&buf[..count])?;
        Ok((record.logical_type(), context.hash(record)?))
    }

    /// Obtain a [`TrieHash`] for a node, given its block ID and pointer.
    ///
    /// If `trie_offset` is `Some`, uses the pre-resolved offset (bypassing the offset
    /// cache). Otherwise resolves the offset from the cache or SQL.
    pub fn get_node_hash(
        &self,
        db: &Connection,
        block_id: u32,
        ptr: &TriePtr,
        trie_offset: Option<u64>,
    ) -> Result<TrieHash, Error> {
        let offset = trie_offset.map_or_else(|| self.get_trie_offset(db, block_id), Ok)?;
        self.read_hash_at(offset + ptr.ptr())
    }

    /// Read a trie item (node or patch) at the given block and pointer.
    ///
    /// If `trie_offset` is `Some`, uses the pre-resolved offset (bypassing the offset
    /// cache). Otherwise resolves the offset from the cache or SQL.
    pub fn read_trie_item<'a>(
        &self,
        db: &Connection,
        block_id: u32,
        ptr: &TriePtr,
        trie_offset: Option<u64>,
        scratch: &'a mut impl NodeDecodeScratch,
    ) -> Result<ReadTrieItem<'a>, Error> {
        let offset = trie_offset.map_or_else(|| self.get_trie_offset(db, block_id), Ok)?;
        self.read_item_at_offset(offset + ptr.ptr(), ptr, scratch)
    }

    /// Read a trie item as borrowed bytes from the mmap region (zero-copy).
    /// Returns `None` if the requested bytes are not covered by mmap.
    /// Takes `&self` — no cursor state needed.
    pub fn read_trie_item_borrowed<'a>(
        &'a self,
        db: &Connection,
        block_id: u32,
        ptr: &TriePtr,
        trie_offset: Option<u64>,
    ) -> Result<Option<MappedTrieItem<'a>>, Error> {
        if !matches!(self, TrieFile::Disk(disk) if disk.mmap.is_some()) {
            return Ok(None);
        }
        let offset = trie_offset.map_or_else(|| self.get_trie_offset(db, block_id), Ok)?;
        let format = self.record_context().format;
        let max_len = format.max_record_len(ptr.id())?;
        let Some(bytes) = self.mmap_slice_at(offset + ptr.ptr(), max_len) else {
            return Ok(None);
        };
        let record = format.parse(bytes)?;
        if record.logical_type() == TrieNodeID::Patch {
            let hash = record
                .hash
                .ok_or_else(|| Error::CorruptionError("Patch missing hash".into()))?;
            return Ok(Some(MappedTrieItem::Patch {
                hash,
                marker: record.marker,
                payload: record.payload,
            }));
        }
        if record.logical_type() as u8 != clear_ctrl_bits(ptr.id()) {
            return Err(Error::CorruptionError(
                "Mapped node disagrees with pointer".into(),
            ));
        }
        if record.marker == TrieNodeID::InlineLeaf as u8 {
            let mut path = NodePath::default();
            let path_len = bits::path_from_bytes_slice_into(record.payload, &mut path)?;
            let (value_range, descriptor_range, _) =
                InlineValue::ranges(&record.payload[path_len..])?;
            let start = usize::try_from(offset.checked_add(ptr.ptr()).ok_or(Error::OverflowError)?)
                .map_err(|_| Error::OverflowError)?
                .checked_add(record.prefix_len)
                .and_then(|n| n.checked_add(path_len))
                .ok_or(Error::OverflowError)?;
            let range = start
                .checked_add(value_range.start)
                .ok_or(Error::OverflowError)?
                ..start
                    .checked_add(descriptor_range.end)
                    .ok_or(Error::OverflowError)?;
            let TrieFile::Disk(disk) = self else {
                unreachable!("checked mapped disk")
            };
            let mapping = disk.mmap.as_ref().expect("checked mapping").clone();
            let inline = InlineValue::from_mapping(mapping, range, value_range.len() as u8)?;
            let leaf = TrieLeaf {
                path,
                data: None,
                extent: None,
                inline: Some(inline),
            };
            return Ok(Some(MappedTrieItem::Node(ReadTrieNode::from_owned(
                TrieNodeType::Leaf(leaf),
                None,
            ))));
        }
        let node_bytes = BorrowedNodeBytes::from_record(record);
        Ok(Some(MappedTrieItem::Node(ReadTrieNode::from_stable_bytes(
            node_bytes,
            record.hash,
        ))))
    }

    /// Read the node type ID and hash at the given block and pointer.
    pub fn read_node_type_id(
        &self,
        db: &Connection,
        block_id: u32,
        ptr: &TriePtr,
        trie_offset: Option<u64>,
    ) -> Result<(TrieNodeID, TrieHash), Error> {
        let offset = trie_offset.map_or_else(|| self.get_trie_offset(db, block_id), Ok)?;
        self.read_node_type_at(offset + ptr.ptr())
    }

    /// Append a serialized trie to the TrieFile.
    /// Returns the offset at which it was appended.
    pub fn append_trie_blob(&mut self, db: &Connection, buf: &[u8]) -> Result<u64, Error> {
        let offset = trie_sql::get_external_blobs_length(db)?;
        test_debug!("Write trie of {} bytes at {}", buf.len(), offset);

        match self {
            TrieFile::Disk(ref mut disk) => {
                pwrite_all(&disk.fd, buf, offset)?;
                disk.fd.sync_data()?;
                disk.refresh_mapping()?;
            }
            TrieFile::RAM(ref mut ram) => {
                let data = ram.fd.get_mut();
                let start = offset as usize;
                let end = start + buf.len();
                if data.len() < end {
                    data.resize(end, 0);
                }
                data.get_mut(start..end)
                    .expect("BUG: just resized to cover range")
                    .copy_from_slice(buf);
            }
        }
        Ok(offset)
    }

    /// Read a block's [`BlobHeader`].
    pub(super) fn read_blob_header<T: MarfTrieId>(
        &mut self,
        db: &Connection,
        block_id: u32,
    ) -> Result<BlobHeader<T>, Error> {
        let blob_offset = self.get_trie_offset(db, block_id)?;
        let format = self.record_context().format;
        let mut buf = vec![0u8; format.reader_prefix_len()];
        self.read_blob_bytes_at(blob_offset, &mut buf)?;
        Ok(BlobHeader::parse_format(format, &buf)?)
    }

    /// Bulk-read the [`BlobHeader`] of every entry in offset order.
    /// Returns a map keyed by block hash.
    ///
    /// Fans the entries out to oversubscribed reader threads in contiguous
    /// offset-sorted chunks: blocked positioned reads on N threads keep the
    /// device queue ~N deep, hiding per-read latency. Each entry costs one
    /// header-sized read, so only the pages backing headers are touched.
    ///
    /// Requires a `Disk`-backed `TrieFile`; callers should fall back to
    /// [`Self::read_blob_header`] otherwise.
    pub(super) fn bulk_read_blob_headers_sorted<T: MarfTrieId + Send + Sync>(
        &self,
        sorted_entries: &[MarfDataEntry<T>],
    ) -> Result<HashMap<T, BlobHeader<T>>, Error> {
        let TrieFile::Disk(disk) = self else {
            return Err(Error::UnsupportedTrieFileType(
                "bulk_read_blob_headers_sorted",
            ));
        };
        if sorted_entries.is_empty() {
            return Ok(HashMap::new());
        }

        let num_threads = header_read_parallelism().min(sorted_entries.len());
        let chunk_size = sorted_entries.len().div_ceil(num_threads);
        let path = &disk.path;
        let format = disk.record_context.format;

        std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(num_threads);
            for chunk in sorted_entries.chunks(chunk_size) {
                let handle = std::thread::Builder::new()
                    .name("marf-header-read".into())
                    .spawn_scoped(scope, move || {
                        read_blob_header_chunk::<T>(path, chunk, format)
                    })
                    .map_err(Error::IOError)?;
                handles.push(handle);
            }

            let mut headers = HashMap::with_capacity(sorted_entries.len());
            let mut first_err: Option<Error> = None;
            for handle in handles {
                match handle.join() {
                    Ok(Ok(chunk_headers)) => headers.extend(chunk_headers),
                    Ok(Err(e)) => {
                        first_err.get_or_insert(e);
                    }
                    Err(_) => {
                        first_err.get_or_insert(Error::IOError(io::Error::other(
                            "blob header reader thread panicked",
                        )));
                    }
                }
            }
            match first_err {
                Some(e) => Err(e),
                None => Ok(headers),
            }
        })
    }

    /// Read blob bytes without moving the file cursor.
    fn read_blob_bytes_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), Error> {
        match self {
            TrieFile::Disk(disk) => read_exact_at(&disk.fd, buf, offset).map_err(Error::IOError),
            TrieFile::RAM(ram) => {
                let bytes = ram.fd.get_ref();
                let start = usize::try_from(offset).map_err(|_| Error::OverflowError)?;
                let end = start.checked_add(buf.len()).ok_or(Error::OverflowError)?;
                let slice = bytes.get(start..end).ok_or_else(|| {
                    Error::CorruptionError(format!(
                        "TrieFile::RAM read out of bounds: offset {start} + len {} > buffer len {}",
                        buf.len(),
                        bytes.len()
                    ))
                })?;
                buf.copy_from_slice(slice);
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod testing {
    use rusqlite::params;

    use super::*;
    use crate::types::chainstate::TrieHash;

    impl TrieFile {
        pub fn read_trie_blob(&self, db: &Connection, block_id: u32) -> Result<Vec<u8>, Error> {
            self.read_trie_blob_bytes(db, block_id)
        }

        /// Obtain a TrieHash for a node, given the node's block's hash (used only in testing)
        pub fn get_node_hash_by_bhh<T: MarfTrieId>(
            &self,
            db: &Connection,
            bhh: &T,
            ptr: &TriePtr,
        ) -> Result<TrieHash, Error> {
            let (offset, _length) = trie_sql::get_external_trie_offset_length_by_bhh(db, bhh)?;
            self.read_hash_at(offset + ptr.ptr())
        }

        /// Get all (root hash, trie hash) pairs for this TrieFile
        pub fn read_all_block_hashes_and_roots<T: MarfTrieId>(
            &self,
            db: &Connection,
        ) -> Result<Vec<(TrieHash, T)>, Error> {
            let mut s =
                db.prepare("SELECT block_hash, external_offset FROM marf_data WHERE unconfirmed = 0 ORDER BY block_hash")?;
            let rows = s.query_and_then(params![], |row| {
                let block_hash: T = row.get_unwrap("block_hash");
                let offset_i64: i64 = row.get_unwrap("external_offset");
                let offset = offset_i64 as u64;
                let start = blob_layout::ROOT_NODE_OFFSET as u64;

                let root_hash = self.read_hash_at(offset + start)?;

                trace!(
                    "Root hash for block {} at offset {} is {}",
                    &block_hash,
                    offset + start,
                    &root_hash
                );
                Ok((root_hash, block_hash))
            })?;
            rows.collect()
        }
    }
}

#[cfg(all(test, unix))]
mod hash_tail_tests {
    use super::*;
    use crate::chainstate::stacks::index::node::TrieNodeType;
    use crate::chainstate::stacks::index::{MARFValue, TrieLeaf};

    /// Hash probes must read the full leaf when only its prefix lies in mapped complete pages.
    #[test]
    fn hashless_leaf_crossing_mapped_prefix_uses_file_tail() {
        // SAFETY: sysconf returns the process page size and does not dereference pointers.
        let page = unsafe { nix::libc::sysconf(nix::libc::_SC_PAGESIZE) };
        assert!(page > 0);
        for format in [
            NodeRecordFormat::TypeFirstV2,
            NodeRecordFormat::TypeFirstV3,
            NodeRecordFormat::TypeFirstV4,
        ] {
            let leaf = TrieLeaf::from_value(&[17; 31], MARFValue([7; 40]));
            let expected = bits::get_leaf_hash(&leaf);
            let mut encoded = vec![];
            format
                .write_node(&mut encoded, &TrieNodeType::Leaf(leaf), expected, false)
                .unwrap();
            let offset = page as usize - 40;
            let mut bytes = vec![0; offset];
            bytes.extend(&encoded);
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("tail.blobs");
            fs::write(&path, bytes).unwrap();
            let mut file = TrieFile::new_mmap(path.to_str().unwrap(), true).unwrap();
            file.set_record_context(RecordContext {
                format,
                value_resolver: None,
            });
            assert_eq!(
                file.read_node_type_at(offset as u64).unwrap(),
                (TrieNodeID::Leaf, expected)
            );
        }
    }
}
