//! Immutable mmap offset maps, independent of the source and destination trie codecs.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use memmap2::Mmap;
use rusqlite::Connection;
use sha2::{Digest, Sha256};

use crate::relocation::RelocationPlan;

/// Versioned index magic; header and descriptor fields are little endian.
const MAGIC: &[u8; 8] = b"MRFPLAN1";
/// Fixed header size, including binding and payload checksum.
const HEADER: usize = 128;
/// Fixed descriptor: signed block ID, byte offset, pair count and output length.
const DESCRIPTOR: usize = 32;

/// Read one fixed-width little-endian field after container validation.
fn word(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(
        bytes[offset..offset + 8]
            .try_into()
            .expect("validated field"),
    )
}

/// An owned, read-only mapping of immutable offset-map bytes.
pub struct RelocationIndex {
    /// Mapping owner retained by every borrowed plan lifetime.
    map: Mmap,
    /// Number of sorted block descriptors.
    count: usize,
}

/// A borrowed sorted offset map with no allocation or record decoding.
#[derive(Clone, Copy)]
pub struct MappedPlan<'a> {
    /// Packed original/destination u64 offset pairs.
    pairs: &'a [u8],
    /// Destination trie length.
    length: u64,
}

impl RelocationPlan for MappedPlan<'_> {
    fn len(&self) -> usize {
        self.pairs.len() / 16
    }
    fn length(&self) -> u64 {
        self.length
    }
    fn pair(&self, index: usize) -> (u64, u64) {
        (
            word(self.pairs, index * 16),
            word(self.pairs, index * 16 + 8),
        )
    }
}

/// Remove only the unique incomplete index owned by the current builder.
struct Pending(PathBuf);
impl Drop for Pending {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

impl RelocationIndex {
    /// Open and validate a completed immutable index for the exact conversion binding.
    /// Callers must keep the published file immutable while any mapping is alive.
    pub fn open(path: &Path, binding: &[u8; 32]) -> Result<Self, Box<dyn std::error::Error>> {
        let file = File::open(path)?;
        // SAFETY: converter-owned indexes are published by atomic replacement, never edited
        // or truncated in place. The mapping owns its lifetime independently of the handle.
        let map = unsafe { Mmap::map(&file)? };
        if map.len() < HEADER || &map[..8] != MAGIC || &map[32..64] != binding {
            return Err("relocation index header or conversion binding mismatch".into());
        }
        let count = usize::try_from(word(&map, 8))?;
        let data_start = HEADER
            .checked_add(
                count
                    .checked_mul(DESCRIPTOR)
                    .ok_or("descriptor size overflow")?,
            )
            .ok_or("index size overflow")?;
        if word(&map, 16) != map.len() as u64
            || word(&map, 24) != data_start as u64
            || data_start > map.len()
        {
            return Err("truncated relocation index".into());
        }
        if map[96..HEADER].iter().any(|byte| *byte != 0) {
            return Err("unsupported relocation index header".into());
        }
        let digest = Sha256::digest(&map[HEADER..]);
        if digest[..] != map[64..96] {
            return Err("relocation index checksum mismatch".into());
        }
        let mut previous_id = None;
        let mut expected = data_start;
        for i in 0..count {
            let descriptor = HEADER + i * DESCRIPTOR;
            let id = word(&map, descriptor) as i64;
            let offset = usize::try_from(word(&map, descriptor + 8))?;
            let pairs = usize::try_from(word(&map, descriptor + 16))?;
            let length = word(&map, descriptor + 24);
            if previous_id.is_some_and(|previous| previous >= id)
                || id == 0
                || offset != expected
                || pairs == 0
            {
                return Err("invalid relocation descriptor ordering or range".into());
            }
            let end = offset
                .checked_add(pairs.checked_mul(16).ok_or("pair count overflow")?)
                .ok_or("pair range overflow")?;
            let bytes = map.get(offset..end).ok_or("truncated relocation pairs")?;
            let mut previous = None;
            for entry in bytes.chunks_exact(16) {
                let pair = (word(entry, 0), word(entry, 8));
                if previous.is_none() && pair != (36, 36) {
                    return Err("invalid root relocation".into());
                }
                if pair.1 >= length
                    || previous.is_some_and(|old: (u64, u64)| old.0 >= pair.0 || old.1 >= pair.1)
                {
                    return Err("invalid relocation pair ordering or destination bound".into());
                }
                previous = Some(pair);
            }
            previous_id = Some(id);
            expected = end;
        }
        if expected != map.len() {
            return Err("unexpected relocation index tail".into());
        }
        Ok(Self { map, count })
    }

    /// Resolve a block descriptor and borrow its pair array without SQLite or heap allocation.
    pub fn plan(&self, block: i64) -> Result<MappedPlan<'_>, String> {
        let mut low = 0;
        let mut high = self.count;
        while low < high {
            let mid = low + (high - low) / 2;
            let descriptor = HEADER + mid * DESCRIPTOR;
            match (word(&self.map, descriptor) as i64).cmp(&block) {
                std::cmp::Ordering::Less => low = mid + 1,
                std::cmp::Ordering::Greater => high = mid,
                std::cmp::Ordering::Equal => {
                    let start = word(&self.map, descriptor + 8) as usize;
                    let end = start + word(&self.map, descriptor + 16) as usize * 16;
                    return Ok(MappedPlan {
                        pairs: &self.map[start..end],
                        length: word(&self.map, descriptor + 24),
                    });
                }
            }
        }
        Err(format!("missing relocation plan for block {block}"))
    }

