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

//! Offline in-place preparation for disposable validation chainstates.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::path::{Component, Path, PathBuf};

use blockstack_lib::chainstate::stacks::index::direct_hash_index::{self, DirectHashIndex};
use blockstack_lib::chainstate::stacks::index::record::NodeRecordFormat;
use blockstack_lib::clarity_vm::database::binary_value_store;
use blockstack_lib::clarity_vm::database::value_extents::ValueBackend;
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};

use crate::source::owned_legacy_files;
use crate::{Config, Result, migrate};

/// Durable inventory for roll-forward replacement of one offline MARF's files.
#[derive(Serialize, Deserialize)]
struct Cutover {
    /// Version of this publication protocol.
    version: u32,
    /// SQLite basename; all inventory entries are checked single path components.
    database: String,
    /// Whether the completed destination owns Clarity stable-ID values.
    clarity: bool,
    /// Original generation files retained until destination verification succeeds.
    previous: Vec<String>,
    /// Completed destination files, including registered sidecar directories.
    replacement: Vec<String>,
}

/// Verify the complete canonical registration without creating a missing database.
pub fn verify(path: &Path, clarity: bool) -> Result<()> {
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    if NodeRecordFormat::from_database(&db)? != NodeRecordFormat::Optimized {
        return Err("canonical record format is not published".into());
    }
    if !PathBuf::from(format!("{}.blobs", path.display())).is_file() {
        return Err("canonical external blob file is missing".into());
    }
    if DirectHashIndex::open(&db, path)?.is_none() {
        return Err("canonical direct hash index is missing".into());
    }
    if clarity {
        binary_value_store::verify_complete(&db)?;
        let values = ValueBackend::open_registered(&db, path)?
            .ok_or("canonical Clarity store is missing")?;
        if !values.is_stable() {
            return Err("canonical Clarity store requires stable IDs".into());
        }
        let indexed: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='clarity_stable_ptrhash_base')",
            [],
            |row| row.get(0),
        )?;
        if !indexed {
            return Err("canonical Clarity PtrHash registration is missing".into());
        }
    }
    Ok(())
}

/// Add absent derived indexes to a current store; malformed published registrations still fail.
fn prepare_current_indexes(path: &Path, clarity: bool) -> Result<()> {
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let direct_missing = DirectHashIndex::open(&db, path)?.is_none();
    let ptrhash_missing = if clarity {
        binary_value_store::verify_complete(&db)?;
        ValueBackend::open_registered(&db, path)?.ok_or("canonical Clarity store is missing")?;
        !db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='clarity_stable_ptrhash_base')",
            [],
            |row| row.get::<_, bool>(0),
        )?
    } else {
        false
    };
    drop(db);
    if direct_missing {
        direct_hash_index::build(path)?;
    }
    if ptrhash_missing {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or("invalid MARF filename")?;
        let output = path.with_file_name(format!("{name}.stable-ptrhash"));
        extent_ptrhash::stable::build_and_activate(path, &output)
            .map_err(|error| error.to_string())?;
    }
    verify(path, clarity)
}

/// Prepare one disposable offline MARF, recovering a previously interrupted publication first.
pub fn prepare_marf_in_place(path: &Path, clarity: bool) -> Result<()> {
    let parent = path.parent().ok_or("MARF has no parent")?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("invalid MARF filename")?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(parent.join(format!(".{name}.canonical.lock")))?;
    lock.try_lock()
        .map_err(|error| format!("another preparation owns this MARF: {error}"))?;
    let journal = parent.join(format!(".{name}.canonical-cutover"));
    let reclaimed = parent.join(format!(".{name}.canonical-reclaimed"));
    if reclaimed.exists() {
        verify(path, clarity)?;
        fs::remove_dir_all(&reclaimed)?;
        File::open(parent)?.sync_all()?;
    }
    if journal.exists() {
        apply(&journal, parent, &mut || Ok(()))?;
    }
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    // Startup owns a disposable, offline clone; unlike the standalone migrator, it may checkpoint it.
    let (busy, _, _): (i64, i64, i64) =
        db.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
    if busy != 0 {
        return Err("cannot checkpoint a busy validation chainstate".into());
    }
    let format = NodeRecordFormat::from_database(&db)?;
    let previous_files = if format == NodeRecordFormat::Legacy {
        owned_legacy_files(&db, path)?
    } else {
        BTreeSet::new()
    };
    drop(db);
    if format == NodeRecordFormat::Optimized {
        return prepare_current_indexes(path, clarity);
    }
    if format != NodeRecordFormat::Legacy {
        return Err("retired experimental MARF requires its frozen migration tooling".into());
    }
    fs::create_dir(&journal)?;
    fs::create_dir(journal.join("previous"))?;
    let result = migrate(&Config {
        source: path.to_path_buf(),
        destination: journal.join("replacement"),
        clarity_values: clarity,
        max_trie_bytes: 512 * 1024 * 1024,
    })?;
    verify(&result.database, clarity)?;
    let mut previous = Vec::new();
    for owned in previous_files {
        let metadata = match fs::symlink_metadata(&owned) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if !metadata.is_file() {
            return Err("source companions must be regular files".into());
        }
        previous.push(
            owned
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or("non-UTF8 source companion")?
                .to_owned(),
        );
    }
    let mut replacement = Vec::new();
    for entry in fs::read_dir(journal.join("replacement"))? {
        let entry = entry?;
        let filename = entry
            .file_name()
            .into_string()
            .map_err(|_| "non-UTF8 replacement companion")?;
        if parent.join(&filename).exists() && !previous.contains(&filename) {
            return Err(format!("replacement would overwrite unrelated file {filename}").into());
        }
        replacement.push(filename);
    }
    previous.sort();
    replacement.sort();
    let manifest = Cutover {
        version: 1,
        database: name.to_owned(),
        clarity,
        previous,
        replacement,
    };
    let temporary = journal.join("manifest.pending");
    fs::write(&temporary, serde_json::to_vec_pretty(&manifest)?)?;
    File::open(&temporary)?.sync_all()?;
    fs::rename(temporary, journal.join("manifest.json"))?;
    File::open(&journal)?.sync_all()?;
    apply(&journal, parent, &mut || Ok(()))
}

