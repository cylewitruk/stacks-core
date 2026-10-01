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

//! Whole-chainstate publication and preservation of unrelated sibling files.

use std::fs;
use std::path::Path;

use blockstack_lib::chainstate::stacks::index::marf::{MARF, MARFOpenOpts, MarfConnection};
use blockstack_lib::chainstate::stacks::index::{ClarityMarfTrieId, MARFValue};
use clarity::vm::database::SqliteConnection;
use rusqlite::{Connection, params};
use stacks_common::types::chainstate::StacksBlockId;
use stacks_storage_migrate::{migrate_chainstate, verify};
use tempfile::tempdir;

/// Four distinct owners include both legacy physical backends and an external Clarity value.
fn fixture(root: &Path) -> Vec<(&'static str, bool)> {
    let owners = vec![
        ("chainstate/vm/clarity/marf.sqlite", true),
        ("chainstate/vm/index.sqlite", false),
        ("burnchain/sortition/marf.sqlite", false),
        ("chainstate/headers/marf.sqlite", false),
    ];
    for (number, (relative, clarity)) in owners.iter().enumerate() {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut opts = MARFOpenOpts::default();
        opts.external_blobs = number % 2 == 0;
        let mut marf = MARF::<StacksBlockId>::from_path(path.to_str().unwrap(), opts).unwrap();
        if *clarity {
            SqliteConnection::initialize_conn(marf.sqlite_conn()).unwrap();
        }
        let value = "canonical migration external value".repeat(16);
        let commitment = MARFValue::from_value(&value);
        let block = StacksBlockId([1; 32]);
        let mut tx = marf.begin_tx().unwrap();
        tx.begin(&StacksBlockId::sentinel(), &block).unwrap();
        tx.insert_batch(&["key".into()], vec![commitment.clone()])
            .unwrap();
        if *clarity {
            tx.sqlite_tx()
                .execute(
                    "INSERT INTO data_table(key,value) VALUES(?1,?2)",
                    params![commitment.to_hex(), value],
                )
                .unwrap();
        }
        tx.commit().unwrap();
        drop(marf);
        let db = Connection::open(&path).unwrap();
        db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
    }
    fs::write(
        root.join("chainstate/vm/index.sqlite.notes"),
        b"unrelated sibling",
    )
    .unwrap();
    owners
}

/// All owners migrate once; already canonical input can also be copied without conversion.
#[test]
fn all_marfs_migrate_and_canonical_copy_is_portable() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("source");
    let owners = fixture(&source);
    let destination = temp.path().join("canonical");
    let source_db = source.join(owners[0].0);
    let before = fs::read(&source_db).unwrap();
    assert_eq!(
        migrate_chainstate(&source, &destination, 64 * 1024 * 1024).unwrap(),
        4
    );
    assert_eq!(fs::read(source_db).unwrap(), before);
    assert_eq!(
        fs::read(destination.join("chainstate/vm/index.sqlite.notes")).unwrap(),
        b"unrelated sibling"
    );
    for (relative, clarity) in &owners {
        verify(&destination.join(relative), *clarity).unwrap();
    }
    assert!(migrate_chainstate(&source, &destination, 64 * 1024 * 1024).is_err());
    let moved = temp.path().join("portable");
    assert_eq!(
        migrate_chainstate(&destination, &moved, 64 * 1024 * 1024).unwrap(),
        4
    );
    for (relative, clarity) in owners {
        verify(&moved.join(relative), clarity).unwrap();
    }
    fs::write(
        moved.join("chainstate/vm/index.sqlite.notes"),
        b"changed in copy",
    )
    .unwrap();
    assert_eq!(
        fs::read(destination.join("chainstate/vm/index.sqlite.notes")).unwrap(),
        b"unrelated sibling"
    );
}

/// Live input and nested output fail before exposing an incomplete destination.
#[test]
fn unsafe_chainstate_is_not_published() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("source");
    fixture(&source);
    assert!(migrate_chainstate(&source, &source.join("nested"), 1024).is_err());
    let output = temp.path().join("output");
    fs::write(source.join("chainstate/vm/index.sqlite-wal"), b"live").unwrap();
    assert!(migrate_chainstate(&source, &output, 1024).is_err());
    assert!(!output.exists());
}

/// An archived Clarity database is never mistaken for another generic active MARF.
#[test]
fn archived_clarity_owner_fails_preflight() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("source");
    fixture(&source);
    fs::copy(
        source.join("chainstate/vm/clarity/marf.sqlite"),
        source.join("chainstate/vm/clarity/marf.legacy.sqlite"),
    )
    .unwrap();
    let destination = temp.path().join("output");
    let error = migrate_chainstate(&source, &destination, 64 * 1024 * 1024).unwrap_err();
    assert!(error.to_string().contains("archived MARF"));
    assert!(!destination.exists());
}
