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

//! Authoritative SQLite schema contract for the Binary V1 Clarity side store.

use clarity::vm::errors::VmExecutionError;
use clarity::vm::types::codec::packed::ValueDescriptorVersion;
use rusqlite::{Connection, OptionalExtension, params};

use super::{sql_error, storage_error};

/// Current completed Clarity side-store schema version.
pub const STORAGE_VERSION: i64 = 3;
/// Current Binary V1 record-envelope version.
pub const RECORD_VERSION: u8 = 1;
/// Current normalized value-shape descriptor version.
pub const SHAPE_VERSION: u8 = ValueDescriptorVersion::V1.as_u8();

/// Content-addressed Clarity value table.
pub const DATA_TABLE: &str = "data_table";
/// Clarity metadata table.
pub const METADATA_TABLE: &str = "metadata_table";
/// Dictionary interning schema-independent active-value descriptors.
pub const SHAPE_TABLE: &str = "clarity_value_shapes";
/// Singleton Binary V1 format-marker table.
pub const FORMAT_TABLE: &str = "clarity_side_store_format";
/// Optional data-row column referencing the normalized shape dictionary.
pub const VALUE_SHAPE_ID_COLUMN: &str = "value_shape_id";

/// Every Clarity-owned table in a Binary V1 side store.
pub const TABLE_NAMES: &[&str] = &[DATA_TABLE, METADATA_TABLE, SHAPE_TABLE, FORMAT_TABLE];

/// Shared insert used by live writes and offline migration reconciliation.
pub const INSERT_DATA: &str = "INSERT OR IGNORE INTO data_table
    (key, value, value_shape_id) VALUES (?1, ?2, ?3)";
/// Hot typed-read query that intentionally avoids the shape dictionary.
pub const GET_TYPED: &str = "SELECT value FROM data_table WHERE key = ?1";
/// Generic-read query that resolves reconstruction metadata in one lookup.
pub const GET_GENERIC: &str = "SELECT data_table.value, clarity_value_shapes.descriptor
    FROM data_table
    LEFT JOIN clarity_value_shapes
      ON clarity_value_shapes.id = data_table.value_shape_id
    WHERE data_table.key = ?1";
/// Insert a normalized shape when its canonical descriptor is not interned.
pub const INSERT_SHAPE: &str = "INSERT OR IGNORE INTO clarity_value_shapes(descriptor) VALUES (?1)";
/// Resolve a normalized shape descriptor to its database-local identifier.
pub const GET_SHAPE_ID: &str = "SELECT id FROM clarity_value_shapes WHERE descriptor = ?1";
/// Insert one metadata row in either supported block-ID storage class.
pub const INSERT_METADATA: &str =
    "INSERT INTO metadata_table (blockhash, key, value) VALUES (?1, ?2, ?3)";
/// Fetch one metadata payload by physical block ID and logical key.
pub const GET_METADATA: &str = "SELECT value FROM metadata_table WHERE blockhash = ?1 AND key = ?2";
/// Move all metadata rows between physical block IDs.
pub const COMMIT_METADATA: &str = "UPDATE metadata_table SET blockhash = ?1 WHERE blockhash = ?2";
/// Delete all metadata rows associated with one physical block ID.
pub const DROP_METADATA: &str = "DELETE FROM metadata_table WHERE blockhash = ?1";
/// Stream complete metadata rows for representation-preserving copies.
pub const VISIT_METADATA_ROWS: &str = "SELECT key, blockhash, value FROM metadata_table";
/// Stream metadata keys in deterministic order.
pub const VISIT_METADATA_KEYS: &str = "SELECT key FROM metadata_table ORDER BY key";
/// Audit query resolving every optional normalized descriptor.
pub const AUDIT_DATA: &str =
    "SELECT data_table.key, data_table.value, clarity_value_shapes.descriptor
    FROM data_table
    LEFT JOIN clarity_value_shapes
      ON clarity_value_shapes.id = data_table.value_shape_id
    ORDER BY data_table.rowid";

