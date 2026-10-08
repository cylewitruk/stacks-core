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

//! All-or-nothing direct migration with one final trie rewrite.

use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Instant;

use blockstack_lib::chainstate::stacks::index::record::NodeRecordFormat;
use blockstack_lib::chainstate::stacks::index::value_relocation::BlobRelocation;
use blockstack_lib::chainstate::stacks::index::{Error as MarfError, direct_hash_index};
use blockstack_lib::util_lib::db::sqlite_readonly_uri;
use memmap2::Mmap;
use rusqlite::{Connection, params};
use sha2::{Digest, Sha256};

use crate::ancestors::AncestorCache;
use crate::clarity::ClarityValues;
use crate::source::Source;
use crate::trie_pipeline::PreparedLeaves;
use crate::{Result, memberships, ordered_pipeline, schema, space};

/// Offline source and a new destination directory owned by this conversion.
#[derive(Clone, Debug)]
pub struct Config {
    /// Legacy SQLite MARF; external blobs, when present, are adjacent.
    pub source: PathBuf,
    /// New output directory. Existing destinations are never overwritten.
    pub destination: PathBuf,
    /// Maximum encoded input size per trie, guarding pathological allocations.
    pub max_trie_bytes: u64,
    /// Explicitly extract Clarity values into the canonical stable-ID store.
    pub clarity_values: bool,
}

/// Validated physical conversion statistics.
#[derive(Clone, Debug)]
pub struct Outcome {
    /// Final SQLite file in the published destination.
    pub database: PathBuf,
    /// Number of nonempty physical tries, including mined/unconfirmed rows.
    pub tries: u64,
    /// Total input trie bytes, independent of SQLite overhead.
    pub source_bytes: u64,
    /// Total rewritten trie bytes, including SQL-resident mined records.
    pub destination_bytes: u64,
    /// Hash of ordered identities, original parents and preserved root commitments.
    pub identity_digest: [u8; 32],
}

/// Convert legacy storage directly and publish only after final validation and synchronization.
pub fn migrate(config: &Config) -> Result<Outcome> {
    migrate_inner(config, None)
}

/// Revalidate a stopped Clarity extraction and reuse matching output bytes on a new private copy.
/// The retained directory must be offline; it is never modified. All logical source tables,
/// relocation plans and missing output bytes are regenerated, and mismatched prefixes fail closed.
pub fn migrate_reusing_clarity(config: &Config, retained: &Path) -> Result<Outcome> {
    if !config.clarity_values {
        return Err("extraction reuse is only applicable to Clarity".into());
    }
    migrate_inner(config, Some(retained))
}

