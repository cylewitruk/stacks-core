// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020-2026 Stacks Open Internet Foundation
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

use std::fs;

use rusqlite::{Connection, OpenFlags};

use super::*;
use crate::chainstate::stacks::index::file::*;
use crate::chainstate::stacks::index::test::marf::MarfTestExt as _;
use crate::chainstate::stacks::index::*;
use crate::util_lib::db::*;

fn db_path(test_name: &str) -> String {
    let path = format!("/tmp/{}.sqlite", test_name);
    path
}

fn setup_db(test_name: &str) -> Connection {
    let path = db_path(test_name);
    if fs::metadata(&path).is_ok() {
        fs::remove_file(&path).unwrap();
    }

    let mut db = sqlite_open(
        &path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        true,
    )
    .unwrap();
    trie_sql::create_tables_if_needed(&mut db).unwrap();
    db
}

#[test]
fn test_load_store_trie_blob() {
    let mut db = setup_db("test_load_store_trie_blob");
    let mut blobs =
        TrieFile::from_db_path(&db_path("test_load_store_trie_blob"), false, false).unwrap();
    trie_sql::migrate_tables_if_needed::<BlockHeaderHash>(&mut db).unwrap();

    blobs
        .store_trie_blob::<BlockHeaderHash>(&db, &BlockHeaderHash([0x01; 32]), &[1, 2, 3, 4, 5])
        .unwrap();
    blobs
        .store_trie_blob::<BlockHeaderHash>(
            &db,
            &BlockHeaderHash([0x02; 32]),
            &[10, 20, 30, 40, 50],
        )
        .unwrap();

    let block_id = trie_sql::get_block_identifier(&db, &BlockHeaderHash([0x01; 32])).unwrap();
    assert_eq!(blobs.get_trie_offset(&db, block_id).unwrap(), 0);

    let buf = blobs.read_trie_blob(&db, block_id).unwrap();
    assert_eq!(buf, vec![1, 2, 3, 4, 5]);

    let block_id = trie_sql::get_block_identifier(&db, &BlockHeaderHash([0x02; 32])).unwrap();
    assert_eq!(blobs.get_trie_offset(&db, block_id).unwrap(), 5);

    let buf = blobs.read_trie_blob(&db, block_id).unwrap();
    assert_eq!(buf, vec![10, 20, 30, 40, 50]);
}

#[test]
fn test_migrate_tables_readonly_succeeds_when_current() {
    let mut db = setup_db(function_name!());
    // First migrate in writable mode to bring schema to current version
    let previous_version = trie_sql::migrate_tables_if_needed::<BlockHeaderHash>(&mut db).unwrap();
    assert_eq!(previous_version, 1);
    // Now a read-only migration check should succeed
    trie_sql::ensure_no_migration_necessary::<BlockHeaderHash>(&mut db).unwrap();
}

#[test]
fn test_migrate_tables_readonly_fails_when_outdated() {
    let path = db_path("test_migrate_tables_readonly_fail");
    if fs::metadata(&path).is_ok() {
        fs::remove_file(&path).unwrap();
    }
    let mut db = sqlite_open(
        &path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        true,
    )
    .unwrap();
    trie_sql::create_tables_if_needed(&mut db).unwrap();
    // Don't migrate - schema is at version 1.
    // A read-only open should fail because the schema is outdated.
    let err = trie_sql::ensure_no_migration_necessary::<BlockHeaderHash>(&mut db).unwrap_err();
    assert!(
        matches!(&err, crate::chainstate::stacks::index::Error::CorruptionError(msg) if msg.contains("not compatible with read-only")),
        "instead got: {err}"
    );
}

