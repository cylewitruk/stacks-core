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

//! Durable trie writes and extension locks.
use std::path::Path;

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use super::{Store, u64_sql};
use crate::observe::Observer;
use crate::sqlite::{NO_PARAMS, sqlite_open};
use crate::{Error, TrieId, corrupt};

impl<T: TrieId, O: Observer> Store<T, O> {
    /// Persist a confirmed trie under `bhh`; returns its storage id.
    pub fn write_confirmed(
        &mut self,
        conn: &Connection,
        bhh: &T,
        blob: &[u8],
    ) -> Result<u32, Error> {
        let _span = observe!(O, StoreWriteConfirmed);
        self.retire_direct_hash_sidecar(conn)?;
        let block_id: i64 = match self.blobs.as_ref() {
            Some(blobs) => {
                let offset = external_blobs_length::<O>(conn)?;
                blobs.write_at(offset, blob)?;
                conn.prepare_cached(
                    "INSERT INTO marf_data (block_hash, data, unconfirmed, external_offset, external_length) VALUES (?1, ?2, 0, ?3, ?4)",
                )?
                .insert(params![crate::sqlite::SqlId(bhh), &[] as &[u8], u64_sql(offset)?, u64_sql(blob.len() as u64)?])?
            }
            None => conn
                .prepare_cached(
                    "INSERT INTO marf_data (block_hash, data, unconfirmed, external_offset, external_length) VALUES (?1, ?2, 0, 0, 0)",
                )?
                .insert(params![crate::sqlite::SqlId(bhh), blob])?,
        };
        let block_id = u32::try_from(block_id).map_err(|_| Error::OverflowError)?;
        self.ids.insert(bhh.clone(), block_id);
        self.hashes.insert(block_id, bhh.clone());
        self.locations.remove(&block_id);
        Ok(block_id)
    }

    /// Persist (or overwrite) an unconfirmed trie, always inside SQLite.
    pub fn write_unconfirmed(
        &mut self,
        conn: &Connection,
        bhh: &T,
        blob: &[u8],
    ) -> Result<u32, Error> {
        let _span = observe!(O, StoreWriteUnconfirmed);
        self.retire_direct_hash_sidecar(conn)?;
        if self.confirmed_block_id(conn, bhh)?.is_some() {
            return Err(corrupt!("tried to overwrite confirmed MARF trie {bhh}"));
        }
        let id = match self.unconfirmed_block_id(conn, bhh)? {
            Some(id) => {
                conn.execute(
                    "UPDATE marf_data SET data = ?1 WHERE block_id = ?2",
                    params![blob, id],
                )?;
                id
            }
            None => {
                let id = conn
                    .prepare_cached(
                        "INSERT INTO marf_data (block_hash, data, unconfirmed, external_offset, external_length) VALUES (?1, ?2, 1, 0, 0)",
                    )?
                    .insert(params![crate::sqlite::SqlId(bhh), blob])?;
                u32::try_from(id).map_err(|_| Error::OverflowError)?
            }
        };
        self.locations.remove(&id);
        Ok(id)
    }

    /// Persist a mined (never-chained) trie.
    pub fn write_mined(&mut self, conn: &Connection, bhh: &T, blob: &[u8]) -> Result<(), Error> {
        let _span = observe!(O, StoreWriteMined);
        self.retire_direct_hash_sidecar(conn)?;
        let existing: Option<u32> = conn
            .query_row(
                "SELECT block_id FROM mined_blocks WHERE block_hash = ?1",
                params![crate::sqlite::SqlId(bhh)],
                |r| r.get(0),
            )
            .optional()?;
        match existing {
            Some(id) => conn.execute(
                "UPDATE mined_blocks SET data = ?1 WHERE block_id = ?2",
                params![blob, id],
            )?,
            None => conn.execute(
                "INSERT INTO mined_blocks (block_hash, data) VALUES (?1, ?2)",
                params![crate::sqlite::SqlId(bhh), blob],
            )?,
        };
        Ok(())
    }

    pub fn drop_unconfirmed(&mut self, conn: &Connection, bhh: &T) -> Result<(), Error> {
        let _span = observe!(O, StoreDropUnconfirmed);
        if let Some(id) = self.unconfirmed_block_id(conn, bhh)? {
            self.hashes.remove(&id);
            self.locations.remove(&id);
        }
        conn.execute(
            "DELETE FROM marf_data WHERE block_hash = ?1 AND unconfirmed = 1",
            params![crate::sqlite::SqlId(bhh)],
        )?;
        Ok(())
    }

    /// Take the extension lock for `bhh`; `false` if it is already taken (or,
    /// for confirmed tries, already stored).
    pub fn lock(&self, conn: &Connection, bhh: &T, unconfirmed: bool) -> Result<bool, Error> {
        let _span = observe!(O, StoreLock);
        if !unconfirmed {
            let stored = conn
                .query_row(
                    "SELECT 1 FROM marf_data WHERE block_hash = ?1 LIMIT 1",
                    params![crate::sqlite::SqlId(bhh)],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if stored {
                return Ok(false);
            }
        }
        let locked = conn
            .query_row(
                "SELECT 1 FROM block_extension_locks WHERE block_hash = ?1 LIMIT 1",
                params![crate::sqlite::SqlId(bhh)],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if locked {
            return Ok(false);
        }
        conn.execute(
            "INSERT INTO block_extension_locks (block_hash) VALUES (?1)",
            params![crate::sqlite::SqlId(bhh)],
        )?;
        Ok(true)
    }

    pub fn unlock(&self, conn: &Connection, bhh: &T) -> Result<(), Error> {
        let _span = observe!(O, StoreUnlock);
        conn.execute(
            "DELETE FROM block_extension_locks WHERE block_hash = ?1",
            params![crate::sqlite::SqlId(bhh)],
        )?;
        Ok(())
    }
}

/// Find the append offset for external blobs.
pub fn external_blobs_length<O: Observer>(conn: &Connection) -> Result<u64, Error> {
    let _span = observe!(O, ExternalBlobsLength);
    let len: Option<i64> = conn
        .query_row(
            "SELECT (external_offset + external_length) AS blobs_length FROM marf_data ORDER BY external_offset DESC LIMIT 1",
            NO_PARAMS,
            |r| r.get(0),
        )
        .optional()?;
    Ok(len.unwrap_or(0) as u64)
}

/// Remove extension locks after a crash.
pub fn clear_locks<O: Observer>(db_path: &Path) -> Result<(), Error> {
    let _span = observe!(O, ClearLocks);
    let conn = sqlite_open::<O>(db_path, OpenFlags::SQLITE_OPEN_READ_WRITE, false)?;
    conn.execute("DELETE FROM block_extension_locks", NO_PARAMS)?;
    Ok(())
}
