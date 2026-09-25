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

//! Offline Binary V1 migration engine used by the command-line wrapper.

use std::error::Error;
use std::ffi::OsString;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use stacks_common::types::chainstate::StacksBlockId;
use stacks_common::util::hash::hex_bytes;

use super as binary_value_store;
use super::{MetadataBlockId, MetadataRow, MigrationWriter, ValueStorageFormat};
use crate::chainstate::stacks::index::marf::{MARF, MARFOpenOpts};
use crate::chainstate::stacks::index::storage::TrieFileStorage;
use crate::chainstate::stacks::index::{MARF_SQLITE_TABLES, MARFValue};
use crate::util_lib::db::{
    SqliteSchemaObject, quote_sql_identifier, sqlite_readonly_uri, sqlite_schema_objects,
};

/// Number of streamed rows committed per destination transaction.
const DATA_COMMIT_ROWS: u64 = 250_000;
/// Row interval used for long-running migration progress reports.
const PROGRESS_ROWS: u64 = 1_000_000;
/// Filename suffix used by production MARFs for external trie storage.
const EXTERNAL_BLOB_SUFFIX: &str = ".blobs";
/// Default SQLite page-cache and mmap budget, in mebibytes.
pub const DEFAULT_CACHE_MIB: u64 = 1_024;
/// Upper bound on a user-selected cache budget, in mebibytes.
pub const MAX_CACHE_MIB: u64 = 32 * 1_024;

/// Operation selected for an offline migration invocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MigrationMode {
    /// Build and publish a new Binary V1 destination.
    Create,
    /// Verify and publish, or activate, an existing destination.
    ResumeFinalize,
}

/// Integrity work performed before a destination is published.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntegrityLevel {
    /// Run SQLite's bounded `quick_check` only.
    Quick,
    /// Run full SQLite and record-level semantic integrity checks.
    Full,
}

/// Inputs and operational policy for one offline migration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MigrationConfig {
    /// Legacy database read under an exclusive writer reservation.
    pub source: PathBuf,
    /// New Binary V1 database populated without modifying `source`.
    pub destination: PathBuf,
    /// Whether to compact the completed destination before publication.
    pub vacuum: bool,
    /// Integrity work required before publication.
    pub integrity: IntegrityLevel,
    /// SQLite page-cache and mmap budget, in mebibytes.
    pub cache_mib: u64,
    /// Whether to create or resume the destination.
    pub mode: MigrationMode,
    /// Optional path that enables atomic cutover while preserving the legacy database.
    pub cutover_backup: Option<PathBuf>,
}

/// Byte and row counters accumulated while rewriting `data_table`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DataMigrationStats {
    /// Total migrated content-addressed rows.
    pub rows: u64,
    /// Bytes occupied by legacy hexadecimal keys.
    pub source_key_bytes: u64,
    /// Bytes occupied by legacy canonical values.
    pub source_value_bytes: u64,
    /// Bytes occupied by Binary V1 raw-hash keys.
    pub destination_key_bytes: u64,
    /// Bytes occupied by Binary V1 record envelopes and payloads.
    pub destination_record_bytes: u64,
    /// Rows encoded with packed Clarity payloads and shape descriptors.
    pub packed_rows: u64,
    /// Rows encoded in intrinsically reversible UTF-8 or raw-consensus kinds.
    pub reversible_rows: u64,
}

/// Shape-dictionary cache counters collected during value migration.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ShapeCacheStats {
    /// Descriptor IDs resolved from the bounded in-memory cache.
    pub hits: u64,
    /// Descriptor IDs requiring a database lookup or insert.
    pub misses: u64,
    /// Cached descriptor mappings discarded during bounded eviction.
    pub evictions: u64,
}

impl From<(u64, u64, u64)> for ShapeCacheStats {
    fn from((hits, misses, evictions): (u64, u64, u64)) -> Self {
        Self {
            hits,
            misses,
            evictions,
        }
    }
}

/// Named stages exposed to migration observers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MigrationPhase {
    /// Create destination schemas.
    CreateSchema,
    /// Copy co-located MARF tables without transforming them.
    CopyMarfDomain,
    /// Convert Clarity metadata rows.
    CopyClarityMetadata,
    /// Convert content-addressed Clarity values.
    CopyClarityValues,
    /// Build deferred destination indexes.
    CreateIndexes,
    /// Verify schema and row-count parity.
    Verify,
    /// Compact the destination when explicitly requested.
    Vacuum,
    /// Build SQLite planner statistics.
    Analyze,
    /// Run physical and optional semantic integrity checks.
    Integrity,
    /// Reconstruct and authenticate every Binary V1 value record.
    SemanticIntegrity,
    /// Reopen the published destination through production detection.
    Reopen,
    /// Atomically activate the completed destination.
    Cutover,
}

/// Structured progress emitted synchronously by the migration engine.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MigrationEvent {
    /// A migration phase is beginning.
    PhaseStarted(MigrationPhase),
    /// One co-located MARF table finished copying.
    TableCopied {
        /// Table name.
        table: String,
        /// Rows copied.
        rows: u64,
        /// Wall-clock copy duration.
        elapsed: Duration,
    },
    /// Clarity metadata finished copying.
    MetadataCopied {
        /// Rows copied.
        rows: u64,
    },
    /// Value migration crossed a bounded progress interval.
    ValuesProgress {
        /// Rows copied so far.
        rows: u64,
    },
    /// Value migration completed with byte and cache counters.
    ValuesCopied {
        /// Value-row and logical-byte counters.
        stats: DataMigrationStats,
        /// Shape-cache counters.
        cache: ShapeCacheStats,
        /// Wall-clock migration duration.
        elapsed: Duration,
    },
    /// Deferred indexes finished building.
    IndexesCreated {
        /// Wall-clock index-build duration.
        elapsed: Duration,
    },
    /// Explicit compaction completed.
    VacuumCompleted {
        /// Wall-clock compaction duration.
        elapsed: Duration,
    },
    /// Semantic integrity crossed a bounded progress interval.
    SemanticIntegrityProgress {
        /// Records audited so far.
        rows: u64,
    },
    /// Semantic integrity completed.
    SemanticIntegrityCompleted {
        /// Records audited.
        rows: u64,
        /// Wall-clock audit duration.
        elapsed: Duration,
    },
    /// The destination was durably published and reopened.
    DestinationCompleted {
        /// Final destination SQLite file length, excluding the external trie blob.
        sqlite_bytes: u64,
    },
    /// Cutover activated the destination and preserved the legacy source.
    CutoverCompleted {
        /// Active database path.
        active: PathBuf,
        /// Preserved legacy database path.
        backup: PathBuf,
    },
}

/// Final outcome returned independently of presentation callbacks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MigrationReport {
    /// Value counters when this invocation performed the row migration.
    pub data: Option<DataMigrationStats>,
    /// Metadata row count when this invocation performed the row migration.
    pub metadata_rows: Option<u64>,
    /// Final destination file length before any optional filename cutover.
    pub destination_sqlite_bytes: u64,
    /// Whether this invocation activated the destination at the source path.
    pub cutover_completed: bool,
}

/// SQLite schema object copied verbatim for the co-located MARF domain.
type SchemaObject = SqliteSchemaObject;