    /// Export complete SQL plans once, atomically publishing an independently versioned index.
    /// Binding must include source identity, value generation, and the exact plan/codec version.
    pub fn build(
        db: &Connection,
        path: &Path,
        binding: &[u8; 32],
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let count: u64 = db.query_row("SELECT COUNT(*) FROM migration.plans", [], |r| r.get(0))?;
        let temporary = path.with_extension(format!("building-{}", std::process::id()));
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        let _cleanup = Pending(temporary.clone());
        let data_start = (HEADER as u64)
            .checked_add(
                count
                    .checked_mul(DESCRIPTOR as u64)
                    .ok_or("descriptor size overflow")?,
            )
            .ok_or("index size overflow")?;
        file.set_len(data_start)?;
        let descriptors_path = path.with_extension(format!("descriptors-{}", std::process::id()));
        let descriptors_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&descriptors_path)?;
        let _descriptor_cleanup = Pending(descriptors_path);
        let mut descriptors = BufWriter::with_capacity(1024 * 1024, descriptors_file);
        file.seek(SeekFrom::Start(data_start))?;
        let mut payload = BufWriter::with_capacity(1024 * 1024, &mut file);
        let mut data_end = data_start;
        let mut row_count = 0;
        let mut query =
            db.prepare("SELECT block_id,offsets,length FROM migration.plans ORDER BY block_id")?;
        let mut rows = query.query([])?;
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let pairs = row.get_ref(1)?.as_blob()?;
            let length: u64 = row.get(2)?;
            if pairs.is_empty() || pairs.len() % 16 != 0 {
                return Err("invalid SQL plan vector".into());
            }
            let mut descriptor = [0u8; DESCRIPTOR];
            for (i, value) in [id as u64, data_end, (pairs.len() / 16) as u64, length]
                .into_iter()
                .enumerate()
            {
                descriptor[i * 8..i * 8 + 8].copy_from_slice(&value.to_le_bytes());
            }
            descriptors.write_all(&descriptor)?;
            payload.write_all(pairs)?;
            data_end = data_end
                .checked_add(pairs.len() as u64)
                .ok_or("index length overflow")?;
            row_count += 1;
        }
        if row_count != count {
            return Err("relocation plans changed during export".into());
        }
        payload.flush()?;
        drop(payload);
        descriptors.flush()?;
        let mut descriptors_file = descriptors.into_inner()?;
        descriptors_file.seek(SeekFrom::Start(0))?;
        file.seek(SeekFrom::Start(HEADER as u64))?;
        io::copy(&mut descriptors_file, &mut file)?;
        file.seek(SeekFrom::Start(HEADER as u64))?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0; 1024 * 1024];
        loop {
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
        }
        let mut header = [0u8; HEADER];
        header[..8].copy_from_slice(MAGIC);
        header[8..16].copy_from_slice(&count.to_le_bytes());
        header[16..24].copy_from_slice(&data_end.to_le_bytes());
        header[24..32].copy_from_slice(&data_start.to_le_bytes());
        header[32..64].copy_from_slice(binding);
        header[64..96].copy_from_slice(&hasher.finalize());
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&header)?;
        file.sync_all()?;
        let verified = Self::open(&temporary, binding)?;
        fs::rename(&temporary, path)?;
        File::open(
            path.parent()
                .ok_or_else(|| io::Error::other("index parent missing"))?,
        )?
        .sync_all()?;
        Ok(verified)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    /// Mapped plans match exact SQL pairs, including sparse, historical and mined IDs.
    #[test]
    fn mapped_plans_roundtrip_and_reject_corruption() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("ATTACH DATABASE ':memory:' AS migration; CREATE TABLE migration.plans(block_id INTEGER PRIMARY KEY,offsets BLOB,length INTEGER)").unwrap();
        let pairs: Vec<u8> = [36u64, 36, 99, 70, 200, 100]
            .into_iter()
            .flat_map(u64::to_le_bytes)
            .collect();
        for id in [-9i64, 1, 2000000000] {
            db.execute(
                "INSERT INTO migration.plans VALUES (?1,?2,120)",
                params![id, pairs],
            )
            .unwrap();
        }
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("plans");
        let binding = [7; 32];
        let index = RelocationIndex::build(&db, &path, &binding).unwrap();
        for id in [-9, 1, 2000000000] {
            let plan = index.plan(id).unwrap();
            assert_eq!(plan.resolve(99).unwrap(), 70);
            assert!(plan.resolve(100).is_err());
            assert_eq!(plan.length(), 120);
        }
        assert!(index.plan(2).is_err());
        assert!(RelocationIndex::open(&path, &[8; 32]).is_err());
        let replacement = RelocationIndex::build(&db, &path, &binding).unwrap();
        assert_eq!(index.plan(1).unwrap().resolve(200).unwrap(), 100);
        drop(replacement);
        drop(index);
        let original = fs::read(&path).unwrap();
        let mut bad = original.clone();
        *bad.last_mut().unwrap() ^= 1;
        fs::write(&path, &bad).unwrap();
        assert!(RelocationIndex::open(&path, &binding).is_err());
        fs::write(&path, &original[..original.len() - 1]).unwrap();
        assert!(RelocationIndex::open(&path, &binding).is_err());
        // Reject invalid vectors before replacing the existing published file.
        db.execute(
            "UPDATE migration.plans SET offsets=?1 WHERE block_id=1",
            [&[0u8; 16][..]],
        )
        .unwrap();
        assert!(RelocationIndex::build(&db, &path, &binding).is_err());
        assert_eq!(fs::read(&path).unwrap(), original[..original.len() - 1]);
    }
}
