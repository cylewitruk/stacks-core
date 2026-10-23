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

use clarity::vm::database::SqliteConnection;
use rusqlite::params;
use stacks_common::codec::StacksMessageCodec;
use stacks_common::types::chainstate::StacksBlockId;
use stacks_marf_next::sqlite::SqliteAccess;
use stacks_marf_next::{
    ClarityMarfTrieId, MARF, MARFOpenOpts, MARFValue, MarfConnection, TrieMerkleProof,
};
#[test]
fn real_clarity_side_store_and_shared_sql_transaction() {
    let mut m = MARF::<StacksBlockId>::from_path(":memory:", MARFOpenOpts::default()).unwrap();
    SqliteConnection::initialize_conn(m.sqlite_conn()).unwrap();
    let tip = StacksBlockId([9; 32]);
    let value = MARFValue::from_value("real-side-value");
    {
        let mut t = m.begin_tx().unwrap();
        t.begin(&StacksBlockId::sentinel(), &tip).unwrap();
        SqliteConnection::put(t.sqlite_tx(), &value.to_hex(), "real-side-value").unwrap();
        SqliteConnection::insert_metadata(t.sqlite_tx(), &tip, "contract", "key", "metadata")
            .unwrap();
        t.insert("key", value.clone()).unwrap();
        assert!(!t.sqlite_tx().is_autocommit());
        assert_eq!(
            SqliteConnection::get(t.sqlite_tx(), &value.to_hex())
                .unwrap()
                .as_deref(),
            Some("real-side-value")
        );
        t.commit().unwrap();
    }
    assert_eq!(m.get(&tip, "key").unwrap(), Some(value.clone()));
    assert_eq!(
        SqliteConnection::get(m.sqlite_conn(), &value.to_hex())
            .unwrap()
            .as_deref(),
        Some("real-side-value")
    );
    {
        let t = m.begin_tx().unwrap();
        SqliteConnection::put(t.sqlite_tx(), "rollback", "gone").unwrap();
        t.rollback();
    }
    assert_eq!(
        SqliteConnection::get(m.sqlite_conn(), "rollback").unwrap(),
        None
    );
    {
        let mut t = m.begin_tx().unwrap();
        t.begin(&tip, &StacksBlockId([10; 32])).unwrap();
        SqliteConnection::put(t.sqlite_tx(), "forced", "gone").unwrap();
        t.sqlite_tx().execute_batch("ROLLBACK").unwrap();
        assert!(t.sqlite_tx().is_autocommit());
        assert!(t.commit().is_err());
    }
    assert_eq!(
        SqliteConnection::get(m.sqlite_conn(), "forced").unwrap(),
        None
    );
    let rows: u32 = m
        .sqlite_conn()
        .query_row(
            "SELECT COUNT(*) FROM marf_data WHERE block_hash = ?",
            params![StacksBlockId([10; 32])],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rows, 0);
    assert_eq!(m.get(&tip, "key").unwrap(), Some(value));
}
#[test]
fn golden_proof_and_layout() {
    let bytes = include_bytes!("fixtures/legacy-proof.bin");
    let proof =
        TrieMerkleProof::<StacksBlockId>::consensus_deserialize(&mut bytes.as_slice()).unwrap();
    let ptr = proof.0.as_ptr();
    let mut encoded = vec![];
    proof.consensus_serialize(&mut encoded).unwrap();
    assert_eq!(bytes.as_slice(), encoded);
    assert_eq!(ptr, proof.0.as_ptr());
    assert_eq!(
        std::mem::size_of::<stacks_marf_next::Id<StacksBlockId>>(),
        32
    );
    assert_eq!(
        std::mem::size_of::<stacks_marf_next::TrieMerkleProofType<StacksBlockId>>(),
        8224
    );
}

