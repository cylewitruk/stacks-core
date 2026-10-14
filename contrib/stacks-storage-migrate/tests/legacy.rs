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

//! End-to-end direct conversion across legacy blob backends and fork histories.

use std::fs;

use blockstack_lib::chainstate::stacks::index::marf::{MARF, MARFOpenOpts, MarfConnection};
use blockstack_lib::chainstate::stacks::index::record::NodeRecordFormat;
use blockstack_lib::chainstate::stacks::index::storage::TrieFileStorage;
use blockstack_lib::chainstate::stacks::index::{ClarityMarfTrieId, MARFValue};
use rusqlite::Connection;
use stacks_common::types::chainstate::StacksBlockId;
use stacks_storage_migrate::{Config, migrate};
use tempfile::tempdir;

/// Mined candidates and mutable unconfirmed tries retain their separate SQL storage and roots.
#[test]
fn legacy_mined_and_unconfirmed_records() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("legacy.sqlite");
    let mut confirmed =
        MARF::<StacksBlockId>::from_path(path.to_str().unwrap(), MARFOpenOpts::default()).unwrap();
    let tip = StacksBlockId([1; 32]);
    let mut tx = confirmed.begin_tx().unwrap();
    tx.begin(&StacksBlockId::sentinel(), &tip).unwrap();
    tx.insert_batch(&["confirmed".into()], vec![MARFValue([3; 40])])
        .unwrap();
    tx.commit().unwrap();
    let mut tx = confirmed.begin_tx().unwrap();
    tx.begin(&tip, &StacksBlockId([2; 32])).unwrap();
    tx.insert_batch(&["mined".into()], vec![MARFValue([4; 40])])
        .unwrap();
    tx.commit_mined(&StacksBlockId([2; 32])).unwrap();
    drop(confirmed);
    let storage = TrieFileStorage::<StacksBlockId>::open_unconfirmed(
        path.to_str().unwrap(),
        MARFOpenOpts::default(),
    )
    .unwrap();
    let mut unconfirmed = MARF::from_storage(storage);
    let mut tx = unconfirmed.begin_tx().unwrap();
    let pending_tip = tx.begin_unconfirmed(&tip).unwrap();
    tx.insert_batch(&["pending".into()], vec![MARFValue([5; 40])])
        .unwrap();
    tx.commit().unwrap();
    let expected = unconfirmed
        .get_with_proof(&pending_tip, "pending")
        .unwrap()
        .unwrap();
    drop(unconfirmed);
    let db = Connection::open(&path).unwrap();
    db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
    let mined: Vec<u8> = db
        .query_row("SELECT data FROM mined_blocks", [], |row| row.get(0))
        .unwrap();
    let root = NodeRecordFormat::Legacy.parse(&mined[36..]).unwrap().hash;
    drop(db);
    let original = fs::read(&path).unwrap();
    let output = migrate(&Config {
        source: path.clone(),
        destination: directory.path().join("canonical"),
        clarity_values: false,
        max_trie_bytes: 1024 * 1024,
    })
    .unwrap();
    assert_eq!(output.tries, 3);
    assert_eq!(fs::read(&path).unwrap(), original);
    let db = Connection::open(&output.database).unwrap();
    let mined: Vec<u8> = db
        .query_row("SELECT data FROM mined_blocks", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        NodeRecordFormat::Optimized
            .parse(&mined[36..])
            .unwrap()
            .hash,
        root
    );
    let unconfirmed_location: (usize, u64) = db
        .query_row(
            "SELECT length(data),external_length FROM marf_data WHERE unconfirmed=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert!(unconfirmed_location.0 > 0);
    assert_eq!(unconfirmed_location.1, 0);
    drop(db);
    let storage = TrieFileStorage::<StacksBlockId>::open_unconfirmed(
        output.database.to_str().unwrap(),
        MARFOpenOpts::default(),
    )
    .unwrap();
    let mut result = MARF::from_storage(storage);
    let actual = result
        .get_with_proof(&pending_tip, "pending")
        .unwrap()
        .unwrap();
    assert_eq!(actual.0, expected.0);
    assert_eq!(actual.1.to_hex(), expected.1.to_hex());
    assert_eq!(
        result.get(&pending_tip, "confirmed").unwrap(),
        Some(MARFValue([3; 40]))
    );
}

