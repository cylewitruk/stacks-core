//! Immutable partitioned PtrHash lookup for the historical extent deduplication index.
//! Files are trusted, locally constructed artifacts; publication is offline and transactional.

use std::fmt;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::collections::HashMap;

use epserde::prelude::{Deserialize, Serialize};
use memmap2::{Mmap, MmapOptions};
use ptr_hash::{bucket_fn::Linear, hash::Xxh3_128, PtrHash, PtrHashParams};
use rusqlite::{params, Connection};
use serde::{Deserialize as SerdeDeserialize, Serialize as SerdeSerialize};
use sha2::{Digest, Sha256};

#[cfg(feature = "diagnostics")]
pub mod diagnostics;

/// All commitment bytes participate in hashing; candidate verification remains mandatory.
type Function = PtrHash<[u8; 40], Linear, Vec<u32>, Xxh3_128, Vec<u8>>;
/// Small fixed-width disk record, independent of Rust struct alignment.
const SLOT_BYTES: usize = 16;
/// Limit construction to one byte-prefix partition at a time.
const PARTITIONS: usize = 256;
/// Reject pathological input partitions rather than exceeding the construction budget.
const MAX_PARTITION_KEYS: usize = 2_000_000;
/// Fallible prototype operations preserve a useful underlying diagnostic.
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Physical record candidate. A matching fingerprint does not prove membership.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Location {
    /// Absolute record offset in the matching immutable value generation.
    pub offset: u64,
    /// Complete stored record length.
    pub length: u64,
}

/// One completed shard's portable metadata.
#[derive(SerdeSerialize, SerdeDeserialize)]
struct ShardInfo {
    /// Number of unique full commitments.
    count: usize,
    /// Digest of the native PtrHash serialization.
    function_sha256: String,
    /// Digest of the fixed-width little-endian location array.
    slots_sha256: String,
}

/// Atomic publication manifest, bound to the writer database's activation row.
#[derive(SerdeSerialize, SerdeDeserialize)]
struct Manifest {
    /// Prototype format version; native function format is additionally pinned by the crate.
    version: u32,
    /// Value-file generation identity.
    store_id: Vec<u8>,
    /// File length at the locked SQLite snapshot used for construction.
    watermark: u64,
    /// Total historical keys.
    count: u64,
    /// One descriptor per digest-byte partition.
    shards: Vec<ShardInfo>,
}

/// Query state for a nonempty partition.
struct Shard {
    /// Resident compact perfect-hash function.
    function: Function,
    /// Demand-paged immutable location array.
    slots: Mmap,
}

/// Historical immutable base shared across related and overlapping opens.
pub struct Base {
    /// Snapshot metadata.
    manifest: Manifest,
    /// Empty digest partitions do not allocate query state.
    shards: Vec<Option<Shard>>,
}

impl fmt::Debug for Base {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PtrHashBase").field("keys", &self.manifest.count).finish()
    }
}

/// Encode a SHA-256 as lowercase hex without adding another dependency.
fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// Convert an invariant failure into a storage error.
fn invalid(message: &str) -> Box<dyn std::error::Error + Send + Sync> {
    io::Error::new(io::ErrorKind::InvalidData, message).into()
}

/// Read the independently selected fingerprint bytes; never use it as an identity.
fn fingerprint(key: &[u8; 40]) -> [u8; 4] {
    key[28..32].try_into().unwrap()
}

