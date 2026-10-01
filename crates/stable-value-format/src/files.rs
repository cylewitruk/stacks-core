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

//! Positionally addressed files for an unpublished stable-ID generation.
//!
//! The caller owns transaction boundaries and publishes only synchronized,
//! verified files. These primitives never expose a MARF leaf or SQLite marker.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use crate::{
    compact_commitment, decode_value_record, DescriptorDirectoryRow, DescriptorId, FileHeader,
    FileKind, FormatError, ValueDirectoryRow, ValueId, DESCRIPTOR_ROW_BYTES, FILE_HEADER_BYTES,
    MAX_DESCRIPTOR_BYTES, MAX_PARTITION_BYTES, MAX_RECORD_BYTES, VALUE_ROW_BYTES,
};

/// File-set path functions; file names are generation-relative and deterministic.
pub struct GenerationPaths {
    /// Directory containing only one stable-ID generation's files.
    pub root: PathBuf,
}

impl GenerationPaths {
    /// Resolve a single value partition without external artifact paths.
    pub fn value_partition(&self, number: u16) -> PathBuf {
        self.root.join(format!("values-{number:05}.dat"))
    }

    /// Resolve one descriptor-data segment.
    pub fn descriptor_segment(&self, number: u32) -> PathBuf {
        self.root.join(format!("descriptors-{number:08}.dat"))
    }

    /// Resolve the fixed-width value directory.
    pub fn value_directory(&self) -> PathBuf {
        self.root.join("value-directory.dat")
    }

    /// Resolve the fixed-width descriptor directory.
    pub fn descriptor_directory(&self) -> PathBuf {
        self.root.join("descriptor-directory.dat")
    }
}

/// A checked file handle retaining the generation identity and cursor.
pub struct GenerationFile {
    file: File,
    header: FileHeader,
    length: u64,
}

impl GenerationFile {
    /// Create a fresh file and write its versioned identity before any content.
    pub fn create(path: &Path, header: FileHeader) -> io::Result<Self> {
        validate_role(header)?;
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;
        file.write_all(&header.encode())?;
        Ok(Self {
            file,
            header,
            length: FILE_HEADER_BYTES as u64,
        })
    }

    /// Open an existing file, rejecting swapped roles, generations, or truncation.
    pub fn open(path: &Path, header: FileHeader, writable: bool) -> io::Result<Self> {
        validate_role(header)?;
        let mut file = OpenOptions::new().read(true).write(writable).open(path)?;
        let length = file.metadata()?.len();
        if length < FILE_HEADER_BYTES as u64 {
            return Err(invalid(FormatError::Length));
        }
        if matches!(header.kind, FileKind::ValueData | FileKind::DescriptorData)
            && length > MAX_PARTITION_BYTES
        {
            return Err(invalid(FormatError::Bounds));
        }
        let mut bytes = [0; FILE_HEADER_BYTES];
        file.read_exact(&mut bytes)?;
        FileHeader::decode(&bytes, header.kind, header.number, header.store_id).map_err(invalid)?;
        Ok(Self {
            file,
            header,
            length,
        })
    }

    /// Return the observed file length without reading content.
    pub fn len(&self) -> u64 {
        self.length
    }

    /// Return whether this file contains only its header.
    pub fn is_empty(&self) -> bool {
        self.length == FILE_HEADER_BYTES as u64
    }

    /// Reserve physical space for a value partition without advancing its logical EOF.
    ///
    /// This is done only at partition creation or recovery, never per record. The
    /// directory rows and file length still delimit published values.
    pub fn preallocate_value_partition(&self, target: u64) -> io::Result<()> {
        if self.header.kind != FileKind::ValueData
            || target < self.length
            || target > MAX_PARTITION_BYTES
        {
            return Err(invalid(FormatError::Bounds));
        }
        preallocate_keep_size(&self.file, target)
    }

    /// Read exactly one positional range without changing the exposed watermark.
    pub fn read_at(&mut self, offset: u64, length: usize) -> io::Result<Vec<u8>> {
        let end = offset
            .checked_add(length as u64)
            .ok_or_else(|| invalid(FormatError::Bounds))?;
        if offset < FILE_HEADER_BYTES as u64 || end > self.length {
            return Err(invalid(FormatError::Bounds));
        }
        let mut bytes = vec![0; length];
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.read_exact(&mut bytes)?;
        Ok(bytes)
    }

