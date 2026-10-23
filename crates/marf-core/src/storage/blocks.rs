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

//! Block identity and historical metadata queries.
use rusqlite::{Connection, OptionalExtension, params};

use super::Store;
use crate::observe::Observer;
use crate::sqlite::NO_PARAMS;
use crate::{Error, TrieId};

impl<T: TrieId, O: Observer> Store<T, O> {
    /// Storage id of a block, confirmed or unconfirmed.
    pub fn block_id(&mut self, conn: &Connection, bhh: &T) -> Result<u32, Error> {
        let _span = observe!(O, StoreBlockId);
        if let Some(id) = self.ids.get(bhh) {
            return Ok(*id);
        }
        count!(block_id_sql);
        let row: Option<(u32, i64)> = conn
            .prepare_cached("SELECT block_id, unconfirmed FROM marf_data WHERE block_hash = ?1")?
            .query_row(params![crate::sqlite::SqlId(bhh)], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        match row {
            None => Err(Error::NotFoundError),
            Some((id, unconfirmed)) => {
                if unconfirmed == 0 {
                    self.ids.insert(bhh.clone(), id);
                    self.hashes.insert(id, bhh.clone());
                }
                Ok(id)
            }
        }
    }

    /// Block identifier for storage id `id`.
    pub fn block_hash(&mut self, conn: &Connection, id: u32) -> Result<&T, Error> {
        if !self.hashes.contains_key(&id) {
            let row: Option<(T, i64)> = conn
                .prepare_cached(
                    "SELECT block_hash, unconfirmed FROM marf_data WHERE block_id = ?1",
                )?
                .query_row(params![id], |r| {
                    Ok((r.get::<_, crate::sqlite::StoredId<T>>(0)?.0, r.get(1)?))
                })
                .optional()?;
            let Some((bhh, unconfirmed)) = row else {
                return Err(Error::NotFoundError);
            };
            if unconfirmed == 0 {
                self.ids.insert(bhh.clone(), id);
            }
            // Unconfirmed rows are cached by id only; their hash never changes
            // while the row exists, and a dropped row is never read again.
            self.hashes.insert(id, bhh);
        }
        Ok(self.hashes.get(&id).expect("just inserted"))
    }

    pub fn confirmed_block_id(&self, conn: &Connection, bhh: &T) -> Result<Option<u32>, Error> {
        let _span = observe!(O, StoreConfirmedBlockId);
        if let Some(id) = self.ids.get(bhh) {
            return Ok(Some(*id));
        }
        Ok(conn
            .prepare_cached(
                "SELECT block_id FROM marf_data WHERE block_hash = ?1 AND unconfirmed = 0",
            )?
            .query_row(params![crate::sqlite::SqlId(bhh)], |r| r.get(0))
            .optional()?)
    }

    pub fn unconfirmed_block_id(&self, conn: &Connection, bhh: &T) -> Result<Option<u32>, Error> {
        let _span = observe!(O, StoreUnconfirmedBlockId);
        Ok(conn
            .prepare_cached(
                "SELECT block_id FROM marf_data WHERE block_hash = ?1 AND unconfirmed = 1",
            )?
            .query_row(params![crate::sqlite::SqlId(bhh)], |r| r.get(0))
            .optional()?)
    }

    pub fn has_block(&self, conn: &Connection, bhh: &T) -> Result<bool, Error> {
        let _span = observe!(O, StoreHasBlock);
        Ok(self.confirmed_block_id(conn, bhh)?.is_some()
            || self.unconfirmed_block_id(conn, bhh)?.is_some())
    }

    /// Highest confirmed storage id (0 when empty).
    pub fn num_blocks(&self, conn: &Connection) -> Result<u32, Error> {
        let _span = observe!(O, StoreNumBlocks);
        Ok(conn
            .prepare_cached("SELECT IFNULL(MAX(block_id), 0) FROM marf_data WHERE unconfirmed = 0")?
            .query_row(NO_PARAMS, |r| r.get(0))?)
    }

    pub fn latest_confirmed_block(&self, conn: &Connection) -> Result<T, Error> {
        Ok(conn.query_row(
            "SELECT block_hash FROM marf_data WHERE unconfirmed = 0 ORDER BY block_id DESC LIMIT 1",
            NO_PARAMS,
            |r| Ok(r.get::<_, crate::sqlite::StoredId<T>>(0)?.0),
        )?)
    }
    pub fn confirmed_blocks(&self, conn: &Connection) -> Result<Vec<(u32, T)>, Error> {
        let mut stmt = conn.prepare(
            "SELECT block_id, block_hash FROM marf_data WHERE unconfirmed = 0 ORDER BY block_id",
        )?;
        let rows = stmt.query_map(NO_PARAMS, |r| {
            Ok((
                r.get::<_, u32>(0)?,
                r.get::<_, crate::sqlite::StoredId<T>>(1)?.0,
            ))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}
