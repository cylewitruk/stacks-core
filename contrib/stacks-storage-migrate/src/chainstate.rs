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

//! Source-preserving whole-chainstate conversion with one final publication.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use blockstack_lib::chainstate::stacks::index::record::NodeRecordFormat;
use blockstack_lib::util_lib::db::sqlite_readonly_uri;
use rusqlite::{Connection, OpenFlags};

use crate::source::{Source, owned_legacy_files};
use crate::{Config, Result, migrate, space, verify};

/// File identity retained for the complete offline input inventory.
#[derive(Clone, PartialEq, Eq)]
struct Entry {
    /// Source-relative path; no symlinks or special files are admitted.
    relative: PathBuf,
    /// Logical byte size.
    bytes: u64,
    /// Modification time, excluding access time.
    modified: SystemTime,
}

/// Inventory an offline tree without following links or accepting active database journals.
fn inventory(root: &Path) -> Result<Vec<Entry>> {
    let mut directories = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = directories.pop() {
        for item in fs::read_dir(directory)? {
            let item = item?;
            let kind = item.file_type()?;
            if kind.is_dir() {
                directories.push(item.path());
            } else if kind.is_file() {
                let metadata = item.metadata()?;
                let name = item.file_name();
                let name = name.to_str().ok_or("non-UTF8 chainstate filename")?;
                if (name.ends_with("-wal") || name.ends_with("-journal")) && metadata.len() != 0 {
                    return Err("chainstate must be offline and checkpointed".into());
                }
                files.push(Entry {
                    relative: item.path().strip_prefix(root)?.to_path_buf(),
                    bytes: metadata.len(),
                    modified: metadata.modified()?,
                });
                if files.len() > 2_000_000 {
                    return Err("chainstate file inventory exceeds bound".into());
                }
            } else {
                return Err(format!(
                    "symlink or special chainstate input: {}",
                    item.path().display()
                )
                .into());
            }
        }
    }
    files.sort_by(|a, b| a.relative.cmp(&b.relative));
    Ok(files)
}

/// Convert each applicable MARF and copy other state into a new, atomically published directory.
///
/// The caller supplies an offline snapshot. Existing destinations, source nesting and symlinks
/// are rejected. Failed private output remains unselected for inspection and explicit removal.
pub fn migrate_chainstate(source: &Path, destination: &Path, max_trie_bytes: u64) -> Result<usize> {
    if destination.exists() || max_trie_bytes == 0 {
        return Err("destination must be new and trie limit positive".into());
    }
    let source = source.canonicalize()?;
    let parent = destination
        .parent()
        .ok_or("destination parent missing")?
        .canonicalize()?;
    if parent.starts_with(&source) {
        return Err("destination must not be inside the source".into());
    }
    let name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("invalid destination name")?;
    let destination = parent.join(name);
    let initial = inventory(&source)?;
    let clarity = PathBuf::from("chainstate/vm/clarity/marf.sqlite");
    let index = PathBuf::from("chainstate/vm/index.sqlite");
    let mut marfs = BTreeMap::new();
    let mut legacy = BTreeMap::new();
    let mut replaced_files = BTreeSet::new();
    for entry in &initial {
        if entry
            .relative
            .extension()
            .is_none_or(|extension| extension != "sqlite")
        {
            continue;
        }
        let path = source.join(&entry.relative);
        let db = Connection::open_with_flags(
            sqlite_readonly_uri(&path, true)?,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )?;
        let is_marf: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='marf_data')",
            [],
            |row| row.get(0),
        )?;
        if !is_marf {
            continue;
        }
        let owns_values = entry.relative == clarity;
        let has_clarity_values: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='data_table')",
            [],
            |row| row.get(0),
        )?;
        if has_clarity_values != owns_values {
            return Err(format!("unrecognized Clarity owner or archived MARF at {}; supply an offline tree containing only active MARFs", entry.relative.display()).into());
        }
        match NodeRecordFormat::from_database(&db)? {
            NodeRecordFormat::Legacy => {
                Source::open(&path)?;
                legacy.insert(entry.relative.clone(), owns_values);
                replaced_files.extend(owned_legacy_files(&db, &entry.relative)?);
            }
            NodeRecordFormat::Optimized => verify(&path, owns_values)?,
        }
        marfs.insert(entry.relative.clone(), owns_values);
    }
    if !marfs.contains_key(&clarity) || !marfs.contains_key(&index) {
        return Err("chainstate registry requires Clarity and VM-index MARFs".into());
    }
    let stage = parent.join(format!(".{name}.chainstate-{}", std::process::id()));
    fs::create_dir(&stage)?;
    fs::write(
        stage.join("INCOMPLETE"),
        b"Unpublished canonical chainstate conversion\n",
    )?;
    let mut space = space::Guard::new(&stage, 4 * 1024 * 1024 * 1024)?;
    for entry in &initial {
        if replaced_files.contains(&entry.relative) {
            continue;
        }
        space.check()?;
        let target = stage.join(&entry.relative);
        fs::create_dir_all(target.parent().ok_or("target parent missing")?)?;
        if target.exists() {
            return Err("copy destination already exists".into());
        }
        reflink_copy::reflink_or_copy(source.join(&entry.relative), &target)?;
        let output = File::open(&target)?;
        if output.metadata()?.len() != entry.bytes {
            return Err("source length changed during copy".into());
        }
        // The unpublished tree is synchronized once by sync_tree before its final rename.
    }
    for (number, (relative, clarity_values)) in legacy.iter().enumerate() {
        let converted = stage.join(format!(".marf-result-{number}"));
        migrate(&Config {
            source: source.join(relative),
            destination: converted.clone(),
            max_trie_bytes,
            clarity_values: *clarity_values,
        })?;
        let target_parent = stage.join(relative.parent().ok_or("MARF parent missing")?);
        fs::create_dir_all(&target_parent)?;
        for item in fs::read_dir(&converted)? {
            let item = item?;
            let target = target_parent.join(item.file_name());
            if target.exists() {
                return Err("canonical MARF output conflicts with another chainstate file".into());
            }
            fs::rename(item.path(), target)?;
        }
        fs::remove_dir(converted)?;
        File::open(target_parent)?.sync_all()?;
    }
    for (relative, clarity_values) in &marfs {
        verify(&stage.join(relative), *clarity_values)?;
    }
    if initial != inventory(&source)? {
        return Err("source chainstate changed during conversion".into());
    }
    fs::write(
        stage.join("canonical-chainstate.txt"),
        format!(
            "format=6\nmarfs={}\nconverted={}\n",
            marfs.len(),
            legacy.len()
        ),
    )?;
    fs::remove_file(stage.join("INCOMPLETE"))?;
    sync_tree(&stage)?;
    if destination.exists() {
        return Err("destination appeared during conversion".into());
    }
    fs::rename(&stage, &destination)?;
    File::open(parent)?.sync_all()?;
    Ok(marfs.len())
}

/// Flush newly created directory entries before publishing the root directory.
fn sync_tree(path: &Path) -> Result<()> {
    for item in fs::read_dir(path)? {
        let item = item?;
        if item.file_type()?.is_dir() {
            sync_tree(&item.path())?;
        } else {
            File::open(item.path())?.sync_all()?;
        }
    }
    File::open(path)?.sync_all()?;
    Ok(())
}
