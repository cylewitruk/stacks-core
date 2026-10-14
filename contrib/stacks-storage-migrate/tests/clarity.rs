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

//! Direct Clarity migration without intermediate extent formats.

use std::fs;

use blockstack_lib::chainstate::stacks::index::marf::{MARF, MARFOpenOpts, MarfConnection};
use blockstack_lib::chainstate::stacks::index::{ClarityMarfTrieId, MARFValue};
use blockstack_lib::clarity_vm::clarity::ClarityMarfStoreTransaction;
use blockstack_lib::clarity_vm::database::binary_value_store;
use blockstack_lib::clarity_vm::database::marf::MarfedKV;
use clarity::vm::database::{
    ClarityBackingStore, DataStoreEntry, DataStoreValue, SqliteConnection,
};
use rusqlite::{Connection, params};
use stacks_common::types::chainstate::StacksBlockId;
use stacks_storage_migrate::{Config, migrate};
use tempfile::tempdir;

/// Both legacy blob backends produce portable Clarity stores with roots and value history intact.
#[test]
fn legacy_to_combined_clarity_is_portable_and_writable() {
    for (external, binary) in [(false, false), (true, false), (false, true), (true, true)] {
        let directory = tempdir().unwrap();
        let path = directory.path().join("marf.sqlite");
        let mut options = MARFOpenOpts::default()
            .with_mmap(true)
            .with_compression(true);
        options.external_blobs = external;
        let mut marf = MARF::<StacksBlockId>::from_path(path.to_str().unwrap(), options).unwrap();
        SqliteConnection::initialize_conn(marf.sqlite_conn()).unwrap();
        if binary {
            binary_value_store::initialize_empty(marf.sqlite_conn()).unwrap();
        }
        let mut history = Vec::new();
        for height in 1..=6u8 {
            let parent = if height == 1 {
                StacksBlockId::sentinel()
            } else {
                StacksBlockId([if height == 6 { 2 } else { height - 1 }; 32])
            };
            let block = StacksBlockId([height; 32]);
            let mut tx = marf.begin_tx().unwrap();
            tx.begin(&parent, &block).unwrap();
            let large = format!("{height}:{}", "external".repeat(128));
            for n in 0..512 {
                let text = if n % 2 == 0 { "small" } else { &large };
                let hash = MARFValue::from_value(text);
                if binary {
                    binary_value_store::put_entries(
                        tx.sqlite_tx(),
                        vec![DataStoreEntry {
                            key: format!("key-{n}"),
                            value: DataStoreValue::Canonical(text.into()),
                        }],
                    )
                    .unwrap();
                } else {
                    tx.sqlite_tx()
                        .execute(
                            "INSERT OR IGNORE INTO data_table(key,value) VALUES(?1,?2)",
                            params![hash.to_hex(), text],
                        )
                        .unwrap();
                }
                tx.insert_batch(&[format!("key-{n}")], vec![hash]).unwrap();
            }
            tx.commit().unwrap();
            history.push((
                block.clone(),
                marf.get_root_hash_at(&block).unwrap(),
                marf.get_with_proof(&block, "key-1")
                    .unwrap()
                    .unwrap()
                    .1
                    .to_hex(),
                large,
            ));
        }
        marf.sqlite_conn()
            .execute(
                "INSERT INTO metadata_table(key,blockhash,value) VALUES('nullable',NULL,NULL)",
                [],
            )
            .unwrap();
        drop(marf);
        let db = Connection::open(&path).unwrap();
        db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
        drop(db);
        let before = fs::read(&path).unwrap();
        let config = Config {
            source: path.clone(),
            destination: directory.path().join("converted"),
            max_trie_bytes: 64 * 1024 * 1024,
            clarity_values: true,
        };
        let outcome = migrate(&config).unwrap();
        assert_eq!(outcome.tries, 6);
        assert_eq!(before, fs::read(&path).unwrap());
        assert!(!config.destination.join("memberships.scratch").exists());
        // Registrations must survive moving the complete directory, including PtrHash.
        let moved = directory.path().join("portable");
        fs::rename(&config.destination, &moved).unwrap();
        let mut current = MarfedKV::open(moved.to_str().unwrap(), None, None).unwrap();
        for (block, root, proof, large) in &history {
            assert_eq!(current.get_marf().get_root_hash_at(block).unwrap(), *root);
            assert!(
                current
                    .get_marf()
                    .get_leaf_by_key(block, "key-0")
                    .unwrap()
                    .unwrap()
                    .inline
                    .is_some()
            );
            assert!(
                current
                    .get_marf()
                    .get_leaf_by_key(block, "key-1")
                    .unwrap()
                    .unwrap()
                    .value_id
                    .is_some()
            );
            let mut store = current.begin_read_only(Some(block));
            assert_eq!(store.get_data("key-0").unwrap().as_deref(), Some("small"));
            let (actual, bytes) = store.get_data_with_proof("key-1").unwrap().unwrap();
            assert_eq!(actual, *large);
            assert_eq!(stacks_common::util::hash::to_hex(&bytes), *proof);
        }
        let tip = &history.last().unwrap().0;
        let next = StacksBlockId([7; 32]);
        let mut write = current.begin(tip, &next);
        write
            .put_all_data(vec![("key-1".into(), "new external value".repeat(64))])
            .unwrap();
        write.seal_trie();
        write.commit_to_processed_block(&next).unwrap();
        drop(current);
        let mut current = MarfedKV::open(moved.to_str().unwrap(), Some(&next), None).unwrap();
        assert_eq!(
            current
                .begin_read_only(Some(&next))
                .get_data("key-1")
                .unwrap()
                .unwrap(),
            "new external value".repeat(64)
        );
        let db = current.get_marf().sqlite_conn();
        let rows: u64 = db
            .query_row("SELECT COUNT(*) FROM data_table", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 0);
        let old: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name IN ('canonical_inline_values','canonical_external_values','canonical_external_hash','canonical_inline_hash','clarity_extent_format'))", [], |row| row.get(0)).unwrap();
        assert!(!old);
    }
}
