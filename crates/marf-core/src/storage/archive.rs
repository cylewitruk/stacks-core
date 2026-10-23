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

//! Shared-blob persistence and archival metadata.
use rusqlite::{Connection, OptionalExtension, params};

use super::write::external_blobs_length;
use super::{Blobs, Store, u64_sql};
use crate::archive::{ArchiveMetadata, PruneInfo};
use crate::observe::Observer;
use crate::sqlite::{NO_PARAMS, table_exists};
use crate::{Error, TrieHash, TrieId, corrupt};

impl ArchiveMetadata for Connection {
    type Error = Error;
    fn read_prune_info(&self) -> Result<Option<PruneInfo>, Error> {
        if !table_exists(self, "marf_squash_info")? {
            return Ok(None);
        }

        let row: Option<(Vec<u8>, Option<Vec<u8>>, i64)> = self
            .query_row(
                "SELECT archival_marf_root_hash, squash_root_node_hash, squash_height FROM marf_squash_info WHERE id = 1",
                NO_PARAMS,
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;

        let Some((archival, squash_root, height)) = row else {
            return Ok(None);
        };

        let hash = |bytes: &[u8]| {
            TrieHash::from_bytes(bytes).ok_or_else(|| corrupt!("Invalid squash root hash length"))
        };

        let squash_root = squash_root.ok_or_else(|| {
            corrupt!("marf_squash_info row exists but prune_root_node_hash is NULL (squash build did not finalize)")
        })?;

        Ok(Some(PruneInfo {
            archival_marf_root_hash: hash(&archival)?,
            prune_root_node_hash: hash(&squash_root)?,
            prune_height: u32::try_from(height).map_err(|_| corrupt!("Invalid squash height"))?,
        }))
    }
}

impl<T: TrieId, O: Observer> Store<T, O> {
    /// Insert an empty placeholder row for `bhh`; returns its storage id.
    pub fn insert_placeholder(&mut self, conn: &Connection, bhh: &T) -> Result<u32, Error> {
        let _span = observe!(O, StoreInsertPlaceholder);
        let id = conn
            .prepare_cached(
                "INSERT INTO marf_data (block_hash, data, unconfirmed, external_offset, external_length) VALUES (?1, ?2, 0, 0, 0)",
            )?
            .insert(params![crate::sqlite::SqlId(bhh), &[] as &[u8]])?;

        let id = u32::try_from(id).map_err(|_| Error::OverflowError)?;
        self.ids.insert(bhh.clone(), id);
        self.hashes.insert(id, bhh.clone());

        Ok(id)
    }

    /// Write a pruned MARF's shared trie blob at the end of the blobs file
    /// and point every confirmed `marf_data` row at it. `write(blobs, base)`
    /// produces the blob at file offset `base`, in as many positioned writes
    /// as it likes, and returns the blob's length; the file is synced once
    /// afterwards. Returns `(base, length)`.
    pub fn write_shared_blob_with<E>(
        &mut self,
        conn: &Connection,
        write: impl FnOnce(&Blobs, u64) -> Result<u64, E>,
    ) -> Result<(u64, u64), crate::archive::ArchiveWriteError<Error, E>> {
        let _span = observe!(O, StoreWriteSharedBlobWith);
        let blobs = self
            .blobs
            .as_ref()
            .ok_or_else(|| corrupt!("prune destination requires external blobs"))?;
        let base = external_blobs_length::<O>(conn)?;
        let len = write(blobs, base).map_err(crate::archive::ArchiveWriteError::Callback)?;

        blobs.sync().map_err(Error::from)?;

        conn.execute(
            "UPDATE marf_data SET external_offset = ?1, external_length = ?2 WHERE unconfirmed = 0",
            params![u64_sql(base)?, u64_sql(len)?],
        )
        .map_err(Error::from)?;

        self.locations.clear();
        self.scratch.patched.clear();

        Ok((base, len))
    }

    /// Write the squash side tables.
    pub fn write_prune_metadata(
        &mut self,
        conn: &Connection,
        archival_root: &TrieHash,
        prune_root_node_hash: &TrieHash,
        prune_height: u32,
        blocks: &[(u32, T, TrieHash)],
    ) -> Result<(), Error> {
        conn.execute(
            "INSERT OR REPLACE INTO marf_squash_info (id, archival_marf_root_hash, squash_root_node_hash, squash_height) VALUES (1, ?1, ?2, ?3)",
            params![archival_root.as_bytes().to_vec(), prune_root_node_hash.as_bytes().to_vec(), i64::from(prune_height)],
        )?;

        let mut stmt = conn.prepare("INSERT INTO marf_squashed_blocks (height, block_hash, marf_root_hash) VALUES (?1, ?2, ?3)")?;

        for (height, bhh, root) in blocks {
            stmt.execute(params![i64::from(*height), bhh.as_bytes(), root.as_bytes()])?;
        }

        self.prune = Some(PruneInfo {
            archival_marf_root_hash: *archival_root,
            prune_root_node_hash: *prune_root_node_hash,
            prune_height,
        });

        Ok(())
    }

