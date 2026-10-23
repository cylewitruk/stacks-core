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

//! Neutral package contracts, checked ancestry values and native-session defenses.
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};

use marf_core::archive::{ArchiveSource, ArchiveTransaction};
use marf_core::marf::{
    BLOCK_HASH_TO_HEIGHT_MAPPING_KEY, BLOCK_HEIGHT_TO_HASH_MAPPING_KEY, OWN_BLOCK_HEIGHT_KEY,
};
use marf_core::observe::{Observer, Operation};
use marf_core::sqlite::{SqlId, SqliteAccess};
use marf_core::{Error, MARF, MARFOpenOpts, MARFValue, MarfConnection, TrieHash, TrieId, TrieId32};
use rusqlite::params;

/// A display policy deliberately unrelated to authenticated identifier bytes.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct DisplayId([u8; 32]);
impl fmt::Display for DisplayId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("application-name")
    }
}
impl TrieId for DisplayId {
    fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
    fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}
fn root<I: TrieId>() -> TrieHash {
    let mut m = MARF::<I>::from_path(":memory:", MARFOpenOpts::default()).unwrap();
    let tip = I::from_bytes([1; 32]);
    m.begin(&I::sentinel(), &tip).unwrap();
    m.insert("user", MARFValue::from_value("value")).unwrap();
    let root = m.seal().unwrap();
    m.commit().unwrap();
    root
}
#[test]
fn identifier_display_cannot_change_authenticated_state() {
    assert_eq!(root::<DisplayId>(), root::<TrieId32>());
}