/// Run one offline Binary V1 migration and synchronously report structured progress.
pub fn migrate(
    config: &MigrationConfig,
    on_event: &mut dyn FnMut(MigrationEvent),
) -> Result<MigrationReport, Box<dyn Error>> {
    preflight(config)?;

    let source = locked_source_connection(&config.source)?;
    configure_read_cache(&source, config.cache_mib)?;
    if binary_value_store::detect(&source)? != ValueStorageFormat::LegacyText {
        return Err("source is not a legacy Clarity side store".into());
    }
    let source_page_size: u64 = source.query_row("PRAGMA page_size", [], |row| row.get(0))?;
    let schema = generic_schema(&source)?;

    if config.mode == MigrationMode::ResumeFinalize {
        return resume_or_cut_over(config, source, &schema, on_event);
    }

    provision_destination_blob(&config.source, &config.destination)?;

    let mut destination = Connection::open(&config.destination)?;
    destination.busy_timeout(Duration::from_secs(300))?;
    configure_bulk_destination(&destination, source_page_size, config.cache_mib)?;

    on_event(MigrationEvent::PhaseStarted(MigrationPhase::CreateSchema));
    create_generic_tables(&destination, &schema)?;
    binary_value_store::initialize_migration_destination(&destination)?;

    // The reserved source connection below excludes writers, and preflight
    // rejects non-empty journals. An immutable attachment therefore observes
    // the same checkpointed bytes without self-conflicting with that writer
    // reservation inside SQLite's per-process lock manager.
    let source_uri = immutable_uri(&config.source)?;
    destination.execute("ATTACH DATABASE ?1 AS migration_source", [source_uri])?;
    configure_attached_source(&destination, config.cache_mib)?;

    on_event(MigrationEvent::PhaseStarted(MigrationPhase::CopyMarfDomain));
    copy_generic_tables(&destination, &schema, on_event)?;

    on_event(MigrationEvent::PhaseStarted(
        MigrationPhase::CopyClarityMetadata,
    ));
    let metadata_rows = migrate_metadata(&source, &mut destination)?;
    on_event(MigrationEvent::MetadataCopied {
        rows: metadata_rows,
    });

    on_event(MigrationEvent::PhaseStarted(
        MigrationPhase::CopyClarityValues,
    ));
    let started = Instant::now();
    let (data_stats, cache_stats) = migrate_data(&source, &mut destination, on_event)?;
    let elapsed = started.elapsed();
    let cache_stats = ShapeCacheStats::from(cache_stats);
    on_event(MigrationEvent::ValuesCopied {
        stats: data_stats,
        cache: cache_stats,
        elapsed,
    });

    on_event(MigrationEvent::PhaseStarted(MigrationPhase::CreateIndexes));
    let started = Instant::now();
    binary_value_store::create_migration_indexes(&destination)?;
    create_generic_secondary_objects(&destination, &schema)?;
    on_event(MigrationEvent::IndexesCreated {
        elapsed: started.elapsed(),
    });
    destination.execute_batch("DETACH DATABASE migration_source;")?;

    on_event(MigrationEvent::PhaseStarted(MigrationPhase::Verify));
    verify_destination(
        &source,
        &destination,
        &schema,
        Some((data_stats.rows, metadata_rows)),
        false,
    )?;
    let destination_sqlite_bytes = publish_destination(config, destination, on_event)?;
    release_source_lock(source)?;
    let cutover_completed = maybe_cut_over(config, on_event)?;
    Ok(MigrationReport {
        data: Some(data_stats),
        metadata_rows: Some(metadata_rows),
        destination_sqlite_bytes,
        cutover_completed,
    })
}

/// Resume publication, or activate a destination that is already complete.
fn resume_or_cut_over(
    config: &MigrationConfig,
    source: Connection,
    schema: &[SchemaObject],
    on_event: &mut dyn FnMut(MigrationEvent),
) -> Result<MigrationReport, Box<dyn Error>> {
    let destination = Connection::open(&config.destination)?;
    destination.busy_timeout(Duration::from_secs(300))?;
    configure_read_cache(&destination, config.cache_mib)?;
    let complete = binary_value_store::verify_complete(&destination).is_ok();
    on_event(MigrationEvent::PhaseStarted(MigrationPhase::Verify));
    verify_destination(&source, &destination, schema, None, complete)?;
    if complete {
        on_event(MigrationEvent::PhaseStarted(MigrationPhase::Integrity));
        validate_integrity(&destination, config.integrity == IntegrityLevel::Full)?;
        if config.integrity == IntegrityLevel::Full {
            validate_semantic_integrity(&destination, on_event)?;
        }
        drop(destination);
        on_event(MigrationEvent::PhaseStarted(MigrationPhase::Reopen));
        validate_production_reopen(&config.destination)?;
    } else {
        publish_destination(config, destination, on_event)?;
    }
    let destination_sqlite_bytes = fs::metadata(&config.destination)?.len();
    release_source_lock(source)?;
    let cutover_completed = maybe_cut_over(config, on_event)?;
    Ok(MigrationReport {
        data: None,
        metadata_rows: None,
        destination_sqlite_bytes,
        cutover_completed,
    })
}

/// Optimize, verify, durably finalize, and reopen an incomplete destination.
fn publish_destination(
    config: &MigrationConfig,
    destination: Connection,
    on_event: &mut dyn FnMut(MigrationEvent),
) -> Result<u64, Box<dyn Error>> {
    if config.vacuum {
        on_event(MigrationEvent::PhaseStarted(MigrationPhase::Vacuum));
        let started = Instant::now();
        destination.execute_batch("VACUUM;")?;
        on_event(MigrationEvent::VacuumCompleted {
            elapsed: started.elapsed(),
        });
    }
    on_event(MigrationEvent::PhaseStarted(MigrationPhase::Analyze));
    destination.execute_batch("ANALYZE; PRAGMA optimize;")?;
    on_event(MigrationEvent::PhaseStarted(MigrationPhase::Integrity));
    let full_integrity = config.integrity == IntegrityLevel::Full;
    validate_integrity(&destination, full_integrity)?;
    if full_integrity {
        validate_semantic_integrity(&destination, on_event)?;
    }
    restore_durable_pragmas(&destination)?;
    binary_value_store::finalize_migration_destination(&destination)?;
    drop(destination);

    on_event(MigrationEvent::PhaseStarted(MigrationPhase::Reopen));
    validate_production_reopen(&config.destination)?;
    let sqlite_bytes = fs::metadata(&config.destination)?.len();
    on_event(MigrationEvent::DestinationCompleted { sqlite_bytes });
    Ok(sqlite_bytes)
}
/// Configure a newly created destination for disposable, high-throughput bulk loading.
fn configure_bulk_destination(
    destination: &Connection,
    page_size: u64,
    cache_mib: u64,
) -> Result<(), rusqlite::Error> {
    let cache_kib = cache_mib * 1_024;
    destination.execute_batch(&format!(
        "PRAGMA page_size = {page_size};
         PRAGMA journal_mode = OFF;
         PRAGMA synchronous = OFF;
         PRAGMA locking_mode = EXCLUSIVE;
         PRAGMA foreign_keys = OFF;
         PRAGMA automatic_index = OFF;
         PRAGMA secure_delete = OFF;
         PRAGMA temp_store = FILE;
         PRAGMA cache_size = -{cache_kib};
         PRAGMA threads = 8;"
    ))
}

/// Apply the requested cache and mmap budget to a standalone read connection.
fn configure_read_cache(connection: &Connection, cache_mib: u64) -> Result<(), rusqlite::Error> {
    let cache_kib = cache_mib * 1_024;
    let mmap_bytes = cache_mib * 1_024 * 1_024;
    connection.execute_batch(&format!(
        "PRAGMA cache_size = -{cache_kib};
         PRAGMA mmap_size = {mmap_bytes};"
    ))
}