    /// All `(height, block_hash, archival_root_hash)` rows of the squash side table.
    pub fn pruned_blocks(&self, conn: &Connection) -> Result<Vec<(u32, T, TrieHash)>, Error> {
        let mut stmt = conn.prepare(
            "SELECT height, block_hash, marf_root_hash FROM marf_squashed_blocks ORDER BY height",
        )?;

        let rows = stmt.query_map(NO_PARAMS, |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Vec<u8>>(1)?,
                r.get::<_, Vec<u8>>(2)?,
            ))
        })?;

        let mut out = vec![];

        for row in rows {
            let (h, bh, root) = row?;
            let h = u32::try_from(h).map_err(|_| corrupt!("Invalid squash block height"))?;
            let arr: [u8; 32] = bh
                .as_slice()
                .try_into()
                .map_err(|_| corrupt!("Invalid squash block_hash length"))?;
            let root = TrieHash::from_bytes(&root)
                .ok_or_else(|| corrupt!("Invalid squash root hash length"))?;
            out.push((h, T::from_bytes(arr), root));
        }

        Ok(out)
    }
    pub fn pruned_block_height(&self, conn: &Connection, bhh: &T) -> Result<Option<u32>, Error> {
        if self.prune.is_none() {
            return Ok(None);
        }
        let h: Option<i64> = conn
            .prepare_cached("SELECT height FROM marf_squashed_blocks WHERE block_hash = ?1")?
            .query_row(params![bhh.as_bytes()], |r| r.get(0))
            .optional()?;
        h.map(|h| u32::try_from(h).map_err(|_| corrupt!("Invalid squash block height")))
            .transpose()
    }

    pub fn pruned_block_root_hash(
        &self,
        conn: &Connection,
        bhh: &T,
    ) -> Result<Option<TrieHash>, Error> {
        if self.prune.is_none() {
            return Ok(None);
        }
        let bytes: Option<Vec<u8>> = conn
            .prepare_cached(
                "SELECT marf_root_hash FROM marf_squashed_blocks WHERE block_hash = ?1",
            )?
            .query_row(params![bhh.as_bytes()], |r| r.get(0))
            .optional()?;
        bytes
            .map(|b| {
                TrieHash::from_bytes(&b).ok_or_else(|| corrupt!("Invalid squash root hash length"))
            })
            .transpose()
    }

    pub fn pruned_root_hash_at_height(
        &self,
        conn: &Connection,
        height: u32,
    ) -> Result<Option<TrieHash>, Error> {
        let bytes: Option<Vec<u8>> = conn
            .prepare_cached("SELECT marf_root_hash FROM marf_squashed_blocks WHERE height = ?1")?
            .query_row(params![i64::from(height)], |r| r.get(0))
            .optional()?;
        bytes
            .map(|b| {
                TrieHash::from_bytes(&b).ok_or_else(|| corrupt!("Invalid squash root hash length"))
            })
            .transpose()
    }

    pub fn pruned_block_at_height(
        &self,
        conn: &Connection,
        height: u32,
    ) -> Result<Option<T>, Error> {
        let bytes: Option<Vec<u8>> = conn
            .prepare_cached("SELECT block_hash FROM marf_squashed_blocks WHERE height = ?1")?
            .query_row(params![i64::from(height)], |r| r.get(0))
            .optional()?;
        bytes
            .map(|b| {
                let arr: [u8; 32] = b.as_slice().try_into().map_err(|_| {
                    corrupt!(
                        "Invalid squash block_hash length {} at height {height}",
                        b.len()
                    )
                })?;
                Ok(T::from_bytes(arr))
            })
            .transpose()
    }

    /// Reject trie traversal below the squash height, where blocks share the
    /// squash blob and have no per-block state.
    pub fn check_historical_read_allowed(&self, conn: &Connection, bhh: &T) -> Result<(), Error> {
        let Some(prune_height) = self.prune_height() else {
            return Ok(());
        };
        let Some(block_height) = self.pruned_block_height(conn, bhh)? else {
            return Ok(());
        };
        if block_height < prune_height {
            return Err(Error::HistoricalReadInPrunedRange {
                block_height,
                prune_height,
            });
        }
        Ok(())
    }
}