#[test]
fn test_migrate_existing_trie_blobs() {
    let test_file = "/tmp/test_migrate_existing_trie_blobs.sqlite";
    let test_blobs_file = "/tmp/test_migrate_existing_trie_blobs.sqlite.blobs";
    if fs::metadata(&test_file).is_ok() {
        fs::remove_file(&test_file).unwrap();
    }
    if fs::metadata(&test_blobs_file).is_ok() {
        fs::remove_file(&test_blobs_file).unwrap();
    }

    let (data, last_block_header, root_header_map) = {
        let marf_opts = MARFOpenOpts::new(TrieHashCalculationMode::Deferred, false);

        let f = TrieFileStorage::open(test_file, marf_opts).unwrap();
        let mut marf = MARF::from_storage(f);

        // make data to insert
        let data = make_test_insert_data(128, 128);
        let mut last_block_header = BlockHeaderHash::sentinel();
        for (i, block_data) in data.iter().enumerate() {
            let mut block_hash_bytes = [0u8; 32];
            block_hash_bytes[0..8].copy_from_slice(&(i as u64).to_be_bytes());

            let block_header = BlockHeaderHash(block_hash_bytes);
            marf.begin(&last_block_header, &block_header).unwrap();

            for (key, value) in block_data.iter() {
                let path = TrieHash::from_key(key);
                let leaf = TrieLeaf::from_value(&[], value.clone());
                marf.insert_raw(path, leaf).unwrap();
            }
            marf.commit().unwrap();
            last_block_header = block_header;
        }

        let root_header_map =
            trie_sql::read_all_block_hashes_and_roots::<BlockHeaderHash>(marf.sqlite_conn())
                .unwrap();
        (data, last_block_header, root_header_map)
    };

    // migrate
    let mut marf_opts = MARFOpenOpts::new(TrieHashCalculationMode::Deferred, true);
    marf_opts.force_db_migrate = true;

    let f = TrieFileStorage::open(test_file, marf_opts).unwrap();
    let mut marf = MARF::from_storage(f);

    // blobs file exists
    assert!(fs::metadata(&test_blobs_file).is_ok());

    // verify that the new blob structure is well-formed
    let blob_root_header_map = {
        let blobs = TrieFile::from_db_path(test_file, false, false).unwrap();
        let blob_root_header_map = blobs
            .read_all_block_hashes_and_roots::<BlockHeaderHash>(marf.sqlite_conn())
            .unwrap();
        blob_root_header_map
    };

    assert_eq!(blob_root_header_map.len(), root_header_map.len());
    for (e1, e2) in blob_root_header_map.iter().zip(root_header_map.iter()) {
        assert_eq!(e1, e2);
    }

    // verify that we can read everything from the blobs
    for block_data in data.iter() {
        for (key, value) in block_data.iter() {
            let path = TrieHash::from_key(key);
            let marf_leaf = TrieLeaf::from_value(&[], value.clone());

            let leaf = marf.expect_path(&last_block_header, &path);

            assert_eq!(
                leaf.data.as_ref().expect("resolved test leaf").to_vec(),
                marf_leaf
                    .data
                    .as_ref()
                    .expect("resolved test leaf")
                    .to_vec()
            );
        }
    }
}

fn make_test_marf_with_single_block(
    test_file: &str,
    external_blobs: bool,
) -> (MARF<BlockHeaderHash>, BlockHeaderHash) {
    let marf_opts = MARFOpenOpts::new(TrieHashCalculationMode::Deferred, external_blobs);
    let f = TrieFileStorage::open(test_file, marf_opts).unwrap();
    let mut marf = MARF::from_storage(f);

    let block_header = BlockHeaderHash([0x11; 32]);
    marf.begin(&BlockHeaderHash::sentinel(), &block_header)
        .unwrap();
    marf.insert_raw(
        TrieHash::from_key("stable-node-seam"),
        TrieLeaf::from_value(&[], MARFValue::from_value("stable-node-seam-value")),
    )
    .unwrap();
    marf.commit().unwrap();

    (marf, block_header)
}

