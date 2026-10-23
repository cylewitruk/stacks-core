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

//! Facade error classifications reach both ordinary and transactional Clarity stores.
use std::io::{Seek, SeekFrom, Write};

use clarity::vm::database::{ClarityBackingStore, SqliteConnection};
use stacks_common::types::chainstate::{StacksBlockId, TrieHash};
use stacks_marf::sqlite::SqliteAccess;
use stacks_marf::{ClarityMarfTrieId, Error, MARFOpenOpts, MARFValue, MarfConnection};

use super::{MarfedKV, PersistentWritableMarfStore, ReadOnlyMarfStore};

/// Build two committed blocks with a fresh key and an ancestral sibling.
fn fixture(path: &str) -> (MarfedKV, StacksBlockId) {
    let mut opts = MARFOpenOpts::default();
    opts.external_blobs = false;
    opts.compress = false;
    let mut kv = MarfedKV::open(path, None, Some(opts)).unwrap();
    let mut parent = StacksBlockId::sentinel();
    for (n, key) in [(1, "ancestral"), (2, "fresh")] {
        let next = StacksBlockId([n; 32]);
        let value = MARFValue::from_value(key);
        let mut tx = kv.marf.begin_tx().unwrap();
        tx.begin(&parent, &next).unwrap();
        SqliteConnection::put(tx.sqlite_tx(), &value.to_hex(), key).unwrap();
        tx.insert(key, value).unwrap();
        tx.commit().unwrap();
        parent = next;
    }
    (kv, parent)
}
#[test]
fn proof_result_classifications_survive_the_domain_boundary() {
    for scenario in [
        "normal",
        "missing-side-value",
        "proof-source-not-found",
        "corruption",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let (mut kv, tip) = fixture(directory.path().to_str().unwrap());
        match scenario {
            "missing-side-value" => {
                kv.marf
                    .sqlite_conn()
                    .execute(
                        "DELETE FROM data_table WHERE key=?1",
                        [MARFValue::from_value("fresh").to_hex()],
                    )
                    .unwrap();
            }
            "proof-source-not-found" => {
                kv.marf
                    .sqlite_conn()
                    .execute(
                        "DELETE FROM marf_data WHERE block_hash=?1",
                        [StacksBlockId([1; 32])],
                    )
                    .unwrap();
            }
            "corruption" => {
                // MarfedKV always uses external blobs. Keep the valid record
                // length and corrupt only the persisted root's node indicator.
                let offset: u64 = kv
                    .marf
                    .sqlite_conn()
                    .query_row(
                        "SELECT external_offset FROM marf_data WHERE block_hash=?1",
                        [tip.clone()],
                        |r| r.get(0),
                    )
                    .unwrap();
                let path = format!("{}.blobs", kv.marf.get_db_path().display());
                let mut file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
                file.seek(SeekFrom::Start(offset + 68)).unwrap();
                file.write_all(&[7]).unwrap();
                file.sync_all().unwrap();
            }
            _ => {}
        }
        // Reopen to ensure tests use persisted records instead of pre-mutation caches.
        kv.marf = kv.marf.reopen_readonly().unwrap();
        if scenario == "proof-source-not-found" {
            assert_eq!(
                kv.marf.get(&tip, "fresh").unwrap(),
                Some(MARFValue::from_value("fresh"))
            );
            // A successful lookup followed by proof failure exercises Source(NotFound),
            // rather than the facade's missing-key shortcut.
            std::assert_matches!(
                kv.marf.get_with_proof(&tip, "fresh"),
                Err(Error::NotFoundError)
            );
        }
        for transactional in [false, true] {
            // A writable native connection is needed for begin_tx, but all calls below read.
            if transactional {
                drop(kv.marf);
                let (opened, _) = fixture_empty_open(directory.path().to_str().unwrap(), &tip);
                kv.marf = opened;
            }
            let run = |store: &mut dyn ClarityBackingStore| {
                for absent in [false, true] {
                    let key = if absent { "absent" } else { "fresh" };
                    for from_path in [false, true] {
                        let result = if from_path {
                            store.get_data_with_proof_from_path(&TrieHash::from_key(key))
                        } else {
                            store.get_data_with_proof(key)
                        };
                        if scenario == "corruption" || (!absent && scenario == "missing-side-value")
                        {
                            assert!(
                                result.is_err(),
                                "{scenario}, tx={transactional}, path={from_path}"
                            );
                        } else if absent || scenario == "proof-source-not-found" {
                            assert!(
                                result.unwrap().is_none(),
                                "{scenario}, tx={transactional}, path={from_path}"
                            );
                        } else {
                            assert_eq!(result.unwrap().unwrap().0, "fresh");
                        }
                    }
                }
            };
            if transactional {
                let tx = kv.marf.begin_tx().unwrap();
                let mut store = PersistentWritableMarfStore {
                    chain_tip: tip.clone(),
                    marf: tx,
                };
                run(&mut store);
            } else {
                let mut store = ReadOnlyMarfStore {
                    chain_tip: tip.clone(),
                    marf: &mut kv.marf,
                };
                run(&mut store);
            }
        }
    }
}
/// Reopen existing data without recreating the fixture or repairing corruption.
fn fixture_empty_open(
    path: &str,
    tip: &StacksBlockId,
) -> (stacks_marf::MARF<StacksBlockId>, StacksBlockId) {
    let mut opts = MARFOpenOpts::default();
    opts.external_blobs = false;
    opts.compress = false;
    let kv = MarfedKV::open(path, Some(tip), Some(opts)).unwrap();
    (kv.marf, tip.clone())
}
