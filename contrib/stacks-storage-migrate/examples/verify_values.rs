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

//! Read every published stable value and verify reconstruction plus dedup membership.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use std::{env, thread};

use blockstack_lib::chainstate::stacks::index::MARFValue;
use blockstack_lib::clarity_vm::database::value_extents::{StableMappedValueRecord, ValueBackend};
use blockstack_lib::util_lib::db::sqlite_readonly_uri;
use extent_ptrhash::stable::Base;
use rusqlite::{Connection, OpenFlags};
use stable_value_format::{FILE_HEADER_BYTES, VALUE_ROW_BYTES, ValueId};
use stacks_storage_migrate::Result;

/// Open an offline database without journal or recovery writes.
fn open(path: &Path) -> Result<Connection> {
    Ok(Connection::open_with_flags(
        sqlite_readonly_uri(path, true)?,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?)
}

/// Verify a contiguous ID range using bounded runtime mappings and descriptor caches.
fn range(path: &Path, rows: u64, start: u64, end: u64, progress: &AtomicU64) -> Result<u64> {
    let db = open(path)?;
    let ValueBackend::Stable(mut store) =
        ValueBackend::open_registered(&db, path)?.ok_or("stable generation missing")?;
    let base = Base::registered(&db, path, &store.store_id(), rows)
        .map_err(|error| error.to_string())?
        .ok_or("stable dedup base missing")?;
    if base.count() != rows {
        return Err("fresh conversion must index every external value exactly once".into());
    }
    let mut bytes = 0u64;
    let mut last = Instant::now();
    for raw in start..end {
        let value = store.read(ValueId::new(u32::try_from(raw)?)?)?;
        let commitment = value.commitment.clone();
        let text = StableMappedValueRecord::from_stable_parts(value).canonical()?;
        if MARFValue::from_value(&text) != commitment {
            return Err(format!("reconstructed commitment differs at ID {raw}").into());
        }
        if base
            .candidate(&commitment.0)
            .map_err(|error| error.to_string())?
            != Some(raw as u32)
        {
            return Err(format!("dedup membership differs at ID {raw}").into());
        }
        bytes = bytes
            .checked_add(text.len() as u64)
            .ok_or("byte total overflow")?;
        let complete = progress.fetch_add(1, Ordering::Relaxed) + 1;
        if last.elapsed().as_secs() >= 10 {
            eprintln!("verified_values={complete} total={rows}");
            last = Instant::now();
        }
    }
    Ok(bytes)
}

/// Require complete reconstruction and exact membership for every row in a fresh conversion.
fn main() -> Result<()> {
    let args: Vec<_> = env::args().skip(1).collect();
    if args.len() != 1 {
        return Err("usage: verify_values CANONICAL_CLARITY_SQLITE".into());
    }
    let path = PathBuf::from(&args[0]);
    println!("{}", audit(&path)?);
    Ok(())
}

/// Return independent reconstruction evidence for one complete fresh generation.
fn audit(path: &Path) -> Result<serde_json::Value> {
    let db = open(path)?;
    // Opening the registered backend validates the generation-relative path and identity.
    ValueBackend::open_registered(&db, path)?.ok_or("stable generation missing")?;
    let name: String = db.query_row(
        "SELECT path FROM clarity_stable_format WHERE singleton=1",
        [],
        |row| row.get(0),
    )?;
    let length = path
        .parent()
        .ok_or("database parent missing")?
        .join(name)
        .join("value-directory.dat")
        .metadata()?
        .len();
    let payload = length
        .checked_sub(FILE_HEADER_BYTES as u64)
        .ok_or("directory truncated")?;
    if payload % VALUE_ROW_BYTES as u64 != 0 {
        return Err("partial value directory row".into());
    }
    let rows = payload / VALUE_ROW_BYTES as u64;
    if rows > u32::MAX as u64 {
        return Err("ID capacity exceeded".into());
    }
    let workers = thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(4);
    let chunk = rows.div_ceil(workers as u64).max(1);
    let progress = AtomicU64::new(0);
    let result = thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|worker| {
                let start = (1 + worker as u64 * chunk).min(rows + 1);
                let end = (start + chunk).min(rows + 1);
                let progress = &progress;
                scope.spawn(move || {
                    range(path, rows, start, end, progress).map_err(|e| e.to_string())
                })
            })
            .collect();
        handles.into_iter().try_fold(0u64, |sum, handle| {
            let count = handle
                .join()
                .map_err(|_| "audit worker panicked".to_owned())??;
            sum.checked_add(count)
                .ok_or_else(|| "byte total overflow".to_owned())
        })
    });
    let bytes = result?;
    if progress.load(Ordering::Relaxed) != rows {
        return Err("incomplete value audit".into());
    }
    Ok(serde_json::json!({"status":"pass","external_values":rows,
        "reconstructed_canonical_bytes":bytes,"all_external_commitments_and_memberships_verified":true,
        "scope":"fresh conversion external stable values; inline values are checked during extraction and sampled leaf audit"}))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use blockstack_lib::chainstate::stacks::index::ClarityMarfTrieId;
    use blockstack_lib::chainstate::stacks::index::marf::{MARF, MARFOpenOpts, MarfConnection};
    use clarity::vm::database::SqliteConnection;
    use rusqlite::params;
    use stacks_common::types::chainstate::StacksBlockId;
    use stacks_storage_migrate::{Config, migrate};

    use super::*;

    /// A full audit detects a valid-row retarget through independent dedup membership.
    #[test]
    fn verifies_fresh_conversion_and_rejects_valid_row_retarget() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("legacy.sqlite");
        let mut marf =
            MARF::<StacksBlockId>::from_path(source.to_str().unwrap(), MARFOpenOpts::default())
                .unwrap();
        SqliteConnection::initialize_conn(marf.sqlite_conn()).unwrap();
        let mut tx = marf.begin_tx().unwrap();
        tx.begin(&StacksBlockId::sentinel(), &StacksBlockId([1; 32]))
            .unwrap();
        for n in 1..=2 {
            let text = format!("value-{n}-{}", "external".repeat(64));
            let commitment = MARFValue::from_value(&text);
            tx.sqlite_tx()
                .execute(
                    "INSERT INTO data_table(key,value) VALUES(?1,?2)",
                    params![commitment.to_hex(), text],
                )
                .unwrap();
            tx.insert_batch(&[format!("key-{n}")], vec![commitment])
                .unwrap();
        }
        tx.commit().unwrap();
        marf.sqlite_conn()
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        drop(marf);
        let output = migrate(&Config {
            source,
            destination: directory.path().join("canonical"),
            clarity_values: true,
            max_trie_bytes: 1024 * 1024,
        })
        .unwrap();
        let report = audit(&output.database).unwrap();
        assert_eq!(report["external_values"], 2);
        let db = open(&output.database).unwrap();
        let name: String = db
            .query_row("SELECT path FROM clarity_stable_format", [], |row| {
                row.get(0)
            })
            .unwrap();
        drop(db);
        let rows = output
            .database
            .parent()
            .unwrap()
            .join(name)
            .join("value-directory.dat");
        let mut bytes = fs::read(&rows).unwrap();
        let start = FILE_HEADER_BYTES;
        bytes.copy_within(start + VALUE_ROW_BYTES..start + 2 * VALUE_ROW_BYTES, start);
        fs::write(rows, bytes).unwrap();
        assert!(
            audit(&output.database)
                .unwrap_err()
                .to_string()
                .contains("dedup membership differs")
        );
    }
}
