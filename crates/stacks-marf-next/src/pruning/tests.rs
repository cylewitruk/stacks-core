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

//! Pruning size and root regression tests.
use std::fs;

use stacks_common::types::chainstate::StacksBlockId;
use stacks_common::types::sqlite::NO_PARAMS;
// Unit tests use the crate alias; the extraction guard uses the external crate.
use stacks_marf_next::marf::{MARF, MARFOpenOpts, MarfConnection, TrieHashCalculationMode};

use super::*;
use crate::archive::Archive;
use crate::{ClarityMarfTrieId, MARFValue};

fn bid(n: u8) -> StacksBlockId {
    let mut b = [0u8; 32];
    b[0] = n;
    b[31] = 1;
    StacksBlockId(b)
}

fn opts() -> MARFOpenOpts {
    MARFOpenOpts::new(TrieHashCalculationMode::Deferred, true).with_compression(true)
}

/// `PruneStats.blob_size` is the shared blob's length: the length
/// published in `marf_data`, and for a fresh destination the length of
/// the whole blobs file. (Regression: the blob's offset was reported,
/// zero for a fresh destination.) Reads, the root hash and proofs of the
/// pruned database must match the source.
#[test]
fn prune_stats_report_the_blob_length() {
    let dir = tempfile::tempdir().unwrap();
    let src_path = dir.path().join("src.sqlite");
    let mut src = MARF::<StacksBlockId>::from_path(&src_path, opts()).unwrap();
    let mut parent = StacksBlockId::sentinel();
    let mut expected: Vec<(String, MARFValue)> = vec![];
    for b in 1..=4u8 {
        let child = bid(b);
        let mut tx = src.begin_tx().unwrap();
        tx.begin(&parent, &child).unwrap();
        for i in 0..50u8 {
            let key = format!("key-{b}-{i}");
            let value = MARFValue([b.wrapping_mul(7).wrapping_add(i); 40]);
            tx.insert(&key, value.clone()).unwrap();
            expected.push((key, value));
        }
        tx.commit().unwrap();
        parent = child;
    }
    let tip = parent;
    let src_root = src.get_root_hash_at(&tip).unwrap();
    drop(src);

    let dst_path = dir.path().join("dst.sqlite");
    let stats = <MARF<StacksBlockId> as Prune<StacksBlockId>>::prune_to_path(
        &src_path,
        &dst_path,
        opts(),
        &tip,
        3,
        "test",
    )
    .unwrap();
    assert!(stats.node_count > 0);
    assert!(stats.blob_size > 0, "a fresh destination reported no bytes");
    let file_len = fs::metadata(<MARF<StacksBlockId> as Archive<StacksBlockId>>::blobs_path(
        &dst_path,
    ))
    .unwrap()
    .len();
    assert_eq!(
        stats.blob_size, file_len,
        "fresh destination: the blob is the whole file"
    );

    let mut dst = MARF::<StacksBlockId>::from_path(&dst_path, opts()).unwrap();
    let published: i64 = dst
        .sqlite_conn()
        .query_row(
            "SELECT DISTINCT external_length FROM marf_data WHERE unconfirmed = 0",
            NO_PARAMS,
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        stats.blob_size, published as u64,
        "published length differs"
    );
    assert_eq!(dst.get_root_hash_at(&tip).unwrap(), src_root);
    for (key, value) in &expected {
        assert_eq!(dst.get(&tip, key).unwrap().as_ref(), Some(value), "{key}");
    }
    assert!(dst.get(&tip, "absent").unwrap().is_none());
    // proofs need the archival ancestry, which a pruned database no
    // longer has: both implementations refuse them (fixture semantics)
    std::assert_matches!(
        dst.get_with_proof(&tip, &expected[7].0),
        Err(Error::UnsupportedOnPrunedMarf(_))
    );
}
