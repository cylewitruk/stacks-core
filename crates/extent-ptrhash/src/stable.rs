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

//! Immutable commitment-to-stable-ID candidates for a canonical Clarity generation.
//! A four-byte fingerprint is only a filter: callers must compare the full stored commitment.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::thread;

use epserde::prelude::{Deserialize, Serialize};
use memmap2::{Mmap, MmapOptions};
use ptr_hash::PtrHashParams;
use rusqlite::{params, Connection};
use serde::{Deserialize as SerdeDeserialize, Serialize as SerdeSerialize};
use stable_value_format::files::GenerationFile;
use stable_value_format::{FileHeader, FileKind, FILE_HEADER_BYTES, VALUE_ROW_BYTES};

use super::{
    digest, fingerprint, invalid, write_new, Function, Result, ShardInfo, MAX_PARTITION_KEYS,
    PARTITIONS,
};

/// Stable slots carry only a u32 ID and the independent four-byte fingerprint.
const SLOT_BYTES: usize = 8;
/// A sorted scratch pair contains a complete commitment and its stable ID.
pub const SORTED_PAIR_BYTES: usize = 44;

/// A completed, portable generation manifest.
#[derive(SerdeSerialize, SerdeDeserialize)]
struct Manifest {
    /// Distinct from the physical-location PtrHash format.
    version: u32,
    /// Identity of the owning stable-value generation.
    store_id: Vec<u8>,
    /// Number of complete value-directory rows at the locked build snapshot.
    row_count: u64,
    /// Number of historical indexed commitments.
    count: u64,
    /// One bounded shard per first commitment byte.
    shards: Vec<ShardInfo>,
}

/// One demand-paged immutable shard.
struct Shard {
    /// Native, checksum-bound perfect hash constructed on this host.
    function: Function,
    /// Eight-byte ID/fingerprint slots.
    slots: Mmap,
}

/// Immutable historical candidate index, shared across related MARF opens.
pub struct Base {
    manifest: Manifest,
    shards: Vec<Option<Shard>>,
}