/// Canonical keys/roots and SQL text/blob acceptance match the frozen baseline.
#[test]
fn production_identity_hash_and_sql_compatibility() {
    fn check<T: stacks_marf_next::MarfTrieId>() {
        use marf_core::TrieId;
        use marf_core::sqlite::{SqlId, StoredId};
        use stacks_marf_next::Id;
        use stacks_marf_next::archive::Archive;
        use stacks_marf_next::pruning::Prune;
        assert_eq!(std::mem::size_of::<Id<T>>(), std::mem::size_of::<T>());
        assert_eq!(std::mem::size_of::<stacks_marf_next::ProofTriePtr<T>>(), 34);
        assert_eq!(
            std::mem::size_of::<stacks_marf_next::ProofTrieNode<T>>(),
            56
        );
        assert_eq!(
            std::mem::size_of::<stacks_marf_next::TrieMerkleProofType<T>>(),
            8224
        );
        for bytes in [[0; 32], [255; 32], {
            let mut b = [0; 32];
            b[31] = 10;
            b
        }] {
            let original = T::from_bytes(bytes);
            let wrapped = Id(original.clone());
            let db = rusqlite::Connection::open_in_memory().unwrap();
            db.execute_batch("CREATE TABLE ids(ordinary TEXT, archive BLOB)")
                .unwrap();
            db.execute(
                "INSERT INTO ids VALUES (?1,?2)",
                params![SqlId(&wrapped), wrapped.as_bytes().as_slice()],
            )
            .unwrap();
            let (stored, blob): (String, Vec<u8>) = db
                .query_row("SELECT ordinary, archive FROM ids", [], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
                .unwrap();
            assert_eq!(stored, original.to_string());
            assert_eq!(blob, bytes);
            for value in [
                stored.clone(),
                stored.to_uppercase(),
                stored
                    .chars()
                    .enumerate()
                    .map(|(i, c)| {
                        if i % 2 == 0 {
                            c.to_ascii_uppercase()
                        } else {
                            c
                        }
                    })
                    .collect(),
                "g".repeat(64),
                "a".repeat(63),
                "00".into(),
                "é".repeat(32),
            ] {
                let old = db.query_row("SELECT ?1", [&value], |r| r.get::<_, T>(0));
                let new = db.query_row("SELECT ?1", [&value], |r| r.get::<_, StoredId<Id<T>>>(0));
                assert_eq!(old.is_ok(), new.is_ok(), "{value}");
                match (old, new) {
                    (Ok(a), Ok(b)) => assert_eq!(a, b.0.0),
                    (Err(a), Err(b)) => {
                        assert_eq!(std::mem::discriminant(&a), std::mem::discriminant(&b))
                    }
                    _ => unreachable!(),
                }
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let src = directory.path().join("src.sqlite");
        let dst = directory.path().join("pruned.sqlite");
        let mut opts = MARFOpenOpts::default();
        opts.external_blobs = false;
        let mut m = MARF::<T>::from_path(&src, opts.clone()).unwrap();
        let mut bytes = [0; 32];
        bytes[31] = 10;
        let tip = T::from_bytes(bytes);
        m.begin(&T::sentinel(), &tip).unwrap();
        m.insert("leading-zero", MARFValue::from_value("value"))
            .unwrap();
        let root = m.seal().unwrap();
        assert_eq!(
            root.to_hex(),
            "dbe21e8e68f33f996229506c08de9b4d810a9a3d9a80dfc724f5c5500b63a0d9"
        );
        m.commit().unwrap();
        let key = format!(
            "__MARF_BLOCK_HASH_TO_HEIGHT::{}",
            marf_core::bytes::to_hex(&bytes)
        );
        assert_eq!(
            m.get(&tip, &key).unwrap().unwrap().decode_height().unwrap(),
            0
        );
        assert_eq!(m.get_block_at_height(0, &tip).unwrap(), Some(tip.clone()));
        let stored: String = m
            .sqlite_conn()
            .query_row(
                "SELECT block_hash FROM marf_data WHERE unconfirmed=0",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored, marf_core::bytes::to_hex(&bytes));
        <MARF<T> as Prune<T>>::prune_to_path(&src, &dst, opts.clone(), &tip, 0, "id").unwrap();
        let reopened = MARF::<T>::from_path(&dst, opts).unwrap();
        let (id, hash): (Vec<u8>, Vec<u8>) = reopened
            .sqlite_conn()
            .query_row(
                "SELECT block_hash,marf_root_hash FROM marf_squashed_blocks WHERE height=0",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(id, bytes);
        assert_eq!(hash, root.as_bytes());
        assert!(<MARF<T> as Archive<T>>::blobs_path(&dst).exists());
    }
    check::<StacksBlockId>();
    check::<stacks_common::types::chainstate::SortitionId>();
    check::<stacks_common::types::chainstate::BurnchainHeaderHash>();
}

/// Native helpers retain Stacks-wide VFS selection and read-only enforcement.
#[test]
#[cfg(unix)]
fn native_vfs_selection_and_sqlite_policy() {
    use stacks_common::util::db::{self, SqliteVfs};
    assert_eq!(db::set_sqlite_vfs(SqliteVfs::UnixExcl), Ok(()));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vfs.sqlite");
    let m = MARF::<StacksBlockId>::from_path(&path, MARFOpenOpts::default()).unwrap();
    let query = |db: &rusqlite::Connection, name: &str| {
        db.pragma_query_value(None, name, |r| r.get::<_, i64>(0))
            .unwrap()
    };
    // Match the legacy helper ordering, including WAL before page_size.
    // SQLite may retain its initial page size when WAL is already active.
    let reference = db::sqlite_open(
        dir.path().join("legacy-policy.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_CREATE,
        false,
    )
    .unwrap();
    db::sql_pragma(&reference, "mmap_size", &db::SQLITE_MMAP_SIZE).unwrap();
    db::sql_pragma(&reference, "page_size", &db::SQLITE_MARF_PAGE_SIZE).unwrap();
    reference
        .execute_batch("CREATE TABLE policy(value)")
        .unwrap();
    assert_eq!(
        marf_core::sqlite::SQLITE_MARF_PAGE_SIZE,
        db::SQLITE_MARF_PAGE_SIZE
    );
    assert_eq!(
        query(m.sqlite_conn(), "page_size"),
        query(&reference, "page_size")
    );
    assert_eq!(query(m.sqlite_conn(), "mmap_size"), 256 * 1024 * 1024);
    assert_eq!(query(m.sqlite_conn(), "synchronous"), 1);
    let readonly = m.reopen_readonly().unwrap();
    assert_eq!(query(readonly.sqlite_conn(), "query_only"), 1);
    assert!(
        readonly
            .sqlite_conn()
            .execute("CREATE TABLE forbidden(x)", [])
            .is_err()
    );
}
/// All reopened adapters retain the explicitly selected ancestor-check tip.
#[test]
fn reopened_connections_preserve_selected_block() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reopen.sqlite");
    let mut m = MARF::<StacksBlockId>::from_path(&path, MARFOpenOpts::default()).unwrap();
    let a = StacksBlockId([1; 32]);
    let b = StacksBlockId([2; 32]);
    m.begin(&StacksBlockId::sentinel(), &a).unwrap();
    m.commit().unwrap();
    m.begin(&a, &b).unwrap();
    m.commit().unwrap();
    m.open_block(&b).unwrap();
    m.reopen_readonly()
        .unwrap()
        .check_ancestor_block_hash(&a)
        .unwrap();
    m.reopen_connection()
        .unwrap()
        .check_ancestor_block_hash(&a)
        .unwrap();
    let tx = m.begin_tx().unwrap();
    tx.reopen_readonly()
        .unwrap()
        .check_ancestor_block_hash(&a)
        .unwrap();
    tx.rollback();
}

/// Integration hooks keep span hierarchy, sampling, captures and error-path drops.
#[test]
#[cfg(feature = "profiler")]
fn profiler_hooks_preserve_sampling_and_capture_controls() {
    use stacks_marf_next::profiler;
    use stacks_profiler::{ProfileStats, Profiler};
    fn walk<'a>(nodes: &'a [ProfileStats], out: &mut Vec<&'a ProfileStats>) {
        for node in nodes {
            out.push(node);
            walk(&node.children, out);
        }
    }
    Profiler::clear();
    Profiler::enable_record();
    let mut m = MARF::<StacksBlockId>::from_path(":memory:", MARFOpenOpts::default()).unwrap();
    let tip = StacksBlockId([8; 32]);
    m.begin(&StacksBlockId::sentinel(), &tip).unwrap();
    m.insert("capture", MARFValue([9; 40])).unwrap();
    m.commit().unwrap();
    for _ in 0..32 {
        assert!(m.get(&tip, "capture").unwrap().is_some());
    }
    std::assert_matches!(
        m.get_root_hash_at(&StacksBlockId([3; 32])),
        Err(stacks_marf_next::Error::NotFoundError)
    );
    let stats = Profiler::take_results().unwrap();
    let mut nodes = vec![];
    walk(&stats, &mut nodes);
    assert!(
        nodes
            .iter()
            .any(|n| n.name() == "get" && n.entered_count == 32)
    );
    assert!(
        nodes.iter().any(|n| n.name() == "site"
            && n.sampled_count > 0
            && n.sampled_count < n.entered_count)
    );
    let keys: Vec<_> = nodes
        .iter()
        .flat_map(|n| n.records.iter().map(|r| r.key))
        .collect();
    assert!(keys.contains(&"KEY"));
    assert!(keys.contains(&"BH"));
    assert!(keys.contains(&"MARF_NODE_HASH_DISK_READ"));
    // Align the site's sample phase, suppress three reads, then require the
    // next visible read to retain the first timed sample of its group.
    let mut calls: usize = nodes
        .iter()
        .filter(|n| n.name() == "site")
        .map(|n| n.entered_count)
        .sum();
    while !calls.is_multiple_of(8) {
        m.get(&tip, "capture").unwrap();
        calls += 1;
    }
    Profiler::take_results().unwrap();
    {
        let _suppression = Profiler::begin_suppression();
        for _ in 0..3 {
            m.get(&tip, "capture").unwrap();
        }
    }
    m.get(&tip, "capture").unwrap();
    let visible = Profiler::take_results().unwrap();
    let mut visible_nodes = vec![];
    walk(&visible, &mut visible_nodes);
    let site_samples: usize = visible_nodes
        .iter()
        .filter(|n| n.name() == "site")
        .map(|n| n.sampled_count)
        .sum();
    assert_eq!(
        site_samples, 1,
        "suppressed reads must not advance sampling"
    );
    let old = profiler::set_flags(0);
    m.get(&tip, "capture").unwrap();
    let stats = Profiler::take_results().unwrap();
    let mut nodes = vec![];
    walk(&stats, &mut nodes);
    assert!(nodes.iter().all(|n| n.records.is_empty()));
    profiler::set_flags(old);
    Profiler::disable_record();
}