/// Apply the requested cache and mmap budget to the attached source schema.
fn configure_attached_source(
    destination: &Connection,
    cache_mib: u64,
) -> Result<(), rusqlite::Error> {
    let cache_kib = cache_mib * 1_024;
    let mmap_bytes = cache_mib * 1_024 * 1_024;
    destination.execute_batch(&format!(
        "PRAGMA migration_source.cache_size = -{cache_kib};
         PRAGMA migration_source.mmap_size = {mmap_bytes};"
    ))
}

/// Restore crash-safe runtime pragmas after disposable bulk loading completes.
fn restore_durable_pragmas(destination: &Connection) -> Result<(), rusqlite::Error> {
    destination.execute_batch(
        "PRAGMA journal_mode = DELETE;
         PRAGMA synchronous = FULL;
         PRAGMA locking_mode = NORMAL;
         PRAGMA automatic_index = ON;",
    )
}

/// Reject unsafe paths, stale destinations, and source journals before opening a migration.
fn preflight(config: &MigrationConfig) -> Result<(), Box<dyn Error>> {
    if !(16..=MAX_CACHE_MIB).contains(&config.cache_mib) {
        return Err(format!("cache_mib must be between 16 and {MAX_CACHE_MIB}").into());
    }
    if !config.source.is_file() {
        return Err(format!("source does not exist: {}", config.source.display()).into());
    }
    let source_blob = suffixed_path(&config.source, EXTERNAL_BLOB_SUFFIX);
    if !source_blob.is_file() {
        return Err(format!(
            "source external trie blob does not exist: {}",
            source_blob.display()
        )
        .into());
    }
    let destination_blob = suffixed_path(&config.destination, EXTERNAL_BLOB_SUFFIX);
    if config.mode == MigrationMode::ResumeFinalize && !config.destination.is_file() {
        return Err(format!(
            "destination does not exist for finalization: {}",
            config.destination.display()
        )
        .into());
    }
    if config.mode == MigrationMode::Create && config.destination.exists() {
        return Err(format!(
            "destination already exists; refusing to overwrite: {}",
            config.destination.display()
        )
        .into());
    }
    match config.mode {
        MigrationMode::Create if destination_blob.exists() => {
            return Err(format!(
                "destination external trie blob already exists; refusing to overwrite: {}",
                destination_blob.display()
            )
            .into());
        }
        MigrationMode::ResumeFinalize if !destination_blob.is_file() => {
            return Err(format!(
                "destination external trie blob does not exist for finalization: {}",
                destination_blob.display()
            )
            .into());
        }
        _ => {}
    }
    if config.source == config.destination {
        return Err("source and destination must differ".into());
    }
    if let Some(backup) = &config.cutover_backup {
        require_same_directory(&config.source, &config.destination, backup)?;
        if backup.exists() {
            return Err(format!("cutover backup already exists: {}", backup.display()).into());
        }
        for suffix in ["-wal", "-shm", "-journal", EXTERNAL_BLOB_SUFFIX] {
            let backup_sidecar = suffixed_path(backup, suffix);
            if backup_sidecar.exists() {
                return Err(format!(
                    "cutover backup sidecar already exists: {}",
                    backup_sidecar.display()
                )
                .into());
            }
        }
    }
    for suffix in ["-wal", "-journal"] {
        let sidecar = suffixed_path(&config.source, suffix);
        if sidecar.exists() && fs::metadata(&sidecar)?.len() != 0 {
            return Err(format!(
                "source has a non-empty {suffix} sidecar ({}); stop and checkpoint the node first",
                sidecar.display()
            )
            .into());
        }
    }
    Ok(())
}

/// Give a new destination an independently named view of the immutable external trie data.
fn provision_destination_blob(source: &Path, destination: &Path) -> Result<(), Box<dyn Error>> {
    let source_blob = suffixed_path(source, EXTERNAL_BLOB_SUFFIX);
    let destination_blob = suffixed_path(destination, EXTERNAL_BLOB_SUFFIX);
    fs::hard_link(&source_blob, &destination_blob).map_err(|error| {
        format!(
            "failed to hard-link external trie blob {} to {} (both paths must be on one filesystem): {error}",
            source_blob.display(),
            destination_blob.display()
        )
    })?;
    let directory = destination
        .parent()
        .ok_or("destination has no parent directory")?;
    File::open(directory)?.sync_all()?;
    Ok(())
}

/// Require all cutover paths to share one directory for recoverable renames.
fn require_same_directory(
    source: &Path,
    destination: &Path,
    backup: &Path,
) -> Result<(), Box<dyn Error>> {
    let source_parent = source.parent().ok_or("source has no parent directory")?;
    let destination_parent = destination
        .parent()
        .ok_or("destination has no parent directory")?;
    let backup_parent = backup.parent().ok_or("backup has no parent directory")?;
    let source_parent = fs::canonicalize(source_parent)?;
    if fs::canonicalize(destination_parent)? != source_parent
        || fs::canonicalize(backup_parent)? != source_parent
    {
        return Err("source, destination, and cutover backup must share one directory".into());
    }
    Ok(())
}

/// Append a SQLite sidecar suffix without changing the database filename.
fn suffixed_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value: OsString = path.as_os_str().to_owned();
    value.push(suffix);
    PathBuf::from(value)
}

/// Activate a completed destination when an explicit backup path was supplied.
fn maybe_cut_over(
    config: &MigrationConfig,
    on_event: &mut dyn FnMut(MigrationEvent),
) -> Result<bool, Box<dyn Error>> {
    let Some(backup) = &config.cutover_backup else {
        return Ok(false);
    };
    on_event(MigrationEvent::PhaseStarted(MigrationPhase::Cutover));
    cut_over(&config.source, &config.destination, backup)?;
    validate_production_reopen(&config.source)?;
    on_event(MigrationEvent::CutoverCompleted {
        active: config.source.clone(),
        backup: backup.clone(),
    });
    Ok(true)
}

/// Preserve the legacy database and activate Binary V1 with recoverable renames.
fn cut_over(source: &Path, destination: &Path, backup: &Path) -> Result<(), Box<dyn Error>> {
    cut_over_with(
        source,
        destination,
        backup,
        |from, to| fs::rename(from, to),
        |path| File::open(path)?.sync_all(),
    )
}

