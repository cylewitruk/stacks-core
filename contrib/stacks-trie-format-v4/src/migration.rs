//! Bounded planning and durable checkpoints for a physical-only rewrite selected by the codec adapter.
use std::error::Error;
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::time::{Instant, UNIX_EPOCH};

use blockstack_lib::chainstate::stacks::index::record::NodeRecordFormat;
use blockstack_lib::util_lib::db::sqlite_readonly_uri;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use sha2::{Digest, Sha256};
use stacks_common::util::hash::to_hex;

use crate::ancestor_cache::AncestorCache;
use crate::ordered_pipeline::{self, Limits};
use crate::relocation::{BlobRelocation, RelocationPlan, rewrite_plan_format};
use crate::relocation_index::RelocationIndex;
use blockstack_lib::chainstate::stacks::index::Error as MarfError;

/// Error type for offline conversion and filesystem failures.
pub type Result<T> = std::result::Result<T, Box<dyn Error>>;
/// Stable conversion binding; change whenever planning semantics change.
use crate::codec::{BINDING as CODEC, Codec, Counts, DESTINATION_FORMAT, SOURCE_FORMAT};
/// One bounded publication batch.
const BATCH: u64 = 256;

/// Immutable source paths and converter-owned scratch files.
#[derive(Clone)]
pub struct Config {
    /// Checkpointed source SQLite database.
    pub source_db: PathBuf,
    /// Source external trie file.
    pub source_blobs: PathBuf,
    /// Persistent plans and exclusive converter lock.
    pub scratch: PathBuf,
}

/// Source descriptor; negative IDs denote the separate mined-block table.
struct Blob {
    /// Database-local identity.
    id: i64,
    /// Embedded bytes when this trie is SQLite-resident.
    inline: Vec<u8>,
    /// External source offset.
    offset: u64,
    /// External source length.
    length: u64,
    /// Planned source checksum, present only while rewriting.
    digest: Option<Vec<u8>>,
}

impl Blob {
    /// Bytes requiring conversion.
    fn bytes(&self) -> u64 {
        if self.inline.is_empty() {
            self.length
        } else {
            self.inline.len() as u64
        }
    }
    /// Read external bytes with positioned I/O, retaining inline bytes without another copy.
    fn read(self, file: &File) -> Result<Vec<u8>> {
        if !self.inline.is_empty() {
            return Ok(self.inline);
        }
        let mut bytes = vec![0; usize::try_from(self.length)?];
        file.read_exact_at(&mut bytes, self.offset)?;
        Ok(bytes)
    }
}

/// Open a checkpointed source without journal creation or locking writes.
fn source(path: &Path) -> Result<Connection> {
    for suffix in ["-wal", "-journal"] {
        let journal = PathBuf::from(format!("{}{suffix}", path.display()));
        if journal.exists() && journal.metadata()?.len() != 0 {
            return Err("source must be offline and checkpointed".into());
        }
    }
    let db = Connection::open_with_flags(
        sqlite_readonly_uri(path, true)?,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?;
    db.execute_batch("PRAGMA cache_size=-16384; PRAGMA mmap_size=268435456")?;
    if NodeRecordFormat::from_database(&db)? != SOURCE_FORMAT {
        return Err("source has an incompatible physical format".into());
    }
    Ok(db)
}

/// Bind all source-generation companion files by canonical path, length and exact mtime.
fn identity(config: &Config) -> Result<String> {
    let parent = config.source_db.parent().ok_or("source has no parent")?;
    let prefix = config
        .source_db
        .file_name()
        .ok_or("source lacks filename")?
        .to_str()
        .ok_or("non-UTF8 source")?;
    let mut paths = vec![config.source_db.clone(), config.source_blobs.clone()];
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        let name = entry.file_name();
        if name
            .to_str()
            .is_some_and(|name| name.starts_with(&format!("{prefix}.")))
            && entry.file_type()?.is_file()
        {
            paths.push(entry.path());
        }
    }
    paths.sort();
    paths.dedup();
    let mut text = CODEC.to_string();
    for path in paths {
        let metadata = path.metadata()?;
        text.push_str(&format!(
            "\n{}\t{}\t{}",
            path.canonicalize()?.display(),
            metadata.len(),
            metadata.modified()?.duration_since(UNIX_EPOCH)?.as_nanos()
        ));
    }
    Ok(text)
}

/// Hold the exclusive scratch lock throughout a phase.
fn lock(config: &Config) -> Result<File> {
    fs::create_dir_all(&config.scratch)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(config.scratch.join("converter.lock"))?;
    lock.try_lock()
        .map_err(|e| format!("converter already running: {e}"))?;
    Ok(lock)
}

