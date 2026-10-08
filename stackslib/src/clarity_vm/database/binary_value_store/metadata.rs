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

//! Metadata-table operations shared by Binary V1 runtime and snapshot tooling.

use std::str;

use clarity::vm::errors::VmExecutionError;
use rusqlite::types::{ToSqlOutput, ValueRef};
use rusqlite::{params, Connection, OptionalExtension, ToSql};

use super::schema::{
    COMMIT_METADATA, DROP_METADATA, GET_METADATA, INSERT_METADATA, METADATA_TABLE,
    VISIT_METADATA_KEYS, VISIT_METADATA_ROWS,
};
use super::sql_error;

/// Borrowed legacy-text or Binary V1 binary metadata block identifier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetadataBlockId<'a> {
    /// SQL `NULL`, retained for legacy rows without a block association.
    Null,
    /// Lowercase hexadecimal legacy representation.
    Hex(&'a str),
    /// Raw 32-byte Binary V1 representation.
    Bytes(&'a [u8]),
}

impl ToSql for MetadataBlockId<'_> {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(match self {
            Self::Null => ToSqlOutput::Owned(rusqlite::types::Value::Null),
            Self::Hex(value) => ToSqlOutput::Borrowed(ValueRef::Text(value.as_bytes())),
            Self::Bytes(value) => ToSqlOutput::Borrowed(ValueRef::Blob(value)),
        })
    }
}

/// Borrowed metadata row preserving its source block-ID representation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MetadataRow<'a> {
    /// Full `clr-meta::<contract id>::<key>` key.
    pub key: &'a str,
    /// Physical `blockhash` value in the source database.
    pub block_id: MetadataBlockId<'a>,
    /// Stored metadata payload.
    pub value: Option<&'a str>,
}

/// Build a logical metadata key shared by legacy and Binary V1 stores.
fn make_key(contract_id: &str, key: &str) -> String {
    format!("clr-meta::{contract_id}::{key}")
}

/// Split a logical metadata key into its contract and metadata components.
pub fn parse_key(key: &str) -> Option<(&str, &str)> {
    key.strip_prefix("clr-meta::")?.split_once("::")
}

/// Insert one Binary V1 metadata row with a raw block identifier.
pub fn insert(
    conn: &Connection,
    block_id: &[u8; 32],
    contract_id: &str,
    key: &str,
    value: &str,
) -> Result<(), VmExecutionError> {
    let key = make_key(contract_id, key);
    conn.prepare_cached(INSERT_METADATA)
        .and_then(|mut statement| statement.execute(params![block_id, key, value]))
        .map_err(sql_error)?;
    Ok(())
}

/// Fetch one Binary V1 metadata value by raw block identifier.
pub fn get(
    conn: &Connection,
    block_id: &[u8; 32],
    contract_id: &str,
    key: &str,
) -> Result<Option<String>, VmExecutionError> {
    let key = make_key(contract_id, key);
    conn.prepare_cached(GET_METADATA)
        .and_then(|mut statement| {
            statement
                .query_row(params![block_id, key], |row| row.get(0))
                .optional()
        })
        .map_err(sql_error)
}

/// Rename Binary V1 metadata rows between raw block identifiers.
pub fn commit(conn: &Connection, from: &[u8; 32], to: &[u8; 32]) -> Result<(), VmExecutionError> {
    conn.execute(COMMIT_METADATA, params![to, from])
        .map_err(sql_error)?;
    Ok(())
}

/// Delete Binary V1 metadata rows for one raw block identifier.
pub fn drop(conn: &Connection, block_id: &[u8; 32]) -> Result<(), VmExecutionError> {
    conn.execute(DROP_METADATA, params![block_id])
        .map_err(sql_error)?;
    Ok(())
}

/// Insert one already-keyed metadata row without changing its storage class.
pub fn insert_row(conn: &Connection, row: &MetadataRow<'_>) -> rusqlite::Result<()> {
    conn.prepare_cached(INSERT_METADATA)?
        .execute(params![row.block_id, row.key, row.value])?;
    Ok(())
}