/// Cut over using injectable filesystem operations for failure-path tests.
fn cut_over_with<R, S>(
    source: &Path,
    destination: &Path,
    backup: &Path,
    mut rename: R,
    mut sync: S,
) -> Result<(), Box<dyn Error>>
where
    R: FnMut(&Path, &Path) -> std::io::Result<()>,
    S: FnMut(&Path) -> std::io::Result<()>,
{
    require_same_directory(source, destination, backup)?;
    if !source.is_file() || !destination.is_file() {
        return Err("cutover source or completed destination is missing".into());
    }
    if backup.exists() {
        return Err(format!("cutover backup already exists: {}", backup.display()).into());
    }
    let source_blob = suffixed_path(source, EXTERNAL_BLOB_SUFFIX);
    let destination_blob = suffixed_path(destination, EXTERNAL_BLOB_SUFFIX);
    let backup_blob = suffixed_path(backup, EXTERNAL_BLOB_SUFFIX);
    if !source_blob.is_file() || !destination_blob.is_file() {
        return Err("cutover source or destination external trie blob is missing".into());
    }
    if backup_blob.exists() {
        return Err(format!(
            "cutover backup has an existing external trie blob: {}",
            backup_blob.display()
        )
        .into());
    }
    for suffix in ["-wal", "-shm", "-journal"] {
        if suffixed_path(destination, suffix).exists() {
            return Err(format!("completed destination has an unexpected {suffix} sidecar").into());
        }
        if suffixed_path(backup, suffix).exists() {
            return Err(format!("cutover backup has an existing {suffix} sidecar").into());
        }
    }

    sync(destination)?;
    sync(&destination_blob)?;
    let directory = source.parent().ok_or("source has no parent directory")?;
    sync(directory)?;
    rename(source, backup)?;

    let mut moved_sidecars = Vec::new();
    for suffix in ["-wal", "-shm", "-journal"] {
        let source_sidecar = suffixed_path(source, suffix);
        if !source_sidecar.exists() {
            continue;
        }
        let backup_sidecar = suffixed_path(backup, suffix);
        if let Err(error) = rename(&source_sidecar, &backup_sidecar) {
            let rollback_errors =
                rollback_source_rename(source, backup, &moved_sidecars, &mut rename, &mut sync);
            return Err(cutover_error(
                format!("failed to preserve legacy {suffix} sidecar: {error}"),
                source,
                backup,
                destination,
                &rollback_errors,
            )
            .into());
        }
        moved_sidecars.push((source_sidecar, backup_sidecar));
    }

    if let Err(error) = rename(&destination_blob, &backup_blob) {
        let rollback_errors =
            rollback_source_rename(source, backup, &moved_sidecars, &mut rename, &mut sync);
        return Err(cutover_error(
            format!("failed to preserve the legacy external trie blob: {error}"),
            source,
            backup,
            destination,
            &rollback_errors,
        )
        .into());
    }
    moved_sidecars.push((destination_blob, backup_blob));

    if let Err(error) = rename(destination, source) {
        let rollback_errors =
            rollback_source_rename(source, backup, &moved_sidecars, &mut rename, &mut sync);
        return Err(cutover_error(
            format!("failed to activate completed Binary V1 database: {error}"),
            source,
            backup,
            destination,
            &rollback_errors,
        )
        .into());
    }
    sync(directory).map_err(|error| {
        format!(
            "Binary V1 activation completed, but syncing {} failed: {error}; active={}, backup={}",
            directory.display(),
            source.display(),
            backup.display()
        )
    })?;
    Ok(())
}

/// Restore the legacy source after an activation failure and report every failure.
fn rollback_source_rename<R, S>(
    source: &Path,
    backup: &Path,
    moved_sidecars: &[(PathBuf, PathBuf)],
    rename: &mut R,
    sync: &mut S,
) -> Vec<String>
where
    R: FnMut(&Path, &Path) -> std::io::Result<()>,
    S: FnMut(&Path) -> std::io::Result<()>,
{
    let mut errors = Vec::new();
    if let Err(error) = rename(backup, source) {
        errors.push(format!(
            "failed to restore {} to {}: {error}",
            backup.display(),
            source.display()
        ));
    }
    for (source_sidecar, backup_sidecar) in moved_sidecars.iter().rev() {
        if let Err(error) = rename(backup_sidecar, source_sidecar) {
            errors.push(format!(
                "failed to restore {} to {}: {error}",
                backup_sidecar.display(),
                source_sidecar.display()
            ));
        }
    }
    if let Some(directory) = source.parent() {
        if let Err(error) = sync(directory) {
            errors.push(format!(
                "failed to sync restored source directory {}: {error}",
                directory.display()
            ));
        }
    }
    errors
}

/// Combine the primary cutover failure with any automatic rollback failures.
fn cutover_error(
    primary: String,
    source: &Path,
    backup: &Path,
    destination: &Path,
    rollback_errors: &[String],
) -> String {
    if rollback_errors.is_empty() {
        return format!("{primary}; the legacy source was restored");
    }
    format!(
        "{primary}; automatic rollback also failed: {}; inspect source={}, backup={}, destination={}",
        rollback_errors.join("; "),
        source.display(),
        backup.display(),
        destination.display()
    )
}

/// Build an immutable read-only URI for checkpointed bytes protected from writers.
fn immutable_uri(path: &Path) -> Result<String, rusqlite::Error> {
    sqlite_readonly_uri(path, true)
}

/// Open a finalized database without permitting recovery writes or sidecar creation.
fn immutable_connection(path: &Path) -> Result<Connection, rusqlite::Error> {
    let uri = immutable_uri(path)?;
    Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
}

/// Reopen a completed database through the production external-blob MARF and read its latest root.
fn validate_production_reopen(path: &Path) -> Result<(), Box<dyn Error>> {
    let connection = immutable_connection(path)?;
    binary_value_store::verify_complete(&connection)?;
    let latest_tip: Option<StacksBlockId> = connection
        .query_row(
            "SELECT block_hash FROM marf_data
             WHERE unconfirmed = 0 ORDER BY block_id DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;

    let path = path
        .to_str()
        .ok_or("MARF database path is not valid UTF-8")?;
    let mut open_opts = MARFOpenOpts::default();
    open_opts.external_blobs = true;
    let storage = TrieFileStorage::<StacksBlockId>::open_readonly(path, open_opts)?;
    let mut marf = MARF::from_storage(storage);
    if let Some(tip) = latest_tip {
        marf.get_root_hash_at(&tip)?;
    }
    Ok(())
}

/// Open the offline source and reserve SQLite's single writer slot for the
/// duration of migration. The transaction never writes; it prevents a node
/// that was accidentally left running from changing the source mid-copy.
fn locked_source_connection(path: &Path) -> Result<Connection, rusqlite::Error> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(Duration::from_secs(1))?;
    connection.execute_batch("BEGIN IMMEDIATE")?;
    connection.execute_batch("PRAGMA query_only = ON")?;
    Ok(connection)
}

/// Roll back the read-only immediate transaction that reserves the source writer slot.
fn release_source_lock(source: Connection) -> Result<(), rusqlite::Error> {
    source.execute_batch("ROLLBACK")
}

/// Inventory only explicitly MARF-owned schema objects for verbatim copying.
fn generic_schema(source: &Connection) -> Result<Vec<SchemaObject>, Box<dyn Error>> {
    let mut marf_objects = Vec::new();
    for object in sqlite_schema_objects(source, "main")? {
        let clarity_owned = binary_value_store::owns_table(&object.table);
        if clarity_owned {
            continue;
        }
        if MARF_SQLITE_TABLES.contains(&object.table.as_str()) {
            marf_objects.push(object);
            continue;
        }
        return Err(format!(
            "unclassified SQLite {} '{}' owned by table '{}'; the Clarity migrator only copies explicitly MARF-owned objects",
            object.kind, object.name, object.table
        )
        .into());
    }
    if !marf_objects
        .iter()
        .any(|object| object.kind == "table" && object.name == "marf_data")
    {
        return Err("source is missing required MARF table 'marf_data'".into());
    }
    Ok(marf_objects)
}

/// Create the co-located MARF tables before copying their rows.
fn create_generic_tables(
    destination: &Connection,
    schema: &[SchemaObject],
) -> Result<(), rusqlite::Error> {
    for object in schema.iter().filter(|object| object.kind == "table") {
        destination.execute_batch(&object.sql)?;
    }
    Ok(())
}

