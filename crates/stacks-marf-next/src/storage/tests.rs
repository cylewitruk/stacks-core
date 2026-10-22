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

//! Cache invalidation and transaction regression tests.
use rusqlite::params;
use stacks_common::types::chainstate::StacksBlockId;

use super::*;
use crate::marf::{MARF, MARFOpenOpts, MarfConnection};
use crate::{ClarityMarfTrieId, MARFValue};

fn sidecar_valid(marf: &MARF<StacksBlockId>) -> i64 {
    marf.sqlite_conn()
        .query_row(
            "SELECT valid FROM marf_direct_hash_index WHERE id = 1",
            NO_PARAMS,
            |r| r.get(0),
        )
        .unwrap()
}

/// The direct-hash sidecar is retired inside the first writing
/// transaction. If that transaction rolls back, the retirement rolls back
/// with it, and the next write must issue it again (review finding).
#[test]
fn sidecar_retirement_is_redone_after_a_rolled_back_commit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("marf.sqlite");
    let mut marf = MARF::<StacksBlockId>::from_path(&path, MARFOpenOpts::default()).unwrap();
    marf.sqlite_conn()
        .execute_batch(
            "CREATE TABLE marf_direct_hash_index (id INTEGER PRIMARY KEY, valid INTEGER NOT NULL);
                 INSERT INTO marf_direct_hash_index (id, valid) VALUES (1, 1);",
        )
        .unwrap();
    let b1 = StacksBlockId([1u8; 32]);
    let value = MARFValue([7u8; 40]);

    // first commit fails after the retirement: a row for the block
    // appears underneath it, so the trie insert hits the UNIQUE index
    let mut tx = marf.begin_tx().unwrap();
    tx.begin(&StacksBlockId::sentinel(), &b1).unwrap();
    tx.insert("k", value.clone()).unwrap();
    tx.sqlite_tx()
        .execute(
            "INSERT INTO marf_data (block_hash, data, unconfirmed) VALUES (?1, ?2, 0)",
            params![&b1, &[] as &[u8]],
        )
        .unwrap();
    assert!(tx.commit().is_err());
    assert_eq!(
        sidecar_valid(&marf),
        1,
        "the failed commit rolled the retirement back"
    );

    // the next commit succeeds and must retire the sidecar
    let mut tx = marf.begin_tx().unwrap();
    tx.begin(&StacksBlockId::sentinel(), &b1).unwrap();
    tx.insert("k", value).unwrap();
    tx.commit().unwrap();
    assert_eq!(sidecar_valid(&marf), 0);
}