impl Base {
    /// Number of exact source memberships represented by this immutable base.
    pub fn count(&self) -> u64 {
        self.manifest.count
    }
    /// Open only an explicitly registered, generation-matched index and delta.
    pub fn registered(
        db: &Connection,
        db_path: &Path,
        store_id: &[u8; 16],
        row_count: u64,
    ) -> Result<Option<Arc<Self>>> {
        let exists: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='clarity_stable_ptrhash_base' AND type='table')",
            [],
            |row| row.get(0),
        )?;
        if !exists {
            return Ok(None);
        }
        let (name, expected): (String, String) = db.query_row(
            "SELECT path,manifest_sha256 FROM clarity_stable_ptrhash_base WHERE singleton=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let component = Path::new(&name);
        if component.components().count() != 1
            || !matches!(component.components().next(), Some(Component::Normal(_)))
        {
            return Err(invalid("stable PtrHash must be a sibling directory"));
        }
        let parent = fs::canonicalize(db_path)?
            .parent()
            .ok_or_else(|| invalid("stable PtrHash database has no parent"))?
            .to_path_buf();
        let path = parent.join(component);
        type Registry = HashMap<(PathBuf, String), Weak<Base>>;
        static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
        let mut registry = REGISTRY
            .get_or_init(Mutex::default)
            .lock()
            .map_err(|_| invalid("stable PtrHash registry poisoned"))?;
        registry.retain(|_, value| value.strong_count() != 0);
        let key = (path, expected);
        let base = if let Some(base) = registry.get(&key).and_then(Weak::upgrade) {
            base
        } else {
            let base = Arc::new(Self::load(&key.0, &key.1)?);
            registry.insert(key, Arc::downgrade(&base));
            base
        };
        if base.manifest.store_id != store_id || base.manifest.row_count > row_count {
            return Err(invalid(
                "stable PtrHash belongs to a different or truncated generation",
            ));
        }
        db.prepare("SELECT value_id FROM clarity_stable_value_delta WHERE hash=?1")?;
        Ok(Some(base))
    }

    /// Return an unverified ID candidate; absence includes fingerprint mismatch.
    pub fn candidate(&self, key: &[u8; 40]) -> Result<Option<u32>> {
        let Some(shard) = &self.shards[key[0] as usize] else {
            return Ok(None);
        };
        let slot = shard.function.index(key);
        let bytes = shard
            .slots
            .get(slot * SLOT_BYTES..(slot + 1) * SLOT_BYTES)
            .ok_or_else(|| invalid("stable PtrHash slot out of bounds"))?;
        if bytes[4..8] != fingerprint(key) {
            return Ok(None);
        }
        let id = u32::from_le_bytes(bytes[..4].try_into().expect("fixed ID slot"));
        if id == 0 || u64::from(id) > self.manifest.row_count {
            return Err(invalid("stable PtrHash ID outside its snapshot"));
        }
        Ok(Some(id))
    }

    /// Load one manifest and its immutable, locally constructed shard files.
    fn load(path: &Path, expected: &str) -> Result<Self> {
        let bytes = fs::read(path.join("manifest.json"))?;
        if digest(&bytes) != expected {
            return Err(invalid("stable PtrHash manifest digest mismatch"));
        }
        let manifest: Manifest = serde_json::from_slice(&bytes)?;
        if manifest.version != 1
            || manifest.shards.len() != PARTITIONS
            || manifest.store_id.len() != 16
        {
            return Err(invalid("unsupported stable PtrHash manifest"));
        }
        let mut shards = Vec::with_capacity(PARTITIONS);
        for (number, info) in manifest.shards.iter().enumerate() {
            if info.count == 0 {
                shards.push(None);
                continue;
            }
            if info.count > MAX_PARTITION_KEYS {
                return Err(invalid("stable PtrHash shard exceeds bound"));
            }
            let bytes = fs::read(path.join(format!("{number:02x}.mphf")))?;
            if digest(&bytes) != info.function_sha256 {
                return Err(invalid("stable PtrHash function digest mismatch"));
            }
            // SAFETY: checksum-verified native serialization built with the pinned implementation.
            let function = unsafe { Function::deserialize_full(&mut bytes.as_slice())? };
            if function.max_index() != info.count {
                return Err(invalid("stable PtrHash key count mismatch"));
            }
            let file = File::open(path.join(format!("{number:02x}.ids")))?;
            if file.metadata()?.len() != (info.count * SLOT_BYTES) as u64 {
                return Err(invalid("stable PtrHash slot length mismatch"));
            }
            // SAFETY: published generation files are immutable for all readers' lifetimes.
            let slots = unsafe { MmapOptions::new().map(&file)? };
            shards.push(Some(Shard { function, slots }));
        }
        Ok(Self { manifest, shards })
    }
}