/// A complete table definition and ordered physical-column contract.
struct TableSpec {
    name: &'static str,
    create_sql: &'static str,
    columns: &'static [ColumnSpec],
}

/// A secondary-index definition and ordered indexed-column contract.
struct IndexSpec {
    table: &'static str,
    name: &'static str,
    create_sql: &'static str,
    unique: bool,
    columns: &'static [&'static str],
}

/// One physical SQLite column expected in a Binary V1 table.
#[derive(Debug, Eq, PartialEq)]
struct ColumnSpec {
    name: &'static str,
    kind: &'static str,
    not_null: bool,
    primary_key_position: i64,
}

const DATA_COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::new("key", "BLOB", true, 0),
    ColumnSpec::new("value", "BLOB", true, 0),
    ColumnSpec::new(VALUE_SHAPE_ID_COLUMN, "INTEGER", false, 0),
];
const SHAPE_COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::new("id", "INTEGER", false, 1),
    ColumnSpec::new("descriptor", "BLOB", true, 0),
];
const METADATA_COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::new("key", "TEXT", true, 0),
    ColumnSpec::new("blockhash", "BLOB", false, 0),
    ColumnSpec::new("value", "TEXT", false, 0),
];
const FORMAT_COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::new("singleton", "INTEGER", true, 1),
    ColumnSpec::new("storage_version", "INTEGER", true, 0),
    ColumnSpec::new("record_version", "INTEGER", true, 0),
    ColumnSpec::new("shape_version", "INTEGER", true, 0),
    ColumnSpec::new("complete", "INTEGER", true, 0),
];

const TABLES: &[TableSpec] = &[
    TableSpec {
        name: DATA_TABLE,
        create_sql: "CREATE TABLE data_table (
            key BLOB NOT NULL
                CHECK(typeof(key) = 'blob' AND length(key) = 40),
            value BLOB NOT NULL
                CHECK(typeof(value) = 'blob' AND length(value) >= 2),
            value_shape_id INTEGER
                CHECK(value_shape_id IS NULL OR value_shape_id BETWEEN 1 AND 4294967295)
        )",
        columns: DATA_COLUMNS,
    },
    TableSpec {
        name: SHAPE_TABLE,
        create_sql: "CREATE TABLE clarity_value_shapes (
            id INTEGER PRIMARY KEY CHECK(id BETWEEN 1 AND 4294967295),
            descriptor BLOB NOT NULL UNIQUE CHECK(typeof(descriptor) = 'blob')
        )",
        columns: SHAPE_COLUMNS,
    },
    TableSpec {
        name: METADATA_TABLE,
        create_sql: "CREATE TABLE metadata_table (
            key TEXT NOT NULL,
            blockhash BLOB
                CHECK(blockhash IS NULL OR
                      (typeof(blockhash) = 'blob' AND length(blockhash) = 32)),
            value TEXT
        )",
        columns: METADATA_COLUMNS,
    },
    TableSpec {
        name: FORMAT_TABLE,
        create_sql: "CREATE TABLE clarity_side_store_format (
            singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
            storage_version INTEGER NOT NULL,
            record_version INTEGER NOT NULL,
            shape_version INTEGER NOT NULL,
            complete INTEGER NOT NULL CHECK(complete IN (0, 1))
        ) WITHOUT ROWID",
        columns: FORMAT_COLUMNS,
    },
];

const INDEXES: &[IndexSpec] = &[
    IndexSpec {
        table: DATA_TABLE,
        name: "data_table_keys",
        create_sql: "CREATE UNIQUE INDEX data_table_keys ON data_table(key)",
        unique: true,
        columns: &["key"],
    },
    IndexSpec {
        table: METADATA_TABLE,
        name: "metadata_keys",
        create_sql: "CREATE UNIQUE INDEX metadata_keys ON metadata_table(key, blockhash)",
        unique: true,
        columns: &["key", "blockhash"],
    },
    IndexSpec {
        table: METADATA_TABLE,
        name: "md_blockhashes",
        create_sql: "CREATE INDEX md_blockhashes ON metadata_table(blockhash)",
        unique: false,
        columns: &["blockhash"],
    },
];

