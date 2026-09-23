//! Resumable offline conversion of a frozen legacy Clarity database to direct value extents.

use std::collections::HashMap;
use std::error::Error;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::mem;
use std::path::PathBuf;
use std::thread;
use std::time::{Instant, UNIX_EPOCH};

use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use stacks_common::util::hash::{hex_bytes, to_hex};

use crate::chainstate::stacks::index::record::NodeRecordFormat;
use crate::chainstate::stacks::index::value_relocation::BlobRelocation;
use crate::chainstate::stacks::index::{Error as MarfError, MARFValue, ValueExtent};
use crate::clarity_vm::database::binary_value_store::{self, MigrationEvent};
use crate::clarity_vm::database::value_extents::{PreparedValueExtent, ValueExtentStore};
use crate::util_lib::db::sqlite_readonly_uri;

/// Bound on canonical value bytes buffered before one durable append batch.
const VALUE_BATCH_BYTES: usize = 64 * 1024 * 1024;
/// Bound on tiny records admitted per checkpoint.
const VALUE_BATCH_ROWS: usize = 65_536;
/// Maximum parallel packing workers; the writer remains ordered.
const VALUE_WORKERS: usize = 8;
/// Bound on cached historical relocation vectors.
const PLAN_CACHE_BYTES: usize = 1024 * 1024 * 1024;
/// Maximum source bytes queued per parallel planning batch, apart from one oversized trie.
const PLAN_BATCH_BYTES: usize = 32 * 1024 * 1024;
/// Maximum tiny tries queued per planning batch.
const PLAN_BATCH_ROWS: usize = 1024;
/// Maximum independent trie planning workers.
const PLAN_WORKERS: usize = 8;

/// Source and destination for an offline migration; the destination is a new directory.
#[derive(Debug, Clone)]
pub struct ExtentMigrationConfig {
    /// Frozen, checkpointed legacy SQLite database; always opened read-only.
    pub source_db: PathBuf,
    /// Matching legacy MARF blob file, which may have a different basename.
    pub source_blobs: PathBuf,
    /// New directory containing marf.sqlite, its blobs and value extents.
    pub destination: PathBuf,
    /// Maximum scratch index cache or resident lookup-table memory in MiB.
    pub index_cache_mib: u32,
}

/// Progress at durable migration boundaries.
#[derive(Debug, Clone)]
pub struct ExtentMigrationProgress {
    /// Current packing, index construction/loading, planning, rewriting, or completion phase.
    pub phase: &'static str,
    /// Source ROWID during packing; item count in other phases.
    pub completed: u64,
}

/// A source trie and its destination relocation plan.
struct SourceBlob {
    /// Source row identifier; negative identifiers denote mined_blocks rows.
    id: i64,
    /// Inline source bytes, or an empty vector for external storage.
    inline: Vec<u8>,
    /// Absolute source blob-file offset.
    offset: u64,
    /// Source blob-file byte length.
    length: u64,
}

/// A compact immutable value-index entry used only by the offline converter.
struct ValueLocation {
    /// Exact logical value key, in SQLite BLOB sort order.
    hash: [u8; 40],
    /// Durable extent record offset.
    offset: u64,
    /// Durable extent record length.
    length: u64,
}

/// Sorted in-memory value locations, or a bounded-memory SQLite fallback.
struct ValueLookup {
    /// Complete sorted table when it fits the configured memory budget.
    entries: Option<Vec<ValueLocation>>,
}

impl ValueLookup {
    /// Load the completed immutable mapping after releasing the large SQLite page cache.
    fn load(
        db: &Connection,
        budget_bytes: u64,
        count_hint: Option<u64>,
        on_event: &mut dyn FnMut(ExtentMigrationProgress),
    ) -> Result<Self, Box<dyn Error>> {
        on_event(ExtentMigrationProgress {
            phase: "loading-value-index",
            completed: 0,
        });
        let count = match count_hint {
            Some(count) => count,
            None => db.query_row("SELECT COUNT(*) FROM migration.values_map", [], |row| {
                row.get(0)
            })?,
        };
        let bytes = count
            .checked_mul(mem::size_of::<ValueLocation>() as u64)
            .ok_or("value index size overflow")?;
        if bytes > budget_bytes {
            eprintln!("value-lookup mode=sqlite rows={count} required_bytes={bytes}");
            return Ok(Self { entries: None });
        }
        db.execute_batch("PRAGMA migration.cache_size=-131072; PRAGMA shrink_memory")?;
        let mut entries: Vec<ValueLocation> = Vec::new();
        if entries.try_reserve_exact(usize::try_from(count)?).is_err() {
            db.execute_batch(&format!(
                "PRAGMA migration.cache_size=-{}",
                (budget_bytes / 1024).max(1)
            ))?;
            eprintln!("value-lookup mode=sqlite reason=allocation rows={count}");
            return Ok(Self { entries: None });
        }
        let started = Instant::now();
        on_event(ExtentMigrationProgress {
            phase: "loading-value-index",
            completed: 0,
        });
        let mut statement =
            db.prepare("SELECT hash,offset,length FROM migration.values_map ORDER BY hash")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            if entries.len() as u64 >= count {
                return Err("value index exceeds checkpoint count".into());
            }
            let hash: [u8; 40] = row.get_ref(0)?.as_blob()?.try_into()?;
            if entries.last().is_some_and(|previous| previous.hash >= hash) {
                return Err("value index is not strictly ordered".into());
            }
            entries.push(ValueLocation {
                hash,
                offset: row.get(1)?,
                length: row.get(2)?,
            });
            if entries.len() % 1_048_576 == 0 {
                on_event(ExtentMigrationProgress {
                    phase: "loading-value-index",
                    completed: entries.len() as u64,
                });
            }
        }
        if entries.len() as u64 != count {
            return Err("value index changed while loading".into());
        }
        eprintln!(
            "value-lookup mode=memory rows={count} bytes={bytes} load_ms={}",
            started.elapsed().as_millis()
        );
        Ok(Self {
            entries: Some(entries),
        })
    }

    /// Resolve an exact logical key without database access when the table is resident.
    fn locate(&self, db: &Connection, hash: &MARFValue) -> Result<Option<(u64, u64)>, MarfError> {
        if let Some(entries) = &self.entries {
            return Ok(find_value_location(entries, hash));
        }
        db.prepare_cached("SELECT offset,length FROM migration.values_map WHERE hash=?1")?
            .query_row([hash.0.as_slice()], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()
            .map_err(MarfError::SQLError)
    }
}

/// Look up one immutable value location by its complete logical key.
fn find_value_location(entries: &[ValueLocation], hash: &MARFValue) -> Option<(u64, u64)> {
    entries
        .binary_search_by(|entry| entry.hash.cmp(&hash.0))
        .ok()
        .map(|index| {
            let entry = &entries[index];
            (entry.offset, entry.length)
        })
}

/// One source trie buffered for independent CPU planning.
struct PlanInput {
    /// Durable source block identifier.
    id: i64,
    /// Complete immutable legacy trie bytes.
    bytes: Vec<u8>,
}

/// One encoded relocation plan awaiting coordinator publication.
#[derive(Debug, PartialEq, Eq)]
struct PlannedBlob {
    /// Source block identifier.
    id: i64,
    /// Original/destination node-offset pairs in portable little-endian form.
    offsets: Vec<u8>,
    /// Reserved destination trie length.
    length: u64,
}

/// Encode a plan for the scratch database without changing its deterministic order.
fn encode_plan(id: i64, plan: BlobRelocation) -> PlannedBlob {
    let mut offsets = Vec::with_capacity(plan.offsets.len() * 16);
    for (old, new) in plan.offsets {
        offsets.extend(old.to_le_bytes());
        offsets.extend(new.to_le_bytes());
    }
    PlannedBlob {
        id,
        offsets,
        length: plan.length,
    }
}