/// Visit every metadata row while preserving TEXT versus BLOB block IDs.
pub fn visit_rows<E, F>(conn: &Connection, mut visit: F) -> Result<(), E>
where
    E: From<rusqlite::Error>,
    F: FnMut(&MetadataRow<'_>) -> Result<(), E>,
{
    let mut statement = conn.prepare(VISIT_METADATA_ROWS)?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let key = row.get_ref(0)?.as_str().map_err(rusqlite::Error::from)?;
        let block_id = match row.get_ref(1)? {
            ValueRef::Null => MetadataBlockId::Null,
            ValueRef::Text(value) => {
                MetadataBlockId::Hex(str::from_utf8(value).map_err(rusqlite::Error::from)?)
            }
            ValueRef::Blob(value) => MetadataBlockId::Bytes(value),
            value => {
                return Err(rusqlite::Error::InvalidColumnType(
                    1,
                    "blockhash".into(),
                    value.data_type(),
                )
                .into());
            }
        };
        let value = match row.get_ref(2)? {
            ValueRef::Null => None,
            ValueRef::Text(value) => Some(str::from_utf8(value).map_err(rusqlite::Error::from)?),
            value => {
                return Err(rusqlite::Error::InvalidColumnType(
                    2,
                    "value".into(),
                    value.data_type(),
                )
                .into());
            }
        };
        visit(&MetadataRow {
            key,
            block_id,
            value,
        })?;
    }
    Ok(())
}

/// Visit every metadata key in deterministic ascending order.
pub fn visit_keys<E, F>(conn: &Connection, mut visit: F) -> Result<(), E>
where
    E: From<rusqlite::Error>,
    F: FnMut(&str) -> Result<(), E>,
{
    let mut statement = conn.prepare(VISIT_METADATA_KEYS)?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        visit(row.get_ref(0)?.as_str().map_err(rusqlite::Error::from)?)?;
    }
    Ok(())
}

/// Count metadata rows in either legacy or Binary V1 storage.
pub fn row_count(conn: &Connection) -> rusqlite::Result<u64> {
    let sql = format!("SELECT COUNT(*) FROM {METADATA_TABLE}");
    conn.query_row(&sql, [], |row| row.get(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_visiting_preserves_text_and_binary_block_ids() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE metadata_table (key TEXT, blockhash, value TEXT)",
            [],
        )
        .unwrap();
        let bytes = [0x22; 32];
        for row in [
            MetadataRow {
                key: "clr-meta::contract::hex",
                block_id: MetadataBlockId::Hex("11"),
                value: Some("hex-value"),
            },
            MetadataRow {
                key: "clr-meta::contract::bytes",
                block_id: MetadataBlockId::Bytes(&bytes),
                value: Some("bytes-value"),
            },
            MetadataRow {
                key: "clr-meta::contract::null",
                block_id: MetadataBlockId::Null,
                value: None,
            },
        ] {
            insert_row(&conn, &row).unwrap();
        }

        let mut saw_hex = false;
        let mut saw_bytes = false;
        let mut saw_null = false;
        visit_rows(&conn, |row| -> rusqlite::Result<()> {
            saw_hex |= row.block_id == MetadataBlockId::Hex("11");
            saw_bytes |= row.block_id == MetadataBlockId::Bytes(&bytes);
            saw_null |= row.block_id == MetadataBlockId::Null && row.value.is_none();
            Ok(())
        })
        .unwrap();
        assert!(saw_hex && saw_bytes && saw_null);
    }

    #[test]
    fn metadata_keys_round_trip_and_visit_in_order() {
        assert_eq!(
            parse_key(&make_key("ST1.contract", "key::part")),
            Some(("ST1.contract", "key::part"))
        );
        assert_eq!(parse_key("not-metadata"), None);
    }
}