/// Shared canonical conversion; reuse changes preparation only, never the codec or validation.
fn migrate_inner(config: &Config, retained: Option<&Path>) -> Result<Outcome> {
    if config.max_trie_bytes == 0 || config.destination.exists() {
        return Err("destination must be new and trie byte limit must be positive".into());
    }
    let source = Source::open(&config.source)?;
    let clarity: bool = source.db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='data_table')",
        [],
        |row| row.get(0),
    )?;
    if clarity != config.clarity_values {
        return Err("Clarity schema and explicit value-store selection disagree".into());
    }
    let parent = config
        .destination
        .parent()
        .ok_or("destination has no parent")?;
    let reserve = config
        .max_trie_bytes
        .checked_mul(2)
        .and_then(|bytes| {
            bytes.checked_add(if clarity {
                4 * 1024 * 1024 * 1024
            } else {
                64 * 1024 * 1024
            })
        })
        .ok_or("migration space reserve overflow")?;
    let mut space = space::Guard::new(parent, reserve)?;
    eprintln!(
        "canonical preflight free_bytes={} reserve_bytes={reserve}",
        space::available(parent)?
    );
    let name = config
        .destination
        .file_name()
        .ok_or("destination has no name")?
        .to_str()
        .ok_or("non-UTF8 destination")?;
    let staging = parent.join(format!(".{name}.migrating-{}", std::process::id()));
    fs::create_dir(&staging)?;
    fs::write(
        staging.join("INCOMPLETE"),
        b"Owned unpublished canonical migration. Do not open as a chainstate.\n",
    )?;
    // Failed output remains visibly incomplete for inspection; it is never resumed or selected.
    let db_name = source.path.file_name().ok_or("source has no name")?;
    let db_path = staging.join(db_name);
    if let Some(seed) = retained {
        crate::reused_values::copy(
            seed,
            &staging,
            db_name.to_str().ok_or("invalid database name")?,
        )?;
    }
    let db = Connection::open(&db_path)?;
    // The private bulk copy may encounter children before their referenced tables/indexes.
    // Check all foreign keys after loading the complete schema, before publication.
    db.execute_batch("PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF; PRAGMA foreign_keys=OFF; PRAGMA cache_size=-65536; PRAGMA temp_store=FILE; BEGIN")?;
    let secondary = if retained.is_some() {
        schema::refresh(&source.db, &db, &sqlite_readonly_uri(&source.path, true)?)?
    } else {
        let objects = schema::copy(
            &source.db,
            &db,
            &sqlite_readonly_uri(&source.path, true)?,
            clarity,
        )?;
        db.execute_batch("CREATE TABLE marf_record_format(singleton INTEGER PRIMARY KEY CHECK(singleton=1),version INTEGER NOT NULL); INSERT INTO marf_record_format VALUES(1,-6)")?;
        objects
    };
    db.execute_batch("COMMIT")?;
    let values = if retained.is_some() {
        Some(ClarityValues::reuse(
            &source.db,
            &db,
            &staging,
            db_name.to_str().ok_or("invalid database name")?,
        )?)
    } else if clarity {
        Some(ClarityValues::prepare(
            &source.db,
            &db,
            &staging,
            db_name.to_str().ok_or("non-UTF8 source name")?,
        )?)
    } else {
        None
    };
    db.execute_batch("BEGIN")?;
    let cache_path = staging.join("relocations.scratch");
    let mut ancestors = AncestorCache::create(&cache_path)?;
    let blob_path = staging.join(format!(
        "{}.blobs",
        db_name.to_str().ok_or("non-UTF8 source name")?
    ));
    let mut prefix = if retained.is_some() && fs::metadata(&blob_path)?.len() != 0 {
        // SAFETY: the copied prefix is only read while mapped; it is dropped before truncation or writes.
        Some(unsafe { Mmap::map(&File::open(&blob_path)?)? })
    } else {
        None
    };
    let file = if retained.is_some() {
        OpenOptions::new().write(true).open(&blob_path)?
    } else {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&blob_path)?
    };
    let mut output = BufWriter::with_capacity(8 * 1024 * 1024, file);
    let mut reused_bytes = 0u64;
    let mut outcome = Outcome {
        database: config.destination.join(db_name),
        tries: 0,
        source_bytes: 0,
        destination_bytes: 0,
        identity_digest: [0; 32],
    };
    let mut digest = Sha256::new();
    let mut external_end = 0u64;
    let start = Instant::now();
    let mut last = start;
    let has_external: bool = source.db.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('marf_data') WHERE name='external_offset')",
        [],
        |row| row.get(0),
    )?;
    let projection = if has_external {
        "external_offset,external_length"
    } else {
        "0,0"
    };
    let source_path = source.path.clone();
    let has_values = values.is_some();
    let lookup = values.as_ref().map(|v| &v.lookup);
    let max_trie_bytes = config.max_trie_bytes;
    let stats = ordered_pipeline::run(
        ordered_pipeline::Limits {
            workers: thread::available_parallelism()
                .map_or(1, |n| n.get())
                .min(8),
            bytes: usize::try_from(max_trie_bytes)?
                .checked_mul(8)
                .ok_or("pipeline budget overflow")?
                .max(512 * 1024 * 1024),
            jobs: 32,
        },
        move |emit| {
            let source = Source::open(&source_path).map_err(|e| e.to_string())?;
            for (sql, mined) in [
                (
                    format!(
                        "SELECT block_id,data,{projection},unconfirmed FROM marf_data ORDER BY block_id"
                    ),
                    false,
                ),
                (
                    "SELECT block_id,data,0,0,0 FROM mined_blocks ORDER BY block_id".into(),
                    true,
                ),
            ] {
                let mut query = source.db.prepare(&sql).map_err(|e| e.to_string())?;
                let mut rows = query.query([]).map_err(|e| e.to_string())?;
                while let Some(row) = rows.next().map_err(|e| e.to_string())? {
                    let id: i64 = row.get(0).map_err(|e| e.to_string())?;
                    let inline: Vec<u8> = row.get(1).map_err(|e| e.to_string())?;
                    let offset: u64 = row.get(2).map_err(|e| e.to_string())?;
                    let length: u64 = row.get(3).map_err(|e| e.to_string())?;
                    let unconfirmed: bool = row.get(4).map_err(|e| e.to_string())?;
                    if inline.is_empty() && length == 0 {
                        continue;
                    }
                    if id <= 0 || (length > 0 && !inline.is_empty()) {
                        return Err("invalid source trie location".into());
                    }
                    let bytes = if inline.is_empty() {
                        source.blob(offset, length).map_err(|e| e.to_string())?
                    } else {
                        &inline
                    };
                    if bytes.len() as u64 > max_trie_bytes {
                        return Err(format!("trie {id} exceeds configured input limit"));
                    }
                    // Input plus deduplicated leaf map, visited offsets and output are charged.
                    // One legal oversized job is rejected rather than silently exceeding this cap.
                    let charge = bytes
                        .len()
                        .checked_mul(8)
                        .ok_or("pipeline charge overflow")?;
                    emit((id, mined, unconfirmed, bytes.to_vec()), charge)?;
                }
            }
            source.verify_unchanged().map_err(|e| e.to_string())
        },
        |(id, mined, unconfirmed, bytes)| {
            let leaves = PreparedLeaves::read(&bytes, lookup)?;
            Ok((id, mined, unconfirmed, bytes, leaves))
        },
        |(id, mined, unconfirmed, owned, leaves)| {
            let mut publish = || -> Result<()> {
                space.check()?;
                let bytes = owned.as_slice();
                let resolve = |block, offset| {
                    ancestors
                        .resolve(block, offset)
                        .map_err(MarfError::CorruptionError)
                };
                let plan = BlobRelocation::plan_packed_format(
                    bytes,
                    NodeRecordFormat::Legacy,
                    NodeRecordFormat::Optimized,
                    resolve,
                    |leaf| {
                        if has_values {
                            leaves.transform(leaf)
                        } else {
                            Ok(())
                        }
                    },
                )?;
                let rewritten = plan.rewrite_format(
                    bytes,
                    NodeRecordFormat::Legacy,
                    NodeRecordFormat::Optimized,
                    resolve,
                    |leaf| {
                        if has_values {
                            leaves.transform(leaf)
                        } else {
                            Ok(())
                        }
                    },
                )?;
                let old_root = NodeRecordFormat::Legacy
                    .parse(bytes.get(36..).ok_or("missing source root")?)?;
                let new_root = NodeRecordFormat::Optimized
                    .parse(rewritten.get(36..).ok_or("missing output root")?)?;
                if bytes.get(..32) != rewritten.get(..32)
                    || old_root.hash.is_none()
                    || old_root.hash != new_root.hash
                {
                    return Err(format!("trie {id} parent/root commitment changed").into());
                }
                digest.update([u8::from(mined)]);
                digest.update(id.to_le_bytes());
                digest.update(&bytes[..32]);
                digest.update(old_root.hash.unwrap().0);
                let changed = if mined {
                    db.execute(
                        "UPDATE mined_blocks SET data=?1 WHERE block_id=?2",
                        params![rewritten, id],
                    )?
                } else if unconfirmed {
                    db.execute(
                        "UPDATE marf_data SET data=?1 WHERE block_id=?2",
                        params![rewritten, id],
                    )?
                } else {
                    let start = usize::try_from(external_end)?;
                    let end = start
                        .checked_add(rewritten.len())
                        .ok_or("prefix range overflow")?;
                    if let Some(saved) = prefix.as_ref().and_then(|map| map.get(start..end)) {
                        if saved != rewritten {
                            return Err(format!(
                                "retained trie {id} differs from regenerated canonical bytes"
                            )
                            .into());
                        }
                        output.seek(SeekFrom::Start(end as u64))?;
                        reused_bytes += rewritten.len() as u64;
                    } else {
                        // Any incomplete buffered tail is regenerated; no mapped owner spans this truncate.
                        if prefix.take().is_some() {
                            output.get_ref().set_len(external_end)?;
                        }
                        output.write_all(&rewritten)?;
                    }
                    let changed = db.execute(
                    "UPDATE marf_data SET external_offset=?1,external_length=?2 WHERE block_id=?3",
                    params![external_end, rewritten.len() as u64, id],
                )?;
                    external_end = external_end
                        .checked_add(rewritten.len() as u64)
                        .ok_or("output size overflow")?;
                    changed
                };
                if changed != 1 {
                    return Err("destination lost source identity".into());
                }
                let mut pairs = Vec::with_capacity(plan.offsets.len() * 16);
                for (old, new) in &plan.offsets {
                    pairs.extend_from_slice(&old.to_le_bytes());
                    pairs.extend_from_slice(&new.to_le_bytes());
                }
                ancestors.publish(if mined { -id } else { id }, &pairs, plan.length)?;
                outcome.tries += 1;
                outcome.source_bytes += bytes.len() as u64;
                outcome.destination_bytes += rewritten.len() as u64;
                if outcome.tries % 1024 == 0 {
                    db.execute_batch("COMMIT; BEGIN")?;
                    if last.elapsed().as_secs() >= 10 {
                        eprintln!(
                            "canonical rewriting={} input_bytes={} output_bytes={} elapsed_s={:.1}",
                            outcome.tries,
                            outcome.source_bytes,
                            outcome.destination_bytes,
                            start.elapsed().as_secs_f64()
                        );
                        last = Instant::now();
                    }
                }
                Ok(())
            };
            publish().map_err(|error| error.to_string())
        },
    )?;
    eprintln!(
        "canonical pipeline_published={} peak_charged_bytes={} peak_jobs={}",
        stats.published, stats.peak_bytes, stats.peak_jobs
    );
    drop(prefix);
    output.flush()?;
    output.get_ref().set_len(external_end)?;
    output.get_ref().sync_all()?;
    eprintln!("canonical retained_prefix_bytes_verified={reused_bytes}");
    drop(output);
    drop(ancestors);
    fs::remove_file(cache_path)?;
    source.verify_unchanged()?;
    if let Some(values) = values {
        values.finish(&db)?;
        crate::value_lookup::ValueLookup::remove_files(&staging)?;
    }
    schema::finalize(&db, secondary)?;
    schema::verify_foreign_keys(&db)?;
    NodeRecordFormat::Optimized.publish(&db)?;
    db.execute_batch("COMMIT; DETACH DATABASE original; PRAGMA optimize=0x10002")?;
    let integrity: String = db.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(format!("destination integrity: {integrity}").into());
    }
    drop(db);
    // Derived hashes bind the final canonical offsets and are built only once.
    direct_hash_index::build(&db_path)?;
    if clarity {
        memberships::build(&db_path, &staging)?;
    }
    source.verify_unchanged()?;
    outcome.identity_digest = digest.finalize().into();
    fs::write(
        staging.join(format!("{}.conversion.txt", db_name.to_string_lossy())),
        format!(
            "format=6\ntries={}\nsource_bytes={}\ndestination_bytes={}\nidentity_digest={:02x?}\n",
            outcome.tries, outcome.source_bytes, outcome.destination_bytes, outcome.identity_digest
        ),
    )?;
    fs::remove_file(staging.join("INCOMPLETE"))?;
    sync_directory(&staging)?;
    if config.destination.exists() {
        return Err("destination appeared during conversion".into());
    }
    fs::rename(&staging, &config.destination)?;
    File::open(parent)?.sync_all()?;
    Ok(outcome)
}