/// Copy each MARF table in a dedicated transaction through the attached source.
fn copy_generic_tables(
    destination: &Connection,
    schema: &[SchemaObject],
    on_event: &mut dyn FnMut(MigrationEvent),
) -> Result<(), Box<dyn Error>> {
    for object in schema.iter().filter(|object| object.kind == "table") {
        let name = quote_sql_identifier(&object.name);
        let source_rows: u64 = destination.query_row(
            &format!("SELECT COUNT(*) FROM migration_source.{name}"),
            [],
            |row| row.get(0),
        )?;
        let started = Instant::now();
        destination.execute_batch("BEGIN IMMEDIATE")?;
        if let Err(error) = destination.execute(
            &format!("INSERT INTO main.{name} SELECT * FROM migration_source.{name}"),
            [],
        ) {
            let _ = destination.execute_batch("ROLLBACK");
            return Err(error.into());
        }
        destination.execute_batch("COMMIT")?;
        on_event(MigrationEvent::TableCopied {
            table: object.name.clone(),
            rows: source_rows,
            elapsed: started.elapsed(),
        });
    }
    Ok(())
}

/// Recreate MARF indexes and other non-table schema objects after bulk copying.
fn create_generic_secondary_objects(
    destination: &Connection,
    schema: &[SchemaObject],
) -> Result<(), rusqlite::Error> {
    for object in schema.iter().filter(|object| object.kind != "table") {
        destination.execute_batch(&object.sql)?;
    }
    Ok(())
}

/// Stream Clarity metadata while converting nullable block hashes from hex text to bytes.
fn migrate_metadata(
    source: &Connection,
    destination: &mut Connection,
) -> Result<u64, Box<dyn Error>> {
    let mut source_statement =
        source.prepare("SELECT key, blockhash, value FROM metadata_table ORDER BY rowid")?;
    let mut source_rows = source_statement.query([])?;
    destination.execute_batch("BEGIN IMMEDIATE")?;
    let mut count = 0;
    while let Some(row) = source_rows.next()? {
        let key = row.get_ref(0)?.as_str()?;
        let blockhash = row
            .get::<_, Option<String>>(1)?
            .map(|value| decode_fixed_hex::<32>(&value, "metadata block hash"))
            .transpose()?;
        let value = row.get::<_, Option<String>>(2)?;
        let block_id = blockhash
            .as_ref()
            .map_or(MetadataBlockId::Null, |bytes| MetadataBlockId::Bytes(bytes));
        binary_value_store::insert_metadata_row(
            destination,
            &MetadataRow {
                key,
                block_id,
                value: value.as_deref(),
            },
        )?;
        count += 1;
        if count % DATA_COMMIT_ROWS == 0 {
            destination.execute_batch("COMMIT; BEGIN IMMEDIATE")?;
        }
    }
    destination.execute_batch("COMMIT")?;
    Ok(count)
}

/// Stream content-addressed values into Binary V1 records in bounded transactions.
fn migrate_data(
    source: &Connection,
    destination: &mut Connection,
    on_event: &mut dyn FnMut(MigrationEvent),
) -> Result<(DataMigrationStats, (u64, u64, u64)), Box<dyn Error>> {
    // Preserve the legacy table's sequential scan. Its key index is not
    // covering, so key-order traversal turns each payload fetch into a random
    // lookup across the much larger source database.
    let mut statement = source.prepare("SELECT key, value FROM data_table ORDER BY rowid")?;
    let mut rows = statement.query([])?;
    let mut stats = DataMigrationStats::default();
    let mut writer = MigrationWriter::new();
    destination.execute_batch("BEGIN IMMEDIATE")?;

    while let Some(row) = rows.next()? {
        let key = row.get_ref(0)?.as_str()?;
        let canonical = row.get_ref(1)?.as_str()?;
        let key_bytes = decode_fixed_hex::<40>(key, "data-table key")?;
        let value_hash = MARFValue(key_bytes);
        let encoded = binary_value_store::encode_migrated(canonical)?;
        writer.put(destination, &value_hash, canonical, &encoded)?;

        stats.rows += 1;
        stats.source_key_bytes += key.len() as u64;
        stats.source_value_bytes += canonical.len() as u64;
        stats.destination_key_bytes += key_bytes.len() as u64;
        stats.destination_record_bytes += encoded.record().len() as u64;
        if encoded.shape().is_some() {
            stats.packed_rows += 1;
        } else {
            stats.reversible_rows += 1;
        }

        if stats.rows % DATA_COMMIT_ROWS == 0 {
            destination.execute_batch("COMMIT; BEGIN IMMEDIATE")?;
        }
        if stats.rows % PROGRESS_ROWS == 0 {
            on_event(MigrationEvent::ValuesProgress { rows: stats.rows });
        }
    }
    destination.execute_batch("COMMIT")?;
    Ok((stats, writer.shape_cache_stats()))
}

/// Verify schema identity, row-count parity, and normalized shape referential integrity.
fn verify_destination(
    source: &Connection,
    destination: &Connection,
    schema: &[SchemaObject],
    expected_clarity_rows: Option<(u64, u64)>,
    complete: bool,
) -> Result<(), Box<dyn Error>> {
    if complete {
        binary_value_store::verify_complete(destination)?;
    } else {
        binary_value_store::verify_finalization_ready(destination)?;
    }
    let destination_data = binary_value_store::data_row_count(destination)?;
    let destination_metadata = binary_value_store::metadata_row_count(destination)?;
    let (source_data, source_metadata) = match expected_clarity_rows {
        Some(rows) => rows,
        None => (
            source.query_row("SELECT COUNT(*) FROM data_table", [], |row| row.get(0))?,
            source.query_row("SELECT COUNT(*) FROM metadata_table", [], |row| row.get(0))?,
        ),
    };
    if destination_data != source_data || destination_metadata != source_metadata {
        return Err("Clarity table row counts changed during migration".into());
    }
    for object in schema.iter().filter(|object| object.kind == "table") {
        let name = quote_sql_identifier(&object.table);
        let source_rows: u64 =
            source.query_row(&format!("SELECT COUNT(*) FROM {name}"), [], |row| {
                row.get(0)
            })?;
        let destination_rows: u64 =
            destination.query_row(&format!("SELECT COUNT(*) FROM {name}"), [], |row| {
                row.get(0)
            })?;
        if source_rows != destination_rows {
            return Err(format!(
                "row-count mismatch for {}: source={source_rows}, destination={destination_rows}",
                object.table
            )
            .into());
        }
    }
    for object in schema {
        let destination_sql: Option<String> = destination
            .query_row(
                "SELECT sql FROM sqlite_master
                 WHERE type = ?1 AND name = ?2 AND tbl_name = ?3",
                params![&object.kind, &object.name, &object.table],
                |row| row.get(0),
            )
            .optional()?;
        if destination_sql.as_deref() != Some(object.sql.as_str()) {
            return Err(format!("schema mismatch for {} {}", object.kind, object.name).into());
        }
    }
    binary_value_store::verify_shape_references(destination)?;
    Ok(())
}

/// Run SQLite's quick or exhaustive physical-integrity checker.
fn validate_integrity(connection: &Connection, full: bool) -> Result<(), Box<dyn Error>> {
    let pragma = if full {
        "PRAGMA integrity_check"
    } else {
        "PRAGMA quick_check"
    };
    let result: String = connection.query_row(pragma, [], |row| row.get(0))?;
    if result != "ok" {
        return Err(format!("{pragma} failed: {result}").into());
    }
    Ok(())
}

/// Audit every Binary V1 record and report semantic-validation throughput.
fn validate_semantic_integrity(
    connection: &Connection,
    on_event: &mut dyn FnMut(MigrationEvent),
) -> Result<(), Box<dyn Error>> {
    on_event(MigrationEvent::PhaseStarted(
        MigrationPhase::SemanticIntegrity,
    ));
    let started = Instant::now();
    let rows = validate_binary_records(connection, on_event)?;
    on_event(MigrationEvent::SemanticIntegrityCompleted {
        rows,
        elapsed: started.elapsed(),
    });
    Ok(())
}