/// Build and activate directly from sorted, fixed-width hash/ID pairs.
///
/// This is an offline, all-or-nothing path. The caller owns and discards the
/// entire destination on failure; the input database is never opened writable.
pub fn build_and_activate_from_pairs(
    db_path: &Path,
    output: &Path,
    pairs_path: &Path,
    prefix_counts: &[u64; PARTITIONS],
) -> Result<()> {
    let expected_count = prefix_counts
        .iter()
        .try_fold(0u64, |sum, count| sum.checked_add(*count))
        .ok_or_else(|| invalid("sorted membership count overflow"))?;
    let db_parent = fs::canonicalize(db_path)?
        .parent()
        .ok_or_else(|| invalid("stable database has no parent"))?
        .to_path_buf();
    if fs::canonicalize(output.parent().unwrap_or(Path::new(".")))? != db_parent {
        return Err(invalid("stable PtrHash output must be a database sibling"));
    }
    let partial = output.with_extension("building");
    if output.exists() || partial.exists() {
        return Err(invalid("refusing to replace stable PtrHash generation"));
    }
    let mut db = Connection::open(db_path)?;
    db.execute_batch("PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF; PRAGMA cache_size=-65536")?;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let existing: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='clarity_stable_ptrhash_base')",
        [],
        |row| row.get(0),
    )?;
    if existing {
        return Err(invalid("stable PtrHash base already active"));
    }
    let (name, id): (String, Vec<u8>) = tx.query_row(
        "SELECT path,store_id FROM clarity_stable_format WHERE singleton=1 AND version=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let store_id: [u8; 16] = id
        .try_into()
        .map_err(|_| invalid("invalid stable store ID"))?;
    if !matches!(
        Path::new(&name).components().next(),
        Some(Component::Normal(_))
    ) || Path::new(&name).components().count() != 1
    {
        return Err(invalid("stable generation must be a sibling directory"));
    }
    let generation = db_parent.join(name);
    let directory = GenerationFile::open(
        &generation.join("value-directory.dat"),
        FileHeader {
            kind: FileKind::ValueDirectory,
            number: 0,
            store_id,
        },
        false,
    )?;
    let body = directory.len() - FILE_HEADER_BYTES as u64;
    if body % VALUE_ROW_BYTES as u64 != 0 {
        return Err(invalid("stable value directory has a partial row"));
    }
    let row_count = body / VALUE_ROW_BYTES as u64;
    let file = File::open(pairs_path)?;
    if file.metadata()?.len()
        != expected_count
            .checked_mul(SORTED_PAIR_BYTES as u64)
            .ok_or_else(|| invalid("sorted membership stream length overflow"))?
    {
        return Err(invalid("sorted membership stream length mismatch"));
    }
    // SAFETY: the converter owns this immutable scratch file until every shard worker exits.
    let pairs = unsafe { MmapOptions::new().map(&file)? };
    let mut ranges = [(0u64, 0u64); PARTITIONS];
    let mut start = 0u64;
    for (range, count) in ranges.iter_mut().zip(prefix_counts) {
        if *count > MAX_PARTITION_KEYS as u64 {
            return Err(invalid("stable PtrHash shard exceeds construction bound"));
        }
        *range = (start, *count);
        start += count;
    }
    fs::create_dir(&partial)?;
    let next = AtomicUsize::new(0);
    let results = Mutex::new(
        (0..PARTITIONS)
            .map(|_| None)
            .collect::<Vec<Option<ShardInfo>>>(),
    );
    let threads = thread::available_parallelism().map_or(1, |count| count.get());
    thread::scope(|scope| -> Result<()> {
        let mut workers = Vec::with_capacity(threads);
        for _ in 0..threads {
            let partial = &partial;
            let ranges = &ranges;
            let next = &next;
            let results = &results;
            let pairs = &pairs;
            workers.push(scope.spawn(move || -> Result<()> {
                loop {
                    let number = next.fetch_add(1, Ordering::Relaxed);
                    if number >= PARTITIONS {
                        break;
                    }
                    let (start, count) = ranges[number];
                    if count as usize > MAX_PARTITION_KEYS {
                        return Err(invalid("stable PtrHash shard exceeds construction bound"));
                    }
                    let byte_start = usize::try_from(start)?
                        .checked_mul(SORTED_PAIR_BYTES)
                        .ok_or_else(|| invalid("membership shard offset overflow"))?;
                    let byte_end = usize::try_from(
                        start
                            .checked_add(count)
                            .ok_or_else(|| invalid("membership shard range overflow"))?,
                    )?
                    .checked_mul(SORTED_PAIR_BYTES)
                    .ok_or_else(|| invalid("membership shard end overflow"))?;
                    let data = pairs
                        .get(byte_start..byte_end)
                        .ok_or_else(|| invalid("membership shard outside scratch mapping"))?;
                    let mut rows = Vec::with_capacity(count as usize);
                    let mut previous = None;
                    for record in data.chunks_exact(SORTED_PAIR_BYTES) {
                        if record[0] as usize != number {
                            return Err(invalid("membership stream prefix differs from manifest"));
                        }
                        let key: [u8; 40] = record[..40].try_into().expect("fixed scratch key");
                        if previous.is_some_and(|prior| key <= prior) {
                            return Err(invalid("membership stream is not strictly sorted"));
                        }
                        previous = Some(key);
                        let id =
                            u32::from_le_bytes(record[40..].try_into().expect("fixed scratch ID"));
                        if id == 0 || u64::from(id) > row_count {
                            return Err(invalid("membership stream ID outside directory"));
                        }
                        rows.push((key, id));
                    }
                    let info = build_shard(partial, number, &rows, false)?;
                    results
                        .lock()
                        .map_err(|_| invalid("shard result lock poisoned"))?[number] = Some(info);
                }
                Ok(())
            }));
        }
        for worker in workers {
            worker
                .join()
                .map_err(|_| invalid("stable PtrHash worker panicked"))??;
        }
        Ok(())
    })?;
    let shards = results
        .into_inner()
        .map_err(|_| invalid("shard result lock poisoned"))?
        .into_iter()
        .map(|info| info.ok_or_else(|| invalid("missing stable PtrHash shard")))
        .collect::<Result<Vec<_>>>()?;
    let manifest = Manifest {
        version: 1,
        store_id: store_id.to_vec(),
        row_count,
        count: expected_count,
        shards,
    };
    let bytes = serde_json::to_vec_pretty(&manifest)?;
    let mut manifest_file = File::options()
        .write(true)
        .create_new(true)
        .open(partial.join("manifest.json"))?;
    manifest_file.write_all(&bytes)?;
    drop(manifest_file);
    fs::rename(&partial, output)?;
    let name = output
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid("non-UTF8 stable PtrHash directory"))?;
    tx.execute_batch(
        "CREATE TABLE clarity_stable_value_delta(\
             hash BLOB PRIMARY KEY CHECK(length(hash)=40),\
             value_id INTEGER NOT NULL UNIQUE CHECK(value_id>0)) WITHOUT ROWID;\
         CREATE TABLE clarity_stable_ptrhash_base(\
             singleton INTEGER PRIMARY KEY CHECK(singleton=1),\
             path TEXT NOT NULL,manifest_sha256 TEXT NOT NULL);\
         CREATE TABLE clarity_stable_value_high(\
             singleton INTEGER PRIMARY KEY CHECK(singleton=1),\
             value_id INTEGER NOT NULL CHECK(value_id>0))",
    )?;
    tx.execute(
        "INSERT INTO clarity_stable_ptrhash_base VALUES(1,?1,?2)",
        params![name, digest(&bytes)],
    )?;
    tx.execute(
        "INSERT INTO clarity_stable_value_high VALUES(1,?1)",
        [i64::try_from(row_count)?],
    )?;
    tx.commit()?;
    Ok(())
}