/// Synchronize all converter-created files before atomic directory publication.
fn sync_directory(path: &Path) -> Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            sync_directory(&entry.path())?;
        } else {
            File::open(entry.path())?.sync_all()?;
        }
    }
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod reuse_tests {
    use std::collections::BTreeMap;

    use blockstack_lib::chainstate::stacks::index::marf::{MARF, MARFOpenOpts, MarfConnection};
    use blockstack_lib::chainstate::stacks::index::{ClarityMarfTrieId, MARFValue};
    use clarity::vm::database::SqliteConnection;
    use stacks_common::types::chainstate::StacksBlockId;

    use super::*;

    /// Preserve every retained seed byte, including after a rejected restart.
    fn inventory(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut pending = vec![root.to_path_buf()];
        let mut result = BTreeMap::new();
        while let Some(path) = pending.pop() {
            for entry in fs::read_dir(path).unwrap() {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_dir() {
                    pending.push(entry.path());
                } else {
                    result.insert(entry.path(), fs::read(entry.path()).unwrap());
                }
            }
        }
        result
    }

    /// Interrupted buffered tails are regenerated; completed prefix bytes and the seed stay exact.
    #[test]
    fn reuses_extraction_and_checked_prefix_without_trusting_interrupted_offsets() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("marf.sqlite");
        let mut marf = MARF::<StacksBlockId>::from_path(
            path.to_str().unwrap(),
            MARFOpenOpts::default().with_compression(true),
        )
        .unwrap();
        SqliteConnection::initialize_conn(marf.sqlite_conn()).unwrap();
        for height in 1..=4u8 {
            let parent = if height == 1 {
                StacksBlockId::sentinel()
            } else {
                StacksBlockId([height - 1; 32])
            };
            let block = StacksBlockId([height; 32]);
            let mut tx = marf.begin_tx().unwrap();
            tx.begin(&parent, &block).unwrap();
            for n in 0..64 {
                let value = if n % 2 == 0 {
                    format!("small{height}")
                } else {
                    format!("{height}{}", "large".repeat(100))
                };
                let key = MARFValue::from_value(&value);
                tx.sqlite_tx()
                    .execute(
                        "INSERT OR IGNORE INTO data_table VALUES(?1,?2)",
                        params![key.to_hex(), value],
                    )
                    .unwrap();
                tx.insert_batch(&[format!("key{n}")], vec![key]).unwrap();
            }
            tx.commit().unwrap();
        }
        drop(marf);
        let source = Connection::open(&path).unwrap();
        source
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        let seed = directory.path().join("retained");
        fs::create_dir(&seed).unwrap();
        fs::write(seed.join("INCOMPLETE"), b"retained test attempt").unwrap();
        let db = Connection::open(seed.join("marf.sqlite")).unwrap();
        db.execute_batch(
            "PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF; PRAGMA foreign_keys=OFF; BEGIN",
        )
        .unwrap();
        schema::copy(
            &source,
            &db,
            &sqlite_readonly_uri(&path, true).unwrap(),
            true,
        )
        .unwrap();
        db.execute_batch("CREATE TABLE marf_record_format(singleton INTEGER PRIMARY KEY,version INTEGER); INSERT INTO marf_record_format VALUES(1,-6); COMMIT").unwrap();
        drop(ClarityValues::prepare(&source, &db, &seed, "marf.sqlite").unwrap());
        crate::value_lookup::ValueLookup::remove_files(&seed).unwrap();
        drop(db);
        drop(source);
        let config = Config {
            source: path.clone(),
            destination: directory.path().join("fresh"),
            max_trie_bytes: 1024 * 1024,
            clarity_values: true,
        };
        let fresh = migrate(&config).unwrap();
        let bytes = fs::read(config.destination.join("marf.sqlite.blobs")).unwrap();
        fs::write(seed.join("marf.sqlite.blobs"), &bytes[..bytes.len() / 2]).unwrap();
        let before = inventory(&seed);
        let mut reuse = config.clone();
        reuse.destination = directory.path().join("reused");
        let recovered = migrate_reusing_clarity(&reuse, &seed).unwrap();
        assert_eq!(fresh.tries, recovered.tries);
        assert_eq!(fresh.identity_digest, recovered.identity_digest);
        assert_eq!(
            bytes,
            fs::read(reuse.destination.join("marf.sqlite.blobs")).unwrap()
        );
        assert_eq!(before, inventory(&seed));
        crate::verify(&recovered.database, true).unwrap();
        // Complete but wrong prefix bytes must not be silently overwritten or accepted.
        let mut corrupt = bytes;
        corrupt[36] ^= 1;
        fs::write(seed.join("marf.sqlite.blobs"), &corrupt).unwrap();
        let before = inventory(&seed);
        reuse.destination = directory.path().join("bad-prefix");
        assert!(
            migrate_reusing_clarity(&reuse, &seed)
                .unwrap_err()
                .to_string()
                .contains("differs from regenerated")
        );
        assert_eq!(before, inventory(&seed));
        assert!(!reuse.destination.exists());
        // An extraction from a different source cannot pass merely because its SQL opens.
        let db = Connection::open(seed.join("marf.sqlite")).unwrap();
        db.execute("DELETE FROM canonical_external_values WHERE value_id=1", [])
            .unwrap();
        drop(db);
        reuse.destination = directory.path().join("missing-value");
        assert!(
            migrate_reusing_clarity(&reuse, &seed)
                .unwrap_err()
                .to_string()
                .contains("missing a source commitment")
        );
        assert!(!reuse.destination.exists());
    }
}