    /// Append bytes after validating the target file's maximum length.
    fn append(&mut self, bytes: &[u8], maximum: u64) -> io::Result<u64> {
        let end = self
            .length
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| invalid(FormatError::Bounds))?;
        if end > maximum {
            return Err(invalid(FormatError::Bounds));
        }
        let offset = self.length;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(bytes)?;
        self.length = end;
        Ok(offset)
    }

    /// Synchronize this file once at the enclosing publication boundary.
    pub fn sync_all(&self) -> io::Result<()> {
        self.file.sync_all()
    }

    /// Borrow the underlying descriptor for an owning runtime mapping.
    pub fn file(&self) -> &File {
        &self.file
    }

    /// Append a compact record to one value partition, never crossing 4 GiB.
    pub fn append_value(
        &mut self,
        commitment: [u8; 40],
        payload: &[u8],
    ) -> io::Result<ValueDirectoryRow> {
        self.append_value_owned(commitment, payload)
            .map(|(row, _)| row)
    }

    /// Append one compact record and return the bytes for pending in-memory reads.
    fn append_value_owned(
        &mut self,
        commitment: [u8; 40],
        payload: &[u8],
    ) -> io::Result<(ValueDirectoryRow, Vec<u8>)> {
        if self.header.kind != FileKind::ValueData {
            return Err(invalid(FormatError::Identity));
        }
        let partition =
            u16::try_from(self.header.number).map_err(|_| invalid(FormatError::Bounds))?;
        let digest = compact_commitment(commitment).map_err(invalid)?;
        let record_length = 32usize
            .checked_add(payload.len())
            .ok_or_else(|| invalid(FormatError::Bounds))?;
        if !(33..=MAX_RECORD_BYTES as usize).contains(&record_length) {
            return Err(invalid(FormatError::Bounds));
        }
        let mut record = Vec::with_capacity(record_length);
        record.extend_from_slice(&digest);
        record.extend_from_slice(payload);
        let offset = self.append(&record, MAX_PARTITION_BYTES)?;
        Ok((
            ValueDirectoryRow {
                partition,
                offset: u32::try_from(offset).map_err(|_| invalid(FormatError::Bounds))?,
                record_length: record_length as u32,
                descriptor_id: 0,
            },
            record,
        ))
    }

    /// Append several records with one positioned write, retaining their bytes for readers.
    pub fn append_value_batch(
        &mut self,
        values: &[([u8; 40], &[u8])],
    ) -> io::Result<Vec<(ValueDirectoryRow, Vec<u8>)>> {
        if self.header.kind != FileKind::ValueData {
            return Err(invalid(FormatError::Identity));
        }
        let partition =
            u16::try_from(self.header.number).map_err(|_| invalid(FormatError::Bounds))?;
        let mut next = self.length;
        let mut joined = Vec::new();
        let mut records = Vec::with_capacity(values.len());
        for (commitment, payload) in values {
            let digest = compact_commitment(*commitment).map_err(invalid)?;
            let length = 32usize
                .checked_add(payload.len())
                .ok_or_else(|| invalid(FormatError::Bounds))?;
            if !(33..=MAX_RECORD_BYTES as usize).contains(&length) {
                return Err(invalid(FormatError::Bounds));
            }
            let end = next
                .checked_add(length as u64)
                .ok_or_else(|| invalid(FormatError::Bounds))?;
            if end > MAX_PARTITION_BYTES {
                return Err(invalid(FormatError::Bounds));
            }
            let mut record = Vec::with_capacity(length);
            record.extend_from_slice(&digest);
            record.extend_from_slice(payload);
            joined.extend_from_slice(&record);
            records.push((
                ValueDirectoryRow {
                    partition,
                    offset: u32::try_from(next).map_err(|_| invalid(FormatError::Bounds))?,
                    record_length: length as u32,
                    descriptor_id: 0,
                },
                record,
            ));
            next = end;
        }
        if !joined.is_empty() {
            self.append(&joined, MAX_PARTITION_BYTES)?;
        }
        Ok(records)
    }

    /// Append exact descriptor bytes to their assigned segment.
    pub fn append_descriptor(
        &mut self,
        id: DescriptorId,
        bytes: &[u8],
    ) -> io::Result<DescriptorDirectoryRow> {
        if self.header.kind != FileKind::DescriptorData || id.segment().0 != self.header.number {
            return Err(invalid(FormatError::Identity));
        }
        if bytes.is_empty() || bytes.len() > MAX_DESCRIPTOR_BYTES as usize {
            return Err(invalid(FormatError::Bounds));
        }
        let offset = self.append(bytes, MAX_PARTITION_BYTES)?;
        Ok(DescriptorDirectoryRow {
            offset: u32::try_from(offset).map_err(|_| invalid(FormatError::Bounds))?,
            length: bytes.len() as u32,
        })
    }

    /// Add one row to the value directory at the expected sequential ID.
    pub fn append_value_row(&mut self, id: ValueId, row: ValueDirectoryRow) -> io::Result<()> {
        if self.header.kind != FileKind::ValueDirectory
            || self.length != ValueDirectoryRow::directory_offset(id)
            || row.offset < FILE_HEADER_BYTES as u32
            || !(33..=MAX_RECORD_BYTES).contains(&row.record_length)
        {
            return Err(invalid(FormatError::Identity));
        }
        self.append(&row.encode(), u64::MAX)?;
        Ok(())
    }

    /// Append consecutive fixed-width value rows with one write.
    pub fn append_value_row_batch(
        &mut self,
        rows: &[(ValueId, ValueDirectoryRow)],
    ) -> io::Result<()> {
        if self.header.kind != FileKind::ValueDirectory {
            return Err(invalid(FormatError::Identity));
        }
        let mut next = self.length;
        let mut bytes = Vec::with_capacity(rows.len() * VALUE_ROW_BYTES);
        for (id, row) in rows {
            if next != ValueDirectoryRow::directory_offset(*id)
                || row.offset < FILE_HEADER_BYTES as u32
                || !(33..=MAX_RECORD_BYTES).contains(&row.record_length)
            {
                return Err(invalid(FormatError::Identity));
            }
            bytes.extend_from_slice(&row.encode());
            next += VALUE_ROW_BYTES as u64;
        }
        if !bytes.is_empty() {
            self.append(&bytes, u64::MAX)?;
        }
        Ok(())
    }

    /// Add one row to the descriptor directory at the expected sequential ID.
    pub fn append_descriptor_row(
        &mut self,
        id: DescriptorId,
        row: DescriptorDirectoryRow,
    ) -> io::Result<()> {
        if self.header.kind != FileKind::DescriptorDirectory
            || self.length != DescriptorDirectoryRow::directory_offset(id)
            || row.offset < FILE_HEADER_BYTES as u32
            || row.length == 0
            || row.length > MAX_DESCRIPTOR_BYTES
        {
            return Err(invalid(FormatError::Identity));
        }
        self.append(&row.encode(), u64::MAX)?;
        Ok(())
    }

    /// Read and validate one value row against a partition's published length.
    pub fn value_row(
        &mut self,
        id: ValueId,
        partition_length: u64,
    ) -> io::Result<ValueDirectoryRow> {
        if self.header.kind != FileKind::ValueDirectory {
            return Err(invalid(FormatError::Identity));
        }
        let bytes = self.read_at(ValueDirectoryRow::directory_offset(id), VALUE_ROW_BYTES)?;
        ValueDirectoryRow::decode(&bytes, partition_length).map_err(invalid)
    }

    /// Read and validate one descriptor row against a segment's published length.
    pub fn descriptor_row(
        &mut self,
        id: DescriptorId,
        segment_length: u64,
    ) -> io::Result<DescriptorDirectoryRow> {
        if self.header.kind != FileKind::DescriptorDirectory {
            return Err(invalid(FormatError::Identity));
        }
        let bytes = self.read_at(
            DescriptorDirectoryRow::directory_offset(id),
            DESCRIPTOR_ROW_BYTES,
        )?;
        DescriptorDirectoryRow::decode(&bytes, segment_length).map_err(invalid)
    }

    /// Fetch a value record and verify its canonical commitment against the leaf.
    pub fn value_record(
        &mut self,
        row: ValueDirectoryRow,
        expected: [u8; 40],
    ) -> io::Result<Vec<u8>> {
        if self.header.kind != FileKind::ValueData || self.header.number != u32::from(row.partition)
        {
            return Err(invalid(FormatError::Identity));
        }
        ValueDirectoryRow::decode(&row.encode(), self.length).map_err(invalid)?;
        let bytes = self.read_at(u64::from(row.offset), row.record_length as usize)?;
        let (digest, _) = decode_value_record(&bytes).map_err(invalid)?;
        if compact_commitment(expected).map_err(invalid)? != digest {
            return Err(invalid(FormatError::Commitment));
        }
        Ok(bytes)
    }
}

