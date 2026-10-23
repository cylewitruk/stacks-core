//! Names kept for source compatibility with code written against the
//! original `stackslib::chainstate::stacks::index` module. Everything here is
//! a thin alias or re-export; new code should use the primary names.

use rusqlite::Connection;
use stacks_common::types::chainstate::TrieHash;
use stacks_common::types::sqlite::NO_PARAMS;

use crate::{corrupt, Error, MarfTrieId};

/// One confirmed `marf_data` row.
#[derive(Debug, Clone)]
pub struct MarfDataEntry<T> {
    pub block_id: u32,
    pub block_hash: T,
    pub external_offset: u64,
}

/// The handful of SQL helpers the snapshot tooling reaches for directly.
pub mod trie_sql {
    use super::*;

    pub use crate::storage::PruneInfo as SqlPruneInfo;
    /// The original's name for [`SqlPruneInfo`].
    pub type SqlSquashInfo = SqlPruneInfo;

    /// The original's name for [`read_prune_info`].
    pub fn read_squash_info(conn: &Connection) -> Result<Option<SqlSquashInfo>, Error> {
        read_prune_info(conn)
    }

    /// Test helper: record a pruned block at an explicit height.
    #[cfg(any(test, feature = "testing"))]
    pub fn test_insert_squashed_block<T: MarfTrieId>(
        conn: &Connection,
        height: u32,
        block_hash: &T,
        marf_root_hash: &TrieHash,
    ) -> Result<(), Error> {
        conn.execute(
            "INSERT INTO marf_squashed_blocks (height, block_hash, marf_root_hash) VALUES (?1, ?2, ?3)",
            rusqlite::params![height, block_hash.as_bytes(), marf_root_hash.as_bytes()],
        )?;
        Ok(())
    }

    /// Test helper: record a pruned block at the next height.
    #[cfg(any(test, feature = "testing"))]
    pub fn test_append_squashed_block<T: MarfTrieId>(
        conn: &Connection,
        block_hash: &T,
        marf_root_hash: &TrieHash,
    ) -> Result<(), Error> {
        conn.execute(
            "INSERT INTO marf_squashed_blocks (height, block_hash, marf_root_hash) \
             VALUES ((SELECT COALESCE(MAX(height) + 1, 0) FROM marf_squashed_blocks), ?1, ?2)",
            rusqlite::params![block_hash.as_bytes(), marf_root_hash.as_bytes()],
        )?;
        Ok(())
    }

    pub fn get_latest_confirmed_block_hash<T: MarfTrieId>(conn: &Connection) -> Result<T, Error> {
        Ok(conn.query_row(
            "SELECT block_hash FROM marf_data WHERE unconfirmed = 0 ORDER BY block_id DESC LIMIT 1",
            NO_PARAMS,
            |r| r.get(0),
        )?)
    }

    pub fn bulk_read_squashed_blocks<T: MarfTrieId>(
        conn: &Connection,
    ) -> Result<Vec<(u32, T, TrieHash)>, Error> {
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

    pub fn read_prune_info(conn: &Connection) -> Result<Option<SqlPruneInfo>, Error> {
        crate::storage::read_prune_info(conn)
    }

    pub fn bulk_read_block_entries<T: MarfTrieId>(
        conn: &Connection,
    ) -> Result<Vec<MarfDataEntry<T>>, Error> {
        let mut stmt = conn.prepare(
            "SELECT block_id, block_hash, external_offset FROM marf_data WHERE unconfirmed = 0 ORDER BY block_id",
        )?;
        let rows = stmt.query_map(NO_PARAMS, |r| {
            Ok((r.get::<_, u32>(0)?, r.get::<_, T>(1)?, r.get::<_, i64>(2)?))
        })?;
        let mut out = vec![];
        for row in rows {
            let (block_id, block_hash, offset) = row?;
            out.push(MarfDataEntry {
                block_id,
                block_hash,
                external_offset: u64::try_from(offset).map_err(|_| Error::OverflowError)?,
            });
        }
        Ok(out)
    }
}

/// The external blobs file, by its original name.
pub mod file {
    pub struct TrieFile;

    impl TrieFile {
        /// Does `<db_path>.blobs` exist?
        pub fn exists(db_path: impl AsRef<std::path::Path>) -> Result<bool, crate::Error> {
            crate::storage::Blobs::exists(db_path)
        }
    }
}