impl Base {
    /// Load only an explicitly activated, generation-matched immutable snapshot.
    pub fn registered(db: &Connection, db_path: &Path, store_id: &[u8; 16], file_length: u64) -> Result<Option<Arc<Self>>> {
        let exists: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='clarity_ptrhash_base' AND type='table')", [], |r| r.get(0))?;
        if !exists { return Ok(None); }
        let (path, expected): (String, String) = db.query_row("SELECT path,manifest_sha256 FROM clarity_ptrhash_base WHERE singleton=1", [], |r| Ok((r.get(0)?, r.get(1)?)))?;
        type Registry = HashMap<(PathBuf, String), Weak<Base>>;
        static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
        let registered_path = PathBuf::from(path);
        let resolved_path = if registered_path.is_absolute() {
            registered_path
        } else {
            if registered_path.components().count() != 1
                || !matches!(registered_path.components().next(), Some(Component::Normal(_)))
            {
                return Err(invalid("PtrHash relative path must be a sibling directory name"));
            }
            fs::canonicalize(db_path)?
                .parent()
                .ok_or_else(|| invalid("PtrHash database has no parent directory"))?
                .join(registered_path)
        };
        let key = (resolved_path, expected);
        let mut registry = REGISTRY.get_or_init(Mutex::default).lock().map_err(|_| invalid("PtrHash registry poisoned"))?;
        registry.retain(|_, v| v.strong_count() != 0);
        let base = if let Some(base) = registry.get(&key).and_then(Weak::upgrade) { base } else {
            let base = Arc::new(Self::load(&key.0, &key.1)?);
            registry.insert(key, Arc::downgrade(&base));
            base
        };
        if base.manifest.store_id != store_id || base.manifest.watermark > file_length {
            return Err(invalid("PtrHash base belongs to a different or truncated extent generation"));
        }
        // The delta is created in the same activation transaction as the marker.
        db.prepare("SELECT offset,length FROM clarity_extent_delta WHERE hash=?1")?;
        Ok(Some(base))
    }

    /// Load a published manifest and its immutable locally constructed function files.
    fn load(path: &Path, expected: &str) -> Result<Self> {
        let bytes = fs::read(path.join("manifest.json"))?;
        if digest(&bytes) != expected { return Err(invalid("PtrHash manifest digest mismatch")); }
        let manifest: Manifest = serde_json::from_slice(&bytes)?;
        if manifest.version != 1 || manifest.shards.len() != PARTITIONS || manifest.store_id.len() != 16 {
            return Err(invalid("unsupported PtrHash manifest"));
        }
        let mut shards = Vec::with_capacity(PARTITIONS);
        for (i, info) in manifest.shards.iter().enumerate() {
            if info.count == 0 { shards.push(None); continue; }
            if info.count > MAX_PARTITION_KEYS { return Err(invalid("PtrHash partition exceeds bound")); }
            let bytes = fs::read(path.join(format!("{i:02x}.mphf")))?;
            if digest(&bytes) != info.function_sha256 { return Err(invalid("PtrHash function digest mismatch")); }
            // SAFETY: this is the checksum-verified native serialization of a locally built function,
            // bound by the SQLite activation record. Arbitrary imported index artifacts are unsupported.
            let function = unsafe { Function::deserialize_full(&mut bytes.as_slice())? };
            if function.max_index() != info.count { return Err(invalid("PtrHash key count mismatch")); }
            let file = File::open(path.join(format!("{i:02x}.slots")))?;
            if file.metadata()?.len() != (info.count * SLOT_BYTES) as u64 { return Err(invalid("PtrHash slot length mismatch")); }
            // SAFETY: published generation files are immutable for all readers' lifetimes.
            let slots = unsafe { MmapOptions::new().map(&file)? };
            shards.push(Some(Shard { function, slots }));
        }
        let base = Self { manifest, shards };
        #[cfg(feature = "diagnostics")]
        base.report_residency("open");
        Ok(base)
    }

    /// Emit aggregate mapping residency without reading the slot data.
    #[cfg(feature = "diagnostics")]
    pub fn report_residency(&self, phase: &str) {
        let (mut total,mut resident,mut errors)=(0usize,0usize,0usize);
        for shard in self.shards.iter().flatten() {
            match diagnostics::residency(&shard.slots) {
                Ok((pages, present)) => {total+=pages;resident+=present;},
                Err(_) => errors+=1,
            }
        }
        eprintln!("PTRHASH_RESIDENCY {}",serde_json::json!({"phase":phase,"slot_pages":total,"resident_pages":resident,"page_bytes":diagnostics::page_size(),"errors":errors,"function_bytes_estimate_at_3_bits_per_key":self.manifest.shards.iter().map(|s|s.count).sum::<usize>()*3/8}));
    }

    /// Locate a candidate; the caller must compare the full digest in its extent header.
    pub fn candidate(&self, key: &[u8; 40]) -> Result<Option<Location>> {
        let Some(shard) = &self.shards[key[0] as usize] else { return Ok(None); };
        #[cfg(feature = "diagnostics")]
        let _hash = stacks_profiler::diagnostic_span!("PtrHash: Function");
        let slot = shard.function.index(key);
        #[cfg(feature = "diagnostics")]
        drop(_hash);
        #[cfg(feature = "diagnostics")]
        diagnostics::slot(key[0] as usize, slot * SLOT_BYTES);
        #[cfg(feature = "diagnostics")]
        let _slot = stacks_profiler::diagnostic_span!("PtrHash: Slot access");
        let b = shard.slots.get(slot * SLOT_BYTES..(slot + 1) * SLOT_BYTES).ok_or_else(|| invalid("PtrHash slot out of bounds"))?;
        if b[12..16] != fingerprint(key) { return Ok(None); }
        let location = Location { offset: u64::from_le_bytes(b[..8].try_into().unwrap()), length: u32::from_le_bytes(b[8..12].try_into().unwrap()) as u64 };
        if location.offset < 48 || location.length < 88 || location.length > 32 * 1024 * 1024 || location.offset.checked_add(location.length).is_none_or(|end| end > self.manifest.watermark) {
            return Err(invalid("PtrHash extent outside its snapshot"));
        }
        Ok(Some(location))
    }
}