fn assert_reopened_root_stable_node_bytes(
    marf: &mut MARF<BlockHeaderHash>,
    block_header: &BlockHeaderHash,
) {
    let mut reopened = marf.reopen_connection().unwrap();
    let (root_ptr, expected_hash) = {
        let mut conn = reopened.connection();
        conn.open_block(block_header).unwrap();
        let root_ptr = conn.root_trieptr();
        let expected_hash = conn.read_node_hash(&root_ptr).unwrap();
        (root_ptr, expected_hash)
    };

    let decoded_node = {
        let mut conn = reopened.connection();
        conn.open_block(block_header).unwrap();
        let mut scratch = MarfReadState::new();
        conn.read_node_with_state(&root_ptr, &mut scratch)
            .unwrap()
            .into_owned_node()
            .unwrap()
            .0
    };
    let decoded_ref = TrieNodeRef::from(&decoded_node);

    {
        let mut scratch = MarfReadState::new();
        let stable_node = reopened.read_node(&root_ptr, &mut scratch).unwrap();
        assert_eq!(stable_node.node_type(), Some(TrieNodeID::Node256));
        assert_eq!(stable_node.hash, Some(expected_hash));
        assert_eq!(stable_node.is_leaf().unwrap(), decoded_ref.is_leaf());
        assert_eq!(stable_node.path_bytes().unwrap(), decoded_ref.path_bytes());
        assert_eq!(stable_node.ptrs().unwrap(), decoded_ref.ptrs());
        assert_eq!(stable_node.walk(0x11).unwrap(), decoded_ref.walk(0x11));
        assert_eq!(stable_node.walk(0xaa).unwrap(), decoded_ref.walk(0xaa));
        assert_eq!(
            stable_node.as_leaf().unwrap().map(|leaf| leaf
                .data
                .as_ref()
                .expect("resolved test leaf")
                .to_vec()),
            decoded_ref.as_leaf().map(|leaf| leaf
                .data
                .as_ref()
                .expect("resolved test leaf")
                .to_vec())
        );
        assert_eq!(
            stable_node.as_node_ref().unwrap().0.ptrs(),
            decoded_ref.ptrs()
        );
        assert_eq!(stable_node.into_owned_node().unwrap().0, decoded_node);
    }

    // Verify that a second read_node call returns consistent results (per-node path).
    {
        let mut scratch = MarfReadState::new();
        let node = reopened.read_node(&root_ptr, &mut scratch).unwrap();
        assert_eq!(node.node_type(), Some(TrieNodeID::Node256));
        assert_eq!(node.hash, Some(expected_hash));
        assert_eq!(node.into_owned_node().unwrap().0, decoded_node);
    }

    let path = TrieHash::from_key("stable-node-seam");
    let expected_value = MARFValue::from_value("stable-node-seam-value");

    let reopened_value = reopened.get(block_header, "stable-node-seam").unwrap();
    assert_eq!(reopened_value, Some(expected_value.clone()));

    let reopened_value_from_hash = reopened.get_from_hash(block_header, &path).unwrap();
    assert_eq!(reopened_value_from_hash, Some(expected_value.clone()));

    let proof_entry = reopened
        .get_with_proof(block_header, "stable-node-seam")
        .unwrap()
        .expect("expected reopened proof for stable-node-seam");
    assert_eq!(proof_entry.0, expected_value);
    assert!(!proof_entry.1 .0.is_empty());

    let proof_entry_from_hash = reopened
        .get_with_proof_from_hash(block_header, &path)
        .unwrap()
        .expect("expected reopened proof from hash for stable-node-seam");
    assert_eq!(proof_entry_from_hash.0, expected_value);
    assert!(!proof_entry_from_hash.1 .0.is_empty());
}

#[test]
fn test_reopened_connection_stable_blob_seam_sqlite_backed() {
    let test_file = "/tmp/test_reopened_connection_stable_blob_seam_sqlite_backed.sqlite";
    if fs::metadata(test_file).is_ok() {
        fs::remove_file(test_file).unwrap();
    }

    let (mut marf, block_header) = make_test_marf_with_single_block(test_file, false);
    assert_reopened_root_stable_node_bytes(&mut marf, &block_header);
}

#[test]
fn test_reopened_connection_stable_blob_seam_external_backed() {
    let test_file = "/tmp/test_reopened_connection_stable_blob_seam_external_backed.sqlite";
    let test_blobs_file =
        "/tmp/test_reopened_connection_stable_blob_seam_external_backed.sqlite.blobs";
    if fs::metadata(test_file).is_ok() {
        fs::remove_file(test_file).unwrap();
    }
    if fs::metadata(test_blobs_file).is_ok() {
        fs::remove_file(test_blobs_file).unwrap();
    }

    let (mut marf, block_header) = make_test_marf_with_single_block(test_file, true);
    assert!(fs::metadata(test_blobs_file).is_ok());
    assert_reopened_root_stable_node_bytes(&mut marf, &block_header);
}

#[test]
fn test_bulk_read_block_entries_rejects_negative_external_offset() {
    let mut db = setup_db("test_bulk_read_block_entries_rejects_negative_external_offset");
    trie_sql::migrate_tables_if_needed::<BlockHeaderHash>(&mut db).unwrap();

    let block_hash = BlockHeaderHash([0x11; 32]);
    db.execute(
        "INSERT INTO marf_data (block_hash, data, unconfirmed, external_offset, external_length) \
         VALUES (?1, ?2, 0, ?3, ?4)",
        rusqlite::params![block_hash.to_string(), Vec::<u8>::new(), -1i64, 0i64],
    )
    .unwrap();

    let err = trie_sql::bulk_read_block_entries::<BlockHeaderHash>(&db).unwrap_err();
    assert!(
        matches!(err, crate::chainstate::stacks::index::Error::OverflowError),
        "instead got: {err:?}"
    );
}

