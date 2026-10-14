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

//! Immutable legacy input and mmap-backed external trie access.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use blockstack_lib::chainstate::stacks::index::record::NodeRecordFormat;
use blockstack_lib::util_lib::db::sqlite_readonly_uri;
use memmap2::Mmap;
use rusqlite::{Connection, OpenFlags};

use crate::Result;

/// Enumerate only files owned by a legacy database, using registrations for optional sidecars.
pub fn owned_legacy_files(db: &Connection, path: &Path) -> Result<BTreeSet<PathBuf>> {
    let mut files = BTreeSet::from([path.to_path_buf()]);
    for suffix in [".blobs", "-wal", "-shm", "-journal"] {
        files.insert(PathBuf::from(format!("{}{suffix}", path.display())));
    }
    let indexed: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='marf_direct_hash_index')",
        [],
        |row| row.get(0),
    )?;
    if indexed {
        let mut statement = db.prepare("SELECT token FROM marf_direct_hash_index")?;
        for token in statement.query_map([], |row| row.get::<_, Vec<u8>>(0))? {
            let token = token?;
            if token.len() != 16 {
                return Err("invalid source direct-index token".into());
            }
            let encoded = stacks_common::util::hash::to_hex(&token);
            files.insert(PathBuf::from(format!(
                "{}.hashes-{encoded}",
                path.display()
            )));
        }
    }
    Ok(files)
}

/// Metadata bound before and after conversion; excludes access times.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Identity {
    /// Canonical source path.
    path: PathBuf,
    /// Byte length at preflight.
    length: u64,
    /// Last modification time at preflight.
    modified: SystemTime,
}

impl Identity {
    /// Capture a regular source file without modifying it.
    fn read(path: &Path) -> Result<Self> {
        let metadata = fs::metadata(path)?;
        if !metadata.is_file() {
            return Err("source is not a regular file".into());
        }
        Ok(Self {
            path: path.canonicalize()?,
            length: metadata.len(),
            modified: metadata.modified()?,
        })
    }
}

/// An offline source and its immutable external blob mapping, if present.
pub struct Source {
    /// Read-only immutable SQLite snapshot.
    pub db: Connection,
    /// Canonical source database path.
    pub path: PathBuf,
    /// External blobs; SQL-resident sources need no such file.
    blobs: Option<Mmap>,
    /// Original database/blob metadata.
    identities: Vec<Identity>,
}

impl Source {
    /// Require a checkpointed legacy database before creating any destination.
    pub fn open(path: &Path) -> Result<Self> {
        Self::check_journals(path)?;
        let path = path.canonicalize()?;
        let mut identities = vec![Identity::read(&path)?];
        let db = Connection::open_with_flags(
            sqlite_readonly_uri(&path, true)?,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )?;
        db.execute_batch("PRAGMA cache_size=-16384; PRAGMA mmap_size=268435456")?;
        if NodeRecordFormat::from_database(&db)? != NodeRecordFormat::Legacy {
            return Err(
                "input must be legacy; experimental formats require their frozen importer".into(),
            );
        }
        let pending: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name IN ('format_migration_state','clarity_extent_format','clarity_stable_format'))",
            [], |row| row.get(0),
        )?;
        if pending {
            return Err("source contains experimental or incomplete migration metadata".into());
        }
        let blob_path = PathBuf::from(format!("{}.blobs", path.display()));
        let blobs = if blob_path.exists() {
            let identity = Identity::read(&blob_path)?;
            let map = if identity.length == 0 {
                None
            } else {
                let file = File::open(&blob_path)?;
                // SAFETY: the caller supplies an offline snapshot. We retain mapping ownership,
                // never write to the source, and verify source metadata before publication.
                let map = unsafe { Mmap::map(&file)? };
                #[cfg(target_os = "linux")]
                {
                    let _ = map.advise(memmap2::Advice::Sequential);
                }
                Some(map)
            };
            identities.push(identity);
            map
        } else {
            None
        };
        Ok(Self {
            db,
            path,
            blobs,
            identities,
        })
    }

    /// Refuse live rollback journals or WAL content; no source checkpoint is performed.
    pub fn check_journals(path: &Path) -> Result<()> {
        for suffix in ["-wal", "-journal"] {
            let journal = PathBuf::from(format!("{}{suffix}", path.display()));
            if journal.exists() && journal.metadata()?.len() != 0 {
                return Err("source must be offline and checkpointed".into());
            }
        }
        Ok(())
    }

    /// Borrow a checked external range without copying or positioned I/O.
    pub fn blob(&self, offset: u64, length: u64) -> Result<&[u8]> {
        let start = usize::try_from(offset)?;
        let end = usize::try_from(offset.checked_add(length).ok_or("blob range overflow")?)?;
        self.blobs
            .as_ref()
            .and_then(|map| map.get(start..end))
            .ok_or_else(|| "external trie exceeds source blob file".into())
    }

    /// Detect source mutation before publishing any canonical registration.
    pub fn verify_unchanged(&self) -> Result<()> {
        Self::check_journals(&self.path)?;
        for old in &self.identities {
            if Identity::read(&old.path)? != *old {
                return Err(format!("source changed: {}", old.path.display()).into());
            }
        }
        Ok(())
    }
}