/// Plan independent immutable tries concurrently; preserve input order and propagate failures.
fn prepare_plan_batch(
    batch: &[PlanInput],
    entries: &[ValueLocation],
    workers: usize,
) -> Result<Vec<PlannedBlob>, Box<dyn Error>> {
    if batch.is_empty() {
        return Ok(Vec::new());
    }
    let workers = workers.max(1).min(batch.len());
    thread::scope(|scope| {
        let handles = batch
            .chunks(batch.len().div_ceil(workers))
            .map(|chunk| {
                scope.spawn(move || {
                    chunk
                        .iter()
                        .map(|input| {
                            BlobRelocation::plan_narrow(&input.bytes, |hash| {
                                Ok(find_value_location(entries, hash).is_some())
                            })
                            .map(|plan| encode_plan(input.id, plan))
                            .map_err(|error| error.to_string())
                        })
                        .collect::<Result<Vec<_>, String>>()
                })
            })
            .collect::<Vec<_>>();
        let mut plans = Vec::with_capacity(batch.len());
        for handle in handles {
            plans.extend(
                handle
                    .join()
                    .map_err(|_| "trie planning worker panicked")??,
            );
        }
        Ok(plans)
    })
}

/// Publish a complete successfully planned batch on the sole SQLite-writing thread.
fn flush_plan_batch(
    db: &Connection,
    batch: &mut Vec<PlanInput>,
    entries: &[ValueLocation],
    workers: usize,
    completed: u64,
    on_event: &mut dyn FnMut(ExtentMigrationProgress),
) -> Result<(), Box<dyn Error>> {
    if batch.is_empty() {
        return Ok(());
    }
    let plans = prepare_plan_batch(batch, entries, workers)?;
    db.execute_batch("BEGIN IMMEDIATE")?;
    {
        let mut insert = db.prepare_cached(
            "INSERT INTO migration.plans(block_id,offsets,length) VALUES (?1,?2,?3)",
        )?;
        for plan in plans {
            insert.execute(params![plan.id, plan.offsets, plan.length])?;
        }
    }
    db.execute_batch("COMMIT")?;
    batch.clear();
    on_event(ExtentMigrationProgress {
        phase: "planning",
        completed,
    });
    Ok(())
}

/// An immutable ancestor plan linked to its neighbors in access order.
struct CachedPlan {
    /// Original-to-new node offsets.
    plan: BlobRelocation,
    /// Immediately older cached block.
    older: Option<i64>,
    /// Immediately newer cached block.
    newer: Option<i64>,
}

/// Ancestor relocation plans with byte-bounded, constant-time LRU bookkeeping.
struct PlanCache {
    /// Cached plans and their links; keys remain stable across hash-table resizing.
    entries: HashMap<i64, CachedPlan>,
    /// Least recently accessed block.
    oldest: Option<i64>,
    /// Most recently accessed block.
    newest: Option<i64>,
    /// Bytes held by decoded relocation pairs, excluding map metadata.
    bytes: usize,
    /// Maximum bytes of decoded relocation pairs.
    budget: usize,
}

impl PlanCache {
    /// Create a cache with an explicit decoded-offset memory bound.
    fn new(budget: usize) -> Self {
        Self {
            entries: HashMap::new(),
            oldest: None,
            newest: None,
            bytes: 0,
            budget,
        }
    }

    /// Move an existing plan to the newest end without allocating ordering records.
    fn touch(&mut self, key: i64) {
        if self.newest == Some(key) {
            return;
        }
        let cached = self.entries.get(&key).expect("cached plan exists");
        let (older, newer) = (cached.older, cached.newer);
        if let Some(older) = older {
            self.entries
                .get_mut(&older)
                .expect("older plan exists")
                .newer = newer;
        } else {
            self.oldest = newer;
        }
        if let Some(newer) = newer {
            self.entries
                .get_mut(&newer)
                .expect("newer plan exists")
                .older = older;
        }
        if let Some(newest) = self.newest {
            self.entries
                .get_mut(&newest)
                .expect("newest plan exists")
                .newer = Some(key);
        }
        let cached = self.entries.get_mut(&key).expect("cached plan exists");
        cached.older = self.newest;
        cached.newer = None;
        self.newest = Some(key);
    }

    /// Remove the oldest plan and release its decoded-offset budget.
    fn evict_oldest(&mut self) {
        let key = self
            .oldest
            .expect("cache byte accounting requires an entry");
        let cached = self.entries.remove(&key).expect("oldest plan exists");
        self.oldest = cached.newer;
        if let Some(oldest) = self.oldest {
            self.entries
                .get_mut(&oldest)
                .expect("next oldest plan exists")
                .older = None;
        } else {
            self.newest = None;
        }
        self.bytes -= cached.plan.offsets.len() * mem::size_of::<(u64, u64)>();
    }

    /// Resolve a historical pointer while retaining recently accessed ancestor layouts.
    fn resolve(&mut self, db: &Connection, block: u32, offset: u64) -> Result<u64, MarfError> {
        let key = i64::from(block);
        if let Some(cached) = self.entries.get(&key) {
            let result = cached.plan.resolve(offset);
            self.touch(key);
            return result;
        }
        let plan = load_plan(db, key).map_err(marf_error)?;
        let size = plan.offsets.len() * mem::size_of::<(u64, u64)>();
        if size > self.budget {
            return plan.resolve(offset);
        }
        while self.bytes.saturating_add(size) > self.budget {
            self.evict_oldest();
        }
        let result = plan.resolve(offset);
        if let Some(newest) = self.newest {
            self.entries
                .get_mut(&newest)
                .expect("newest plan exists")
                .newer = Some(key);
        } else {
            self.oldest = Some(key);
        }
        self.entries.insert(
            key,
            CachedPlan {
                plan,
                older: self.newest,
                newer: None,
            },
        );
        self.newest = Some(key);
        self.bytes += size;
        result
    }
}

