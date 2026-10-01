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

//! Offline bulk-loaded value lookups and sorted stable-ID membership publication.

use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;

use rusqlite::{Connection, OpenFlags};

use crate::Result;

/// Append into private row tables; build random-access indexes only after extraction finishes.
pub fn initialize(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE canonical_external_values(hash BLOB NOT NULL CHECK(length(hash)=40),value_id INTEGER NOT NULL CHECK(value_id>0));
         CREATE TABLE canonical_inline_values(hash BLOB NOT NULL CHECK(length(hash)=40),record BLOB NOT NULL,descriptor BLOB NOT NULL)",
    )?;
    Ok(())
}

/// Validate uniqueness and create the two source-commitment lookup indexes by bulk sorting.
pub fn index(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE UNIQUE INDEX canonical_external_hash ON canonical_external_values(hash,value_id);
         CREATE UNIQUE INDEX canonical_inline_hash ON canonical_inline_values(hash,record,descriptor)",
    )?;
    // Covering indexes keep later hash lookups and membership streaming off random table pages.
    // Full-key uniqueness is checked separately before either immutable lookup is used.
    for table in ["canonical_external_values", "canonical_inline_values"] {
        let duplicated: bool = db.query_row(
            &format!(
                "SELECT EXISTS(SELECT 1 FROM {table} GROUP BY hash HAVING count(*)>1 LIMIT 1)"
            ),
            [],
            |row| row.get(0),
        )?;
        if duplicated {
            return Err("duplicate source commitment in bulk value lookup".into());
        }
    }
    Ok(())
}

/// Build the final PtrHash base from a sorted stream, without populating a runtime SQL index.
pub fn build(db_path: &Path, directory: &Path) -> Result<()> {
    let path = directory.join("memberships.scratch");
    let mut output = BufWriter::with_capacity(
        1024 * 1024,
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?,
    );
    let db = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut statement =
        db.prepare("SELECT hash,value_id FROM canonical_external_values ORDER BY hash")?;
    let mut rows = statement.query([])?;
    let mut counts = [0u64; 256];
    let mut previous = None;
    while let Some(row) = rows.next()? {
        let bytes = row.get_ref(0)?.as_blob()?;
        let hash: [u8; 40] = bytes
            .try_into()
            .map_err(|_| "invalid membership hash length")?;
        let id: u32 = row.get(1)?;
        if id == 0 || previous.is_some_and(|prior| hash <= prior) {
            return Err("invalid or unordered membership".into());
        }
        counts[usize::from(hash[0])] += 1;
        output.write_all(&hash)?;
        output.write_all(&id.to_le_bytes())?;
        previous = Some(hash);
    }
    output.flush()?;
    drop(output);
    drop(rows);
    drop(statement);
    drop(db);
    let base = directory.join("stable-ptrhash");
    if counts.iter().all(|&count| count == 0) {
        // The ordinary empty-index builder does not mmap a zero-byte pair stream.
        extent_ptrhash::stable::build_and_activate(db_path, &base)
            .map_err(|error| error.to_string())?;
    } else {
        extent_ptrhash::stable::build_and_activate_from_pairs(db_path, &base, &path, &counts)
            .map_err(|error| error.to_string())?;
    }
    fs::remove_file(path)?;
    let db = Connection::open(db_path)?;
    db.execute_batch("PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF; DROP TABLE canonical_external_values; VACUUM; PRAGMA optimize=0x10002")?;
    // The caller synchronizes the entire unpublished tree before selecting it.
    drop(db);
    File::open(directory)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use rusqlite::params;

    use super::*;

    /// Bulk indexes retain exact mappings and reject duplicate hashes before trie rewriting.
    #[test]
    fn bulk_lookups_validate_uniqueness() {
        let db = Connection::open_in_memory().unwrap();
        initialize(&db).unwrap();
        for id in (1..=100).rev() {
            let key = [id as u8; 40];
            db.execute(
                "INSERT INTO canonical_external_values VALUES(?1,?2)",
                params![key.as_slice(), id],
            )
            .unwrap();
        }
        index(&db).unwrap();
        let actual: i64 = db
            .query_row(
                "SELECT value_id FROM canonical_external_values WHERE hash=?1",
                [[17u8; 40].as_slice()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(actual, 17);
        let plan: String = db.query_row(
            "EXPLAIN QUERY PLAN SELECT hash,value_id FROM canonical_external_values ORDER BY hash",
            [], |r| r.get(3),
        ).unwrap();
        assert!(
            plan.contains("COVERING INDEX canonical_external_hash"),
            "{plan}"
        );
        let duplicate = Connection::open_in_memory().unwrap();
        initialize(&duplicate).unwrap();
        duplicate.execute_batch("INSERT INTO canonical_inline_values VALUES(zeroblob(40),x'01',x''),(zeroblob(40),x'01',x'')").unwrap();
        assert!(index(&duplicate).is_err());
        let ambiguous = Connection::open_in_memory().unwrap();
        initialize(&ambiguous).unwrap();
        ambiguous
            .execute_batch(
                "INSERT INTO canonical_external_values VALUES(zeroblob(40),1),(zeroblob(40),2)",
            )
            .unwrap();
        assert!(
            index(&ambiguous)
                .unwrap_err()
                .to_string()
                .contains("duplicate source commitment")
        );
    }
}