/// Write and sync a new immutable file; never replace an existing generation file.
fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = File::options().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?; file.sync_all()?; Ok(())
}

/// Build one bounded partition and verify every input maps to its original location.
fn build_shard(path: &Path, number: usize, rows: &[([u8; 40], Location)]) -> Result<ShardInfo> {
    if rows.is_empty() { return Ok(ShardInfo { count: 0, function_sha256: String::new(), slots_sha256: String::new() }); }
    let keys: Vec<_> = rows.iter().map(|r| r.0).collect();
    let function = Function::try_new(&keys, PtrHashParams::default()).ok_or_else(|| invalid("PtrHash construction failed; no generation published"))?;
    let mut slots = vec![0u8; rows.len() * SLOT_BYTES];
    for (key, location) in rows {
        let i = function.index(key) * SLOT_BYTES;
        if slots[i..i+8] != [0;8] { return Err(invalid("PtrHash collision during construction")); }
        slots[i..i+8].copy_from_slice(&location.offset.to_le_bytes());
        slots[i+8..i+12].copy_from_slice(&u32::try_from(location.length)?.to_le_bytes());
        slots[i+12..i+16].copy_from_slice(&fingerprint(key));
    }
    let function_path = path.join(format!("{number:02x}.mphf"));
    let mut file = BufWriter::new(File::options().write(true).create_new(true).open(&function_path)?);
    // SAFETY: Function is a valid object constructed by the pinned PtrHash implementation.
    unsafe { function.serialize(&mut file)? }; file.flush()?; file.get_ref().sync_all()?;
    let bytes = fs::read(&function_path)?;
    // Verify the serialized representation, not just the original in-memory construction.
    let loaded = unsafe { Function::deserialize_full(&mut bytes.as_slice())? };
    for (key, location) in rows {
        let i = loaded.index(key) * SLOT_BYTES;
        if slots[i..i+8] != location.offset.to_le_bytes() || slots[i+8..i+12] != (location.length as u32).to_le_bytes() || slots[i+12..i+16] != fingerprint(key) {
            return Err(invalid("PtrHash serialized lookup mismatch"));
        }
    }
    write_new(&path.join(format!("{number:02x}.slots")), &slots)?;
    Ok(ShardInfo { count: rows.len(), function_sha256: digest(&bytes), slots_sha256: digest(&slots) })
}

