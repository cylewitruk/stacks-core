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

//! Append-only blob files and lazily refreshed mappings.
use std::path::{Path, PathBuf};
use std::{fs, io};

use super::Blobs;
use crate::positioned_io::write_all_at;
use crate::{Error, corrupt};

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
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Make sure the map covers `[0, end)`. `Ok(false)` if the file is shorter.
    fn ensure_mapped(&mut self, end: u64) -> io::Result<bool> {
        if let Some(m) = &self.map
            && m.len() as u64 >= end
        {
            return Ok(true);
        }
        let len = self.file.metadata()?.len();
        if len < end {
            return Ok(false);
        }
        if len == 0 {
            return Ok(true);
        }
        let len = usize::try_from(len).map_err(|_| {
            io::Error::new(io::ErrorKind::Unsupported, "blobs file too large to map")
        })?;
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