/// Reconstruct every value and re-derive its content-addressed key.
fn validate_binary_records(
    connection: &Connection,
    on_event: &mut dyn FnMut(MigrationEvent),
) -> Result<u64, Box<dyn Error>> {
    binary_value_store::audit_all_records(connection, |count| {
        if count % PROGRESS_ROWS == 0 {
            on_event(MigrationEvent::SemanticIntegrityProgress { rows: count });
        }
    })
    .map_err(Into::into)
}

/// Decode a legacy hex field and enforce its exact binary width.
fn decode_fixed_hex<const N: usize>(value: &str, label: &str) -> Result<[u8; N], Box<dyn Error>> {
    let bytes = hex_bytes(value).map_err(|error| format!("invalid {label}: {error}"))?;
    bytes
        .try_into()
        .map_err(|_| format!("invalid {label} length").into())
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use clarity::vm::types::Value;
    use stacks_common::util::hash::to_hex;

    use super::*;
    use crate::chainstate::stacks::index::ClarityMarfTrieId as _;

    fn legacy_database(path: &Path) {
        let mut open_opts = MARFOpenOpts::default();
        open_opts.external_blobs = true;
        let mut marf = MARF::<StacksBlockId>::from_path(path.to_str().unwrap(), open_opts).unwrap();
        let block = StacksBlockId([1; 32]);
        marf.begin(&StacksBlockId::sentinel(), &block).unwrap();
        marf.insert("marf-key", MARFValue::from_value("marf-value"))
            .unwrap();
        marf.seal().unwrap();
        marf.commit().unwrap();
        drop(marf);

        let connection = Connection::open(path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE data_table(key TEXT PRIMARY KEY, value TEXT);
                 CREATE TABLE metadata_table(key TEXT NOT NULL, blockhash TEXT, value TEXT, UNIQUE(key, blockhash));
                 CREATE INDEX md_blockhashes ON metadata_table(blockhash);",
            )
            .unwrap();
        let canonical = to_hex(&Value::UInt(42).serialize_to_vec().unwrap());
        let hash = MARFValue::from_value(&canonical);
        connection
            .execute(
                "INSERT INTO data_table VALUES (?1, ?2)",
                params![hash.to_hex(), canonical],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO metadata_table VALUES ('key', ?1, 'value')",
                ["11".repeat(32)],
            )
            .unwrap();
    }

    #[test]
    fn streams_legacy_database_into_binary_v1_schema() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("source.sqlite");
        let destination_path = directory.path().join("destination.sqlite");
        legacy_database(&source_path);
        let source_bytes = fs::read(&source_path).unwrap();

        let source = locked_source_connection(&source_path).unwrap();
        let schema = generic_schema(&source).unwrap();
        let mut destination = Connection::open(&destination_path).unwrap();
        create_generic_tables(&destination, &schema).unwrap();
        binary_value_store::initialize_migration_destination(&destination).unwrap();
        destination
            .execute(
                "ATTACH DATABASE ?1 AS migration_source",
                [immutable_uri(&source_path).unwrap()],
            )
            .unwrap();
        let mut events = Vec::new();
        let mut on_event = |event| events.push(event);
        copy_generic_tables(&destination, &schema, &mut on_event).unwrap();
        let metadata_rows = migrate_metadata(&source, &mut destination).unwrap();
        let (stats, _) = migrate_data(&source, &mut destination, &mut on_event).unwrap();
        binary_value_store::create_migration_indexes(&destination).unwrap();
        create_generic_secondary_objects(&destination, &schema).unwrap();
        destination
            .execute_batch("DETACH DATABASE migration_source")
            .unwrap();
        verify_destination(
            &source,
            &destination,
            &schema,
            Some((stats.rows, metadata_rows)),
            false,
        )
        .unwrap();
        let config = MigrationConfig {
            source: source_path.clone(),
            destination: destination_path.clone(),
            vacuum: false,
            integrity: IntegrityLevel::Quick,
            cache_mib: 16,
            mode: MigrationMode::ResumeFinalize,
            cutover_backup: None,
        };
        provision_destination_blob(&source_path, &destination_path).unwrap();
        publish_destination(&config, destination, &mut on_event).unwrap();
        let destination = immutable_connection(&destination_path).unwrap();
        assert_eq!(
            binary_value_store::detect(&destination).unwrap(),
            ValueStorageFormat::BinaryV1
        );
        assert_eq!(
            destination
                .query_row("SELECT typeof(key) FROM data_table", [], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap(),
            "blob"
        );
        assert_eq!(
            destination
                .query_row("SELECT typeof(blockhash) FROM metadata_table", [], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap(),
            "blob"
        );
        let migrated_key: Vec<u8> = destination
            .query_row("SELECT key FROM data_table", [], |row| row.get(0))
            .unwrap();
        let migrated_hash = MARFValue(migrated_key.try_into().unwrap());
        let canonical = to_hex(&Value::UInt(42).serialize_to_vec().unwrap());
        assert_eq!(
            binary_value_store::get_generic(&destination, &migrated_hash)
                .unwrap()
                .as_deref(),
            Some(canonical.as_str())
        );
        assert_eq!(
            validate_binary_records(&destination, &mut on_event).unwrap(),
            1
        );
        drop(destination);
        release_source_lock(source).unwrap();
        assert_eq!(fs::read(&source_path).unwrap(), source_bytes);
        assert!(!PathBuf::from(format!("{}-wal", source_path.display())).exists());
        assert!(!PathBuf::from(format!("{}-shm", source_path.display())).exists());
    }

    #[test]
    fn public_engine_reports_progress_and_outcome() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.sqlite");
        let destination = directory.path().join("destination.sqlite");
        legacy_database(&source);
        let config = MigrationConfig {
            source,
            destination: destination.clone(),
            vacuum: false,
            integrity: IntegrityLevel::Full,
            cache_mib: 16,
            mode: MigrationMode::Create,
            cutover_backup: None,
        };
        let mut events = Vec::new();

        let report = migrate(&config, &mut |event| events.push(event)).unwrap();

        assert_eq!(report.data.unwrap().rows, 1);
        assert_eq!(report.metadata_rows, Some(1));
        assert_eq!(
            report.destination_sqlite_bytes,
            fs::metadata(destination).unwrap().len()
        );
        assert!(!report.cutover_completed);
        assert_matches!(
            events.last(),
            Some(MigrationEvent::DestinationCompleted { sqlite_bytes })
                if *sqlite_bytes == report.destination_sqlite_bytes
        );
        let phases: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                MigrationEvent::PhaseStarted(phase) => Some(*phase),
                _ => None,
            })
            .collect();
        assert_eq!(
            phases,
            [
                MigrationPhase::CreateSchema,
                MigrationPhase::CopyMarfDomain,
                MigrationPhase::CopyClarityMetadata,
                MigrationPhase::CopyClarityValues,
                MigrationPhase::CreateIndexes,
                MigrationPhase::Verify,
                MigrationPhase::Analyze,
                MigrationPhase::Integrity,
                MigrationPhase::SemanticIntegrity,
                MigrationPhase::Reopen,
            ]
        );
    }

    #[test]
    fn immutable_source_uris_handle_paths_requiring_escaping() {
        let directory = tempfile::tempdir().unwrap();
        let nested = directory.path().join("space and # hash");
        fs::create_dir(&nested).unwrap();
        let source_path = nested.join("marf.sqlite");
        legacy_database(&source_path);

        let immutable = immutable_uri(&source_path).unwrap();
        assert!(immutable.contains("space%20and%20%23%20hash"));
        assert!(immutable.contains("immutable=1"));
        assert_eq!(
            binary_value_store::detect(&immutable_connection(&source_path).unwrap()).unwrap(),
            ValueStorageFormat::LegacyText
        );
    }

    #[test]
    fn schema_inventory_rejects_unclassified_tables() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE marf_data(block_id INTEGER PRIMARY KEY);
                 CREATE TABLE data_table(key TEXT PRIMARY KEY, value TEXT);
                 CREATE TABLE metadata_table(key TEXT, blockhash TEXT, value TEXT);
                 CREATE TABLE unexpected_extension(id INTEGER PRIMARY KEY);",
            )
            .unwrap();

        let error = generic_schema(&connection).unwrap_err().to_string();
        assert!(error.contains("unclassified SQLite table 'unexpected_extension'"));
    }

    #[test]
    fn failed_row_stream_does_not_modify_the_source() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("source.sqlite");
        let destination_path = directory.path().join("destination.sqlite");
        legacy_database(&source_path);
        let source = Connection::open(&source_path).unwrap();
        source
            .execute(
                "INSERT INTO data_table(key, value) VALUES ('invalid-key', 'value')",
                [],
            )
            .unwrap();
        drop(source);
        let source_bytes = fs::read(&source_path).unwrap();

        let source = locked_source_connection(&source_path).unwrap();
        let mut destination = Connection::open(&destination_path).unwrap();
        binary_value_store::initialize_migration_destination(&destination).unwrap();
        assert!(migrate_data(&source, &mut destination, &mut |_| {}).is_err());
        drop(destination);
        release_source_lock(source).unwrap();

        assert_eq!(fs::read(&source_path).unwrap(), source_bytes);
        assert!(!suffixed_path(&source_path, "-wal").exists());
        assert!(!suffixed_path(&source_path, "-shm").exists());
    }

    #[test]
    fn cutover_preserves_legacy_database_and_sidecars() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("marf.sqlite");
        let destination_path = directory.path().join("marf.binary-v1.sqlite");
        let backup_path = directory.path().join("marf.legacy.sqlite");
        legacy_database(&source_path);
        fs::write(suffixed_path(&source_path, "-shm"), b"legacy-sidecar").unwrap();

        let destination = Connection::open(&destination_path).unwrap();
        binary_value_store::initialize_migration_destination(&destination).unwrap();
        binary_value_store::create_migration_indexes(&destination).unwrap();
        binary_value_store::finalize_migration_destination(&destination).unwrap();
        drop(destination);
        provision_destination_blob(&source_path, &destination_path).unwrap();

        cut_over(&source_path, &destination_path, &backup_path).unwrap();

        assert!(!destination_path.exists());
        assert_eq!(
            fs::read(suffixed_path(&backup_path, "-shm")).unwrap(),
            b"legacy-sidecar"
        );
        assert_eq!(
            binary_value_store::detect(&immutable_connection(&source_path).unwrap()).unwrap(),
            ValueStorageFormat::BinaryV1
        );
        assert_eq!(
            binary_value_store::detect(&immutable_connection(&backup_path).unwrap()).unwrap(),
            ValueStorageFormat::LegacyText
        );
        assert!(suffixed_path(&source_path, EXTERNAL_BLOB_SUFFIX).is_file());
        assert!(suffixed_path(&backup_path, EXTERNAL_BLOB_SUFFIX).is_file());
    }

    #[test]
    fn cutover_restores_legacy_names_when_activation_rename_fails() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("marf.sqlite");
        let destination_path = directory.path().join("marf.binary-v1.sqlite");
        let backup_path = directory.path().join("marf.legacy.sqlite");
        legacy_database(&source_path);
        fs::write(suffixed_path(&source_path, "-shm"), b"legacy-sidecar").unwrap();

        let destination = Connection::open(&destination_path).unwrap();
        binary_value_store::initialize_migration_destination(&destination).unwrap();
        binary_value_store::create_migration_indexes(&destination).unwrap();
        binary_value_store::finalize_migration_destination(&destination).unwrap();
        drop(destination);
        provision_destination_blob(&source_path, &destination_path).unwrap();

        let mut rejected_activation = false;
        let result = cut_over_with(
            &source_path,
            &destination_path,
            &backup_path,
            |from, to| {
                if !rejected_activation && from == destination_path && to == source_path {
                    rejected_activation = true;
                    Err(std::io::Error::other("injected activation failure"))
                } else {
                    fs::rename(from, to)
                }
            },
            |path| File::open(path)?.sync_all(),
        );
        assert!(result.is_err());
        assert!(rejected_activation);
        assert!(destination_path.is_file());
        assert!(suffixed_path(&destination_path, EXTERNAL_BLOB_SUFFIX).is_file());
        assert!(!backup_path.exists());
        assert!(!suffixed_path(&backup_path, EXTERNAL_BLOB_SUFFIX).exists());
        assert_eq!(
            fs::read(suffixed_path(&source_path, "-shm")).unwrap(),
            b"legacy-sidecar"
        );
        assert_eq!(
            binary_value_store::detect(&immutable_connection(&source_path).unwrap()).unwrap(),
            ValueStorageFormat::LegacyText
        );
    }

    #[test]
    fn cutover_reports_an_activation_and_rollback_failure() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("marf.sqlite");
        let destination_path = directory.path().join("marf.binary-v1.sqlite");
        let backup_path = directory.path().join("marf.legacy.sqlite");
        legacy_database(&source_path);
        fs::write(&destination_path, b"completed-destination").unwrap();
        provision_destination_blob(&source_path, &destination_path).unwrap();

        let result = cut_over_with(
            &source_path,
            &destination_path,
            &backup_path,
            |from, to| {
                if (from == destination_path && to == source_path)
                    || (from == backup_path && to == source_path)
                {
                    Err(std::io::Error::other("injected rename failure"))
                } else {
                    fs::rename(from, to)
                }
            },
            |path| File::open(path)?.sync_all(),
        );

        let error = result.unwrap_err().to_string();
        assert!(error.contains("failed to activate completed Binary V1 database"));
        assert!(error.contains("automatic rollback also failed"));
        assert!(error.contains(source_path.to_str().unwrap()));
        assert!(error.contains(backup_path.to_str().unwrap()));
        assert!(error.contains(destination_path.to_str().unwrap()));
        assert!(!source_path.exists());
        assert!(backup_path.is_file());
        assert!(destination_path.is_file());
    }

    #[test]
    fn cutover_leaves_names_unchanged_when_preflight_sync_fails() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("marf.sqlite");
        let destination_path = directory.path().join("marf.binary-v1.sqlite");
        let backup_path = directory.path().join("marf.legacy.sqlite");
        legacy_database(&source_path);

        let destination = Connection::open(&destination_path).unwrap();
        binary_value_store::initialize_migration_destination(&destination).unwrap();
        binary_value_store::create_migration_indexes(&destination).unwrap();
        binary_value_store::finalize_migration_destination(&destination).unwrap();
        drop(destination);
        provision_destination_blob(&source_path, &destination_path).unwrap();

        let result = cut_over_with(
            &source_path,
            &destination_path,
            &backup_path,
            |from, to| fs::rename(from, to),
            |_| Err(std::io::Error::other("injected sync failure")),
        );
        assert!(result.is_err());
        assert!(source_path.is_file());
        assert!(destination_path.is_file());
        assert!(!backup_path.exists());
        assert_eq!(
            binary_value_store::detect(&immutable_connection(&source_path).unwrap()).unwrap(),
            ValueStorageFormat::LegacyText
        );
        assert_eq!(
            binary_value_store::detect(&immutable_connection(&destination_path).unwrap()).unwrap(),
            ValueStorageFormat::BinaryV1
        );
    }

    #[test]
    fn preflight_rejects_an_existing_cutover_backup() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("marf.sqlite");
        let destination_path = directory.path().join("marf.binary-v1.sqlite");
        let backup_path = directory.path().join("marf.legacy.sqlite");
        legacy_database(&source_path);
        fs::write(&backup_path, b"do-not-overwrite").unwrap();

        let config = MigrationConfig {
            source: source_path,
            destination: destination_path,
            vacuum: false,
            integrity: IntegrityLevel::Quick,
            cache_mib: 16,
            mode: MigrationMode::Create,
            cutover_backup: Some(backup_path),
        };
        let error = preflight(&config).unwrap_err().to_string();
        assert!(error.contains("cutover backup already exists"));
    }

    #[test]
    fn preflight_rejects_a_nonempty_rollback_journal() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("marf.sqlite");
        let destination_path = directory.path().join("marf.binary-v1.sqlite");
        legacy_database(&source_path);
        fs::write(
            suffixed_path(&source_path, "-journal"),
            b"pending transaction",
        )
        .unwrap();

        let config = MigrationConfig {
            source: source_path,
            destination: destination_path,
            vacuum: false,
            integrity: IntegrityLevel::Quick,
            cache_mib: 16,
            mode: MigrationMode::Create,
            cutover_backup: None,
        };
        let error = preflight(&config).unwrap_err().to_string();
        assert!(error.contains("non-empty -journal sidecar"));
    }

    #[test]
    fn preflight_requires_the_external_blob_pair() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("marf.sqlite");
        let destination_path = directory.path().join("marf.binary-v1.sqlite");
        legacy_database(&source_path);

        fs::remove_file(suffixed_path(&source_path, EXTERNAL_BLOB_SUFFIX)).unwrap();
        let create = MigrationConfig {
            source: source_path.clone(),
            destination: destination_path.clone(),
            vacuum: false,
            integrity: IntegrityLevel::Quick,
            cache_mib: 16,
            mode: MigrationMode::Create,
            cutover_backup: None,
        };
        assert!(
            preflight(&create)
                .unwrap_err()
                .to_string()
                .contains("source external trie blob does not exist")
        );

        let second_source = directory.path().join("second.sqlite");
        legacy_database(&second_source);
        fs::write(&destination_path, b"incomplete-destination").unwrap();
        let resume = MigrationConfig {
            source: second_source,
            destination: destination_path,
            mode: MigrationMode::ResumeFinalize,
            ..create
        };
        assert!(
            preflight(&resume)
                .unwrap_err()
                .to_string()
                .contains("destination external trie blob does not exist")
        );
    }

    #[test]
    fn migration_source_lock_blocks_writers_without_modifying_rows() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("marf.sqlite");
        legacy_database(&source_path);

        let source = locked_source_connection(&source_path).unwrap();
        assert!(
            source
                .execute("INSERT INTO data_table VALUES ('blocked', 'self')", [])
                .is_err()
        );

        let competing_writer = Connection::open(&source_path).unwrap();
        competing_writer
            .busy_timeout(Duration::from_millis(0))
            .unwrap();
        assert!(
            competing_writer
                .execute("INSERT INTO data_table VALUES ('blocked', 'other')", [])
                .is_err()
        );

        release_source_lock(source).unwrap();
        competing_writer
            .execute(
                "INSERT INTO data_table VALUES ('allowed', 'after-release')",
                [],
            )
            .unwrap();
        assert_eq!(
            competing_writer
                .query_row(
                    "SELECT count(*) FROM data_table WHERE key IN ('blocked', 'allowed')",
                    [],
                    |row| row.get::<_, u64>(0),
                )
                .unwrap(),
            1
        );
    }

    #[test]
    fn completed_destination_can_be_cut_over_later() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("marf.sqlite");
        let destination_path = directory.path().join("marf.binary-v1.sqlite");
        let backup_path = directory.path().join("marf.legacy.sqlite");
        legacy_database(&source_path);

        let create = MigrationConfig {
            source: source_path.clone(),
            destination: destination_path.clone(),
            vacuum: false,
            integrity: IntegrityLevel::Quick,
            cache_mib: 16,
            mode: MigrationMode::Create,
            cutover_backup: None,
        };
        migrate(&create, &mut |_| {}).unwrap();

        let config = MigrationConfig {
            source: source_path.clone(),
            destination: destination_path,
            vacuum: false,
            integrity: IntegrityLevel::Full,
            cache_mib: 16,
            mode: MigrationMode::ResumeFinalize,
            cutover_backup: Some(backup_path.clone()),
        };
        migrate(&config, &mut |_| {}).unwrap();

        assert_eq!(
            binary_value_store::detect(&immutable_connection(&source_path).unwrap()).unwrap(),
            ValueStorageFormat::BinaryV1
        );
        assert_eq!(
            binary_value_store::detect(&immutable_connection(&backup_path).unwrap()).unwrap(),
            ValueStorageFormat::LegacyText
        );
    }

    #[test]
    fn semantic_integrity_rejects_a_corrupt_record() {
        let connection = Connection::open_in_memory().unwrap();
        binary_value_store::initialize_migration_destination(&connection).unwrap();
        let canonical = to_hex(&Value::UInt(42).serialize_to_vec().unwrap());
        let hash = MARFValue::from_value(&canonical);
        let encoded = binary_value_store::encode_migrated(&canonical).unwrap();
        MigrationWriter::new()
            .put(&connection, &hash, &canonical, &encoded)
            .unwrap();

        connection
            .execute(
                "UPDATE data_table SET value = X'0101FF', value_shape_id = NULL",
                [],
            )
            .unwrap();
        assert!(validate_binary_records(&connection, &mut |_| {}).is_err());
    }
}