/// Construct and activate a historical base on an offline writable clone of the source DB.
/// The SQLite writer lock fixes the snapshot; new values subsequently go only to the delta.
/// A crash before SQLite commit leaves at most an unreferenced immutable generation.
pub fn build_and_activate(db_path: &Path, output: &Path) -> Result<()> {
    let mut db = Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    db.execute_batch("PRAGMA cache_size=-8192; PRAGMA mmap_size=0; PRAGMA busy_timeout=0;")?;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let exists: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='clarity_ptrhash_base')", [], |r| r.get(0))?;
    if exists { return Err(invalid("database already has a PtrHash base")); }
    if output.exists() { return Err(invalid("refusing to replace existing generation")); }
    let partial = output.with_extension("building"); fs::create_dir(&partial)?;
    let store_id: Vec<u8> = tx.query_row("SELECT store_id FROM clarity_extent_format WHERE singleton=1 AND version=1", [], |r| r.get(0))?;
    let value_path = PathBuf::from(format!("{}.values", db_path.display()));
    let watermark = fs::metadata(&value_path)?.len();
    let mut header = [0u8; 48];
    use std::io::Read;
    File::open(&value_path)?.read_exact(&mut header)?;
    if &header[..8] != b"CLREXT01" || header[8..24] != store_id { return Err(invalid("extent generation header mismatch")); }
    let mut statement = tx.prepare("SELECT hash,offset,length FROM clarity_extent_index ORDER BY hash")?;
    let mut query = statement.query([])?;
    let mut next = query.next()?;
    let mut manifest = Manifest { version: 1, store_id, watermark, count: 0, shards: Vec::new() };
    let start = std::time::Instant::now();
    for p in 0..PARTITIONS {
        let mut rows = Vec::new();
        while let Some(row) = next {
            let hash: Vec<u8> = row.get(0)?;
            let key: [u8;40] = hash.try_into().map_err(|_| invalid("invalid commitment length"))?;
            if key[0] as usize != p { break; }
            let location = Location { offset: row.get(1)?, length: row.get(2)? };
            if location.offset < 48 || !(88..=32*1024*1024).contains(&location.length) || location.offset.checked_add(location.length).is_none_or(|end| end > watermark) {
                return Err(invalid("source extent outside snapshot"));
            }
            if rows.len() >= MAX_PARTITION_KEYS { return Err(invalid("partition exceeds construction memory bound")); }
            rows.push((key,location)); next = query.next()?;
        }
        let info = build_shard(&partial, p, &rows)?;
        manifest.count += info.count as u64; manifest.shards.push(info);
        eprintln!("PTRHASH_BUILD partition={p} keys={} seconds={:.3}", manifest.count, start.elapsed().as_secs_f64());
    }
    if next.is_some() { return Err(invalid("unconsumed source keys")); }
    drop(query); drop(statement);
    let bytes = serde_json::to_vec_pretty(&manifest)?;
    write_new(&partial.join("manifest.json"), &bytes)?; File::open(&partial)?.sync_all()?;
    fs::rename(&partial, output)?; File::open(output.parent().unwrap_or(Path::new(".")))?.sync_all()?;
    let path = fs::canonicalize(output)?;
    let db_parent = fs::canonicalize(db_path)?
        .parent()
        .ok_or_else(|| invalid("PtrHash database has no parent directory"))?
        .to_path_buf();
    let registered_path = if path.parent() == Some(db_parent.as_path()) {
        path.file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| invalid("non-UTF8 sibling index directory"))?
            .to_owned()
    } else {
        path.to_str().ok_or_else(|| invalid("non-UTF8 index path"))?.to_owned()
    };
    tx.execute_batch("CREATE TABLE clarity_extent_delta(hash BLOB PRIMARY KEY CHECK(length(hash)=40),offset INTEGER NOT NULL CHECK(offset>=48),length INTEGER NOT NULL CHECK(length>=88)) WITHOUT ROWID; CREATE TABLE clarity_ptrhash_base(singleton INTEGER PRIMARY KEY CHECK(singleton=1),path TEXT NOT NULL,manifest_sha256 TEXT NOT NULL);")?;
    tx.execute("INSERT INTO clarity_ptrhash_base VALUES(1,?1,?2)", params![registered_path, digest(&bytes)])?;
    tx.commit()?;
    eprintln!("PTRHASH_BUILD complete keys={} seconds={:.3}",manifest.count,start.elapsed().as_secs_f64());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Minimal valid physical envelopes for index-format tests, not packed-value decoding.
    fn fixture() -> (TempDir, PathBuf, Vec<([u8; 40], Location)>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.sqlite");
        let db = Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE clarity_extent_format(singleton INTEGER,version INTEGER,store_id BLOB); CREATE TABLE clarity_extent_index(hash BLOB PRIMARY KEY,offset INTEGER,length INTEGER) WITHOUT ROWID;").unwrap();
        db.execute("INSERT INTO clarity_extent_format VALUES(1,1,?1)", [[7u8;16].as_slice()]).unwrap();
        let mut bytes = vec![0u8;48]; bytes[..8].copy_from_slice(b"CLREXT01"); bytes[8..24].fill(7);
        let mut rows = Vec::new();
        for i in 0u64..300 {
            let mut key = [0;40]; key[..32].copy_from_slice(&Sha256::digest(i.to_le_bytes()));
            let offset = bytes.len() as u64; bytes.extend_from_slice(b"CLRVAL01"); bytes.extend_from_slice(&key); bytes.resize(bytes.len()+40,0);
            db.execute("INSERT INTO clarity_extent_index VALUES(?1,?2,88)",params![key.as_slice(),offset]).unwrap();
            rows.push((key,Location {offset,length:88}));
        }
        fs::write(format!("{}.values",path.display()),bytes).unwrap();
        (dir,path,rows)
    }

    /// Full source lookups survive serialization, registration, and sharing across opens.
    #[test]
    fn snapshot_roundtrip_and_unknown_keys() {
        let (dir,path,rows) = fixture(); let index=dir.path().join("base");
        build_and_activate(&path,&index).unwrap();
        let db=Connection::open(&path).unwrap();
        let base=Base::registered(&db,&path,&[7;16],u64::MAX).unwrap().unwrap();
        let second=Base::registered(&db,&path,&[7;16],u64::MAX).unwrap().unwrap();
        assert!(Arc::ptr_eq(&base,&second));
        for (key,location) in rows { assert_eq!(base.candidate(&key).unwrap(),Some(location)); }
        assert!(base.candidate(&[255;40]).unwrap().is_none());
        assert!(Base::registered(&db,&path,&[8;16],u64::MAX).is_err());
        assert!(Base::registered(&db,&path,&[7;16],48).is_err());
        assert!(build_and_activate(&path,&dir.path().join("second")).is_err());
    }

    /// A sibling registration opens after moving the whole database directory.
    #[test]
    fn sibling_generation_moves_with_database() {
        let (dir, path, rows) = fixture();
        build_and_activate(&path, &dir.path().join("base")).unwrap();
        let db = Connection::open(&path).unwrap();
        let registered: String = db.query_row(
            "SELECT path FROM clarity_ptrhash_base WHERE singleton=1",
            [],
            |row| row.get(0),
        ).unwrap();
        assert_eq!(registered, "base");
        drop(db);

        let destination = tempfile::tempdir().unwrap();
        let moved = destination.path().join("moved");
        fs::rename(dir.path(), &moved).unwrap();
        let moved_db = moved.join("index.sqlite");
        let db = Connection::open(&moved_db).unwrap();
        let base = Base::registered(&db, &moved_db, &[7; 16], u64::MAX).unwrap().unwrap();
        let (key, location) = rows[0];
        assert_eq!(base.candidate(&key).unwrap(), Some(location));

        // Previously published absolute registrations still open.
        db.execute("UPDATE clarity_ptrhash_base SET path=?1", [moved.join("base").to_str().unwrap()]).unwrap();
        assert!(Base::registered(&db, &moved_db, &[7; 16], u64::MAX).unwrap().is_some());
        db.execute("UPDATE clarity_ptrhash_base SET path='../base'", []).unwrap();
        assert!(Base::registered(&db, &moved_db, &[7; 16], u64::MAX).is_err());
    }

    /// Incomplete publication and corrupt metadata never become an active base.
    #[test]
    fn failed_publication_and_corruption_are_rejected() {
        let (dir,path,_) = fixture(); let index=dir.path().join("base");
        fs::create_dir(index.with_extension("building")).unwrap();
        assert!(build_and_activate(&path,&index).is_err());
        let db=Connection::open(&path).unwrap(); assert!(Base::registered(&db,&path,&[7;16],u64::MAX).unwrap().is_none());
        fs::remove_dir(index.with_extension("building")).unwrap();
        build_and_activate(&path,&index).unwrap();
        fs::write(index.join("manifest.json"),b"{}").unwrap();
        assert!(Base::registered(&db,&path,&[7;16],u64::MAX).is_err());
    }

    /// Size and function checks reject a damaged published generation on first open.
    #[test]
    fn damaged_function_or_truncated_slots_fail_open() {
        for function in [true,false] {
            let (dir,path,rows)=fixture(); let index=dir.path().join("base"); build_and_activate(&path,&index).unwrap();
            let shard=rows[0].0[0];
            fs::write(index.join(format!("{shard:02x}.{}",if function {"mphf"} else {"slots"})),b"bad").unwrap();
            assert!(Base::registered(&Connection::open(&path).unwrap(),&path,&[7;16],u64::MAX).is_err());
        }
    }
}

#[cfg(feature = "diagnostics")]
impl Drop for Base {
    fn drop(&mut self) {
        self.report_residency("drop");
        diagnostics::report_pages();
    }
}