/// Migrate or resume a frozen source. No source file is written, renamed, linked or truncated.
/// Incomplete destinations fail normal Binary V1 detection until final publication.
pub fn migrate_value_extents(
    config: &ExtentMigrationConfig,
    on_event: &mut dyn FnMut(ExtentMigrationProgress),
) -> Result<(), Box<dyn Error>> {
    for suffix in ["-wal", "-journal"] {
        let sidecar = PathBuf::from(format!("{}{suffix}", config.source_db.display()));
        if sidecar.exists() && sidecar.metadata()?.len() != 0 {
            return Err("source must be an offline, checkpointed snapshot".into());
        }
    }
    if config.index_cache_mib == 0 || config.index_cache_mib > 16_384 {
        return Err("index cache must be between 1 and 16384 MiB".into());
    }
    let identity = source_identity(config)?;
    let source = Connection::open_with_flags(
        sqlite_readonly_uri(&config.source_db, true)?,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?;
    source.execute_batch("PRAGMA cache_size=-131072; PRAGMA mmap_size=268435456")?;
    if NodeRecordFormat::from_database(&source)? != NodeRecordFormat::Legacy {
        return Err("migration source already uses a versioned physical trie format".into());
    }
    let mut source_file = File::open(&config.source_blobs)?;
    let db_path = config.destination.join("marf.sqlite");
    let progress_path = config.destination.join("migration.sqlite");
    let fresh = !config.destination.exists();
    if fresh {
        fs::create_dir(&config.destination)?;
    }
    if !fresh && !progress_path.exists() {
        return Err(
            "destination lacks a resumable migration checkpoint; use a fresh directory".into(),
        );
    }
    let db = if fresh {
        binary_value_store::prepare_extent_destination(
            &config.source_db,
            &db_path,
            &mut |event: MigrationEvent| eprintln!("extent migration: {event:?}"),
        )?
    } else {
        Connection::open(&db_path)?
    };
    db.execute_batch(
        "PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; PRAGMA cache_size=-131072",
    )?;
    db.execute(
        "ATTACH DATABASE ?1 AS migration",
        [progress_path.to_str().ok_or("non-UTF8 destination")?],
    )?;
    db.execute_batch("PRAGMA migration.journal_mode=DELETE; PRAGMA migration.synchronous=FULL; PRAGMA migration.cache_size=-131072")?;
    if fresh {
        db.execute_batch("BEGIN IMMEDIATE;
            CREATE TABLE migration.state (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE migration.values_map (hash BLOB PRIMARY KEY, offset INTEGER NOT NULL, length INTEGER NOT NULL) WITHOUT ROWID;
            CREATE TABLE migration.plans (block_id INTEGER PRIMARY KEY, offsets BLOB NOT NULL, length INTEGER NOT NULL, written INTEGER NOT NULL DEFAULT 0);
            COMMIT;")?;
        db.execute(
            "INSERT INTO migration.state VALUES ('source', ?1)",
            [&identity],
        )?;
    } else if state(&db, "source")?.as_deref() != Some(&identity) {
        return Err("source identity changed since the migration checkpoint".into());
    }
    if state(&db, "complete")?.is_some() {
        return Err("migration already completed".into());
    }
    let value_path = config.destination.join("marf.sqlite.values");
    let mut values = if fresh {
        let values = ValueExtentStore::open(&value_path, true)?;
        set_state(&db, "extent_generation", &to_hex(&values.store_id()))?;
        values
    } else {
        let values = ValueExtentStore::open_existing(&value_path, true)?;
        if state(&db, "extent_generation")?.as_deref() != Some(to_hex(&values.store_id()).as_str())
        {
            return Err("extent generation differs from migration checkpoint".into());
        }
        values
    };
    // During packing, all SQLite mutations belong to the scratch database. WAL keeps
    // checkpoints sequential; later cross-database trie publication uses rollback journals.
    db.execute_batch("PRAGMA migration.journal_mode=WAL; PRAGMA migration.synchronous=FULL")?;
    db.execute_batch(&format!(
        "PRAGMA migration.cache_size=-{}",
        u64::from(config.index_cache_mib) * 1024
    ))?;
    db.execute_batch("CREATE TABLE IF NOT EXISTS migration.values_pending (hash BLOB NOT NULL CHECK(length(hash)=40), offset INTEGER NOT NULL, length INTEGER NOT NULL)")?;
    migrate_values(&source, &db, &mut values, on_event)?;
    merge_pending_values(&db, on_event)?;
    db.execute_batch(
        "PRAGMA migration.wal_checkpoint(TRUNCATE); PRAGMA migration.journal_mode=DELETE",
    )?;
    seed_value_index(&db, on_event)?;
    // Offset plans are tied to the physical codec, independently of packed value records.
    if state(&db, "plan_format")?.as_deref() != Some("type-first-v1-compact-u32") {
        let written: u64 = db.query_row(
            "SELECT COUNT(*) FROM migration.plans WHERE written=1",
            [],
            |row| row.get(0),
        )?;
        if written != 0 {
            return Err("cannot reuse rewritten tries from another physical format".into());
        }
        db.execute_batch("BEGIN IMMEDIATE; DELETE FROM migration.plans")?;
        set_state(&db, "plan_format", "type-first-v1-compact-u32")?;
        db.execute_batch("COMMIT")?;
    }
    let lookup = ValueLookup::load(
        &db,
        u64::from(config.index_cache_mib) * 1024 * 1024,
        state(&db, "index_count")?
            .map(|count| count.parse())
            .transpose()?,
        on_event,
    )?;
    plan_blobs(&source, &db, &mut source_file, &lookup, on_event)?;
    let oversized: u64 = db.query_row(
        "SELECT COUNT(*) FROM migration.plans WHERE length>?1",
        [u64::from(u32::MAX)],
        |row| row.get(0),
    )?;
    if oversized != 0 {
        return Err(
            "compact relocation requires every referenced trie to fit four-byte offsets".into(),
        );
    }
    let mut output = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(config.destination.join("marf.sqlite.blobs"))?;
    let published_end: u64 = db.query_row(
        "SELECT COALESCE(MAX(m.external_offset+m.external_length),0) FROM marf_data m
         JOIN migration.plans p ON p.block_id=m.block_id WHERE p.written=1",
        [],
        |row| row.get(0),
    )?;
    if output.metadata()?.len() < published_end {
        return Err("destination blob file is missing durable published records".into());
    }
    rewrite_blobs(
        &source,
        &db,
        &mut source_file,
        &mut output,
        values.store_id(),
        &lookup,
        (u64::from(config.index_cache_mib) * 1024 * 1024 / 16).min(PLAN_CACHE_BYTES as u64)
            as usize,
        on_event,
    )?;
    if source_identity(config)? != identity {
        return Err("source changed during migration".into());
    }
    let planned: u64 =
        db.query_row("SELECT COUNT(*) FROM migration.plans", [], |row| row.get(0))?;
    let written: u64 = db.query_row(
        "SELECT COUNT(*) FROM migration.plans WHERE written=1",
        [],
        |row| row.get(0),
    )?;
    if planned != written {
        return Err("not every planned trie was written".into());
    }
    let check: String = db.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    if check != "ok" {
        return Err(format!("destination integrity: {check}").into());
    }
    output.sync_all()?;
    File::open(&config.destination)?.sync_all()?;
    db.execute_batch("BEGIN IMMEDIATE")?;
    db.execute_batch("CREATE TABLE clarity_extent_format (singleton INTEGER PRIMARY KEY CHECK(singleton=1), version INTEGER NOT NULL CHECK(version=1), store_id BLOB NOT NULL CHECK(length(store_id)=16))")?;
    db.execute(
        "INSERT INTO clarity_extent_format VALUES (1, 1, ?1)",
        [values.store_id().as_slice()],
    )?;
    NodeRecordFormat::TypeFirstV1.publish(&db)?;
    binary_value_store::finalize_migration_destination(&db)?;
    db.execute("INSERT INTO migration.state VALUES ('complete','1')", [])?;
    db.execute_batch("COMMIT")?;
    on_event(ExtentMigrationProgress {
        phase: "complete",
        completed: written,
    });
    Ok(())
}

/// Reset only trie planning in a stopped, cloned migration with an empty replacement blob file.
/// Packed values and their durable deduplication index remain unchanged.
pub fn reset_cloned_trie_plans(config: &ExtentMigrationConfig) -> Result<(), Box<dyn Error>> {
    let blob = config.destination.join("marf.sqlite.blobs");
    if fs::metadata(&blob)?.len() != 0 {
        return Err("reset requires an empty replacement blob file in a cloned destination".into());
    }
    let db = Connection::open(config.destination.join("marf.sqlite"))?;
    db.execute(
        "ATTACH DATABASE ?1 AS migration",
        [config
            .destination
            .join("migration.sqlite")
            .to_str()
            .ok_or("invalid path")?],
    )?;
    db.execute_batch(
        "PRAGMA migration.journal_mode=DELETE; PRAGMA migration.synchronous=FULL; BEGIN IMMEDIATE",
    )?;
    if state(&db, "source")?.as_deref() != Some(source_identity(config)?.as_str())
        || state(&db, "complete")?.is_some()
        || state(&db, "values_done")?.as_deref() != Some("1")
        || state(&db, "pending_merge_done")?.as_deref() != Some("1")
        || state(&db, "index_done")?.as_deref() != Some("1")
        || state(&db, "plan_format")?.as_deref() != Some("type-first-v1")
    {
        return Err(
            "clone does not contain an incomplete padded-layout migration with durable values"
                .into(),
        );
    }
    let values =
        ValueExtentStore::open_existing(&config.destination.join("marf.sqlite.values"), false)?;
    if state(&db, "extent_generation")?.as_deref() != Some(to_hex(&values.store_id()).as_str()) {
        return Err("clone extent generation mismatch".into());
    }
    db.execute_batch("DROP TABLE migration.plans;
        CREATE TABLE migration.plans (block_id INTEGER PRIMARY KEY, offsets BLOB NOT NULL, length INTEGER NOT NULL, written INTEGER NOT NULL DEFAULT 0)")?;
    set_state(&db, "plan_format", "type-first-v1-compact-u32")?;
    set_state(&db, "repacked_from", "type-first-v1")?;
    db.execute_batch("COMMIT")?;
    File::open(&config.destination)?.sync_all()?;
    Ok(())
}

/// Pack a bounded batch concurrently, preserving source order for deterministic appends.
fn prepare_value_batch(
    batch: &[(MARFValue, String)],
) -> Result<Vec<PreparedValueExtent>, Box<dyn Error>> {
    if batch.is_empty() {
        return Ok(Vec::new());
    }
    let workers = thread::available_parallelism()
        .map_or(1, usize::from)
        .min(VALUE_WORKERS)
        .min(batch.len());
    thread::scope(|scope| {
        let handles = batch
            .chunks(batch.len().div_ceil(workers))
            .map(|chunk| {
                scope.spawn(move || {
                    chunk
                        .iter()
                        .map(|(hash, canonical)| {
                            PreparedValueExtent::from_canonical(hash.clone(), canonical)
                                .map_err(|error| error.to_string())
                        })
                        .collect::<Result<Vec<_>, String>>()
                })
            })
            .collect::<Vec<_>>();
        let mut records = Vec::with_capacity(batch.len());
        for handle in handles {
            records.extend(
                handle
                    .join()
                    .map_err(|_| "value packing worker panicked")??,
            );
        }
        Ok(records)
    })
}

/// Persist content once and index its physical location only in the offline scratch database.
fn migrate_values(
    source: &Connection,
    db: &Connection,
    values: &mut ValueExtentStore,
    on_event: &mut dyn FnMut(ExtentMigrationProgress),
) -> Result<(), Box<dyn Error>> {
    if state(db, "values_done")?.is_some() {
        return Ok(());
    }
    let mut last: i64 = state(db, "value_row")?
        .unwrap_or_else(|| "0".into())
        .parse()?;
    let mut statement =
        source.prepare("SELECT rowid,key,value FROM data_table WHERE rowid>?1 ORDER BY rowid")?;
    let mut rows = statement.query([last])?;
    loop {
        let started = Instant::now();
        let mut batch = Vec::new();
        let mut bytes = 0;
        while bytes < VALUE_BATCH_BYTES && batch.len() < VALUE_BATCH_ROWS {
            let Some(row) = rows.next()? else {
                break;
            };
            last = row.get(0)?;
            let hash = hex_bytes(&row.get::<_, String>(1)?)?;
            let canonical: String = row.get(2)?;
            let hash = MARFValue(
                hash.try_into()
                    .map_err(|_| "invalid source content hash length")?,
            );
            bytes += canonical.len();
            batch.push((hash, canonical));
        }
        if batch.is_empty() {
            break;
        }
        let row_count = batch.len();
        let read_time = started.elapsed();
        let packed_at = Instant::now();
        let prepared = prepare_value_batch(&batch)?;
        let pack_time = packed_at.elapsed();
        let append_at = Instant::now();
        let locators = values.append_prepared(&prepared)?;
        drop(prepared);
        drop(batch);
        let append_time = append_at.elapsed();
        let index_at = Instant::now();
        db.execute_batch("BEGIN IMMEDIATE")?;
        {
            let mut insert =
                db.prepare_cached("INSERT INTO migration.values_pending VALUES (?1,?2,?3)")?;
            for (hash, extent) in locators {
                insert.execute(params![hash.0.as_slice(), extent.offset, extent.length])?;
            }
        }
        set_state(db, "value_row", &last.to_string())?;
        db.execute_batch("COMMIT")?;
        eprintln!("value-batch cursor={last} rows={row_count} canonical_bytes={bytes} read_ms={} pack_ms={} append_ms={} index_ms={} total_ms={}",
            read_time.as_millis(),pack_time.as_millis(),append_time.as_millis(),index_at.elapsed().as_millis(),started.elapsed().as_millis());
        on_event(ExtentMigrationProgress {
            phase: "values",
            completed: last as u64,
        });
    }
    set_state(db, "values_done", "1")?;
    Ok(())
}

/// Fixed tables and checkpoint keys for an ordered index transfer.
struct IndexCopyPlan {
    /// Immutable source table with a unique ordered hash index.
    source: &'static str,
    /// Destination table whose hash primary key rejects duplicate content entries.
    target: &'static str,
    /// Prefix for durable cursor, count and completion keys.
    checkpoint: &'static str,
    /// Progress phase reported after each durable batch.
    phase: &'static str,
}

/// Copy bounded key ranges inside SQLite, avoiding per-row Rust serialization.
fn copy_value_index(
    db: &Connection,
    plan: IndexCopyPlan,
    on_event: &mut dyn FnMut(ExtentMigrationProgress),
) -> Result<(), Box<dyn Error>> {
    let cursor_key = format!("{}_cursor", plan.checkpoint);
    let count_key = format!("{}_count", plan.checkpoint);
    let done_key = format!("{}_done", plan.checkpoint);
    if state(db, &done_key)?.is_some() {
        return Ok(());
    }
    let mut cursor = state(db, &cursor_key)?
        .map(|text| hex_bytes(&text))
        .transpose()?
        .unwrap_or_default();
    let mut count: u64 = state(db, &count_key)?
        .unwrap_or_else(|| "0".into())
        .parse()?;
    let boundary_sql = format!(
        "SELECT hash FROM {} WHERE hash>?1 ORDER BY hash LIMIT 1 OFFSET 262143",
        plan.source
    );
    let tail_sql = format!("SELECT MAX(hash) FROM {} WHERE hash>?1", plan.source);
    let copy_sql = format!(
        "INSERT INTO {} SELECT hash,offset,length FROM {} WHERE hash>?1 AND hash<=?2 ORDER BY hash",
        plan.target, plan.source
    );
    loop {
        let upper: Option<Vec<u8>> = db
            .prepare_cached(&boundary_sql)?
            .query_row([cursor.as_slice()], |row| row.get(0))
            .optional()?;
        let upper = match upper {
            Some(key) => Some(key),
            None => db
                .prepare_cached(&tail_sql)?
                .query_row([cursor.as_slice()], |row| row.get::<_, Option<Vec<u8>>>(0))?,
        };
        let Some(upper) = upper else {
            break;
        };
        let started = Instant::now();
        db.execute_batch("BEGIN IMMEDIATE")?;
        let copied = db
            .prepare_cached(&copy_sql)?
            .execute(params![cursor, upper])?;
        if copied == 0 {
            return Err("empty index copy range".into());
        }
        count += copied as u64;
        cursor = upper;
        set_state(db, &cursor_key, &to_hex(&cursor))?;
        set_state(db, &count_key, &count.to_string())?;
        db.execute_batch("COMMIT")?;
        eprintln!(
            "index-batch phase={} rows={} total_rows={} total_ms={}",
            plan.phase,
            copied,
            count,
            started.elapsed().as_millis()
        );
        on_event(ExtentMigrationProgress {
            phase: plan.phase,
            completed: count,
        });
    }
    set_state(db, &done_key, "1")?;
    Ok(())
}

/// Sort newly appended mappings once, then merge them into the preexisting global map.
fn merge_pending_values(
    db: &Connection,
    on_event: &mut dyn FnMut(ExtentMigrationProgress),
) -> Result<(), Box<dyn Error>> {
    if state(db, "pending_merge_done")?.is_some() {
        return Ok(());
    }
    on_event(ExtentMigrationProgress {
        phase: "sorting-value-index",
        completed: 0,
    });
    db.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS migration.values_pending_hash ON values_pending(hash)",
    )?;
    copy_value_index(
        db,
        IndexCopyPlan {
            source: "migration.values_pending",
            target: "migration.values_map",
            checkpoint: "pending_merge",
            phase: "merging-value-index",
        },
        on_event,
    )
}

/// Seed the permanent write-side index in hash order, retaining a restartable cursor.
fn seed_value_index(
    db: &Connection,
    on_event: &mut dyn FnMut(ExtentMigrationProgress),
) -> Result<(), Box<dyn Error>> {
    ValueExtentStore::initialize_index(db)?;
    copy_value_index(
        db,
        IndexCopyPlan {
            source: "migration.values_map",
            target: "clarity_extent_index",
            checkpoint: "index",
            phase: "indexing",
        },
        on_event,
    )
}

/// Read one source trie without exposing source write handles.
fn read_blob(blob: &SourceBlob, file: &mut File) -> Result<Vec<u8>, Box<dyn Error>> {
    if !blob.inline.is_empty() {
        return Ok(blob.inline.clone());
    }
    let size = usize::try_from(blob.length)?;
    let mut bytes = vec![0; size];
    file.seek(SeekFrom::Start(blob.offset))?;
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// Stream all persisted confirmed, unconfirmed, and mined trie records.
fn visit_blobs(
    source: &Connection,
    mut visit: impl FnMut(SourceBlob) -> Result<(), Box<dyn Error>>,
) -> Result<(), Box<dyn Error>> {
    let mut query = source.prepare(
        "SELECT block_id,data,external_offset,external_length FROM marf_data ORDER BY block_id",
    )?;
    let mut rows = query.query([])?;
    while let Some(row) = rows.next()? {
        let blob = SourceBlob {
            id: row.get(0)?,
            inline: row.get(1)?,
            offset: row.get(2)?,
            length: row.get(3)?,
        };
        if blob.id <= 0 {
            return Err("unsupported nonpositive MARF block id".into());
        }
        if !blob.inline.is_empty() || blob.length != 0 {
            visit(blob)?;
        }
    }
    let mut query = source.prepare("SELECT block_id,data FROM mined_blocks ORDER BY block_id")?;
    let mut rows = query.query([])?;
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        if id <= 0 {
            return Err("unsupported nonpositive mined block id".into());
        }
        let inline: Vec<u8> = row.get(1)?;
        if !inline.is_empty() {
            visit(SourceBlob {
                id: -id,
                inline,
                offset: 0,
                length: 0,
            })?;
        }
    }
    Ok(())
}

/// Plan bounded batches on independent workers while keeping all database access serial.
fn plan_blobs(
    source: &Connection,
    db: &Connection,
    file: &mut File,
    lookup: &ValueLookup,
    on_event: &mut dyn FnMut(ExtentMigrationProgress),
) -> Result<(), Box<dyn Error>> {
    let Some(entries) = &lookup.entries else {
        return plan_blobs_serial(source, db, file, lookup, on_event);
    };
    let workers = thread::available_parallelism()
        .map_or(1, usize::from)
        .min(PLAN_WORKERS);
    let mut batch = Vec::new();
    let mut bytes = 0usize;
    let mut count = 0u64;
    eprintln!("trie-planning workers={workers} batch_bytes={PLAN_BATCH_BYTES} batch_rows={PLAN_BATCH_ROWS}");
    visit_blobs(source, |blob| {
        let exists: bool = db
            .prepare_cached("SELECT EXISTS(SELECT 1 FROM migration.plans WHERE block_id=?1)")?
            .query_row([blob.id], |row| row.get(0))?;
        if !exists {
            let input = read_blob(&blob, file)?;
            if !batch.is_empty() && input.len() > PLAN_BATCH_BYTES.saturating_sub(bytes) {
                flush_plan_batch(db, &mut batch, entries, workers, count, on_event)?;
                bytes = 0;
            }
            bytes = bytes
                .checked_add(input.len())
                .ok_or("planning batch size overflow")?;
            batch.push(PlanInput {
                id: blob.id,
                bytes: input,
            });
        }
        count += 1;
        if bytes >= PLAN_BATCH_BYTES || batch.len() >= PLAN_BATCH_ROWS {
            flush_plan_batch(db, &mut batch, entries, workers, count, on_event)?;
            bytes = 0;
        }
        Ok(())
    })?;
    flush_plan_batch(db, &mut batch, entries, workers, count, on_event)?;
    on_event(ExtentMigrationProgress {
        phase: "planning",
        completed: count,
    });
    Ok(())
}

/// Inventory source node boundaries in a disk-backed, block-indexed relocation map.
fn plan_blobs_serial(
    source: &Connection,
    db: &Connection,
    file: &mut File,
    lookup: &ValueLookup,
    on_event: &mut dyn FnMut(ExtentMigrationProgress),
) -> Result<(), Box<dyn Error>> {
    let mut count = 0;
    db.execute_batch("BEGIN IMMEDIATE")?;
    visit_blobs(source, |blob| {
        let exists: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM migration.plans WHERE block_id=?1)",
            [blob.id],
            |row| row.get(0),
        )?;
        if !exists {
            let plan = BlobRelocation::plan_narrow(&read_blob(&blob, file)?, |hash| {
                lookup.locate(db, hash).map(|location| location.is_some())
            })?;
            let mut encoded = Vec::with_capacity(plan.offsets.len() * 16);
            for (old, new) in plan.offsets {
                encoded.extend(old.to_le_bytes());
                encoded.extend(new.to_le_bytes());
            }
            db.execute(
                "INSERT INTO migration.plans(block_id,offsets,length) VALUES (?1,?2,?3)",
                params![blob.id, encoded, plan.length],
            )?;
        }
        count += 1;
        if count % 1024 == 0 {
            db.execute_batch("COMMIT; BEGIN IMMEDIATE")?;
            on_event(ExtentMigrationProgress {
                phase: "planning",
                completed: count,
            });
        }
        Ok(())
    })?;
    db.execute_batch("COMMIT")?;
    Ok(())
}

/// Decode one bounded ancestor map; scratch corruption cannot silently redirect pointers.
fn load_plan(db: &Connection, id: i64) -> Result<BlobRelocation, Box<dyn Error>> {
    let (bytes, length): (Vec<u8>, u64) = db.query_row(
        "SELECT offsets,length FROM migration.plans WHERE block_id=?1",
        [id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if bytes.len() % 16 != 0 {
        return Err("truncated relocation vector".into());
    }
    let offsets = bytes
        .chunks_exact(16)
        .map(|part| {
            (
                u64::from_le_bytes(part[..8].try_into().expect("eight bytes")),
                u64::from_le_bytes(part[8..].try_into().expect("eight bytes")),
            )
        })
        .collect();
    Ok(BlobRelocation { offsets, length })
}

/// Publish relocated blobs only after flushing each durable batch; resume skips published rows.
fn rewrite_blobs(
    source: &Connection,
    db: &Connection,
    input: &mut File,
    output: &mut File,
    store_id: [u8; 16],
    lookup: &ValueLookup,
    plan_cache_bytes: usize,
    on_event: &mut dyn FnMut(ExtentMigrationProgress),
) -> Result<(), Box<dyn Error>> {
    let mut cache = PlanCache::new(plan_cache_bytes);
    eprintln!("trie-rewriting plan_cache_bytes={plan_cache_bytes} policy=linked-lru");
    let mut count = 0;
    db.execute_batch("BEGIN IMMEDIATE")?;
    visit_blobs(source, |blob| {
        let written: bool = db.query_row(
            "SELECT written FROM migration.plans WHERE block_id=?1",
            [blob.id],
            |row| row.get(0),
        )?;
        if !written {
            let source_bytes = read_blob(&blob, input)?;
            let plan = load_plan(db, blob.id)?;
            let rewritten = plan.rewrite(
                &source_bytes,
                |block, offset| cache.resolve(db, block, offset),
                |hash| {
                    lookup.locate(db, hash).map(|location| {
                        location.map(|(offset, length)| ValueExtent {
                            store_id,
                            offset,
                            length,
                        })
                    })
                },
            )?;
            if rewritten.get(37..69) != source_bytes.get(36..68) {
                return Err("root hash changed during relocation".into());
            }
            if blob.id > 0 && !blob.inline.is_empty() {
                db.execute("UPDATE marf_data SET data=?1,external_offset=0,external_length=0 WHERE block_id=?2", params![rewritten, blob.id])?;
            } else if blob.id > 0 {
                let offset = output.seek(SeekFrom::End(0))?;
                output.write_all(&rewritten)?;
                db.execute("UPDATE marf_data SET data=X'',external_offset=?1,external_length=?2 WHERE block_id=?3", params![offset, rewritten.len() as u64, blob.id])?;
            } else {
                db.execute(
                    "UPDATE mined_blocks SET data=?1 WHERE block_id=?2",
                    params![rewritten, -blob.id],
                )?;
            }
            db.execute(
                "UPDATE migration.plans SET written=1 WHERE block_id=?1",
                [blob.id],
            )?;
        }
        count += 1;
        if count % 256 == 0 {
            output.sync_data()?;
            db.execute_batch("COMMIT; BEGIN IMMEDIATE")?;
            on_event(ExtentMigrationProgress {
                phase: "rewriting",
                completed: count,
            });
        }
        Ok(())
    })?;
    output.sync_all()?;
    db.execute_batch("COMMIT")?;
    on_event(ExtentMigrationProgress {
        phase: "rewriting",
        completed: count,
    });
    Ok(())
}

/// Read a durable checkpoint value.
fn state(db: &Connection, key: &str) -> Result<Option<String>, rusqlite::Error> {
    db.query_row(
        "SELECT value FROM migration.state WHERE key=?1",
        [key],
        |row| row.get(0),
    )
    .optional()
}

/// Update a checkpoint in the caller's transaction.
fn set_state(db: &Connection, key: &str, value: &str) -> Result<(), rusqlite::Error> {
    db.execute(
        "INSERT OR REPLACE INTO migration.state VALUES (?1,?2)",
        params![key, value],
    )?;
    Ok(())
}

/// Identify immutable source paths, byte lengths and modification timestamps across resumes.
fn source_identity(config: &ExtentMigrationConfig) -> Result<String, Box<dyn Error>> {
    let mut identity = String::new();
    for path in [&config.source_db, &config.source_blobs] {
        let metadata = path.metadata()?;
        identity.push_str(&format!(
            "{}:{}:{}\n",
            fs::canonicalize(path)?.display(),
            metadata.len(),
            metadata.modified()?.duration_since(UNIX_EPOCH)?.as_nanos()
        ));
    }
    Ok(identity)
}

/// Convert coordinator failures to the physical rewriter's error type.
fn marf_error(error: Box<dyn Error>) -> MarfError {
    MarfError::CorruptionError(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chainstate::stacks::index::marf::{MARFOpenOpts, MarfConnection, MARF};
    use crate::chainstate::stacks::index::{ClarityMarfTrieId, TrieMerkleProof};
    use crate::clarity_vm::clarity::ClarityMarfStoreTransaction as _;
    use crate::clarity_vm::database::marf::MarfedKV;
    use clarity::vm::database::{ClarityBackingStore, SqliteConnection};
    use stacks_common::codec::StacksMessageCodec;
    use stacks_common::types::chainstate::{StacksBlockId, TrieHash};

    /// Eviction preserves recently used plans and oversized loads do not flush useful entries.
    #[test]
    fn ancestor_plan_cache_evicts_incrementally() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("ATTACH DATABASE ':memory:' AS migration; CREATE TABLE migration.plans(block_id INTEGER PRIMARY KEY,offsets BLOB,length INTEGER)").unwrap();
        for id in 1..=4 {
            let mut offsets = Vec::new();
            for i in 0..if id == 4 { 3 } else { 1 } {
                offsets.extend((36u64 + i).to_le_bytes());
                offsets.extend((72u64 + i).to_le_bytes());
            }
            db.execute(
                "INSERT INTO migration.plans VALUES(?1,?2,100)",
                params![id, offsets],
            )
            .unwrap();
        }
        let mut cache = PlanCache::new(32);
        assert_eq!(cache.resolve(&db, 1, 36).unwrap(), 72);
        assert_eq!(cache.resolve(&db, 2, 36).unwrap(), 72);
        assert_eq!(cache.resolve(&db, 1, 36).unwrap(), 72);
        assert_eq!(cache.resolve(&db, 3, 36).unwrap(), 72);
        assert!(cache.entries.contains_key(&1));
        assert!(!cache.entries.contains_key(&2));
        assert_eq!(cache.bytes, 32);
        assert_eq!(cache.oldest, Some(1));
        assert_eq!(cache.newest, Some(3));
        assert_eq!(cache.resolve(&db, 4, 38).unwrap(), 74);
        assert!(!cache.entries.contains_key(&4));
        assert_eq!(cache.bytes, 32);
        db.execute("DELETE FROM migration.plans WHERE block_id IN (1,2)", [])
            .unwrap();
        assert_eq!(cache.resolve(&db, 1, 36).unwrap(), 72);
        assert!(cache.resolve(&db, 2, 36).is_err());
        assert!(cache.resolve(&db, 1, 37).is_err());
        assert_eq!(cache.entries.len(), 2);
        assert_eq!(cache.oldest, Some(3));
        assert_eq!(cache.newest, Some(1));
    }

    /// Compare byte-identical serial/parallel plans and reject a corrupt batch before publication.
    fn verify_parallel_planning(config: &ExtentMigrationConfig) {
        let source = Connection::open(&config.source_db).unwrap();
        let mut file = File::open(&config.source_blobs).unwrap();
        let mut inputs = Vec::new();
        visit_blobs(&source, |blob| {
            inputs.push(PlanInput {
                id: blob.id,
                bytes: read_blob(&blob, &mut file)?,
            });
            Ok(())
        })
        .unwrap();
        assert!(inputs.len() > PLAN_WORKERS);
        let entries = source
            .prepare("SELECT key FROM data_table ORDER BY key")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(|row| ValueLocation {
                hash: hex_bytes(&row.unwrap()).unwrap().try_into().unwrap(),
                offset: 48,
                length: 88,
            })
            .collect::<Vec<_>>();
        let serial = inputs
            .iter()
            .map(|input| {
                encode_plan(
                    input.id,
                    BlobRelocation::plan_narrow(&input.bytes, |hash| {
                        Ok(find_value_location(&entries, hash).is_some())
                    })
                    .unwrap(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(serial, prepare_plan_batch(&inputs, &entries, 1).unwrap());
        assert_eq!(
            serial,
            prepare_plan_batch(&inputs, &entries, PLAN_WORKERS).unwrap()
        );
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("ATTACH DATABASE ':memory:' AS migration; CREATE TABLE migration.plans(block_id INTEGER PRIMARY KEY,offsets BLOB,length INTEGER,written INTEGER DEFAULT 0)").unwrap();
        inputs.last_mut().unwrap().bytes.truncate(5);
        assert!(
            flush_plan_batch(&db, &mut inputs, &entries, PLAN_WORKERS, 12, &mut |_| {}).is_err()
        );
        let count: u64 = db
            .query_row("SELECT COUNT(*) FROM migration.plans", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    /// Memory and SQLite lookups agree for exact keys, shared prefixes, bounds and misses.
    #[test]
    fn memory_value_lookup_matches_sqlite_and_budget_fallback() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("ATTACH DATABASE ':memory:' AS migration; CREATE TABLE migration.values_map(hash BLOB PRIMARY KEY,offset INTEGER,length INTEGER) WITHOUT ROWID").unwrap();
        for key in [0u8, 2, 4, 255] {
            let mut hash = [11u8; 40];
            hash[39] = key;
            db.execute(
                "INSERT INTO migration.values_map VALUES (?1,?2,?3)",
                params![hash.as_slice(), u64::from(key) + 48, 100],
            )
            .unwrap();
        }
        let memory = ValueLookup::load(&db, 4096, None, &mut |_| {}).unwrap();
        let fallback = ValueLookup::load(&db, 1, None, &mut |_| {}).unwrap();
        assert!(ValueLookup::load(&db, 4096, Some(3), &mut |_| {}).is_err());
        assert!(ValueLookup::load(&db, 4096, Some(5), &mut |_| {}).is_err());
        assert!(ValueLookup::load(&db, 4096, Some(4), &mut |_| {}).is_ok());
        assert!(memory.entries.is_some());
        assert!(fallback.entries.is_none());
        for key in 0..=255u8 {
            let mut hash = [11u8; 40];
            hash[39] = key;
            assert_eq!(
                memory.locate(&db, &MARFValue(hash)).unwrap(),
                fallback.locate(&db, &MARFValue(hash)).unwrap()
            );
        }
        let mut hit = [11u8; 40];
        hit[39] = 2;
        db.execute_batch("DROP TABLE migration.values_map").unwrap();
        assert_eq!(
            memory.locate(&db, &MARFValue(hit)).unwrap(),
            Some((50, 100))
        );
        assert_eq!(memory.locate(&db, &MARFValue([11u8; 40])).unwrap(), None);
        assert!(fallback.locate(&db, &MARFValue([11u8; 40])).is_err());
    }

    /// Key-range boundaries and durable cursors preserve every row across interrupted copies.
    #[test]
    fn ordered_index_copy_resumes_across_batch_boundary() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("ATTACH DATABASE ':memory:' AS migration; CREATE TABLE migration.state(key TEXT PRIMARY KEY,value TEXT); CREATE TABLE migration.values_pending(hash BLOB NOT NULL,offset INTEGER,length INTEGER); CREATE TABLE migration.values_map(hash BLOB PRIMARY KEY,offset INTEGER,length INTEGER) WITHOUT ROWID; BEGIN IMMEDIATE").unwrap();
        {
            let mut insert = db
                .prepare("INSERT INTO migration.values_pending VALUES(?1,?2,?3)")
                .unwrap();
            for i in 0..262_150u64 {
                let mut hash = [0u8; 40];
                hash[32..].copy_from_slice(&i.to_be_bytes());
                insert.execute(params![hash.as_slice(), i + 48, 8]).unwrap();
            }
        }
        db.execute_batch("COMMIT").unwrap();
        let interrupted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            merge_pending_values(&db, &mut |event| {
                if event.completed == 262_144 {
                    panic!("durable range stop");
                }
            })
            .unwrap();
        }));
        assert!(interrupted.is_err());
        assert_eq!(
            state(&db, "pending_merge_count").unwrap().as_deref(),
            Some("262144")
        );
        merge_pending_values(&db, &mut |_| {}).unwrap();
        let interrupted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            seed_value_index(&db, &mut |event| {
                if event.completed == 262_144 {
                    panic!("durable permanent-index stop");
                }
            })
            .unwrap();
        }));
        assert!(interrupted.is_err());
        seed_value_index(&db, &mut |_| {}).unwrap();
        let count: u64 = db
            .query_row("SELECT COUNT(*) FROM clarity_extent_index", [], |row| {
                row.get(0)
            })
            .unwrap();
        let missing: u64 = db.query_row("SELECT COUNT(*) FROM (SELECT * FROM migration.values_pending EXCEPT SELECT * FROM clarity_extent_index)",[],|row| row.get(0)).unwrap();
        assert_eq!(count, 262_150);
        assert_eq!(missing, 0);
    }

    /// Weighted eviction and access order agree with a simple reference model over mixed requests.
    #[test]
    fn ancestor_plan_cache_matches_weighted_reference() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("ATTACH DATABASE ':memory:' AS migration; CREATE TABLE migration.plans(block_id INTEGER PRIMARY KEY,offsets BLOB,length INTEGER)").unwrap();
        for key in 1..=23u32 {
            let count = key as usize % 5 + 1;
            let mut offsets = Vec::new();
            for i in 0..count as u64 {
                offsets.extend((36 + i).to_le_bytes());
                offsets.extend((72 + i).to_le_bytes());
            }
            db.execute(
                "INSERT INTO migration.plans VALUES(?1,?2,200)",
                params![key, offsets],
            )
            .unwrap();
        }
        for budget in [0, 16, 48, 112, 512, 4096] {
            let mut cache = PlanCache::new(budget);
            let mut reference = Vec::<u32>::new();
            let mut seed = 17u64;
            for _ in 0..2000 {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                let key = ((seed >> 32) % 23 + 1) as u32;
                let size = (key as usize % 5 + 1) * 16;
                if let Some(index) = reference.iter().position(|candidate| *candidate == key) {
                    reference.remove(index);
                    reference.push(key);
                } else if size <= budget {
                    while reference
                        .iter()
                        .map(|key| (*key as usize % 5 + 1) * 16)
                        .sum::<usize>()
                        + size
                        > budget
                    {
                        reference.remove(0);
                    }
                    reference.push(key);
                }
                assert_eq!(cache.resolve(&db, key, 36).unwrap(), 72);
                let mut actual = Vec::new();
                let mut next = cache.oldest;
                let mut previous = None;
                while let Some(id) = next {
                    assert!(actual.len() < cache.entries.len(), "cycle in cache links");
                    let entry = &cache.entries[&id];
                    assert_eq!(entry.older, previous);
                    actual.push(id as u32);
                    previous = Some(id);
                    next = entry.newer;
                }
                assert_eq!(actual, reference);
                assert_eq!(cache.newest, previous);
                assert_eq!(cache.entries.len(), reference.len());
                assert_eq!(
                    cache.bytes,
                    reference
                        .iter()
                        .map(|key| (*key as usize % 5 + 1) * 16)
                        .sum::<usize>()
                );
                assert!(cache.bytes <= budget);
            }
        }
    }

    /// Parallel packing preserves source order, rejects bad commitments, and leaves no partial writes.
    #[test]
    fn parallel_value_packing_is_ordered_and_checked() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("values");
        let mut store = ValueExtentStore::open(&path, true).unwrap();
        let batch = (0..1024)
            .map(|index| {
                let canonical = format!("value-{index}");
                (MARFValue::from_value(&canonical), canonical)
            })
            .collect::<Vec<_>>();
        let prepared = prepare_value_batch(&batch).unwrap();
        let located = store.append_prepared(&prepared).unwrap();
        for ((hash, extent), (expected, canonical)) in located.into_iter().zip(&batch) {
            assert_eq!(&hash, expected);
            assert_eq!(
                store.read(extent, &hash).unwrap().canonical().unwrap(),
                *canonical
            );
        }
        let length = fs::metadata(&path).unwrap().len();
        let mut invalid = batch;
        invalid[100].0 = MARFValue::from_value("wrong");
        assert!(prepare_value_batch(&invalid).is_err());
        assert_eq!(fs::metadata(&path).unwrap().len(), length);
    }

    /// Archive relocation preserves fork roots, patched backpointers and value reads without SQL.
    #[test]
    fn migrated_archive_preserves_forks_and_values() {
        for compression in [false, true] {
            let temporary = tempfile::tempdir().unwrap();
            let source_db = temporary.path().join("source.sqlite");
            let mut opts = MARFOpenOpts::default()
                .with_compression(compression)
                .with_mmap(true);
            opts.external_blobs = true;
            let mut marf =
                MARF::<StacksBlockId>::from_path(source_db.to_str().unwrap(), opts).unwrap();
            SqliteConnection::initialize_conn(marf.sqlite_conn()).unwrap();
            let mut roots = Vec::new();
            let mut parent = StacksBlockId::sentinel();
            for height in 1..=12u8 {
                let block = StacksBlockId([height; 32]);
                if height == 12 {
                    parent = StacksBlockId([4; 32]);
                }
                let mut tx = marf.begin_tx().unwrap();
                tx.begin(&parent, &block).unwrap();
                for key in 0..if height == 1 { 100 } else { 5 } {
                    let value = format!("value-{height}-{key}");
                    let hash = MARFValue::from_value(&value);
                    tx.sqlite_tx()
                        .execute(
                            "INSERT OR IGNORE INTO data_table(key,value) VALUES (?1,?2)",
                            params![hash.to_hex(), value],
                        )
                        .unwrap();
                    tx.insert_batch(&[format!("key-{key}")], &[hash]).unwrap();
                }
                tx.seal().unwrap();
                tx.commit().unwrap();
                roots.push((
                    block.clone(),
                    marf.get_root_hash_at(&block).unwrap(),
                    height,
                ));
                parent = block;
            }
            drop(marf);
            let config = ExtentMigrationConfig {
                index_cache_mib: 64,
                source_blobs: PathBuf::from(format!("{}.blobs", source_db.display())),
                source_db,
                destination: temporary.path().join("destination"),
            };
            verify_parallel_planning(&config);
            let source_before = fs::read(&config.source_db).unwrap();
            let blobs_before = fs::read(&config.source_blobs).unwrap();
            let interrupted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                migrate_value_extents(&config, &mut |event| {
                    if event.phase == "values" {
                        panic!("injected stop after durable value batch");
                    }
                })
                .unwrap();
            }));
            assert!(interrupted.is_err());
            // Emulate resuming a converter that already indexed part of the generation.
            let scratch = Connection::open(config.destination.join("migration.sqlite")).unwrap();
            scratch.execute_batch("BEGIN IMMEDIATE; INSERT INTO values_map SELECT hash,offset,length FROM values_pending WHERE rowid<=10; DELETE FROM values_pending WHERE rowid<=10; COMMIT").unwrap();
            drop(scratch);
            let partial = Connection::open(config.destination.join("marf.sqlite")).unwrap();
            assert!(binary_value_store::detect(&partial).is_err());
            drop(partial);
            let values_path = config.destination.join("marf.sqlite.values");
            let saved_path = config.destination.join("saved.values");
            fs::rename(&values_path, &saved_path).unwrap();
            assert!(migrate_value_extents(&config, &mut |_| {}).is_err());
            assert!(
                !values_path.exists(),
                "resume must not recreate missing extents"
            );
            fs::rename(&saved_path, &values_path).unwrap();
            let original_values = fs::read(&values_path).unwrap();
            let mut wrong_generation = original_values.clone();
            wrong_generation[8] ^= 1;
            fs::write(&values_path, wrong_generation).unwrap();
            assert!(migrate_value_extents(&config, &mut |_| {}).is_err());
            fs::write(&values_path, &original_values).unwrap();
            let interrupted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                migrate_value_extents(&config, &mut |event| {
                    if event.phase == "merging-value-index" {
                        panic!("injected stop after durable pending merge");
                    }
                })
                .unwrap();
            }));
            assert!(interrupted.is_err());

            let interrupted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                migrate_value_extents(&config, &mut |event| {
                    if event.phase == "indexing" {
                        panic!("injected stop after durable index batch");
                    }
                })
                .unwrap();
            }));
            assert!(interrupted.is_err());

            let interrupted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                migrate_value_extents(&config, &mut |event| {
                    if event.phase == "planning" {
                        panic!("injected stop after durable plan batch");
                    }
                })
                .unwrap();
            }));
            assert!(interrupted.is_err());

            let interrupted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                migrate_value_extents(&config, &mut |event| {
                    if event.phase == "rewriting" {
                        panic!("injected stop after durable blob batch");
                    }
                })
                .unwrap();
            }));
            assert!(interrupted.is_err());
            let blobs_path = config.destination.join("marf.sqlite.blobs");
            let original_output = fs::read(&blobs_path).unwrap();
            fs::write(&blobs_path, []).unwrap();
            assert!(migrate_value_extents(&config, &mut |_| {}).is_err());
            fs::write(&blobs_path, original_output).unwrap();
            migrate_value_extents(&config, &mut |_| {}).unwrap();
            assert_eq!(source_before, fs::read(&config.source_db).unwrap());
            assert_eq!(blobs_before, fs::read(&config.source_blobs).unwrap());
            let mut migrated =
                MarfedKV::open(config.destination.to_str().unwrap(), None, None).unwrap();
            let before_reuse = fs::metadata(&values_path).unwrap().len();
            let tip = roots.last().unwrap().0.clone();
            let new_block = StacksBlockId([200; 32]);
            {
                let mut tx = migrated.begin(&tip, &new_block);
                tx.put_all_data(vec![("another-key".into(), "value-1-99".into())])
                    .unwrap();
                tx.commit_to_processed_block(&new_block).unwrap();
            }
            assert_eq!(
                fs::metadata(&values_path).unwrap().len(),
                before_reuse,
                "migrated global index must reuse old content"
            );
            let root_blocks = roots
                .iter()
                .map(|(block, root, _)| (*root, block.clone()))
                .collect();
            for (block, root, height) in roots {
                assert_eq!(migrated.get_marf().get_root_hash_at(&block).unwrap(), root);
                let mut store = migrated.begin_read_only(Some(&block));
                assert_eq!(
                    store.get_data("key-0").unwrap(),
                    Some(format!("value-{height}-0"))
                );
                assert_eq!(
                    store.get_data("key-99").unwrap().as_deref(),
                    Some("value-1-99")
                );
                let count: u64 = store
                    .get_side_store()
                    .query_row("SELECT COUNT(*) FROM data_table", [], |row| row.get(0))
                    .unwrap();
                assert_eq!(count, 0);
                let (canonical, proof) = store.get_data_with_proof("key-0").unwrap().unwrap();
                let proof =
                    TrieMerkleProof::<StacksBlockId>::consensus_deserialize(&mut proof.as_slice())
                        .unwrap();
                assert!(proof.verify(
                    &TrieHash::from_key("key-0"),
                    &MARFValue::from_value(&canonical),
                    &root,
                    &root_blocks
                ));
            }
        }
    }
}