impl ColumnSpec {
    /// Construct one compile-time physical-column contract.
    const fn new(
        name: &'static str,
        kind: &'static str,
        not_null: bool,
        primary_key_position: i64,
    ) -> Self {
        Self {
            name,
            kind,
            not_null,
            primary_key_position,
        }
    }
}

/// Whether the named table is owned by the Clarity side-store format.
pub fn owns_table(name: &str) -> bool {
    TABLE_NAMES.contains(&name)
}

/// Create every Binary V1 table and its initial completion marker.
pub fn create(conn: &Connection, complete: bool) -> Result<(), VmExecutionError> {
    for table in TABLES {
        conn.execute(table.create_sql, []).map_err(sql_error)?;
    }
    if complete {
        create_indexes(conn)?;
    }
    conn.execute(
        "INSERT INTO clarity_side_store_format
         (singleton, storage_version, record_version, shape_version, complete)
         VALUES (1, ?1, ?2, ?3, ?4)",
        params![
            STORAGE_VERSION,
            i64::from(RECORD_VERSION),
            i64::from(SHAPE_VERSION),
            i64::from(complete)
        ],
    )
    .map_err(sql_error)?;
    Ok(())
}

/// Build the secondary indexes intentionally omitted during bulk import.
pub fn create_indexes(conn: &Connection) -> Result<(), VmExecutionError> {
    for index in INDEXES {
        conn.execute(index.create_sql, []).map_err(sql_error)?;
    }
    Ok(())
}

/// Test whether a named SQLite table exists.
pub fn table_exists(conn: &Connection, table: &str) -> Result<bool, VmExecutionError> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        [table],
        |row| row.get(0),
    )
    .map_err(sql_error)
}

/// Count rows in a trusted, statically selected table.
pub fn row_count(conn: &Connection, table: &str) -> Result<u64, VmExecutionError> {
    debug_assert!(owns_table(table));
    let sql = format!("SELECT COUNT(*) FROM {table}");
    conn.query_row(&sql, [], |row| row.get(0))
        .map_err(sql_error)
}

/// Verify the exact marker state and complete physical schema.
pub fn verify(
    conn: &Connection,
    complete: bool,
    require_indexes: bool,
) -> Result<(), VmExecutionError> {
    verify_marker(conn, i64::from(complete))?;
    for table in TABLES {
        verify_table(conn, table)?;
    }
    verify_unique_columns(conn, SHAPE_TABLE, &["descriptor"])?;
    if require_indexes {
        for index in INDEXES {
            verify_index(conn, index)?;
        }
    }
    Ok(())
}

/// Mark an incomplete, fully verified migration destination complete.
pub fn finalize(conn: &Connection) -> Result<(), VmExecutionError> {
    verify(conn, false, true)?;
    let updated = conn
        .execute(
            "UPDATE clarity_side_store_format SET complete = 1
             WHERE singleton = 1 AND storage_version = ?1
               AND record_version = ?2 AND shape_version = ?3 AND complete = 0",
            params![
                STORAGE_VERSION,
                i64::from(RECORD_VERSION),
                i64::from(SHAPE_VERSION)
            ],
        )
        .map_err(sql_error)?;
    if updated != 1 {
        return Err(storage_error(
            "Binary V1 migration destination has an invalid format marker",
        ));
    }
    verify(conn, true, true)
}

/// Copy database-local Binary V1 dictionary and marker rows from attached `src`.
pub fn copy_snapshot_auxiliary_rows(conn: &Connection) -> Result<(), VmExecutionError> {
    // Keep IDs unchanged: reachable data rows refer to this database-local dictionary.
    conn.execute(
        "INSERT INTO clarity_value_shapes SELECT * FROM src.clarity_value_shapes",
        [],
    )
    .map_err(sql_error)?;
    conn.execute(
        "INSERT INTO clarity_side_store_format SELECT * FROM src.clarity_side_store_format",
        [],
    )
    .map_err(sql_error)?;
    Ok(())
}