/// Convert a format failure into an I/O error for file-backed operations.
fn invalid(error: FormatError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

/// Restrict partition numbers and reserve directory header number zero.
fn validate_role(header: FileHeader) -> io::Result<()> {
    let valid = match header.kind {
        FileKind::ValueData => u16::try_from(header.number).is_ok(),
        FileKind::DescriptorData => true,
        FileKind::ValueDirectory | FileKind::DescriptorDirectory => header.number == 0,
    };
    if valid {
        Ok(())
    } else {
        Err(invalid(FormatError::Bounds))
    }
}

/// Reserve disk blocks without changing EOF, which is the recovery watermark.
#[cfg(target_os = "linux")]
fn preallocate_keep_size(file: &File, target: u64) -> io::Result<()> {
    let length = i64::try_from(target).map_err(|_| invalid(FormatError::Bounds))?;
    // SAFETY: the file descriptor remains owned by `file`; the call only reserves
    // blocks and FALLOC_FL_KEEP_SIZE preserves the on-disk logical file length.
    let result = unsafe { libc::fallocate(file.as_raw_fd(), libc::FALLOC_FL_KEEP_SIZE, 0, length) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// APFS preallocation reserves blocks while leaving the published EOF intact.
#[cfg(target_os = "macos")]
fn preallocate_keep_size(file: &File, target: u64) -> io::Result<()> {
    let current = file.metadata()?.len();
    let additional = target
        .checked_sub(current)
        .ok_or_else(|| invalid(FormatError::Bounds))?;
    let mut allocation = libc::fstore_t {
        fst_flags: libc::F_ALLOCATECONTIG,
        fst_posmode: libc::F_PEOFPOSMODE,
        fst_offset: 0,
        fst_length: i64::try_from(additional).map_err(|_| invalid(FormatError::Bounds))?,
        fst_bytesalloc: 0,
    };
    // SAFETY: fcntl borrows a live descriptor and a valid fstore_t. F_PREALLOCATE
    // does not extend EOF; the fallback allows fragmented but fully reserved space.
    let result = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_PREALLOCATE, &mut allocation) };
    if result == 0 {
        return Ok(());
    }
    allocation.fst_flags = libc::F_ALLOCATEALL;
    // SAFETY: same descriptor and initialized allocation request as above.
    let result = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_PREALLOCATE, &mut allocation) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Keep unsupported platforms explicit rather than silently changing durability.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn preallocate_keep_size(_file: &File, _target: u64) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "physical value partition preallocation",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Batched writes preserve ordered IDs and reject invalid input before mutation.
    #[test]
    fn batched_value_and_directory_writes_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store_id = [3; 16];
        let mut values = GenerationFile::create(
            &dir.path().join("values"),
            FileHeader {
                kind: FileKind::ValueData,
                number: 0,
                store_id,
            },
        )
        .unwrap();
        let mut directory = GenerationFile::create(
            &dir.path().join("directory"),
            FileHeader {
                kind: FileKind::ValueDirectory,
                number: 0,
                store_id,
            },
        )
        .unwrap();
        let mut first = [0; 40];
        first[..32].fill(1);
        let mut second = [0; 40];
        second[..32].fill(2);
        let records = values
            .append_value_batch(&[(first, &[9, 8]), (second, &[7, 6, 5])])
            .unwrap();
        assert_eq!(records[0].1, [&first[..32], &[9, 8]].concat());
        assert_eq!(records[1].1, [&second[..32], &[7, 6, 5]].concat());
        let ids = [
            (ValueId::new(1).unwrap(), records[0].0),
            (ValueId::new(2).unwrap(), records[1].0),
        ];
        assert!(directory.append_value_row_batch(&ids[1..]).is_err());
        assert_eq!(directory.len(), FILE_HEADER_BYTES as u64);
        directory.append_value_row_batch(&ids).unwrap();
        for (id, row) in ids {
            assert_eq!(directory.value_row(id, values.len()).unwrap(), row);
        }
        assert_eq!(values.value_record(ids[0].1, first).unwrap(), records[0].1);
        assert_eq!(values.value_record(ids[1].1, second).unwrap(), records[1].1);
    }

    /// The two directories and their data files round-trip independently.
    #[test]
    fn complete_small_generation() {
        let dir = tempfile::tempdir().unwrap();
        let paths = GenerationPaths {
            root: dir.path().to_path_buf(),
        };
        let store_id = [7; 16];
        let header = |kind, number| FileHeader {
            kind,
            number,
            store_id,
        };
        let mut values =
            GenerationFile::create(&paths.value_partition(0), header(FileKind::ValueData, 0))
                .unwrap();
        let mut value_directory = GenerationFile::create(
            &paths.value_directory(),
            header(FileKind::ValueDirectory, 0),
        )
        .unwrap();
        let mut descriptors = GenerationFile::create(
            &paths.descriptor_segment(0),
            header(FileKind::DescriptorData, 0),
        )
        .unwrap();
        let mut descriptor_directory = GenerationFile::create(
            &paths.descriptor_directory(),
            header(FileKind::DescriptorDirectory, 0),
        )
        .unwrap();
        let id = ValueId::new(1).unwrap();
        let descriptor_id = DescriptorId::new(1).unwrap();
        let descriptor = vec![0x92; 67_588];
        let descriptor_row = descriptors
            .append_descriptor(descriptor_id, &descriptor)
            .unwrap();
        descriptor_directory
            .append_descriptor_row(descriptor_id, descriptor_row)
            .unwrap();
        let mut commitment = [0; 40];
        commitment[..32].fill(3);
        let mut row = values.append_value(commitment, &[1, 2, 3]).unwrap();
        row.descriptor_id = descriptor_id.get();
        value_directory.append_value_row(id, row).unwrap();
        for file in [
            &values,
            &value_directory,
            &descriptors,
            &descriptor_directory,
        ] {
            file.sync_all().unwrap();
        }
        let mut values = GenerationFile::open(
            &paths.value_partition(0),
            header(FileKind::ValueData, 0),
            false,
        )
        .unwrap();
        let mut value_directory = GenerationFile::open(
            &paths.value_directory(),
            header(FileKind::ValueDirectory, 0),
            false,
        )
        .unwrap();
        let row = value_directory.value_row(id, values.len()).unwrap();
        assert_eq!(
            values.value_record(row, commitment).unwrap()[32..],
            [1, 2, 3]
        );
        let descriptor_row = descriptor_directory
            .descriptor_row(descriptor_id, descriptors.len())
            .unwrap();
        assert_eq!(
            descriptors
                .read_at(
                    u64::from(descriptor_row.offset),
                    descriptor_row.length as usize
                )
                .unwrap(),
            descriptor
        );
        commitment[0] ^= 1;
        assert!(values.value_record(row, commitment).is_err());
        assert!(GenerationFile::open(
            &paths.value_partition(0),
            header(FileKind::DescriptorData, 0),
            false
        )
        .is_err());
    }

    /// Sequential IDs and file identity are checked before appending any bytes.
    #[test]
    fn wrong_id_or_role_does_not_mutate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("value-directory.dat");
        let mut file = GenerationFile::create(
            &path,
            FileHeader {
                kind: FileKind::ValueDirectory,
                number: 0,
                store_id: [1; 16],
            },
        )
        .unwrap();
        let before = file.len();
        let row = ValueDirectoryRow {
            partition: 0,
            offset: 32,
            record_length: 33,
            descriptor_id: 0,
        };
        assert!(file
            .append_value_row(ValueId::new(2).unwrap(), row)
            .is_err());
        assert!(file
            .append_descriptor_row(
                DescriptorId::new(1).unwrap(),
                DescriptorDirectoryRow {
                    offset: 32,
                    length: 1
                }
            )
            .is_err());
        assert_eq!(file.len(), before);
    }

    /// Physical reservation must not turn unwritten bytes into readable values.
    #[test]
    fn preallocation_preserves_logical_eof() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("values-00000.dat");
        let mut file = GenerationFile::create(
            &path,
            FileHeader {
                kind: FileKind::ValueData,
                number: 0,
                store_id: [9; 16],
            },
        )
        .unwrap();
        file.preallocate_value_partition(1024 * 1024).unwrap();
        assert_eq!(file.len(), FILE_HEADER_BYTES as u64);
        assert_eq!(
            file.file().metadata().unwrap().len(),
            FILE_HEADER_BYTES as u64
        );
        #[cfg(unix)]
        let reserved_blocks = {
            use std::os::unix::fs::MetadataExt;
            let blocks = file.file().metadata().unwrap().blocks();
            assert!(blocks * 512 >= 1024 * 1024);
            blocks
        };
        assert!(file.read_at(FILE_HEADER_BYTES as u64, 1).is_err());
        let mut commitment = [0; 40];
        commitment[..32].fill(1);
        let row = file.append_value(commitment, &[7; 16]).unwrap();
        assert_eq!(file.file().metadata().unwrap().len(), file.len());
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(file.file().metadata().unwrap().blocks(), reserved_blocks);
        }
        assert_eq!(file.value_record(row, commitment).unwrap()[32..], [7; 16]);
    }

    /// A sparse directory row beyond 4 GiB remains addressable by its u32 ID.
    #[test]
    fn sparse_value_directory_reads_past_four_gib() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("value-directory.dat");
        let header = FileHeader {
            kind: FileKind::ValueDirectory,
            number: 0,
            store_id: [13; 16],
        };
        let file = GenerationFile::create(&path, header).unwrap();
        let id = ValueId::new(
            u32::try_from(
                (u64::from(u32::MAX) - FILE_HEADER_BYTES as u64) / VALUE_ROW_BYTES as u64 + 2,
            )
            .unwrap(),
        )
        .unwrap();
        let offset = ValueDirectoryRow::directory_offset(id);
        assert!(offset > u64::from(u32::MAX));
        let row = ValueDirectoryRow {
            partition: 0,
            offset: FILE_HEADER_BYTES as u32,
            record_length: 33,
            descriptor_id: 0,
        };
        file.file()
            .set_len(offset + VALUE_ROW_BYTES as u64)
            .unwrap();
        let mut writable = file.file().try_clone().unwrap();
        writable.seek(SeekFrom::Start(offset)).unwrap();
        writable.write_all(&row.encode()).unwrap();
        drop(file);

        let mut reopened = GenerationFile::open(&path, header, false).unwrap();
        assert_eq!(reopened.value_row(id, 65).unwrap(), row);
    }

    /// A value may end exactly at 4 GiB, after which the writer must roll over.
    #[test]
    fn sparse_value_partition_reaches_exact_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("values-00000.dat");
        let header = FileHeader {
            kind: FileKind::ValueData,
            number: 0,
            store_id: [14; 16],
        };
        let file = GenerationFile::create(&path, header).unwrap();
        file.file().set_len(MAX_PARTITION_BYTES - 33).unwrap();
        drop(file);

        let mut reopened = GenerationFile::open(&path, header, true).unwrap();
        let mut commitment = [0; 40];
        commitment[..32].fill(8);
        let row = reopened.append_value(commitment, &[2]).unwrap();
        assert_eq!(u64::from(row.offset), MAX_PARTITION_BYTES - 33);
        assert_eq!(
            u64::from(row.offset) + u64::from(row.record_length),
            MAX_PARTITION_BYTES
        );
        assert_eq!(reopened.len(), MAX_PARTITION_BYTES);
        assert_eq!(reopened.value_record(row, commitment).unwrap()[32..], [2]);
        assert!(reopened.append_value(commitment, &[3]).is_err());
        assert_eq!(reopened.len(), MAX_PARTITION_BYTES);
        assert_eq!(
            crate::next_value_slot(0, reopened.len(), 33).unwrap(),
            (1, FILE_HEADER_BYTES as u32)
        );
    }
}
