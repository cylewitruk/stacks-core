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

//! Export exact fresh-generation PtrHash memberships for a portable native rebuild.

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use blockstack_lib::clarity_vm::database::value_extents::ValueBackend;
use blockstack_lib::util_lib::db::sqlite_readonly_uri;
use extent_ptrhash::stable::Base;
use rusqlite::{Connection, OpenFlags};
use sha2::{Digest, Sha256};
use stable_value_format::{FILE_HEADER_BYTES, VALUE_ROW_BYTES, ValueId};
use stacks_storage_migrate::Result;

/// A full logical commitment followed by its little-endian stable ID.
const PAIR_BYTES: usize = 44;
/// Bound sorting storage to 176 MiB of pair records for one hash prefix.
const MAX_SHARD_ROWS: usize = 4 * 1024 * 1024;

/// Require an offline fresh generation and publish an independently checksummed stream.
fn main() -> Result<()> {
    let args: Vec<_> = env::args().skip(1).collect();
    if args.len() != 2 {
        return Err(
            "usage: export_stable_pairs CANONICAL_CLARITY_SQLITE NEW_EXPORT_DIRECTORY".into(),
        );
    }
    println!("{}", export(Path::new(&args[0]), Path::new(&args[1]))?);
    Ok(())
}

/// Create a new private file without overwriting prior evidence.
fn create(path: &Path) -> Result<File> {
    Ok(OpenOptions::new().write(true).create_new(true).open(path)?)
}

/// Sort one bounded prefix, reject duplicate commitments, and append canonical pairs.
fn append_shard(
    path: &Path,
    count: usize,
    output: &mut impl Write,
    digest: &mut Sha256,
) -> Result<()> {
    if count > MAX_SHARD_ROWS || path.metadata()?.len() != (count * PAIR_BYTES) as u64 {
        return Err("membership shard exceeds bounds or has an unexpected length".into());
    }
    let mut input = BufReader::new(File::open(path)?);
    let mut rows = vec![[0u8; PAIR_BYTES]; count];
    for row in &mut rows {
        input.read_exact(row)?;
    }
    rows.sort_unstable_by(|a, b| a[..40].cmp(&b[..40]));
    for pair in rows.windows(2) {
        if pair[0][..40] >= pair[1][..40] {
            return Err("duplicate or unordered commitment in membership export".into());
        }
    }
    for row in rows {
        output.write_all(&row)?;
        digest.update(row);
    }
    Ok(())
}