/// Preserve arbitrary 40-byte values, fork roots, proofs and unrelated SQL side stores.
#[test]
fn legacy_internal_and_external_forks() {
    for external in [false, true] {
        for compression in [false, true] {
            let directory = tempdir().unwrap();
            let path = directory.path().join("legacy.sqlite");
            let mut opts = MARFOpenOpts::default()
                .with_compression(compression)
                .with_mmap(true);
            opts.external_blobs = external;
            let mut marf = MARF::<StacksBlockId>::from_path(path.to_str().unwrap(), opts).unwrap();
            let mut expected = Vec::new();
            for n in 1..=5u8 {
                let parent = if n == 1 {
                    StacksBlockId::sentinel()
                } else {
                    StacksBlockId([if n == 5 { 2 } else { n - 1 }; 32])
                };
                let block = StacksBlockId([n; 32]);
                let mut tx = marf.begin_tx().unwrap();
                tx.begin(&parent, &block).unwrap();
                for index in 0..512 {
                    // Nonzero trailing bytes ensure Clarity's commitment padding rule is not applied.
                    tx.insert_batch(
                        &[format!("key-{index}")],
                        vec![MARFValue([n.wrapping_add(index as u8); 40])],
                    )
                    .unwrap();
                }
                tx.commit().unwrap();
                expected.push((
                    block.clone(),
                    marf.get_root_hash_at(&block).unwrap(),
                    marf.get_with_proof(&block, "key-400").unwrap().unwrap(),
                ));
            }
            marf.sqlite_conn().execute_batch("CREATE TABLE __fork_storage(key TEXT PRIMARY KEY, value TEXT); INSERT INTO __fork_storage VALUES('key','retained')").unwrap();
            drop(marf);
            let db = Connection::open(&path).unwrap();
            db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
            drop(db);
            if external {
                blockstack_lib::chainstate::stacks::index::direct_hash_index::build(&path).unwrap();
            }
            let original = fs::read(&path).unwrap();
            let config = Config {
                source: path.clone(),
                destination: directory.path().join("canonical"),
                clarity_values: false,
                max_trie_bytes: 64 * 1024 * 1024,
            };
            let result = migrate(&config).unwrap();
            assert_eq!(result.tries, 5);
            assert_eq!(
                fs::read(&path).unwrap(),
                original,
                "source must be unchanged"
            );
            let mut current = MARF::<StacksBlockId>::from_path(
                result.database.to_str().unwrap(),
                MARFOpenOpts::default(),
            )
            .unwrap();
            assert_eq!(current.record_format(), NodeRecordFormat::Optimized);
            for (block, root, (value, proof)) in expected {
                assert_eq!(current.get_root_hash_at(&block).unwrap(), root);
                let actual = current.get_with_proof(&block, "key-400").unwrap().unwrap();
                assert_eq!(actual.0, value);
                assert_eq!(actual.1.to_hex(), proof.to_hex());
            }
            let retained: String = current
                .sqlite_conn()
                .query_row(
                    "SELECT value FROM __fork_storage WHERE key='key'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(retained, "retained");
            assert!(migrate(&config).is_err(), "never replace published output");
        }
    }
}

/// Live journals and unsupported experimental stores fail before destination creation.
#[test]
fn rejects_unsafe_source_and_existing_output() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("legacy.sqlite");
    let marf =
        MARF::<StacksBlockId>::from_path(path.to_str().unwrap(), MARFOpenOpts::default()).unwrap();
    drop(marf);
    let wal = directory.path().join("legacy.sqlite-wal");
    fs::write(&wal, b"uncheckpointed").unwrap();
    let config = Config {
        source: path,
        destination: directory.path().join("canonical"),
        clarity_values: false,
        max_trie_bytes: 1024,
    };
    assert!(migrate(&config).is_err());
    assert!(!config.destination.exists());
}
