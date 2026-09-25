//! Offline preparation of a disposable chainstate for optimized block validation.

use std::error::Error;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use stackslib::chainstate::stacks::index::direct_hash_index::{self, DirectHashIndex};
use stackslib::chainstate::stacks::index::record::NodeRecordFormat;
use stackslib::clarity_vm::database::binary_value_store;
use stackslib::clarity_vm::database::value_extents::ValueExtentStore;
use stackslib::clarity_vm::database::value_extents_migration::{
    ExtentMigrationConfig, migrate_value_extents,
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

/// Prepare the chainstate owned by this validation run before opening any MARF handle.
pub fn prepare_for_validation(database_root: &Path) -> Result<()> {
    let vm = database_root.join("chainstate/vm");
    if !vm.is_dir() {
        return Err(format!("missing chainstate VM directory: {}", vm.display()).into());
    }
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(vm.join(".stacks-inspect-storage-preparation.lock"))?;
    lock.lock()?;

    let clarity = vm.join("clarity");
    recover_directory_cutover(&vm, &clarity)?;
    if !clarity.join("marf.sqlite").is_file() {
        return Err(format!("missing Clarity MARF: {}", clarity.display()).into());
    }
    checkpoint(&clarity.join("marf.sqlite"))?;
    if format(&clarity.join("marf.sqlite"))? == NodeRecordFormat::Legacy {
        prepare_extents(&vm, &clarity)?;
    }
    for target in [
        NodeRecordFormat::TypeFirstV2,
        NodeRecordFormat::TypeFirstV3,
        NodeRecordFormat::TypeFirstV4,
    ] {
        if format(&clarity.join("marf.sqlite"))?.version() >= target.version() {
            let scratch = vm.join(format!("clarity.plan-v{}", target.version()));
            if scratch.exists() {
                fs::remove_dir_all(scratch)?;
            }
            continue;
        }
        prepare_format(&vm, &clarity, target)?;
    }
    verify_clarity(&clarity.join("marf.sqlite"), NodeRecordFormat::TypeFirstV4)?;

    ensure_direct_hash_index(&vm.join("index.sqlite"))?;
    ensure_direct_hash_index(&clarity.join("marf.sqlite"))?;
    ensure_ptrhash(&clarity.join("marf.sqlite"))?;
    eprintln!("stacks-inspect: optimized Clarity storage and MARF indexes ready");
    Ok(())
}

/// Checkpoint a copied SQLite database so offline migration sees all committed pages.
fn checkpoint(path: &Path) -> Result<()> {
    let db = Connection::open(path)?;
    db.execute_batch("PRAGMA busy_timeout=0; PRAGMA wal_checkpoint(TRUNCATE)")?;
    Ok(())
}

/// Read a published physical format without creating a missing SQLite database.
fn format(path: &Path) -> Result<NodeRecordFormat> {
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    Ok(NodeRecordFormat::from_database(&db)?)
}

/// Verify the completed Clarity format and its matching extent generation.
fn verify_clarity(path: &Path, expected: NodeRecordFormat) -> Result<()> {
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let actual = NodeRecordFormat::from_database(&db)?;
    if actual != expected {
        return Err(format!(
            "Clarity format at {} is {actual:?}, expected {expected:?}",
            path.display()
        )
        .into());
    }
    binary_value_store::verify_complete(&db)?;
    if ValueExtentStore::open_registered(&db, path, false)?.is_none() {
        return Err(format!("missing registered Clarity values at {}", path.display()).into());
    }
    Ok(())
}

/// Complete or resume the legacy-to-extent migration in a sibling directory.
fn prepare_extents(vm: &Path, clarity: &Path) -> Result<()> {
    let source = clarity.join("marf.sqlite");
    let destination = vm.join("clarity.prepare-v1");
    let ready = destination.join("marf.sqlite").is_file()
        && verify_clarity(
            &destination.join("marf.sqlite"),
            NodeRecordFormat::TypeFirstV1,
        )
        .is_ok();
    if !ready {
        eprintln!("stacks-inspect: extracting Clarity values and rewriting the side store");
        let config = ExtentMigrationConfig {
            source_db: source.clone(),
            source_blobs: clarity.join("marf.sqlite.blobs"),
            destination: destination.clone(),
            index_cache_mib: 256,
        };
        migrate_value_extents(&config, &mut |progress| {
            eprintln!("stacks-inspect: {} {}", progress.phase, progress.completed);
        })?;
    }
    promote(vm, clarity, &destination, NodeRecordFormat::TypeFirstV1)
}

/// Convert one published physical format while retaining the previous generation.
fn prepare_format(vm: &Path, clarity: &Path, target: NodeRecordFormat) -> Result<()> {
    let version = match target {
        NodeRecordFormat::TypeFirstV2 => 2,
        NodeRecordFormat::TypeFirstV3 => 3,
        NodeRecordFormat::TypeFirstV4 => 4,
        _ => return Err("unsupported target trie format".into()),
    };
    let source = clarity.join("marf.sqlite");
    let destination = vm.join(format!("clarity.prepare-v{version}"));
    let scratch = vm.join(format!("clarity.plan-v{version}"));
    let ready = destination.join("marf.sqlite").is_file()
        && verify_clarity(&destination.join("marf.sqlite"), target).is_ok();
    if !ready {
        eprintln!("stacks-inspect: converting Clarity trie to format v{version}");
        match target {
            NodeRecordFormat::TypeFirstV2 => {
                let config = stacks_trie_format_v2::Config {
                    source_db: source.clone(),
                    source_blobs: clarity.join("marf.sqlite.blobs"),
                    scratch: scratch.clone(),
                };
                stacks_trie_format_v2::plan(&config)?;
                stacks_trie_format_v2::rewrite(&config, &destination)?;
            }
            NodeRecordFormat::TypeFirstV3 => {
                let config = stacks_trie_format_v3::Config {
                    source_db: source.clone(),
                    source_blobs: clarity.join("marf.sqlite.blobs"),
                    scratch: scratch.clone(),
                };
                stacks_trie_format_v3::plan(&config)?;
                stacks_trie_format_v3::rewrite(&config, &destination)?;
            }
            NodeRecordFormat::TypeFirstV4 => {
                let config = stacks_trie_format_v4::Config {
                    source_db: source.clone(),
                    source_blobs: clarity.join("marf.sqlite.blobs"),
                    scratch: scratch.clone(),
                };
                stacks_trie_format_v4::plan(&config)?;
                stacks_trie_format_v4::rewrite(&config, &destination)?;
            }
            _ => unreachable!(),
        }
    }
    promote(vm, clarity, &destination, target)?;
    if scratch.exists() {
        fs::remove_dir_all(scratch)?;
    }
    Ok(())
}

/// Publish a verified directory and keep the previous one until the new name is durable.
fn promote(vm: &Path, clarity: &Path, destination: &Path, target: NodeRecordFormat) -> Result<()> {
    verify_clarity(&destination.join("marf.sqlite"), target)?;
    let previous = vm.join(format!("clarity.previous-v{}", target.version()));
    if previous.exists() {
        return Err(format!("unresolved prior Clarity cutover: {}", previous.display()).into());
    }
    fs::rename(clarity, &previous)?;
    sync_directory(vm)?;
    if let Err(error) = fs::rename(destination, clarity) {
        fs::rename(&previous, clarity)?;
        sync_directory(vm)?;
        return Err(error.into());
    }
    sync_directory(vm)?;
    verify_clarity(&clarity.join("marf.sqlite"), target)?;
    fs::remove_dir_all(&previous)?;
    sync_directory(vm)?;
    Ok(())
}

/// Recover the narrow two-rename publication window after a crash.
fn recover_directory_cutover(vm: &Path, clarity: &Path) -> Result<()> {
    let previous: Vec<PathBuf> = (1..=4)
        .map(|version| vm.join(format!("clarity.previous-v{version}")))
        .filter(|path| path.exists())
        .collect();
    if !clarity.exists() {
        if previous.len() != 1 {
            return Err(
                "Clarity directory is missing and no unique prior generation exists".into(),
            );
        }
        fs::rename(&previous[0], clarity)?;
        sync_directory(vm)?;
    } else {
        if previous.len() > 1 {
            return Err("multiple Clarity generations need recovery".into());
        }
        let current = match format(&clarity.join("marf.sqlite")) {
            Ok(current) => current,
            Err(error) if previous.len() == 1 => {
                let failed = vm.join("clarity.failed-unreadable");
                if failed.exists() {
                    return Err(format!(
                        "cannot restore unreadable Clarity while {} exists: {error}",
                        failed.display()
                    )
                    .into());
                }
                fs::rename(clarity, &failed)?;
                fs::rename(&previous[0], clarity)?;
                sync_directory(vm)?;
                eprintln!(
                    "stacks-inspect: restored previous Clarity generation after unreadable cutover"
                );
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        for path in previous {
            let version: u8 = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.rsplit('v').next())
                .ok_or("invalid Clarity backup name")?
                .parse()?;
            if current.version() < version {
                return Err(format!("ambiguous Clarity cutover at {}", path.display()).into());
            }
            if let Err(error) = verify_clarity(&clarity.join("marf.sqlite"), current) {
                let failed = vm.join(format!("clarity.failed-v{version}"));
                if failed.exists() {
                    return Err(format!(
                        "cannot restore Clarity while {} exists: {error}",
                        failed.display()
                    )
                    .into());
                }
                fs::rename(clarity, &failed)?;
                fs::rename(&path, clarity)?;
                sync_directory(vm)?;
                eprintln!(
                    "stacks-inspect: restored previous Clarity generation after incomplete cutover"
                );
                return Ok(());
            }
            fs::remove_dir_all(path)?;
        }
    }
    Ok(())
}

/// Persist a directory rename before opening the selected generation.
fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

/// Build a missing direct-addressed MARF hash index on an offline database.
fn ensure_direct_hash_index(path: &Path) -> Result<()> {
    checkpoint(path)?;
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let ready = DirectHashIndex::open(&db, path)?.is_some();
    drop(db);
    if !ready {
        eprintln!(
            "stacks-inspect: building MARF block-hash index for {}",
            path.display()
        );
        direct_hash_index::build(path)?;
    }
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    if DirectHashIndex::open(&db, path)?.is_none() {
        return Err(format!("MARF block-hash index not activated: {}", path.display()).into());
    }
    Ok(())
}

/// Build or validate the registered immutable Clarity value-deduplication base.
fn ensure_ptrhash(path: &Path) -> Result<()> {
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let base_exists: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='clarity_ptrhash_base')",
        [],
        |row| row.get(0),
    )?;
    if base_exists {
        ValueExtentStore::open_registered(&db, path, false)?
            .ok_or("PtrHash registration exists without Clarity extents")?;
        return Ok(());
    }
    drop(db);
    let output = PathBuf::from(format!("{}.ptrhash-base", path.display()));
    let unfinished = output.with_extension("building");
    if unfinished.exists() {
        fs::remove_dir_all(unfinished)?;
    }
    if output.exists() {
        fs::remove_dir_all(&output)?;
    }
    eprintln!("stacks-inspect: building Clarity value-deduplication index");
    extent_ptrhash::build_and_activate(path, &output)
        .map_err(|error| io::Error::other(error.to_string()))?;
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    ValueExtentStore::open_registered(&db, path, false)?.ok_or("PtrHash was not activated")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clarity::vm::database::SqliteConnection;
    use rusqlite::params;
    use stacks_common::types::chainstate::StacksBlockId;
    use stackslib::chainstate::stacks::index::ClarityMarfTrieId;
    use stackslib::chainstate::stacks::index::MARFValue;
    use stackslib::chainstate::stacks::index::marf::{MARF, MARFOpenOpts, MarfConnection};

    /// A populated legacy chainstate retains its root through preparation and a second startup.
    #[test]
    fn prepares_legacy_chainstate_and_reuses_published_formats() {
        let temporary = tempfile::tempdir().unwrap();
        let vm = temporary.path().join("chainstate/vm");
        fs::create_dir_all(vm.join("clarity")).unwrap();
        let mut opts = MARFOpenOpts::default();
        opts.external_blobs = true;
        let clarity_path = vm.join("clarity/marf.sqlite");
        let mut clarity =
            MARF::<StacksBlockId>::from_path(clarity_path.to_str().unwrap(), opts.clone()).unwrap();
        SqliteConnection::initialize_conn(clarity.sqlite_conn()).unwrap();
        let block = StacksBlockId([7; 32]);
        let value = "prepared-value";
        let hash = MARFValue::from_value(value);
        let mut tx = clarity.begin_tx().unwrap();
        tx.begin(&StacksBlockId::sentinel(), &block).unwrap();
        tx.sqlite_tx()
            .execute(
                "INSERT INTO data_table(key,value) VALUES (?1,?2)",
                params![hash.to_hex(), value],
            )
            .unwrap();
        tx.insert_batch(&["prepared-key".into()], &[hash]).unwrap();
        tx.seal().unwrap();
        tx.commit().unwrap();
        let expected_root = clarity.get_root_hash_at(&block).unwrap();
        drop(clarity);
        let headers =
            MARF::<StacksBlockId>::from_path(vm.join("index.sqlite").to_str().unwrap(), opts)
                .unwrap();
        drop(headers);

        prepare_for_validation(temporary.path()).unwrap();
        assert_eq!(
            format(&clarity_path).unwrap(),
            NodeRecordFormat::TypeFirstV4
        );
        let mut reopen_opts = MARFOpenOpts::default().with_mmap(true);
        reopen_opts.external_blobs = true;
        let mut reopened =
            MARF::<StacksBlockId>::from_path(clarity_path.to_str().unwrap(), reopen_opts).unwrap();
        assert_eq!(reopened.get_root_hash_at(&block).unwrap(), expected_root);
        drop(reopened);
        let db = Connection::open(&clarity_path).unwrap();
        assert!(
            db.query_row(
                "SELECT EXISTS(SELECT 1 FROM clarity_ptrhash_base)",
                [],
                |row| row.get::<_, bool>(0)
            )
            .unwrap()
        );
        drop(db);
        prepare_for_validation(temporary.path()).unwrap();
    }

    /// A crash between the two renames restores the source before any new work starts.
    #[test]
    fn restores_previous_directory_after_interrupted_cutover() {
        let temporary = tempfile::tempdir().unwrap();
        let vm = temporary.path();
        let previous = vm.join("clarity.previous-v2");
        fs::create_dir(&previous).unwrap();
        fs::write(previous.join("marker"), b"old generation").unwrap();
        let clarity = vm.join("clarity");
        recover_directory_cutover(vm, &clarity).unwrap();
        assert_eq!(fs::read(clarity.join("marker")).unwrap(), b"old generation");
        assert!(!previous.exists());
    }

    /// An unreadable published candidate cannot displace a valid prior generation.
    #[test]
    fn restores_previous_directory_after_invalid_candidate() {
        let temporary = tempfile::tempdir().unwrap();
        let vm = temporary.path();
        let previous = vm.join("clarity.previous-v2");
        fs::create_dir(&previous).unwrap();
        fs::write(previous.join("marker"), b"old generation").unwrap();
        let clarity = vm.join("clarity");
        fs::create_dir(&clarity).unwrap();
        fs::write(clarity.join("marf.sqlite"), b"invalid database").unwrap();
        recover_directory_cutover(vm, &clarity).unwrap();
        assert_eq!(fs::read(clarity.join("marker")).unwrap(), b"old generation");
        assert!(vm.join("clarity.failed-unreadable").exists());
        assert!(!previous.exists());
    }
}
