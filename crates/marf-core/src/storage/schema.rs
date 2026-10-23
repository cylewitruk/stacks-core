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

//! Database creation, version checks and blob migration.
use std::collections::HashMap;
use std::path::Path;
use std::{fs, io};

use rusqlite::{Connection, OpenFlags, params};

use super::write::external_blobs_length;
use super::{Blobs, Scratch, Store, u64_sql};
use crate::archive::ArchiveMetadata;
use crate::observe::Observer;
use crate::sqlite::{
    NO_PARAMS, SQLITE_MARF_PAGE_SIZE, SQLITE_MMAP_SIZE, sql_pragma, sqlite_open, tx_begin_immediate,
};
use crate::{Error, TrieId, corrupt};

const SQL_MARF_DATA_TABLE: &str = "
CREATE TABLE IF NOT EXISTS marf_data (
   block_id INTEGER PRIMARY KEY,
   block_hash TEXT UNIQUE NOT NULL,
   -- the trie itself.
   -- if not used, then set to a zero-byte entry.
   data BLOB NOT NULL,
   unconfirmed INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS block_hash_marf_data ON marf_data(block_hash);
CREATE INDEX IF NOT EXISTS unconfirmed_marf_data ON marf_data(unconfirmed);
";
const SQL_MARF_MINED_TABLE: &str = "
CREATE TABLE IF NOT EXISTS mined_blocks (
   block_id INTEGER PRIMARY KEY,
   block_hash TEXT UNIQUE NOT NULL,
   data BLOB NOT NULL
);

CREATE INDEX IF NOT EXISTS block_hash_mined_blocks ON mined_blocks(block_hash);
";
const SQL_EXTENSION_LOCKS_TABLE: &str = "
CREATE TABLE IF NOT EXISTS block_extension_locks (block_hash TEXT PRIMARY KEY);
";
const SQL_MARF_DATA_TABLE_SCHEMA_2: &str = "
-- pointer to a .blobs file with the externally-stored blob data.
-- if not used, then set to 1.
CREATE TABLE IF NOT EXISTS schema_version (
    version INTEGER DEFAULT 1 NOT NULL
);
-- migrated_version tracks the schema-2 external-blob data migration.
CREATE TABLE IF NOT EXISTS migrated_version (
    version INTEGER DEFAULT 1 NOT NULL
);
ALTER TABLE marf_data ADD COLUMN external_offset INTEGER DEFAULT 0 NOT NULL;
ALTER TABLE marf_data ADD COLUMN external_length INTEGER DEFAULT 0 NOT NULL;
CREATE INDEX IF NOT EXISTS index_external_offset ON marf_data(external_offset);

INSERT OR REPLACE INTO schema_version (version) VALUES (2);
INSERT OR REPLACE INTO migrated_version (version) VALUES (1);
";
const SQL_MARF_DATA_TABLE_SCHEMA_3: &str = "
CREATE TABLE IF NOT EXISTS marf_squash_info (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    archival_marf_root_hash BLOB NOT NULL,
    squash_root_node_hash BLOB,
    squash_height INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS marf_squashed_blocks (
    height INTEGER PRIMARY KEY,
    block_hash BLOB NOT NULL UNIQUE,
    marf_root_hash BLOB NOT NULL
);
UPDATE schema_version SET version = 3;
";

pub const SQL_MARF_EXTERNAL_BLOBS_SCHEMA_VERSION: u64 = 2;
pub const SQL_MARF_SCHEMA_VERSION: u64 = 3;
/// Schema version the optimized reference assigns to databases published in
/// its type-first record format (format 6). Not readable here yet.
pub const SQL_MARF_TYPE_FIRST_SCHEMA_VERSION: u64 = 4;

fn schema_version(conn: &Connection) -> u64 {
    conn.query_row("SELECT version FROM schema_version", NO_PARAMS, |r| {
        r.get::<_, i64>(0)
    })
    .map(|v| v as u64)
    .unwrap_or(1)
}

fn migrated_version(conn: &Connection) -> u64 {
    conn.query_row("SELECT version FROM migrated_version", NO_PARAMS, |r| {
        r.get::<_, i64>(0)
    })
    .map(|v| v as u64)
    .unwrap_or(1)
}
/// Open or create the database at `db_path`.
pub fn open<T: TrieId, O: Observer>(
    db_path: impl AsRef<Path>,
    external_blobs: bool,
    compress: bool,
    readonly: bool,
    unconfirmed: bool,
    force_migrate: bool,
    patch_cache_capacity: usize,
) -> Result<(Connection, Store<T, O>), Error> {
    let db_path = db_path.as_ref();
    let in_memory = db_path == Path::new(":memory:");
    let mut create = false;
    let flags = if in_memory {
        create = true;
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
    } else {
        match fs::metadata(db_path) {
            Ok(_) if readonly => OpenFlags::SQLITE_OPEN_READ_ONLY,
            Ok(_) => OpenFlags::SQLITE_OPEN_READ_WRITE,
            Err(e) if e.kind() == io::ErrorKind::NotFound && !readonly => {
                create = true;
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(Error::NotFoundError),
            Err(e) => return Err(e.into()),
        }
    };
    let mut conn = sqlite_open::<O>(db_path, flags, false)?;
    sql_pragma(&conn, "mmap_size", &SQLITE_MMAP_SIZE)?;
    sql_pragma(&conn, "page_size", &SQLITE_MARF_PAGE_SIZE)?;

    if create {
        let tx = tx_begin_immediate::<O>(&mut conn)?;
        tx.execute_batch(SQL_MARF_DATA_TABLE)?;
        tx.execute_batch(SQL_MARF_MINED_TABLE)?;
        tx.execute_batch(SQL_EXTENSION_LOCKS_TABLE)?;
        tx.commit()?;
    }

    let blobs = if external_blobs && !in_memory {
        Some(Blobs::open(db_path, readonly)?)
    } else {
        None
    };

    if readonly {
        let v = schema_version(&conn);
        if v != SQL_MARF_SCHEMA_VERSION {
            return Err(corrupt!(
                "MARF schema version {v} is not compatible with read-only open (expected {SQL_MARF_SCHEMA_VERSION})"
            ));
        }
    } else {
        let first_version = migrate::<O>(&mut conn)?;
        if let Some(blobs) = blobs.as_ref()
            && (first_version < SQL_MARF_EXTERNAL_BLOBS_SCHEMA_VERSION || force_migrate)
        {
            export_trie_blobs::<T, O>(&conn, blobs)?;
        }
        if detect_partial_migration(&conn)? {
            return Err(corrupt!(
                "PARTIAL MIGRATION DETECTED! This is an irrecoverable error. You will need to restart your node from genesis."
            ));
        }
    }

    let prune = conn.read_prune_info()?;
    let store = Store {
        observer: std::marker::PhantomData,
        db_path: db_path.to_path_buf(),
        blobs,
        readonly,
        unconfirmed,
        compress,
        prune,
        ids: HashMap::new(),
        hashes: HashMap::new(),
        locations: HashMap::new(),
        scratch: Scratch::default(),
        sidecar_retired: false,
        patch_cache_capacity,
    };
    Ok((conn, store))
}

/// Bring the schema up to date; returns the version found.
fn migrate<O: Observer>(conn: &mut Connection) -> Result<u64, Error> {
    let first = schema_version(conn);
    loop {
        match schema_version(conn) {
            1 => {
                let tx = tx_begin_immediate::<O>(conn)?;
                tx.execute_batch(SQL_MARF_DATA_TABLE_SCHEMA_2)?;
                tx.commit()?;
            }
            2 => {
                let tx = tx_begin_immediate::<O>(conn)?;
                tx.execute_batch(SQL_MARF_DATA_TABLE_SCHEMA_3)?;
                tx.commit()?;
            }
            SQL_MARF_SCHEMA_VERSION => break,
            SQL_MARF_TYPE_FIRST_SCHEMA_VERSION => {
                return Err(corrupt!(
                    "MARF database uses schema {x} (optimized record format 6: packed \
                     branches and stable-ID values), which this implementation does not \
                     read yet; see docs/marf-rewrite/COMPAT.md",
                    x = SQL_MARF_TYPE_FIRST_SCHEMA_VERSION
                ));
            }
            x => {
                return Err(corrupt!(
                    "Unable to migrate MARF data table: unrecognized schema {x}"
                ));
            }
        }
    }
    if first >= SQL_MARF_EXTERNAL_BLOBS_SCHEMA_VERSION
        && migrated_version(conn) != SQL_MARF_SCHEMA_VERSION
        && !detect_partial_migration(conn)?
    {
        set_migrated(conn)?;
    }
    Ok(first)
}

fn set_migrated(conn: &Connection) -> Result<(), Error> {
    conn.execute(
        "UPDATE migrated_version SET version = ?1",
        params![u64_sql(SQL_MARF_SCHEMA_VERSION)?],
    )?;
    Ok(())
}

/// Either every confirmed trie has been moved to the blobs file, or none has.
fn detect_partial_migration(conn: &Connection) -> Result<bool, Error> {
    if migrated_version(conn) == schema_version(conn) {
        return Ok(false);
    }
    let migrated: i64 = conn.query_row(
        "SELECT COUNT(*) FROM marf_data WHERE external_offset = 0 AND external_length = 0 AND unconfirmed = 0",
        NO_PARAMS,
        |r| r.get(0),
    )?;
    let not_migrated: i64 = conn.query_row(
        "SELECT COUNT(*) FROM marf_data WHERE external_offset != 0 AND external_length != 0 AND unconfirmed = 0",
        NO_PARAMS,
        |r| r.get(0),
    )?;
    Ok(migrated > 0 && not_migrated > 0)
}

/// Move confirmed tries to external blobs during schema migration.
pub fn export_trie_blobs<T: TrieId, O: Observer>(
    conn: &Connection,
    blobs: &Blobs,
) -> Result<(), Error> {
    let _span = observe!(O, ExportTrieBlobs);
    if detect_partial_migration(conn)? {
        return Err(corrupt!(
            "PARTIAL MIGRATION DETECTED! This is an irrecoverable error."
        ));
    }
    let rows: Vec<(u32, T)> = {
        let mut stmt = conn.prepare(
            "SELECT block_id, block_hash FROM marf_data WHERE unconfirmed = 0 ORDER BY block_id",
        )?;
        let rows = stmt.query_map(NO_PARAMS, |r| {
            Ok((
                r.get::<_, u32>(0)?,
                r.get::<_, crate::sqlite::StoredId<T>>(1)?.0,
            ))
        })?;
        rows.collect::<Result<_, _>>()?
    };
    let mut offset = external_blobs_length::<O>(conn)?;
    for (block_id, bhh) in rows {
        let data: Vec<u8> = conn.query_row(
            "SELECT data FROM marf_data WHERE block_id = ?1",
            params![block_id],
            |r| r.get(0),
        )?;
        if data.is_empty() {
            continue;
        }
        blobs.write_at(offset, &data)?;
        conn.execute(
            "UPDATE marf_data SET block_hash = ?1, data = ?2, unconfirmed = ?3, external_offset = ?4, external_length = ?5 WHERE block_id = ?6",
            params![crate::sqlite::SqlId(&bhh), &[] as &[u8], 0, u64_sql(offset)?, u64_sql(data.len() as u64)?, block_id],
        )?;
        offset += data.len() as u64;
    }
    conn.execute("VACUUM", NO_PARAMS)?;
    set_migrated(conn)
}

impl Blobs {
    fn open(db_path: &Path, readonly: bool) -> Result<Blobs, Error> {
        let path = Self::path_for(db_path);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(!readonly)
            .create(!readonly)
            .open(&path)?;
        Ok(Blobs {
            file: std::sync::Arc::new(file),
            path,
            map: None,
        })
    }
}