/// Construct one bounded shard and verify its serialized function before publication.
fn build_shard(
    path: &Path,
    number: usize,
    rows: &[([u8; 40], u32)],
    durable: bool,
) -> Result<ShardInfo> {
    if rows.is_empty() {
        return Ok(ShardInfo {
            count: 0,
            function_sha256: String::new(),
            slots_sha256: String::new(),
        });
    }
    let keys: Vec<_> = rows.iter().map(|row| row.0).collect();
    let function = Function::try_new(&keys, PtrHashParams::default())
        .ok_or_else(|| invalid("stable PtrHash construction failed"))?;
    let mut slots = vec![0u8; rows.len() * SLOT_BYTES];
    for (key, id) in rows {
        let index = function.index(key) * SLOT_BYTES;
        if slots[index..index + 4] != [0; 4] {
            return Err(invalid("stable PtrHash collision during construction"));
        }
        slots[index..index + 4].copy_from_slice(&id.to_le_bytes());
        slots[index + 4..index + 8].copy_from_slice(&fingerprint(key));
    }
    let function_path = path.join(format!("{number:02x}.mphf"));
    let mut file = BufWriter::new(
        File::options()
            .write(true)
            .create_new(true)
            .open(&function_path)?,
    );
    // SAFETY: the function was constructed by the pinned PtrHash implementation.
    unsafe { function.serialize(&mut file)? };
    file.flush()?;
    if durable {
        file.get_ref().sync_all()?;
    }
    let bytes = fs::read(&function_path)?;
    // SAFETY: this is the just-created, checksum-verified function serialization.
    let loaded = unsafe { Function::deserialize_full(&mut bytes.as_slice())? };
    for (key, id) in rows {
        let index = loaded.index(key) * SLOT_BYTES;
        if slots[index..index + 4] != id.to_le_bytes()
            || slots[index + 4..index + 8] != fingerprint(key)
        {
            return Err(invalid("stable PtrHash serialized lookup mismatch"));
        }
    }
    if durable {
        write_new(&path.join(format!("{number:02x}.ids")), &slots)?;
    } else {
        let mut output = File::options()
            .write(true)
            .create_new(true)
            .open(path.join(format!("{number:02x}.ids")))?;
        output.write_all(&slots)?;
    }
    Ok(ShardInfo {
        count: rows.len(),
        function_sha256: digest(&bytes),
        slots_sha256: digest(&slots),
    })
}