/// Verify that every non-null data-row shape ID resolves in the dictionary.
pub fn verify_shape_references(conn: &Connection) -> Result<(), VmExecutionError> {
    let dangling: u64 = conn
        .query_row(
            "SELECT COUNT(*) FROM data_table
             WHERE value_shape_id IS NOT NULL AND NOT EXISTS (
                 SELECT 1 FROM clarity_value_shapes
                 WHERE clarity_value_shapes.id = data_table.value_shape_id
             )",
            [],
            |row| row.get(0),
        )
        .map_err(sql_error)?;
    if dangling != 0 {
        return Err(storage_error(format!(
            "Binary V1 data table has {dangling} dangling shape IDs"
        )));
    }
    Ok(())
}

/// Parsed singleton contents of the Binary V1 format table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FormatState {
    storage_version: i64,
    record_version: i64,
    shape_version: i64,
    complete: i64,
}

/// Verify the singleton marker against the supported versions and state.
fn verify_marker(conn: &Connection, complete: i64) -> Result<(), VmExecutionError> {
    let state = format_state(conn)?;
    let expected = Some(FormatState {
        storage_version: STORAGE_VERSION,
        record_version: i64::from(RECORD_VERSION),
        shape_version: i64::from(SHAPE_VERSION),
        complete,
    });
    if state != expected {
        return Err(storage_error(format!(
            "unsupported or incomplete Binary V1 format marker: {state:?}"
        )));
    }
    Ok(())
}

/// Read the singleton format marker when its table exists.
fn format_state(conn: &Connection) -> Result<Option<FormatState>, VmExecutionError> {
    if !table_exists(conn, FORMAT_TABLE)? {
        return Ok(None);
    }
    conn.query_row(
        "SELECT storage_version, record_version, shape_version, complete
         FROM clarity_side_store_format WHERE singleton = 1",
        [],
        |row| {
            Ok(FormatState {
                storage_version: row.get(0)?,
                record_version: row.get(1)?,
                shape_version: row.get(2)?,
                complete: row.get(3)?,
            })
        },
    )
    .optional()
    .map_err(sql_error)
}

/// Verify one table's defining SQL and ordered columns.
fn verify_table(conn: &Connection, table: &TableSpec) -> Result<(), VmExecutionError> {
    let actual: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table.name],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_error)?;
    let Some(actual) = actual else {
        return Err(storage_error(format!(
            "missing Binary V1 table {}",
            table.name
        )));
    };
    if normalize_sql(&actual) != normalize_sql(table.create_sql) {
        return Err(storage_error(format!(
            "invalid Binary V1 {} physical schema",
            table.name
        )));
    }

    let sql = format!("PRAGMA table_info({})", table.name);
    let columns: Vec<ColumnInfo> = conn
        .prepare(&sql)
        .map_err(sql_error)?
        .query_map([], |row| {
            Ok(ColumnInfo {
                name: row.get(1)?,
                kind: row.get(2)?,
                not_null: row.get(3)?,
                primary_key_position: row.get(5)?,
            })
        })
        .map_err(sql_error)?
        .collect::<rusqlite::Result<_>>()
        .map_err(sql_error)?;
    let expected: Vec<ColumnInfo> = table.columns.iter().map(ColumnInfo::from).collect();
    if columns != expected {
        return Err(storage_error(format!(
            "invalid Binary V1 {} columns: {columns:?}",
            table.name
        )));
    }
    Ok(())
}

/// Runtime representation of one SQLite `table_info` row.
#[derive(Debug, Eq, PartialEq)]
struct ColumnInfo {
    name: String,
    kind: String,
    not_null: bool,
    primary_key_position: i64,
}

impl From<&ColumnSpec> for ColumnInfo {
    fn from(column: &ColumnSpec) -> Self {
        Self {
            name: column.name.into(),
            kind: column.kind.into(),
            not_null: column.not_null,
            primary_key_position: column.primary_key_position,
        }
    }
}