/// Enumerate verified IDs, bucket by commitment prefix, and retain only the final pair stream.
fn export(database: &Path, destination: &Path) -> Result<serde_json::Value> {
    let database = database.canonicalize()?;
    let parent = destination
        .parent()
        .ok_or("export parent missing")?
        .canonicalize()?;
    let db_parent = database.parent().ok_or("database parent missing")?;
    if parent.starts_with(db_parent) || destination.exists() {
        return Err("export must be new and outside the Clarity database directory".into());
    }
    let name = destination.file_name().ok_or("export name missing")?;
    let destination = parent.join(name);
    let staging = parent.join(format!(".{}.building", name.to_string_lossy()));
    let db = Connection::open_with_flags(
        sqlite_readonly_uri(&database, true)?,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?;
    let ValueBackend::Stable(mut store) =
        ValueBackend::open_registered(&db, &database)?.ok_or("stable generation missing")?;
    let generation: String = db.query_row(
        "SELECT path FROM clarity_stable_format WHERE singleton=1",
        [],
        |row| row.get(0),
    )?;
    let length = db_parent
        .join(generation)
        .join("value-directory.dat")
        .metadata()?
        .len();
    let body = length
        .checked_sub(FILE_HEADER_BYTES as u64)
        .ok_or("truncated directory")?;
    if body % VALUE_ROW_BYTES as u64 != 0 {
        return Err("partial value-directory row".into());
    }
    let rows = body / VALUE_ROW_BYTES as u64;
    if rows > u32::MAX as u64 {
        return Err("stable ID capacity exceeded".into());
    }
    let base = Base::registered(&db, &database, &store.store_id(), rows)
        .map_err(|error| error.to_string())?
        .ok_or("stable PtrHash base missing")?;
    let delta: u64 = db.query_row("SELECT count(*) FROM clarity_stable_value_delta", [], |r| {
        r.get(0)
    })?;
    if base.count() != rows || delta != 0 {
        return Err("export requires a fresh base with every ID indexed and an empty delta".into());
    }
    let (base_name, base_digest): (String, String) = db.query_row(
        "SELECT path,manifest_sha256 FROM clarity_stable_ptrhash_base WHERE singleton=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let manifest_bytes = fs::read(db_parent.join(base_name).join("manifest.json"))?;
    if format!("{:x}", Sha256::digest(&manifest_bytes)) != base_digest {
        return Err("base manifest changed".into());
    }
    let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes)?;
    let expected: Vec<u64> = manifest["shards"]
        .as_array()
        .ok_or("missing shards")?
        .iter()
        .map(|s| s["count"].as_u64().ok_or("invalid shard count"))
        .collect::<std::result::Result<_, _>>()?;
    if expected.len() != 256 || expected.iter().any(|&n| n > MAX_SHARD_ROWS as u64) {
        return Err("unsupported membership shard size".into());
    }
    fs::create_dir(&staging)?;
    fs::write(
        staging.join("INCOMPLETE"),
        b"Unpublished membership export; discard on failure.\n",
    )?;
    let buckets: Vec<PathBuf> = (0..256)
        .map(|n| staging.join(format!("{n:02x}.scratch")))
        .collect();
    let mut writers = buckets
        .iter()
        .map(|p| create(p).map(|f| BufWriter::with_capacity(64 * 1024, f)))
        .collect::<Result<Vec<_>>>()?;
    let mut counts = [0usize; 256];
    let mut last = Instant::now();
    for raw in 1..=rows {
        let value = store.read(ValueId::new(u32::try_from(raw)?)?)?;
        let key = value.commitment.0;
        if base.candidate(&key).map_err(|error| error.to_string())? != Some(raw as u32) {
            return Err(format!("membership differs at ID {raw}").into());
        }
        let prefix = usize::from(key[0]);
        counts[prefix] += 1;
        if counts[prefix] as u64 > expected[prefix] {
            return Err("membership prefix exceeds registered count".into());
        }
        writers[prefix].write_all(&key)?;
        writers[prefix].write_all(&(raw as u32).to_le_bytes())?;
        if last.elapsed().as_secs() >= 10 {
            eprintln!("exported_memberships={raw} total={rows}");
            last = Instant::now();
        }
    }
    for writer in &mut writers {
        writer.flush()?;
    }
    drop(writers);
    let mut output =
        BufWriter::with_capacity(1024 * 1024, create(&staging.join("committed-pairs.bin"))?);
    let mut digest = Sha256::new();
    for (prefix, path) in buckets.iter().enumerate() {
        if counts[prefix] as u64 != expected[prefix] {
            return Err("membership count differs".into());
        }
        append_shard(path, counts[prefix], &mut output, &mut digest)?;
        fs::remove_file(path)?;
    }
    output.flush()?;
    output.get_ref().sync_all()?;
    let result = serde_json::json!({"status":"pass", "memberships":rows, "prefix_counts":expected,
        "store_id":store.store_id(), "base_manifest_sha256":base_digest,
        "pair_bytes":PAIR_BYTES, "sha256":format!("{:x}",digest.finalize()),
        "scope":"exact fresh-generation memberships; no orphans or post-base delta"});
    let mut metadata = create(&staging.join("MANIFEST.json"))?;
    metadata.write_all(serde_json::to_string_pretty(&result)?.as_bytes())?;
    metadata.sync_all()?;
    fs::remove_file(staging.join("INCOMPLETE"))?;
    File::open(&staging)?.sync_all()?;
    if destination.exists() {
        return Err("export destination appeared".into());
    }
    fs::rename(&staging, destination)?;
    File::open(parent)?.sync_all()?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use blockstack_lib::chainstate::stacks::index::{ClarityMarfTrieId, MARFValue};
    use blockstack_lib::chainstate::stacks::index::marf::{MARF, MARFOpenOpts, MarfConnection};
    use clarity::vm::database::SqliteConnection;
    use rusqlite::params;
    use stacks_common::types::chainstate::StacksBlockId;
    use stacks_storage_migrate::{Config, migrate};

    use super::*;

    /// A real conversion exports its exact memberships without changing the published source.
    #[test]
    fn exports_fresh_conversion_and_rejects_post_base_delta() {
        let temp = tempfile::tempdir().unwrap();
        let legacy = temp.path().join("legacy.sqlite");
        let mut marf =
            MARF::<StacksBlockId>::from_path(legacy.to_str().unwrap(), MARFOpenOpts::default())
                .unwrap();
        SqliteConnection::initialize_conn(marf.sqlite_conn()).unwrap();
        let mut tx = marf.begin_tx().unwrap();
        tx.begin(&StacksBlockId::sentinel(), &StacksBlockId([1; 32]))
            .unwrap();
        let mut expected = Vec::new();
        for id in 1u32..=2 {
            let text = format!("value-{id}-{}", "external".repeat(64));
            let commitment = MARFValue::from_value(&text);
            tx.sqlite_tx()
                .execute(
                    "INSERT INTO data_table(key,value) VALUES(?1,?2)",
                    params![commitment.to_hex(), text],
                )
                .unwrap();
            tx.insert_batch(&[format!("key-{id}")], vec![commitment.clone()])
                .unwrap();
            let mut pair = [0u8; PAIR_BYTES];
            pair[..40].copy_from_slice(&commitment.0);
            pair[40..].copy_from_slice(&id.to_le_bytes());
            expected.push(pair);
        }
        tx.commit().unwrap();
        drop(marf);
        let output = temp.path().join("canonical");
        let converted = migrate(&Config {
            source: legacy,
            destination: output,
            max_trie_bytes: 1024 * 1024,
            clarity_values: true,
        })
        .unwrap();
        let before = fs::read(&converted.database).unwrap();
        let target = temp.path().join("export");
        let evidence = export(&converted.database, &target).unwrap();
        expected.sort_unstable();
        let pairs = fs::read(target.join("committed-pairs.bin")).unwrap();
        assert_eq!(pairs, expected.concat());
        assert_eq!(evidence["memberships"], 2);
        assert_eq!(evidence["sha256"], format!("{:x}", Sha256::digest(&pairs)));
        assert_eq!(before, fs::read(&converted.database).unwrap());
        let db = Connection::open(&converted.database).unwrap();
        db.execute(
            "INSERT INTO clarity_stable_value_delta VALUES(?1,1)",
            [MARFValue::from_value("delta").0.as_slice()],
        )
        .unwrap();
        let rejected = temp.path().join("rejected");
        assert!(
            export(&converted.database, &rejected)
                .unwrap_err()
                .to_string()
                .contains("empty delta")
        );
        assert!(!rejected.exists());
    }

    /// Sorting preserves full commitments/IDs and refuses duplicate-key reconstruction inputs.
    #[test]
    fn sorts_pairs_and_rejects_duplicate_commitments() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("pairs");
        let mut a = [1u8; PAIR_BYTES];
        let mut b = [2u8; PAIR_BYTES];
        a[40..].copy_from_slice(&9u32.to_le_bytes());
        b[40..].copy_from_slice(&3u32.to_le_bytes());
        fs::write(&path, [b, a].concat()).unwrap();
        let mut output = Vec::new();
        append_shard(&path, 2, &mut output, &mut Sha256::new()).unwrap();
        assert_eq!(output, [a, b].concat());
        b[..40].copy_from_slice(&a[..40]);
        fs::write(&path, [a, b].concat()).unwrap();
        assert!(append_shard(&path, 2, &mut Vec::new(), &mut Sha256::new()).is_err());
    }
}
