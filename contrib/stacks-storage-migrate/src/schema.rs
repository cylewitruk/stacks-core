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

//! Copy logical SQL state without copying obsolete physical trie bytes.

use blockstack_lib::clarity_vm::database::binary_value_store;
use rusqlite::{Connection, params};

use crate::Result;

/// Quote an identifier obtained from an inspected SQLite schema.
pub fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// User schema object, restored only after bulk loading tables.
pub struct Object {
    /// SQL definition, owned so the source query can close before writes.
    sql: String,
}

/// Copy source tables in one transaction, excluding physical blobs and derived indexes.
pub fn copy(
    source: &Connection,
    destination: &Connection,
    source_path: &str,
    clarity: bool,
) -> Result<Vec<Object>> {
    copy_inner(source, destination, source_path, clarity, false)
}

/// Rebuild source logical tables while retaining only a completed Clarity extraction.
pub fn refresh(
    source: &Connection,
    destination: &Connection,
    source_path: &str,
) -> Result<Vec<Object>> {
    copy_inner(source, destination, source_path, true, true)
}

/// Shared schema projection for fresh copies and explicitly selected extraction reuse.
fn copy_inner(
    source: &Connection,
    destination: &Connection,
    source_path: &str,
    clarity: bool,
    replace: bool,
) -> Result<Vec<Object>> {
    let mut statement = source.prepare(
        "SELECT type,name,sql,tbl_name FROM sqlite_master WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' ORDER BY type,name",
    )?;
    let objects = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    destination.execute("ATTACH DATABASE ?1 AS original", [source_path])?;
    let mut secondary = Vec::new();
    for (kind, name, sql, owner) in &objects {
        if clarity && binary_value_store::owns_table(owner) {
            continue;
        }
        // Derived registrations bind old physical offsets and are rebuilt at finalization.
        if name.starts_with("direct_hash_")
            || matches!(
                owner.as_str(),
                "marf_direct_hash_index" | "marf_direct_hash_live"
            )
            || name == "marf_record_format"
        {
            continue;
        }
        if kind != "table" {
            secondary.push(Object { sql: sql.clone() });
            continue;
        }
        if replace {
            destination.execute_batch(&format!("DROP TABLE IF EXISTS {}", quote(name)))?;
        }
        destination.execute_batch(sql)?;
        let mut columns = source.prepare(&format!("PRAGMA table_info({})", quote(name)))?;
        let columns = columns
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let names = columns
            .iter()
            .map(|column| quote(column))
            .collect::<Vec<_>>()
            .join(",");
        let projection = columns
            .iter()
            .map(|column| {
                if (name == "marf_data" || name == "mined_blocks") && column == "data" {
                    "X''".to_owned()
                } else if name == "marf_data"
                    && (column == "external_offset" || column == "external_length")
                {
                    "0".to_owned()
                } else {
                    quote(column)
                }
            })
            .collect::<Vec<_>>()
            .join(",");
        destination.execute_batch(&format!(
            "INSERT INTO {} ({names}) SELECT {projection} FROM original.{}",
            quote(name),
            quote(name)
        ))?;
    }
    // Preserve AUTOINCREMENT high-water marks even when their largest rows were deleted.
    let sequence: bool = source.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='sqlite_sequence')",
        [],
        |row| row.get(0),
    )?;
    if sequence {
        destination.execute_batch("DELETE FROM sqlite_sequence; INSERT INTO sqlite_sequence SELECT * FROM original.sqlite_sequence")?;
    }
    // Legacy internal-only schemas may predate external location columns.
    for column in ["external_offset", "external_length"] {
        let exists: bool = destination.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('marf_data') WHERE name=?1)",
            params![column],
            |row| row.get(0),
        )?;
        if !exists {
            destination.execute_batch(&format!(
                "ALTER TABLE marf_data ADD COLUMN {column} INTEGER NOT NULL DEFAULT 0"
            ))?;
        }
    }
    Ok(secondary)
}

/// Restore constraints, indexes, and triggers after logical data and physical offsets are final.
pub fn finalize(destination: &Connection, objects: Vec<Object>) -> Result<()> {
    for object in objects {
        destination.execute_batch(&object.sql)?;
    }
    Ok(())
}

/// Reject any unresolved foreign key after every table, row and index has been loaded.
pub fn verify_foreign_keys(destination: &Connection) -> Result<()> {
    let mut statement = destination.prepare("PRAGMA foreign_key_check")?;
    let mut rows = statement.query([])?;
    if let Some(row) = rows.next()? {
        let table: String = row.get(0)?;
        let parent: String = row.get(2)?;
        return Err(
            format!("destination foreign key violation: {table} references {parent}").into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Forward table references and separately declared unique parent indexes survive bulk copying.
    #[test]
    fn bulk_foreign_keys_validate_after_schema_loading() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("source.sqlite");
        let source = Connection::open(&source_path).unwrap();
        source.execute_batch("PRAGMA foreign_keys=OFF;
            CREATE TABLE a_children(child INTEGER PRIMARY KEY, parent TEXT REFERENCES z_parents(name));
            CREATE TABLE z_parents(name TEXT);
            CREATE UNIQUE INDEX parent_name ON z_parents(name);
            INSERT INTO z_parents VALUES('parent'); INSERT INTO a_children VALUES(1,'parent');
            CREATE TABLE marf_data(block_id INTEGER PRIMARY KEY,data BLOB);
            CREATE TABLE mined_blocks(block_id INTEGER PRIMARY KEY,data BLOB);").unwrap();
        verify_foreign_keys(&source).unwrap();
        let destination = Connection::open_in_memory().unwrap();
        destination
            .execute_batch("PRAGMA foreign_keys=OFF; BEGIN")
            .unwrap();
        let objects = copy(&source, &destination, source_path.to_str().unwrap(), false).unwrap();
        finalize(&destination, objects).unwrap();
        verify_foreign_keys(&destination).unwrap();
        destination
            .execute("INSERT INTO a_children VALUES(2,'missing')", [])
            .unwrap();
        assert!(
            verify_foreign_keys(&destination)
                .unwrap_err()
                .to_string()
                .contains("a_children references z_parents")
        );
    }
}