/// Open scratch as the same attached schema used by the immutable relocation-index exporter.
fn scratch(config: &Config, binding: &str) -> Result<Connection> {
    let db = Connection::open_in_memory()?;
    db.execute(
        "ATTACH DATABASE ?1 AS migration",
        [config
            .scratch
            .join("plans.sqlite")
            .to_str()
            .ok_or("non-UTF8 scratch")?],
    )?;
    db.execute_batch("PRAGMA migration.journal_mode=WAL; PRAGMA migration.synchronous=FULL; PRAGMA migration.cache_size=-262144;
        CREATE TABLE IF NOT EXISTS migration.state(key TEXT PRIMARY KEY,value TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS migration.plans(block_id INTEGER PRIMARY KEY,offsets BLOB NOT NULL,length INTEGER NOT NULL,source_length INTEGER NOT NULL,external INTEGER NOT NULL,source_digest BLOB NOT NULL CHECK(length(source_digest)=32));
        CREATE TABLE IF NOT EXISTS migration.packed_counts(block_id INTEGER PRIMARY KEY,branches TEXT NOT NULL,targets TEXT NOT NULL,origins TEXT NOT NULL,patches INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS migration.leaf_counts(block_id INTEGER PRIMARY KEY,extent_leaves INTEGER NOT NULL,inline_leaves INTEGER NOT NULL,inline_bytes INTEGER NOT NULL);")?;
    let old = state(&db, "source")?;
    if old.as_deref().is_some_and(|old| old != binding) {
        return Err("source/codec identity changed since planning".into());
    }
    if old.is_none() {
        set_state(&db, "source", binding)?;
    }
    Ok(db)
}

/// Read a durable phase property.
fn state(db: &Connection, key: &str) -> Result<Option<String>> {
    Ok(db
        .query_row(
            "SELECT value FROM migration.state WHERE key=?1",
            [key],
            |r| r.get(0),
        )
        .optional()?)
}

/// Write a phase property in the current transaction.
fn set_state(db: &Connection, key: &str, value: &str) -> Result<()> {
    db.execute("INSERT INTO migration.state VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![key,value])?;
    Ok(())
}

/// Stream source descriptors, skipping only durably planned rows from a snapshot of scratch.
fn visit(
    config: &Config,
    skip_planned: bool,
    mut consume: impl FnMut(Blob) -> Result<()>,
) -> Result<()> {
    let db = source(&config.source_db)?;
    db.execute(
        "ATTACH DATABASE ?1 AS migration",
        [sqlite_readonly_uri(
            &config.scratch.join("plans.sqlite"),
            false,
        )?],
    )?;
    let filter = if skip_planned {
        " AND p.block_id IS NULL"
    } else {
        ""
    };
    for (sql, mined) in [
        (
            format!(
                "SELECT m.block_id,m.data,m.external_offset,m.external_length,p.source_digest FROM marf_data m LEFT JOIN migration.plans p ON p.block_id=m.block_id WHERE (length(m.data)>0 OR m.external_length>0){filter} ORDER BY m.block_id"
            ),
            false,
        ),
        (
            format!(
                "SELECT -m.block_id,m.data,0,0,p.source_digest FROM mined_blocks m LEFT JOIN migration.plans p ON p.block_id=-m.block_id WHERE length(m.data)>0{filter} ORDER BY m.block_id"
            ),
            true,
        ),
    ] {
        let mut query = db.prepare(&sql)?;
        let mut rows = query.query([])?;
        while let Some(row) = rows.next()? {
            let blob = Blob {
                id: row.get(0)?,
                inline: row.get(1)?,
                offset: row.get(2)?,
                length: row.get(3)?,
                digest: row.get(4)?,
            };
            if blob.id == 0 || (blob.id < 0) != mined {
                return Err("invalid source block ID".into());
            }
            consume(blob)?;
        }
    }
    Ok(())
}

/// Plan every physical trie with bounded workers; resume from committed plan batches.
pub fn plan(config: &Config) -> Result<()> {
    plan_with_event(config, &mut |_| Ok(()))
}

/// Plan with durable-boundary notifications for monitoring and recovery tests.
fn plan_with_event(config: &Config, event: &mut dyn FnMut(u64) -> Result<()>) -> Result<()> {
    let _lock = lock(config)?;
    let binding = identity(config)?;
    let original = source(&config.source_db)?;
    let db = scratch(config, &binding)?;
    let expected: u64 = original.query_row("SELECT (SELECT count(*) FROM marf_data WHERE length(data)>0 OR external_length>0)+(SELECT count(*) FROM mined_blocks WHERE length(data)>0)", [], |r| r.get(0))?;
    let mut count: u64 = db.query_row("SELECT count(*) FROM migration.plans", [], |r| r.get(0))?;
    let input = File::open(&config.source_blobs)?;
    let mut ancestors = AncestorCache::rebuild(&config.scratch.join("ancestor-cache.tmp"), &db)?;
    let start = Instant::now();
    let mut last = start;
    eprintln!("planning start completed={count} total={expected}");
    db.execute_batch("BEGIN IMMEDIATE")?;
    let result = ordered_pipeline::run(
        Limits {
            workers: 8,
            bytes: 512 * 1024 * 1024,
            jobs: 32,
        },
        |emit| {
            visit(config, true, |blob| {
                let charge = blob
                    .bytes()
                    .checked_mul(8)
                    .and_then(|n| n.checked_add(4096))
                    .ok_or("planning charge overflow")?;
                emit(blob, usize::try_from(charge)?)?;
                Ok(())
            })
            .map_err(|e| e.to_string())
        },
        |blob| {
            let run = || -> Result<_> {
                let id = blob.id;
                let external = blob.inline.is_empty();
                let bytes = blob.read(&input)?;
                let digest = Sha256::digest(&bytes).to_vec();
                Ok((id, external, bytes, digest))
            };
            run().map_err(|e| e.to_string())
        },
        |(id, external, bytes, digest)| {
            let mut publish = || -> Result<()> {
                let plan = BlobRelocation::plan_packed(&bytes, SOURCE_FORMAT, |block, offset| {
                    ancestors
                        .resolve(block, offset)
                        .map_err(MarfError::CorruptionError)
                })?;
                let rewritten = plan.rewrite_format(
                    &bytes,
                    SOURCE_FORMAT,
                    DESTINATION_FORMAT,
                    |block, offset| {
                        ancestors
                            .resolve(block, offset)
                            .map_err(MarfError::CorruptionError)
                    },
                    |_| Ok(()),
                )?;
                let counts = Counts::inspect(&rewritten, &plan)?;
                let source_length = bytes.len() as u64;
                let length = plan.length;
                let mut offsets = Vec::with_capacity(plan.offsets.len() * 16);
                for (old, new) in &plan.offsets {
                    offsets.extend(old.to_le_bytes());
                    offsets.extend(new.to_le_bytes());
                }
                ancestors.publish(id, &offsets, length)?;
                db.execute(
                    "INSERT INTO migration.packed_counts VALUES(?1,?2,?3,?4,?5)",
                    params![
                        id,
                        format!("{:?}", counts.branches),
                        format!("{:?}", counts.targets),
                        format!("{:?}", counts.origins),
                        counts.patches
                    ],
                )?;
                db.execute(
                    "INSERT INTO migration.plans VALUES(?1,?2,?3,?4,?5,?6)",
                    params![id, offsets, length, source_length, external, digest],
                )?;
                db.execute(
                    "INSERT INTO migration.leaf_counts VALUES(?1,?2,?3,?4)",
                    params![id, counts.extents, counts.inlined, counts.inline_bytes],
                )?;
                count += 1;
                if count % BATCH == 0 {
                    db.execute_batch("COMMIT; BEGIN IMMEDIATE")?;
                    event(count)?;
                    if last.elapsed().as_secs() >= 10 {
                        eprintln!(
                            "planning completed={count} total={expected} elapsed_s={:.3}",
                            start.elapsed().as_secs_f64()
                        );
                        last = Instant::now();
                    }
                }
                Ok(())
            };
            publish().map_err(|e| e.to_string())
        },
    );
    let stats = match result {
        Ok(stats) => stats,
        Err(e) => {
            db.execute_batch("ROLLBACK")?;
            return Err(e.into());
        }
    };
    if count != expected || identity(config)? != binding {
        db.execute_batch("ROLLBACK")?;
        return Err("source changed or plan coverage mismatch".into());
    }
    set_state(&db, "planned", "1")?;
    db.execute_batch("COMMIT; PRAGMA migration.wal_checkpoint(TRUNCATE)")?;
    let (source_bytes,output_bytes,external_bytes,index_bytes):(u64,u64,u64,u64)=db.query_row(
        "SELECT coalesce(sum(source_length),0),coalesce(sum(length),0),coalesce(sum(CASE WHEN external THEN length ELSE 0 END),0),128+count(*)*32+coalesce(sum(length(offsets)),0) FROM migration.plans",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
    let report = format!(
        "{{\"planned\":true,\"tries\":{count},\"source_record_bytes\":{source_bytes},\"output_record_bytes\":{output_bytes},\"output_external_bytes\":{external_bytes},\"relocation_index_bytes\":{index_bytes},\"newly_planned\":{},\"elapsed_seconds\":{},\"peak_charged_bytes\":{},\"peak_jobs\":{},\"binding_sha256\":\"{}\"}}\n",
        stats.published,
        start.elapsed().as_secs_f64(),
        stats.peak_bytes,
        stats.peak_jobs,
        to_hex(&Sha256::digest(binding.as_bytes()))
    );
    let (extent_leaves, inline_leaves, inline_bytes): (u64,u64,u64) = db.query_row("SELECT coalesce(sum(extent_leaves),0),coalesce(sum(inline_leaves),0),coalesce(sum(inline_bytes),0) FROM migration.leaf_counts", [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)))?;
    let mut distributions = Vec::new();
    for column in ["branches", "targets", "origins"] {
        let mut totals = [0u64; 4];
        for (index, total) in totals.iter_mut().enumerate() {
            *total = db.query_row(&format!("SELECT coalesce(sum(json_extract({column}, '$[{index}]')),0) FROM migration.packed_counts"), [], |row| row.get(0))?;
        }
        distributions.push(format!("\"{column}\":{totals:?}"));
    }
    let patches: u64 = db.query_row(
        "SELECT coalesce(sum(patches),0) FROM migration.packed_counts",
        [],
        |row| row.get(0),
    )?;
    fs::write(
        config.scratch.join("pointer-summary.json"),
        format!(
            "{{{},\"patches\":{patches},\"record_padding_bytes\":0}}\n",
            distributions.join(",")
        ),
    )?;
    fs::write(
        config.scratch.join("leaf-summary.json"),
        format!(
            "{{\"extent_leaves\":{extent_leaves},\"inline_leaves\":{inline_leaves},\"inline_payload_bytes\":{inline_bytes}}}\n"
        ),
    )?;
    fs::write(config.scratch.join("plan-summary.json"), &report)?;
    println!("{report}");
    Ok(())
}

/// Copy an immutable source file, using APFS cloning on macOS.
fn clone_file(from: &Path, to: &Path) -> Result<()> {
    if to.exists() {
        return Err(format!("refusing to replace {}", to.display()).into());
    }
    #[cfg(target_os = "macos")]
    {
        if !std::process::Command::new("cp")
            .arg("-c")
            .arg(from)
            .arg(to)
            .status()?
            .success()
        {
            return Err("APFS source clone failed".into());
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        // Reflink-aware Linux hosts can retain large immutable sidecars cheaply.
        // A failed copy may leave a partial file, which must not be reused.
        let cloned = std::process::Command::new("cp")
            .arg("--reflink=auto")
            .arg(from)
            .arg(to)
            .status()
            .is_ok_and(|status| status.success());
        if !cloned {
            let _ = fs::remove_file(to);
            fs::copy(from, to)?;
        }
    }
    Ok(())
}

/// Create an owned database and sidecar clone with a fail-closed format marker.
fn destination(config: &Config, directory: &Path, binding: &str) -> Result<Connection> {
    let name = config
        .source_db
        .file_name()
        .ok_or("source lacks filename")?;
    let path = directory.join(name);
    if !directory.exists() {
        fs::create_dir(directory)?;
        fs::write(directory.join("converter-owner"), binding)?;
        let prefix = name.to_str().ok_or("non-UTF8 filename")?;
        for entry in fs::read_dir(config.source_db.parent().ok_or("source parent")?)? {
            let entry = entry?;
            let file_name = entry.file_name();
            let Some(text) = file_name.to_str() else {
                continue;
            };
            if (text == prefix || text.starts_with(&format!("{prefix}.")))
                && entry.file_type()?.is_file()
                && entry.path() != config.source_blobs
            {
                clone_file(&entry.path(), &directory.join(file_name))?;
            }
        }
        let db = Connection::open(&path)?;
        db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; BEGIN IMMEDIATE")?;
        let inherited: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='format_migration_state')",
            [], |row| row.get(0),
        )?;
        if inherited && destination_state(&db, "published")?.as_deref() != Some("1") {
            return Err("source contains an unfinished prior format migration".into());
        }
        db.execute_batch(
            "DROP TABLE IF EXISTS format_migration_state;
            DROP TABLE IF EXISTS format_migration_triggers;
            CREATE TABLE format_migration_state(key TEXT PRIMARY KEY,value TEXT NOT NULL);
            CREATE TABLE format_migration_triggers(name TEXT PRIMARY KEY,sql TEXT NOT NULL);",
        )?;
        db.execute(
            "INSERT INTO format_migration_state VALUES('source',?1)",
            [binding],
        )?;
        let triggers: Vec<(String, String)> = {
            let mut q=db.prepare("SELECT name,sql FROM sqlite_master WHERE type='trigger' AND name IN ('direct_hash_update','direct_hash_delete','direct_hash_insert','direct_hash_live_update','direct_hash_live_delete','direct_hash_live_insert')")?;
            let rows = q
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<std::result::Result<_, _>>()?;
            rows
        };
        for (name, sql) in triggers {
            db.execute(
                "INSERT INTO format_migration_triggers VALUES(?1,?2)",
                params![name, sql],
            )?;
            db.execute_batch(&format!("DROP TRIGGER {name}"))?;
        }
        db.execute_batch("UPDATE marf_record_format SET version=-3; INSERT INTO format_migration_state VALUES('completed','0'),('end','0'); COMMIT")?;
    }
    if fs::read_to_string(directory.join("converter-owner"))? != binding {
        return Err("destination belongs to a different conversion".into());
    }
    let db = Connection::open(path)?;
    db.execute_batch(
        "PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; PRAGMA cache_size=-262144",
    )?;
    let prior: String = db.query_row(
        "SELECT value FROM format_migration_state WHERE key='source'",
        [],
        |r| r.get(0),
    )?;
    if prior != binding {
        return Err("destination source binding differs".into());
    }
    Ok(db)
}

/// Read destination publication state.
fn destination_state(db: &Connection, key: &str) -> Result<Option<String>> {
    Ok(db
        .query_row(
            "SELECT value FROM format_migration_state WHERE key=?1",
            [key],
            |r| r.get(0),
        )
        .optional()?)
}

/// Write destination state atomically with its relocated SQLite rows.
fn set_destination_state(db: &Connection, key: &str, value: impl ToString) -> Result<()> {
    db.execute("INSERT INTO format_migration_state VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",params![key,value.to_string()])?;
    Ok(())
}

/// Flush external data before publishing referencing SQLite rows and their resume boundary.
fn checkpoint(db: &Connection, output: &File, count: u64) -> Result<()> {
    output.sync_data()?;
    set_destination_state(db, "completed", count)?;
    set_destination_state(db, "end", output.metadata()?.len())?;
    db.execute_batch("COMMIT; BEGIN IMMEDIATE")?;
    Ok(())
}

/// Rewrite an owned clone, preserving all source files and reusing immutable value/index sidecars.
pub fn rewrite(config: &Config, directory: &Path) -> Result<()> {
    rewrite_with_event(config, directory, &mut |_| Ok(()))
}

/// Convert with durable-boundary notifications for interruption and recovery tests.
fn rewrite_with_event(
    config: &Config,
    directory: &Path,
    event: &mut dyn FnMut(u64) -> Result<()>,
) -> Result<()> {
    let _lock = lock(config)?;
    let binding = identity(config)?;
    let codec = Codec::open(&config.source_db)?;
    let _source = source(&config.source_db)?;
    let plans = scratch(config, &binding)?;
    if state(&plans, "planned")?.as_deref() != Some("1") {
        return Err("complete planning before rewriting".into());
    }
    let expected: u64 =
        plans.query_row("SELECT count(*) FROM migration.plans", [], |r| r.get(0))?;
    let digest: [u8; 32] = Sha256::digest(binding.as_bytes()).into();
    let index_path = config.scratch.join("relocation.index");
    eprintln!("validating-relocation-index tries={expected}");
    let index = if index_path.exists() {
        RelocationIndex::open(&index_path, &digest)?
    } else {
        RelocationIndex::build(&plans, &index_path, &digest)?
    };
    let db = destination(config, directory, &binding)?;
    if destination_state(&db, "published")?.as_deref() == Some("1") {
        return Err("conversion already published".into());
    }
    let filename = config
        .source_db
        .file_name()
        .ok_or("source filename")?
        .to_str()
        .ok_or("non-UTF8 name")?;
    let output_path = directory.join(format!("{filename}.blobs.building"));
    let published_path = directory.join(format!("{filename}.blobs"));
    if destination_state(&db, "rewritten")?.as_deref() != Some("1") {
        let mut output = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&output_path)?;
        let end: u64 = destination_state(&db, "end")?
            .ok_or("missing durable end")?
            .parse()?;
        if output.metadata()?.len() < end {
            return Err("destination shorter than durable boundary".into());
        }
        output.set_len(end)?;
        output.seek(SeekFrom::End(0))?;
        let input = File::open(&config.source_blobs)?;
        let done: u64 = destination_state(&db, "completed")?
            .ok_or("missing durable count")?
            .parse()?;
        let mut count = done;
        let start = Instant::now();
        let mut last = start;
        eprintln!("rewriting start completed={done} total={expected}");
        db.execute_batch("BEGIN IMMEDIATE")?;
        let result = ordered_pipeline::run(
            Limits {
                workers: 8,
                bytes: 512 * 1024 * 1024,
                jobs: 32,
            },
            |emit| {
                let mut ordinal = 0;
                visit(config, false, |blob| {
                    ordinal += 1;
                    if ordinal <= done {
                        return Ok(());
                    }
                    let plan = index.plan(blob.id)?;
                    let charge = blob
                        .bytes()
                        .checked_add(plan.length())
                        .and_then(|n| n.checked_add(65536))
                        .ok_or("rewrite charge overflow")?;
                    emit(blob, usize::try_from(charge)?)?;
                    Ok(())
                })
                .map_err(|e| e.to_string())
            },
            |blob| {
                let run = || -> Result<_> {
                    let id = blob.id;
                    let inline = !blob.inline.is_empty();
                    let expected = blob.digest.clone().ok_or("missing source checksum")?;
                    let bytes = blob.read(&input)?;
                    if Sha256::digest(&bytes)[..] != expected[..] {
                        return Err("source trie changed since planning".into());
                    }
                    let plan = index.plan(id)?;
                    let rewritten = rewrite_plan_format(
                        &plan,
                        &bytes,
                        SOURCE_FORMAT,
                        DESTINATION_FORMAT,
                        |block, old| {
                            index
                                .plan(i64::from(block))
                                .map_err(MarfError::CorruptionError)?
                                .resolve(old)
                        },
                        |leaf| codec.transform(leaf, &mut Counts::default()),
                    )?;
                    if bytes.get(..32) != rewritten.get(..32)
                        || bytes.get(37..69) != rewritten.get(37..69)
                    {
                        return Err("trie parent or root commitment changed".into());
                    }
                    Ok((id, inline, rewritten))
                };
                run().map_err(|e| e.to_string())
            },
            |(id, inline, bytes)| {
                let mut publish = || -> Result<()> {
                    let changed = if id < 0 {
                        db.execute(
                            "UPDATE mined_blocks SET data=?1 WHERE block_id=?2",
                            params![bytes, -id],
                        )?
                    } else if inline {
                        db.execute("UPDATE marf_data SET data=?1,external_offset=0,external_length=0 WHERE block_id=?2",params![bytes,id])?
                    } else {
                        let offset = output.stream_position()?;
                        output.write_all(&bytes)?;
                        db.execute("UPDATE marf_data SET data=X'',external_offset=?1,external_length=?2 WHERE block_id=?3",params![offset,bytes.len() as u64,id])?
                    };
                    if changed != 1 {
                        return Err("destination block identity missing or duplicated".into());
                    }
                    count += 1;
                    if count % BATCH == 0 {
                        checkpoint(&db, &output, count)?;
                        event(count)?;
                        if last.elapsed().as_secs() >= 10 {
                            eprintln!(
                                "rewriting completed={count} total={expected} bytes={} elapsed_s={:.3}",
                                output.metadata()?.len(),
                                start.elapsed().as_secs_f64()
                            );
                            last = Instant::now();
                        }
                    }
                    Ok(())
                };
                publish().map_err(|e| e.to_string())
            },
        );
        match result {
            Ok(_) => {}
            Err(e) => {
                db.execute_batch("ROLLBACK")?;
                return Err(e.into());
            }
        }
        if count != expected || identity(config)? != binding {
            db.execute_batch("ROLLBACK")?;
            return Err("coverage or source identity changed".into());
        }
        checkpoint(&db, &output, count)?;
        set_destination_state(&db, "rewritten", "1")?;
        db.execute_batch("COMMIT")?;
        eprintln!(
            "rewriting complete tries={count} bytes={} elapsed_s={:.3}",
            output.metadata()?.len(),
            start.elapsed().as_secs_f64()
        );
    }
    let end: u64 = destination_state(&db, "end")?
        .ok_or("missing end")?
        .parse()?;
    if output_path.exists() {
        if published_path.exists() {
            return Err("published blob unexpectedly already exists".into());
        }
        fs::rename(&output_path, &published_path)?;
        File::open(directory)?.sync_all()?;
    }
    if published_path.metadata()?.len() != end || identity(config)? != binding {
        return Err("final file/source verification failed".into());
    }
    event(expected + 1)?;
    db.execute_batch("BEGIN IMMEDIATE")?;
    let triggers: Vec<String> = {
        let mut q = db.prepare("SELECT sql FROM format_migration_triggers ORDER BY name")?;
        let rows = q
            .query_map([], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        rows
    };
    for sql in triggers {
        db.execute_batch(&sql)?;
    }
    DESTINATION_FORMAT.publish(&db)?;
    set_destination_state(&db, "published", "1")?;
    db.execute_batch("COMMIT")?;
    File::open(directory)?.sync_all()?;
    eprintln!("published tries={expected} external_bytes={end}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use blockstack_lib::chainstate::stacks::index::Error as MarfError;
    use blockstack_lib::chainstate::stacks::index::inline_value::InlineValue;
    use blockstack_lib::chainstate::stacks::index::marf::{MARF, MARFOpenOpts, MarfConnection};
    use blockstack_lib::chainstate::stacks::index::{
        ClarityMarfTrieId, MARFValue, TrieLeaf, ValueExtent, ValueExtentResolver, direct_hash_index,
    };
    use blockstack_lib::clarity_vm::database::value_extents::{
        InlineValueRecord, ValueExtentStore,
    };
    use clarity::vm::database::DataStoreValue;
    use stacks_common::types::chainstate::StacksBlockId;
    use std::io::Write;
    use std::sync::Arc;

    /// Deterministic resolver for testing extent identity without value repacking.
    struct Resolver;
    impl ValueExtentResolver for Resolver {
        fn inline_commitment(
            &self,
            value: &InlineValue,
        ) -> std::result::Result<MARFValue, MarfError> {
            InlineValueRecord::from_inline(value)
                .commitment()
                .map_err(|e| MarfError::CorruptionError(e.to_string()))
        }
        fn commitment(&self, _: ValueExtent) -> std::result::Result<MARFValue, MarfError> {
            Ok(MARFValue::from_value("extent"))
        }
    }

    /// Stable block identity independent of local database IDs.
    fn block(height: u32) -> StacksBlockId {
        let mut bytes = [0; 32];
        bytes[..4].copy_from_slice(&height.to_le_bytes());
        StacksBlockId(bytes)
    }

    /// Create forks, raw width modes, extents and patched backpointers.
    fn fixture(directory: &Path, external: bool, blocks: u32) -> Config {
        let source_dir = directory.join("source");
        fs::create_dir(&source_dir).unwrap();
        let source_db = source_dir.join("marf.sqlite");
        let mut values =
            ValueExtentStore::open(&source_dir.join("marf.sqlite.values"), true).unwrap();
        let extent_location = values
            .append(&[DataStoreValue::Canonical("extent".into())])
            .unwrap()[0]
            .1;
        drop(values);
        let mut opts = MARFOpenOpts::default()
            .with_mmap(true)
            .with_compression(true);
        opts.external_blobs = external;
        let mut marf = MARF::<StacksBlockId>::from_path(source_db.to_str().unwrap(), opts).unwrap();
        SOURCE_FORMAT.publish(marf.sqlite_conn()).unwrap();
        marf.set_record_format(SOURCE_FORMAT);
        marf.set_value_extent_resolver(Arc::new(Resolver));
        for height in 1..=blocks {
            let parent = if height == 1 {
                StacksBlockId::sentinel()
            } else {
                block(if height % 17 == 0 {
                    height - 7
                } else {
                    height - 1
                })
            };
            let mut tx = marf.begin_tx().unwrap();
            tx.begin(&parent, &block(height)).unwrap();
            let mut leaves = Vec::new();
            let mut keys = Vec::new();
            for n in 0..8usize {
                let mut raw = [0; 40];
                let width = [0, 4, 32, 40][n % 4];
                raw[..width].fill((height as u8).wrapping_add(n as u8));
                leaves.push(TrieLeaf::from_value(&[], MARFValue(raw)));
                keys.push(format!("key-{n}"));
            }
            let mut extent = TrieLeaf::from_value(&[], MARFValue::from_value("extent"));
            extent.extent = Some(extent_location);
            leaves.push(extent);
            keys.push("extent".into());
            tx.insert_leaf_batch(&keys, leaves).unwrap();
            tx.commit().unwrap();
        }
        drop(marf);
        if external {
            direct_hash_index::build(&source_db).unwrap();
        }
        let source_blobs = PathBuf::from(format!("{}.blobs", source_db.display()));
        if !source_blobs.exists() {
            File::create(&source_blobs).unwrap();
        }
        Config {
            source_db,
            source_blobs,
            scratch: directory.join("scratch"),
        }
    }

    /// Read every historical root and serialized proof through the normal mmap reader.
    fn observations(path: &Path, external: bool, blocks: u32) -> Vec<String> {
        let mut opts = MARFOpenOpts::default()
            .with_mmap(true)
            .with_compression(true);
        opts.external_blobs = external;
        let mut marf = MARF::<StacksBlockId>::from_path(path.to_str().unwrap(), opts).unwrap();
        marf.set_value_extent_resolver(Arc::new(Resolver));
        let mut result = Vec::new();
        for height in 1..=blocks {
            result.push(marf.get_root_hash_at(&block(height)).unwrap().to_string());
            for key in ["key-0", "key-1", "key-2", "key-3", "extent"] {
                let (value, proof) = marf.get_with_proof(&block(height), key).unwrap().unwrap();
                result.push(format!("{}:{}", value.to_hex(), proof.to_hex()));
            }
        }
        result
    }

    /// Conversion preserves all fork roots/proofs and the direct index across resume boundaries.
    #[test]
    fn external_resume_and_crash_tail_preserve_history() {
        let directory = tempfile::tempdir().unwrap();
        let config = fixture(directory.path(), true, 270);
        Connection::open(&config.source_db).unwrap().execute_batch(
            "CREATE TABLE format_migration_state(key TEXT PRIMARY KEY,value TEXT NOT NULL);
             INSERT INTO format_migration_state VALUES('source','previous-generation'),('published','1'),('completed','270'),('end','123');
             CREATE TABLE format_migration_triggers(name TEXT PRIMARY KEY,sql TEXT NOT NULL);
             INSERT INTO format_migration_triggers VALUES('obsolete','invalid old trigger SQL');",
        ).unwrap();
        let expected = observations(&config.source_db, true, 270);
        let before = identity(&config).unwrap();
        assert!(
            plan_with_event(&config, &mut |count| if count == 256 {
                Err("injected plan interruption".into())
            } else {
                Ok(())
            })
            .is_err()
        );
        plan(&config).unwrap();
        let dest = directory.path().join("converted");
        assert!(
            rewrite_with_event(&config, &dest, &mut |count| if count == 256 {
                Err("injected rewrite interruption".into())
            } else {
                Ok(())
            })
            .is_err()
        );
        let db = Connection::open(dest.join("marf.sqlite")).unwrap();
        assert!(NodeRecordFormat::from_database(&db).is_err());
        assert_eq!(
            destination_state(&db, "completed").unwrap().as_deref(),
            Some("256")
        );
        drop(db);
        OpenOptions::new()
            .append(true)
            .open(dest.join("marf.sqlite.blobs.building"))
            .unwrap()
            .write_all(b"unpublished tail")
            .unwrap();
        rewrite(&config, &dest).unwrap();
        assert_eq!(observations(&dest.join("marf.sqlite"), true, 270), expected);
        assert_eq!(identity(&config).unwrap(), before);
        let db = Connection::open(dest.join("marf.sqlite")).unwrap();
        assert!(
            db.execute(
                "UPDATE marf_data SET external_offset=0 WHERE block_id=1",
                []
            )
            .is_err()
        );
        assert_eq!(
            NodeRecordFormat::from_database(&db).unwrap(),
            DESTINATION_FORMAT
        );
        assert!(rewrite(&config, &dest).is_err());
    }

    /// A new conversion must not discard an unfinished predecessor's publication state.
    #[test]
    fn rejects_unpublished_predecessor_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let config = fixture(directory.path(), true, 3);
        Connection::open(&config.source_db)
            .unwrap()
            .execute_batch(
                "CREATE TABLE format_migration_state(key TEXT PRIMARY KEY,value TEXT NOT NULL);
             INSERT INTO format_migration_state VALUES('source','unfinished');",
            )
            .unwrap();
        let before = identity(&config).unwrap();
        let error = destination(&config, &directory.path().join("converted"), &before)
            .unwrap_err()
            .to_string();
        assert!(error.contains("unfinished prior format migration"));
        assert_eq!(identity(&config).unwrap(), before);
    }

    /// SQL-resident records use the same relocation and proof-preserving codec.
    #[test]
    fn sqlite_resident_rewrite_preserves_history() {
        let directory = tempfile::tempdir().unwrap();
        let config = fixture(directory.path(), false, 24);
        let expected = observations(&config.source_db, false, 24);
        plan(&config).unwrap();
        let dest = directory.path().join("converted");
        rewrite(&config, &dest).unwrap();
        assert_eq!(observations(&dest.join("marf.sqlite"), false, 24), expected);
        assert_eq!(
            fs::metadata(dest.join("marf.sqlite.blobs")).unwrap().len(),
            0
        );
    }

    /// Unconfirmed and mined records are covered even though they are outside the durable sidecar prefix.
    #[test]
    fn unconfirmed_and_mined_records_are_converted() {
        let directory = tempfile::tempdir().unwrap();
        let config = fixture(directory.path(), true, 24);
        let mut opts = MARFOpenOpts::default()
            .with_mmap(true)
            .with_compression(true);
        opts.external_blobs = true;
        let mut marf =
            MARF::<StacksBlockId>::from_path(config.source_db.to_str().unwrap(), opts.clone())
                .unwrap();
        marf.set_value_extent_resolver(Arc::new(Resolver));
        let mut tx = marf.begin_tx().unwrap();
        tx.begin(&block(24), &block(999)).unwrap();
        tx.insert_batch(&["mined".into()], vec![MARFValue::from(123u32)])
            .unwrap();
        tx.commit_mined(&block(999)).unwrap();
        drop(marf);
        let mut marf = MARF::<StacksBlockId>::from_path_unconfirmed(
            config.source_db.to_str().unwrap(),
            opts.clone(),
        )
        .unwrap();
        marf.set_value_extent_resolver(Arc::new(Resolver));
        let mut tx = marf.begin_tx().unwrap();
        let tip = tx.begin_unconfirmed(&block(24)).unwrap();
        tx.insert_batch(&["pending".into()], vec![MARFValue::from(456u32)])
            .unwrap();
        tx.commit().unwrap();
        let expected = marf.get_with_proof(&tip, "pending").unwrap().unwrap();
        drop(marf);
        plan(&config).unwrap();
        let dest = directory.path().join("converted");
        rewrite(&config, &dest).unwrap();
        let db = Connection::open(dest.join("marf.sqlite")).unwrap();
        let source = source(&config.source_db).unwrap();
        let old: Vec<u8> = source
            .query_row("SELECT data FROM mined_blocks", [], |r| r.get(0))
            .unwrap();
        let new: Vec<u8> = db
            .query_row("SELECT data FROM mined_blocks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(&old[..32], &new[..32]);
        assert_eq!(&old[37..69], &new[37..69]);
        DESTINATION_FORMAT.validate_trie_header(&new).unwrap();
        let mut reopened = MARF::<StacksBlockId>::from_path_unconfirmed(
            dest.join("marf.sqlite").to_str().unwrap(),
            opts,
        )
        .unwrap();
        reopened.set_value_extent_resolver(Arc::new(Resolver));
        let actual = reopened.get_with_proof(&tip, "pending").unwrap().unwrap();
        assert_eq!(actual.0, expected.0);
        assert_eq!(actual.1.to_hex(), expected.1.to_hex());
        let mut tx = reopened.begin_tx().unwrap();
        assert_eq!(tx.begin_unconfirmed(&block(24)).unwrap(), tip);
        tx.insert_batch(&["pending-2".into()], vec![MARFValue::from(789u32)])
            .unwrap();
        tx.commit().unwrap();
        assert_eq!(
            reopened.get(&tip, "pending").unwrap(),
            Some(MARFValue::from(456u32))
        );
    }

    /// A crash after file rename still leaves readers closed until metadata and guards publish.
    #[test]
    fn publication_resumes_after_file_rename() {
        let directory = tempfile::tempdir().unwrap();
        let config = fixture(directory.path(), true, 3);
        let expected = observations(&config.source_db, true, 3);
        plan(&config).unwrap();
        let dest = directory.path().join("converted");
        assert!(
            rewrite_with_event(&config, &dest, &mut |_| Err(
                "injected pre-publication stop".into()
            ))
            .is_err()
        );
        assert!(dest.join("marf.sqlite.blobs").exists());
        assert!(!dest.join("marf.sqlite.blobs.building").exists());
        let db = Connection::open(dest.join("marf.sqlite")).unwrap();
        assert!(NodeRecordFormat::from_database(&db).is_err());
        drop(db);
        rewrite(&config, &dest).unwrap();
        assert_eq!(observations(&dest.join("marf.sqlite"), true, 3), expected);
    }

    /// A changed source or concurrent coordinator cannot reuse an existing plan.
    #[test]
    fn binding_and_lock_guard_plans() {
        let directory = tempfile::tempdir().unwrap();
        let config = fixture(directory.path(), true, 2);
        plan(&config).unwrap();
        let held = lock(&config).unwrap();
        assert!(plan(&config).is_err());
        drop(held);
        OpenOptions::new()
            .append(true)
            .open(&config.source_blobs)
            .unwrap()
            .write_all(b"mutation")
            .unwrap();
        assert!(plan(&config).is_err());
    }
}