#[test]
fn test_update_squash_root_node_hash_requires_existing_row() {
    let mut db = setup_db("test_update_squash_root_node_hash_requires_existing_row");
    trie_sql::migrate_tables_if_needed::<BlockHeaderHash>(&mut db).unwrap();
    let hash = TrieHash::from_data(b"squash-root");

    let err = trie_sql::update_squash_root_node_hash(&db, &hash).unwrap_err();
    assert!(
        matches!(
            err,
            crate::chainstate::stacks::index::Error::CorruptionError(ref msg)
                if msg.contains("no marf_squash_info row exists")
        ),
        "instead got: {err:?}"
    );
}

#[test]
fn test_migrate_schema_3_creates_squash_tables_on_v2_db() {
    let mut db = setup_db(function_name!());

    // Bring schema to current first, then rewrite the version and drop the
    // schema-3 tables to simulate a pre-squash schema-v2 DB.
    trie_sql::migrate_tables_if_needed::<BlockHeaderHash>(&mut db).unwrap();
    db.execute_batch(
        "DROP TABLE IF EXISTS marf_squash_info; \
         DROP TABLE IF EXISTS marf_squashed_blocks; \
         UPDATE schema_version SET version = 2; \
         UPDATE migrated_version SET version = 2;",
    )
    .unwrap();

    let count_squash_tables = |db: &rusqlite::Connection| -> i64 {
        db.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' \
             AND name IN ('marf_squash_info', 'marf_squashed_blocks')",
            stacks_common::types::sqlite::NO_PARAMS,
            |row| row.get(0),
        )
        .unwrap()
    };
    assert_eq!(
        count_squash_tables(&db),
        0,
        "squash tables should be absent on simulated legacy v2 DB"
    );

    // Migrating from schema 2 to 3 must create the squash tables.
    let previous_version = trie_sql::migrate_tables_if_needed::<BlockHeaderHash>(&mut db).unwrap();
    assert_eq!(previous_version, 2);
    assert_eq!(
        count_squash_tables(&db),
        2,
        "migrate_tables_if_needed must add both squash tables to a v2 DB"
    );
    trie_sql::ensure_no_migration_necessary::<BlockHeaderHash>(&mut db).unwrap();

    // re-running again is a no-op.
    trie_sql::migrate_tables_if_needed::<BlockHeaderHash>(&mut db).unwrap();
    assert_eq!(count_squash_tables(&db), 2);
}

/// The parallel chunked reader must return exactly what per-block
/// `read_blob_header` reads, regardless of how the entries split
/// across worker threads.
#[test]
fn test_bulk_read_blob_headers_sorted_matches_sequential() {
    let dir = tempfile::tempdir().unwrap();
    let marf_path = dir.path().join("bulk_headers.sqlite");
    // 67 blocks: not a multiple of any plausible worker count, so the last
    // chunk is ragged.
    let (mut marf, _, _) = super::marf::setup_marf(marf_path.to_str().unwrap(), 67, 4);

    marf.with_conn(|conn| {
        let mut entries =
            trie_sql::bulk_read_block_entries::<StacksBlockId>(conn.sqlite_conn()).unwrap();
        conn.warm_trie_offsets_from_entries(&entries).unwrap();
        entries.sort_unstable_by_key(|e| (e.external_offset, e.block_id));
        assert!(entries.len() >= 67);

        let bulk = conn.bulk_read_blob_headers_sorted(&entries).unwrap();
        assert_eq!(bulk.len(), entries.len());
        for entry in &entries {
            let expected = conn.read_blob_header(entry.block_id).unwrap();
            assert_eq!(
                bulk.get(&entry.block_hash),
                Some(&expected),
                "bulk header mismatch for block {}",
                entry.block_hash
            );
        }
    });
}

/// An empty entry list is a valid request: zero entries yield an
/// empty map.
#[test]
fn test_bulk_read_blob_headers_sorted_empty_returns_empty() {
    let dir = tempfile::tempdir().unwrap();
    let marf_path = dir.path().join("bulk_headers_empty.sqlite");
    let (mut marf, _, _) = super::marf::setup_marf(marf_path.to_str().unwrap(), 1, 4);

    marf.with_conn(|conn| {
        let entries =
            trie_sql::bulk_read_block_entries::<StacksBlockId>(conn.sqlite_conn()).unwrap();
        // Empty slice of the right element type, without naming the private entry struct.
        let empty = entries.get(..0).unwrap();
        let headers = conn.bulk_read_blob_headers_sorted(empty).unwrap();
        assert!(headers.is_empty());
    });
}