/// Roll forward a validated file inventory; the old database stays recoverable until verification.
fn apply(journal: &Path, parent: &Path, event: &mut dyn FnMut() -> Result<()>) -> Result<()> {
    let manifest: Cutover = serde_json::from_slice(&fs::read(journal.join("manifest.json"))
        .map_err(|error| format!("incomplete private conversion at {} must be inspected/discarded before retry: {error}", journal.display()))?)?;
    if manifest.version != 1 {
        return Err("unknown cutover manifest version".into());
    }
    for name in std::iter::once(&manifest.database)
        .chain(manifest.previous.iter())
        .chain(manifest.replacement.iter())
    {
        let path = Path::new(name);
        if path.components().count() != 1
            || !matches!(path.components().next(), Some(Component::Normal(_)))
        {
            return Err("cutover inventory contains an invalid component".into());
        }
    }
    for name in &manifest.previous {
        let backup = journal.join("previous").join(name);
        if !backup.exists() {
            fs::rename(parent.join(name), &backup)?;
            File::open(journal.join("previous"))?.sync_all()?;
            File::open(parent)?.sync_all()?;
            event()?;
        }
    }
    for name in &manifest.replacement {
        let source = journal.join("replacement").join(name);
        let destination = parent.join(name);
        if source.exists() {
            if destination.exists() {
                return Err("unexpected file appeared during cutover".into());
            }
            fs::rename(source, destination)?;
            File::open(journal.join("replacement"))?.sync_all()?;
            File::open(parent)?.sync_all()?;
            event()?;
        } else if !destination.exists() {
            return Err("cutover lost a replacement file".into());
        }
    }
    verify(&parent.join(&manifest.database), manifest.clarity)?;
    // Move completed publication out of the recovery namespace before deleting any backup.
    // A crash during recursive deletion must never re-enter the roll-forward loop.
    let reclaimed = parent.join(format!(".{}.canonical-reclaimed", manifest.database));
    fs::rename(journal, &reclaimed)?;
    File::open(parent)?.sync_all()?;
    fs::remove_dir_all(reclaimed)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

/// Discover MARFs by schema under the supplied disposable chainstate, with explicit Clarity ownership.
pub fn prepare_chainstate_in_place(root: &Path) -> Result<()> {
    let clarity = root.join("chainstate/vm/clarity/marf.sqlite");
    if !root.join("chainstate/vm").is_dir() {
        return Err("expected the directory containing chainstate/vm".into());
    }
    let mut pending = vec![root.to_path_buf()];
    let mut marfs = BTreeSet::new();
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let name = entry.file_name();
            let kind = entry.file_type()?;
            if let Some(name) = name.to_str().filter(|name| name.starts_with('.')) {
                if kind.is_dir() && name.ends_with(".canonical-cutover") {
                    let database = name
                        .strip_prefix('.')
                        .unwrap()
                        .strip_suffix(".canonical-cutover")
                        .unwrap();
                    let path = entry.path().parent().unwrap().join(database);
                    prepare_marf_in_place(&path, path == clarity)?;
                    marfs.insert(path);
                }
                continue;
            }
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() && entry.path().extension().is_some_and(|ext| ext == "sqlite")
            {
                let db =
                    Connection::open_with_flags(entry.path(), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
                let marf: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='marf_data')", [], |row| row.get(0))?;
                if marf {
                    let owns_values: bool = db.query_row(
                        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='data_table')",
                        [],
                        |row| row.get(0),
                    )?;
                    if owns_values != (entry.path() == clarity) {
                        return Err(
                            "unrecognized Clarity owner or archived MARF; use only active MARFs"
                                .into(),
                        );
                    }
                    marfs.insert(entry.path());
                }
            }
        }
    }
    if !clarity.is_file() || !root.join("chainstate/vm/index.sqlite").is_file() {
        return Err("required Clarity/VM-index MARF is missing after recovery".into());
    }
    for path in marfs {
        eprintln!("canonical preparing {}", path.display());
        prepare_marf_in_place(&path, path == clarity)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use blockstack_lib::chainstate::stacks::index::marf::{MARF, MARFOpenOpts};
    use blockstack_lib::chainstate::stacks::index::{ClarityMarfTrieId, MARFValue};
    use blockstack_lib::clarity_vm::database::marf::MarfedKV;
    use stacks_common::types::chainstate::StacksBlockId;

    use super::*;

    /// Fresh canonical stores can acquire optional historical indexes without trie conversion.
    #[test]
    fn preparation_indexes_fresh_canonical_clarity() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("clarity");
        drop(MarfedKV::open(root.to_str().unwrap(), None, None).unwrap());
        let database = root.join("marf.sqlite");
        prepare_marf_in_place(&database, true).unwrap();
        verify(&database, true).unwrap();
        prepare_marf_in_place(&database, true).unwrap();
        verify(&database, true).unwrap();
    }

    /// Prefix-sharing notes and directories are not part of a legacy MARF's owned inventory.
    #[test]
    fn preparation_preserves_unregistered_siblings() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("marf.sqlite");
        let mut source =
            MARF::<StacksBlockId>::from_path(path.to_str().unwrap(), MARFOpenOpts::default())
                .unwrap();
        let mut tx = source.begin_tx().unwrap();
        tx.begin(&StacksBlockId::sentinel(), &StacksBlockId([8; 32]))
            .unwrap();
        tx.insert_batch(&["key".into()], vec![MARFValue([4; 40])])
            .unwrap();
        tx.commit().unwrap();
        drop(source);
        let notes = directory.path().join("marf.sqlite.notes");
        let archive = directory.path().join("marf.sqlite.archive");
        fs::write(&notes, b"retain notes").unwrap();
        fs::create_dir(&archive).unwrap();
        fs::write(archive.join("retained"), b"retain archive").unwrap();
        prepare_marf_in_place(&path, false).unwrap();
        verify(&path, false).unwrap();
        assert_eq!(fs::read(notes).unwrap(), b"retain notes");
        assert_eq!(
            fs::read(archive.join("retained")).unwrap(),
            b"retain archive"
        );
    }

    /// Every file-move boundary can roll forward without replacing an unrelated sibling.
    #[test]
    fn recover_every_cutover_rename() {
        for fail_after in 1..=7 {
            let directory = tempfile::tempdir().unwrap();
            let parent = directory.path();
            let path = parent.join("marf.sqlite");
            let mut options = MARFOpenOpts::default();
            options.external_blobs = true;
            let mut source =
                MARF::<StacksBlockId>::from_path(path.to_str().unwrap(), options).unwrap();
            let block = StacksBlockId([9; 32]);
            let mut tx = source.begin_tx().unwrap();
            tx.begin(&StacksBlockId::sentinel(), &block).unwrap();
            tx.insert_batch(&["key".into()], vec![MARFValue([7; 40])])
                .unwrap();
            tx.commit().unwrap();
            let expected = source.get_root_hash_at(&block).unwrap();
            drop(source);
            let db = Connection::open(&path).unwrap();
            db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
            drop(db);
            fs::write(parent.join("unrelated"), b"preserve").unwrap();
            let journal = parent.join(".marf.sqlite.canonical-cutover");
            fs::create_dir(&journal).unwrap();
            fs::create_dir(journal.join("previous")).unwrap();
            migrate(&Config {
                source: path.clone(),
                destination: journal.join("replacement"),
                max_trie_bytes: 1024 * 1024,
                clarity_values: false,
            })
            .unwrap();
            let replacement = fs::read_dir(journal.join("replacement"))
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect();
            let manifest = Cutover {
                version: 1,
                database: "marf.sqlite".into(),
                clarity: false,
                previous: vec!["marf.sqlite".into(), "marf.sqlite.blobs".into()],
                replacement,
            };
            fs::write(
                journal.join("manifest.json"),
                serde_json::to_vec(&manifest).unwrap(),
            )
            .unwrap();
            let mut steps = 0;
            let outcome = apply(&journal, parent, &mut || {
                steps += 1;
                if steps == fail_after {
                    Err("injected publication stop".into())
                } else {
                    Ok(())
                }
            });
            if steps == fail_after {
                assert!(outcome.is_err());
            } else {
                outcome.unwrap();
            }
            prepare_marf_in_place(&path, false).unwrap();
            verify(&path, false).unwrap();
            let mut result =
                MARF::<StacksBlockId>::from_path(path.to_str().unwrap(), MARFOpenOpts::default())
                    .unwrap();
            assert_eq!(result.get_root_hash_at(&block).unwrap(), expected);
            assert_eq!(fs::read(parent.join("unrelated")).unwrap(), b"preserve");
            assert!(!journal.exists());
        }
    }
}