/// Verify one named index's properties and ordered columns.
fn verify_index(conn: &Connection, expected: &IndexSpec) -> Result<(), VmExecutionError> {
    let list_sql = format!("PRAGMA index_list({})", expected.table);
    let mut list = conn.prepare(&list_sql).map_err(sql_error)?;
    let mut rows = list.query([]).map_err(sql_error)?;
    let mut found = None;
    while let Some(row) = rows.next().map_err(sql_error)? {
        let name: String = row.get(1).map_err(sql_error)?;
        if name == expected.name {
            let unique: i64 = row.get(2).map_err(sql_error)?;
            let partial: i64 = row.get(4).map_err(sql_error)?;
            found = Some((unique != 0, partial != 0));
            break;
        }
    }
    let Some((unique, partial)) = found else {
        return Err(storage_error(format!(
            "missing Binary V1 index {}",
            expected.name
        )));
    };
    if unique != expected.unique || partial {
        return Err(storage_error(format!(
            "invalid Binary V1 index {} properties",
            expected.name
        )));
    }
    let info_sql = format!("PRAGMA index_info({})", expected.name);
    let columns: Vec<String> = conn
        .prepare(&info_sql)
        .map_err(sql_error)?
        .query_map([], |row| row.get(2))
        .map_err(sql_error)?
        .collect::<rusqlite::Result<_>>()
        .map_err(sql_error)?;
    if columns
        .iter()
        .map(String::as_str)
        .ne(expected.columns.iter().copied())
    {
        return Err(storage_error(format!(
            "invalid Binary V1 index {} columns: {columns:?}",
            expected.name
        )));
    }
    Ok(())
}

/// Verify that some complete unique index covers the expected columns.
fn verify_unique_columns(
    conn: &Connection,
    table: &str,
    expected_columns: &[&str],
) -> Result<(), VmExecutionError> {
    let list_sql = format!("PRAGMA index_list({table})");
    let mut list = conn.prepare(&list_sql).map_err(sql_error)?;
    let mut rows = list.query([]).map_err(sql_error)?;
    while let Some(row) = rows.next().map_err(sql_error)? {
        let name: String = row.get(1).map_err(sql_error)?;
        let unique: i64 = row.get(2).map_err(sql_error)?;
        let partial: i64 = row.get(4).map_err(sql_error)?;
        if unique == 0 || partial != 0 {
            continue;
        }
        let columns: Vec<String> = conn
            .prepare("SELECT name FROM pragma_index_info(?1) ORDER BY seqno")
            .map_err(sql_error)?
            .query_map([name], |row| row.get(0))
            .map_err(sql_error)?
            .collect::<rusqlite::Result<_>>()
            .map_err(sql_error)?;
        if columns
            .iter()
            .map(String::as_str)
            .eq(expected_columns.iter().copied())
        {
            return Ok(());
        }
    }
    Err(storage_error(format!(
        "missing Binary V1 unique constraint on {table}{expected_columns:?}"
    )))
}

/// Normalize insignificant whitespace and a trailing semicolon.
fn normalize_sql(sql: &str) -> String {
    sql.trim_end_matches(';')
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn schema_inventory_is_self_consistent() {
        let specified: BTreeSet<_> = TABLES.iter().map(|table| table.name).collect();
        let published: BTreeSet<_> = TABLE_NAMES.iter().copied().collect();
        assert_eq!(specified, published);
        assert!(published.contains(SHAPE_TABLE));
        assert!(published.contains(FORMAT_TABLE));
        assert!(INDEXES.iter().all(|index| published.contains(index.table)));
    }

    #[test]
    fn shape_descriptor_requires_a_unique_constraint() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE shapes_without_unique (
                 id INTEGER PRIMARY KEY,
                 descriptor BLOB NOT NULL
             );
             CREATE INDEX descriptor_lookup
                 ON shapes_without_unique(descriptor);",
        )
        .unwrap();
        assert!(verify_unique_columns(&conn, "shapes_without_unique", &["descriptor"]).is_err());

        conn.execute(
            "CREATE UNIQUE INDEX descriptor_unique
             ON shapes_without_unique(descriptor)",
            [],
        )
        .unwrap();
        verify_unique_columns(&conn, "shapes_without_unique", &["descriptor"]).unwrap();
    }
}