fn history() -> (MARF<TrieId32>, TrieId32, TrieId32) {
    let mut m = MARF::<TrieId32>::from_path(":memory:", MARFOpenOpts::default()).unwrap();
    let a = TrieId32([1; 32]);
    let b = TrieId32([2; 32]);
    m.begin(&TrieId32::sentinel(), &a).unwrap();
    m.insert("user", MARFValue::from_value("value")).unwrap();
    m.commit().unwrap();
    m.begin(&a, &b).unwrap();
    (m, a, b)
}
#[test]
fn persisted_height_and_identifier_padding_return_corruption() {
    for which in 0..3 {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("marf.sqlite");
        let mut opts = MARFOpenOpts::default();
        opts.external_blobs = false;
        opts.compress = false;
        let mut m = MARF::<TrieId32>::from_path(&path, opts).unwrap();
        let a = TrieId32([1; 32]);
        let b = TrieId32([2; 32]);
        m.begin(&TrieId32::sentinel(), &a).unwrap();
        m.commit().unwrap();
        m.begin(&a, &b).unwrap();
        m.commit().unwrap();
        let key = match which {
            0 => OWN_BLOCK_HEIGHT_KEY.to_string(),
            1 => format!("{BLOCK_HASH_TO_HEIGHT_MAPPING_KEY}::{}", a.to_hex()),
            _ => format!("{BLOCK_HEIGHT_TO_HASH_MAPPING_KEY}::0"),
        };
        corrupt_persisted_padding(&mut m, &b, &key);
        let mut reopened = m.reopen_readonly().unwrap();
        let result = if which == 0 {
            reopened.get_block_height(&b, &b).map(|v| v.is_some())
        } else if which == 1 {
            reopened.get_block_height(&a, &b).map(|v| v.is_some())
        } else {
            reopened.get_block_at_height(0, &b).map(|v| v.is_some())
        };
        std::assert_matches!(result, Err(Error::CorruptionError(_)));
    }
    let (mut m, _, b) = history();
    for height in [0, u32::MAX] {
        m.insert(OWN_BLOCK_HEIGHT_KEY, MARFValue::from(height))
            .unwrap();
        assert_eq!(m.get_block_height(&b, &b).unwrap(), Some(height));
    }
}
#[test]
fn native_rollback_is_detected_before_flush_and_arena_is_retained() {
    let (mut m, _, tip) = history();
    m.drop_current();
    let parent = TrieId32([1; 32]);
    let value = MARFValue::from_value("future");
    {
        let mut tx = m.begin_tx().unwrap();
        tx.begin(&parent, &tip).unwrap();
        tx.insert("future", value.clone()).unwrap();
        tx.sqlite_tx().execute_batch("ROLLBACK").unwrap();
        std::assert_matches!(tx.commit(), Err(Error::CorruptionError(_)));
    }
    let count: u32 = m
        .sqlite_conn()
        .query_row(
            "SELECT COUNT(*) FROM marf_data WHERE block_hash=?",
            params![SqlId(&tip)],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
    assert_eq!(m.get(&tip, "future").unwrap(), Some(value));
    m.drop_current();
    std::assert_matches!(m.get_root_hash_at(&tip), Err(Error::NotFoundError));
}
#[test]
fn dropped_and_rolled_back_archive_ids_can_be_reused() {
    let mut m = MARF::<TrieId32>::from_path(":memory:", MARFOpenOpts::default()).unwrap();
    let old = TrieId32([1; 32]);
    let new = TrieId32([2; 32]);
    for rollback in [false, true] {
        let old_id = {
            let mut tx = m.begin_tx().unwrap();
            let id = tx.insert_placeholder(&old).unwrap();
            tx.connection().open_block(&old).unwrap();
            if rollback {
                tx.rollback();
            }
            id
        };
        let mut tx = m.begin_tx().unwrap();
        let id = tx.insert_placeholder(&new).unwrap();
        assert_eq!(old_id, id);
        std::assert_matches!(tx.connection().open_block(&old), Err(Error::NotFoundError));
        tx.connection().open_block(&new).unwrap();
        tx.rollback();
    }
    assert!(m.confirmed_blocks().unwrap().is_empty());
}
static DROPPED: AtomicUsize = AtomicUsize::new(0);
static TIMED: AtomicUsize = AtomicUsize::new(0);
static COUNTED: AtomicUsize = AtomicUsize::new(0);
/// An independently owned diagnostic guard.
struct Guard;
impl Drop for Guard {
    fn drop(&mut self) {
        DROPPED.fetch_add(1, Ordering::Relaxed);
    }
}
struct Capture;
impl Observer for Capture {
    const ENABLED: bool = true;
    type Span = Guard;
    fn enter(_: Operation, timed: bool) -> Guard {
        if timed {
            TIMED.fetch_add(1, Ordering::Relaxed);
        } else {
            COUNTED.fetch_add(1, Ordering::Relaxed);
        }
        Guard
    }
}
struct Disabled;
impl Observer for Disabled {
    const ENABLED: bool = false;
    type Span = ();
    fn enter(_: Operation, _: bool) {
        panic!("disabled observer called")
    }
}
#[test]
fn static_observers_preserve_nested_error_drop_and_sampling() {
    let mut m = MARF::<TrieId32, Capture>::from_path(":memory:", MARFOpenOpts::default()).unwrap();
    let tip = TrieId32([1; 32]);
    m.begin(&TrieId32::sentinel(), &tip).unwrap();
    m.insert("key", MARFValue::from_value("value")).unwrap();
    m.commit().unwrap();
    for _ in 0..32 {
        m.get(&tip, "key").unwrap();
    }
    std::assert_matches!(
        m.get_root_hash_at(&TrieId32([9; 32])),
        Err(Error::NotFoundError)
    );
    assert!(DROPPED.load(Ordering::Relaxed) > 0);
    assert!(TIMED.load(Ordering::Relaxed) > 0);
    assert!(COUNTED.load(Ordering::Relaxed) > 0);
    let mut disabled =
        MARF::<TrieId32, Disabled>::from_path(":memory:", MARFOpenOpts::default()).unwrap();
    std::assert_matches!(disabled.get_root_hash_at(&tip), Err(Error::NotFoundError));
}

/// Inject malformed persisted bytes through a separate test-only corruption path.
/// Ordinary native clients must not mutate MARF-owned records this way.
fn corrupt_persisted_padding(m: &mut MARF<TrieId32>, tip: &TrieId32, key: &str) {
    use marf_core::access::{AuthenticatedRead, TrieLocation};
    use marf_core::node::Node;
    let path = TrieHash::from_key(key);
    let (block, offset, path_len) = m.with_conn(|view| {
        let mut site = view.select_trie(tip).unwrap();
        let mut ptr = view.root_pointer(site);
        let mut depth = 0;
        loop {
            if ptr.is_back() {
                site = TrieLocation::Block(ptr.back_block);
                ptr = ptr.without_back();
            }
            let node = view.read_node(site, &ptr).unwrap();
            match node {
                Node::Leaf(leaf) => {
                    let TrieLocation::Block(block) = site else {
                        panic!("expected persisted leaf")
                    };
                    break (block, ptr.ptr, leaf.path.len());
                }
                Node::Branch(branch) => {
                    depth += branch.path.len();
                    ptr = *branch.child(path.as_bytes()[depth]).unwrap();
                    depth += 1;
                }
            }
        }
    });
    let mut data: Vec<u8> = m
        .sqlite_conn()
        .query_row(
            "SELECT data FROM marf_data WHERE block_id=?",
            [block],
            |r| r.get(0),
        )
        .unwrap();
    data[offset as usize + 32 + 1 + 1 + path_len + 39] = 1;
    m.sqlite_conn()
        .execute(
            "UPDATE marf_data SET data=? WHERE block_id=?",
            params![data, block],
        )
        .unwrap();
}

#[test]
fn failed_archive_callback_never_publishes_bytes_or_metadata() {
    use marf_core::archive::{ArchiveMetadata, ArchiveWriteError};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("archive.sqlite");
    let mut opts = MARFOpenOpts::default();
    opts.external_blobs = true;
    let mut m = MARF::<TrieId32>::from_path(&path, opts).unwrap();
    let tip = TrieId32([4; 32]);
    {
        let mut tx = m.begin_tx().unwrap();
        tx.insert_placeholder(&tip).unwrap();
        let result = tx.write_shared_blob_with(|blob, base| {
            blob.write_at_unsynced(base, b"unreachable").unwrap();
            Err::<u64, _>(std::io::Error::from(std::io::ErrorKind::InvalidData))
        });
        std::assert_matches!(result,Err(ArchiveWriteError::Callback(ref e)) if e.kind()==std::io::ErrorKind::InvalidData);
    }
    assert!(m.confirmed_blocks().unwrap().is_empty());
    assert!(m.sqlite_conn().read_prune_info().unwrap().is_none());
    let rows: u32 = m
        .sqlite_conn()
        .query_row("SELECT COUNT(*) FROM marf_squashed_blocks", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(rows, 0);
    // The append may remain physically present; it has no published location.
    let mut tx = m.begin_tx().unwrap();
    tx.insert_placeholder(&tip).unwrap();
    tx.rollback();
    assert!(m.confirmed_blocks().unwrap().is_empty());
}
