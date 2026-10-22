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
use stacks_common::types::sqlite::NO_PARAMS;
use stacks_common::util::db::sqlite_open;

use super::{Store, u64_sql};
use crate::{Error, MarfTrieId, corrupt};

impl<T: MarfTrieId> Store<T> {
    /// Persist a confirmed trie under `bhh`; returns its storage id.
    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn write_confirmed(
        &mut self,
        conn: &Connection,
        bhh: &T,
        blob: &[u8],
    ) -> Result<u32, Error> {
        self.retire_direct_hash_sidecar(conn)?;
        let block_id: i64 = match self.blobs.as_ref() {
            Some(blobs) => {
                let offset = external_blobs_length(conn)?;
                blobs.write_at(offset, blob)?;
                conn.prepare_cached(
                    "INSERT INTO marf_data (block_hash, data, unconfirmed, external_offset, external_length) VALUES (?1, ?2, 0, ?3, ?4)",
                )?
                .insert(params![bhh, &[] as &[u8], u64_sql(offset)?, u64_sql(blob.len() as u64)?])?
            }
            None => conn
                .prepare_cached(
                    "INSERT INTO marf_data (block_hash, data, unconfirmed, external_offset, external_length) VALUES (?1, ?2, 0, 0, 0)",
                )?
                .insert(params![bhh, blob])?,
        };
        let block_id = u32::try_from(block_id).map_err(|_| Error::OverflowError)?;
        self.ids.insert(bhh.clone(), block_id);
        self.hashes.insert(block_id, bhh.clone());
        self.locations.remove(&block_id);
        Ok(block_id)
    }

    /// Persist (or overwrite) an unconfirmed trie, always inside SQLite.
    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn write_unconfirmed(
        &mut self,
        conn: &Connection,
        bhh: &T,
        blob: &[u8],
    ) -> Result<u32, Error> {
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
                    .insert(params![bhh, blob])?;
                u32::try_from(id).map_err(|_| Error::OverflowError)?
            }
        };
        self.locations.remove(&id);
        Ok(id)
    }

    /// Persist a mined (never-chained) trie.
    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn write_mined(&mut self, conn: &Connection, bhh: &T, blob: &[u8]) -> Result<(), Error> {
        self.retire_direct_hash_sidecar(conn)?;
        let existing: Option<u32> = conn
            .query_row(
                "SELECT block_id FROM mined_blocks WHERE block_hash = ?1",
                params![bhh],
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
                params![bhh, blob],
            )?,
        };
        Ok(())
    }

    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn drop_unconfirmed(&mut self, conn: &Connection, bhh: &T) -> Result<(), Error> {
        if let Some(id) = self.unconfirmed_block_id(conn, bhh)? {
            self.hashes.remove(&id);
            self.locations.remove(&id);
        }
        conn.execute(
            "DELETE FROM marf_data WHERE block_hash = ?1 AND unconfirmed = 1",
            params![bhh],
        )?;
        Ok(())
    }

    /// Take the extension lock for `bhh`; `false` if it is already taken (or,
    /// for confirmed tries, already stored).
    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn lock(&self, conn: &Connection, bhh: &T, unconfirmed: bool) -> Result<bool, Error> {
        if !unconfirmed {
            let stored = conn
                .query_row(
                    "SELECT 1 FROM marf_data WHERE block_hash = ?1 LIMIT 1",
                    params![bhh],
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
                params![bhh],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if locked {
            return Ok(false);
        }
        conn.execute(
            "INSERT INTO block_extension_locks (block_hash) VALUES (?1)",
            params![bhh],
        )?;
        Ok(true)
    }

    #[cfg_attr(feature = "profiler", stacks_profiler::profile)]
    pub fn unlock(&self, conn: &Connection, bhh: &T) -> Result<(), Error> {
        conn.execute(
            "DELETE FROM block_extension_locks WHERE block_hash = ?1",
            params![bhh],
        )?;
        Ok(())
    }
}

/// Find the append offset for external blobs.
#[cfg_attr(
    feature = "profiler",
    stacks_profiler::profile(sample_rate = 8, unsampled = "count_only")
)]
pub fn external_blobs_length(conn: &Connection) -> Result<u64, Error> {
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
#[cfg_attr(feature = "profiler", stacks_profiler::profile)]
pub fn clear_locks(db_path: &Path) -> Result<(), Error> {
    let conn = sqlite_open(db_path, OpenFlags::SQLITE_OPEN_READ_WRITE, false)?;
    conn.execute("DELETE FROM block_extension_locks", NO_PARAMS)?;
    Ok(())
}