/// Build a historical base under a locked SQLite snapshot and activate its delta.
pub fn build_and_activate(db_path: &Path, output: &Path) -> Result<()> {
    let db_parent = fs::canonicalize(db_path)?
        .parent()
        .ok_or_else(|| invalid("stable database has no parent"))?
        .to_path_buf();
    if fs::canonicalize(output.parent().unwrap_or(Path::new(".")))? != db_parent {
        return Err(invalid("stable PtrHash output must be a database sibling"));
    }
    if output.exists() || output.with_extension("building").exists() {
        return Err(invalid("refusing to replace stable PtrHash generation"));
    }
    let mut db = Connection::open(db_path)?;
    db.execute_batch("PRAGMA cache_size=-8192; PRAGMA mmap_size=0; PRAGMA busy_timeout=0")?;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let already: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='clarity_stable_ptrhash_base')",
        [],
        |row| row.get(0),
    )?;
    if already {
        return Err(invalid("stable PtrHash base already active"));
    }
    let (name, id): (String, Vec<u8>) = tx.query_row(
        "SELECT path,store_id FROM clarity_stable_format WHERE singleton=1 AND version=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let store_id: [u8; 16] = id
        .try_into()
        .map_err(|_| invalid("invalid stable store ID"))?;
    let component = Path::new(&name);
    if component.components().count() != 1
        || !matches!(component.components().next(), Some(Component::Normal(_)))
    {
        return Err(invalid("stable generation must be a sibling directory"));
    }
    let generation = db_parent.join(component);
    let directory = GenerationFile::open(
        &generation.join("value-directory.dat"),
        FileHeader {
            kind: FileKind::ValueDirectory,
            number: 0,
            store_id,
        },
        false,
    )?;
    let body = directory.len() - FILE_HEADER_BYTES as u64;
    if body % VALUE_ROW_BYTES as u64 != 0 {
        return Err(invalid("stable value directory has a partial row"));
    }
    let row_count = body / VALUE_ROW_BYTES as u64;
    let partial = output.with_extension("building");
    fs::create_dir(&partial)?;
    let mut statement =
        tx.prepare("SELECT hash,value_id FROM clarity_stable_value_index ORDER BY hash")?;
    let mut query = statement.query([])?;
    let mut next = query.next()?;
    let mut manifest = Manifest {
        version: 1,
        store_id: store_id.to_vec(),
        row_count,
        count: 0,
        shards: Vec::new(),
    };
    for number in 0..PARTITIONS {
        let mut rows = Vec::new();
        while let Some(row) = next {
            let hash: Vec<u8> = row.get(0)?;
            let key: [u8; 40] = hash
                .try_into()
                .map_err(|_| invalid("invalid commitment length"))?;
            if key[0] as usize != number {
                break;
            }
            let id: u32 = row.get(1)?;
            if id == 0 || u64::from(id) > row_count {
                return Err(invalid("stable index ID outside its source directory"));
            }
            if rows.len() >= MAX_PARTITION_KEYS {
                return Err(invalid("stable PtrHash shard exceeds construction bound"));
            }
            rows.push((key, id));
            next = query.next()?;
        }
        let info = build_shard(&partial, number, &rows, true)?;
        manifest.count += info.count as u64;
        manifest.shards.push(info);
    }
    if next.is_some() {
        return Err(invalid("unconsumed stable index keys"));
    }
    drop(query);
    drop(statement);
    let bytes = serde_json::to_vec_pretty(&manifest)?;
    write_new(&partial.join("manifest.json"), &bytes)?;
    File::open(&partial)?.sync_all()?;
    fs::rename(&partial, output)?;
    File::open(&db_parent)?.sync_all()?;
    let name = output
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid("non-UTF8 stable PtrHash directory"))?;
    tx.execute_batch(
        "CREATE TABLE clarity_stable_value_delta(\
             hash BLOB PRIMARY KEY CHECK(length(hash)=40),\
             value_id INTEGER NOT NULL UNIQUE CHECK(value_id>0)) WITHOUT ROWID;\
         CREATE TABLE clarity_stable_ptrhash_base(\
             singleton INTEGER PRIMARY KEY CHECK(singleton=1),\
             path TEXT NOT NULL,manifest_sha256 TEXT NOT NULL)",
    )?;
    tx.execute(
        "INSERT INTO clarity_stable_ptrhash_base VALUES(1,?1,?2)",
        params![name, digest(&bytes)],
    )?;
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use stable_value_format::files::GenerationPaths;

    /// Create the minimal registered V5 directory needed by the streaming base builder.
    fn pair_fixture() -> (tempfile::TempDir, std::path::PathBuf, [[u8; 40]; 2]) {
        let root = tempfile::tempdir().unwrap();
        let db_path = root.path().join("marf.sqlite");
        let db = Connection::open(&db_path).unwrap();
        db.execute_batch(
            "CREATE TABLE clarity_stable_format(singleton INTEGER,version INTEGER,path TEXT,store_id BLOB)",
        )
        .unwrap();
        let store_id = [7u8; 16];
        db.execute(
            "INSERT INTO clarity_stable_format VALUES(1,1,'marf.sqlite.stable-v1',?1)",
            [store_id.as_slice()],
        )
        .unwrap();
        let paths = GenerationPaths {
            root: root.path().join("marf.sqlite.stable-v1"),
        };
        fs::create_dir(&paths.root).unwrap();
        let mut directory = GenerationFile::create(
            &paths.value_directory(),
            FileHeader {
                kind: FileKind::ValueDirectory,
                number: 0,
                store_id,
            },
        )
        .unwrap();
        let mut partition = GenerationFile::create(
            &paths.value_partition(0),
            FileHeader {
                kind: FileKind::ValueData,
                number: 0,
                store_id,
            },
        )
        .unwrap();
        let mut first = [0u8; 40];
        first[0..2].copy_from_slice(&[1, 1]);
        let mut second = first;
        second[1] = 2;
        for (index, key) in [first, second].iter().enumerate() {
            let row = partition.append_value(*key, &[1]).unwrap();
            directory
                .append_value_row(
                    stable_value_format::ValueId::new(index as u32 + 1).unwrap(),
                    row,
                )
                .unwrap();
        }
        (root, db_path, [first, second])
    }

    /// The retained pair stream builds a base without a populated SQLite reverse index.
    #[test]
    fn sorted_pairs_build_and_reject_reordered_memberships() {
        let (root, db_path, keys) = pair_fixture();
        let mut counts = [0u64; PARTITIONS];
        counts[1] = 2;
        let pairs_path = root.path().join("pairs.bin");
        let pairs: Vec<u8> = keys
            .iter()
            .enumerate()
            .flat_map(|(index, key)| {
                key.iter()
                    .copied()
                    .chain((index as u32 + 1).to_le_bytes())
                    .collect::<Vec<_>>()
            })
            .collect();
        fs::write(&pairs_path, &pairs).unwrap();
        build_and_activate_from_pairs(&db_path, &root.path().join("native"), &pairs_path, &counts)
            .unwrap();
        let db = Connection::open(&db_path).unwrap();
        let base = Base::registered(&db, &db_path, &[7; 16], 2)
            .unwrap()
            .unwrap();
        assert_eq!(base.candidate(&keys[0]).unwrap(), Some(1));
        assert_eq!(base.candidate(&keys[1]).unwrap(), Some(2));

        let (bad_root, bad_db, _) = pair_fixture();
        let bad_pairs = bad_root.path().join("reordered.bin");
        let reversed: Vec<u8> = pairs
            .chunks_exact(SORTED_PAIR_BYTES)
            .rev()
            .flatten()
            .copied()
            .collect();
        fs::write(&bad_pairs, reversed).unwrap();
        assert!(build_and_activate_from_pairs(
            &bad_db,
            &bad_root.path().join("native"),
            &bad_pairs,
            &counts,
        )
        .is_err());
    }

    /// Historical IDs survive build, activation, reload and generation checks.
    #[test]
    fn stable_snapshot_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("original");
        fs::create_dir(&original).unwrap();
        let db_path = original.join("marf.sqlite");
        let db = Connection::open(&db_path).unwrap();
        db.execute_batch(
            "CREATE TABLE clarity_stable_format(singleton INTEGER,version INTEGER,path TEXT,store_id BLOB);\
             CREATE TABLE clarity_stable_value_index(hash BLOB PRIMARY KEY,value_id INTEGER UNIQUE) WITHOUT ROWID",
        ).unwrap();
        let root = original.join("marf.sqlite.stable-v1");
        fs::create_dir(&root).unwrap();
        let paths = GenerationPaths { root };
        let store_id = [7u8; 16];
        db.execute(
            "INSERT INTO clarity_stable_format VALUES(1,1,'marf.sqlite.stable-v1',?1)",
            [store_id.as_slice()],
        )
        .unwrap();
        let mut directory = GenerationFile::create(
            &paths.value_directory(),
            FileHeader {
                kind: FileKind::ValueDirectory,
                number: 0,
                store_id,
            },
        )
        .unwrap();
        let mut partition = GenerationFile::create(
            &paths.value_partition(0),
            FileHeader {
                kind: FileKind::ValueData,
                number: 0,
                store_id,
            },
        )
        .unwrap();
        let mut entries = Vec::new();
        for number in 0u64..300 {
            let mut key = [0; 40];
            key[..32].copy_from_slice(&Sha256::digest(number.to_le_bytes()));
            let id = stable_value_format::ValueId::new(number as u32 + 1).unwrap();
            let row = partition.append_value(key, &[1]).unwrap();
            directory.append_value_row(id, row).unwrap();
            db.execute(
                "INSERT INTO clarity_stable_value_index VALUES(?1,?2)",
                params![key.as_slice(), id.get()],
            )
            .unwrap();
            entries.push((key, id.get()));
        }
        let base_path = original.join("base");
        build_and_activate(&db_path, &base_path).unwrap();
        let base = Base::registered(&db, &db_path, &store_id, 300)
            .unwrap()
            .unwrap();
        let second = Base::registered(&db, &db_path, &store_id, 300)
            .unwrap()
            .unwrap();
        assert!(Arc::ptr_eq(&base, &second));
        for (key, id) in &entries {
            assert_eq!(base.candidate(key).unwrap(), Some(*id));
        }
        assert!(Base::registered(&db, &db_path, &[8; 16], 300).is_err());
        assert!(Base::registered(&db, &db_path, &store_id, 299).is_err());
        assert!(build_and_activate(&db_path, &original.join("other")).is_err());

        drop(base);
        drop(second);
        drop(partition);
        drop(directory);
        drop(db);
        let moved = dir.path().join("relocated");
        fs::rename(original, &moved).unwrap();
        let moved_db_path = moved.join("marf.sqlite");
        let moved_db = Connection::open(&moved_db_path).unwrap();
        let moved_base = Base::registered(&moved_db, &moved_db_path, &store_id, 300)
            .unwrap()
            .unwrap();
        for (key, id) in entries {
            assert_eq!(moved_base.candidate(&key).unwrap(), Some(id));
        }
    }
}