/// Create an incomplete direct-value destination with MARF tables and Binary V1 metadata.
/// The source must be a checkpointed, immutable offline snapshot. Values are migrated separately.
pub fn prepare_extent_destination(
    source_path: &Path,
    destination_path: &Path,
    on_event: &mut dyn FnMut(MigrationEvent),
) -> Result<Connection, Box<dyn Error>> {
    if destination_path.exists() {
        return Err("extent destination already exists".into());
    }
    let source = immutable_connection(source_path)?;
    if binary_value_store::detect(&source)? != ValueStorageFormat::LegacyText {
        return Err("extent migration currently requires a legacy source".into());
    }
    let schema = generic_schema(&source)?;
    let mut destination = Connection::open(destination_path)?;
    destination.execute_batch(
        "PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; PRAGMA cache_size=-262144;",
    )?;
    create_generic_tables(&destination, &schema)?;
    binary_value_store::initialize_migration_destination(&destination)?;
    destination.execute(
        "ATTACH DATABASE ?1 AS migration_source",
        [immutable_uri(source_path)?],
    )?;
    copy_generic_tables(&destination, &schema, on_event)?;
    let rows = migrate_metadata(&source, &mut destination)?;
    on_event(MigrationEvent::MetadataCopied { rows });
    create_generic_secondary_objects(&destination, &schema)?;
    binary_value_store::create_migration_indexes(&destination)?;
    destination.execute_batch("DETACH DATABASE migration_source")?;
    Ok(destination)
}
