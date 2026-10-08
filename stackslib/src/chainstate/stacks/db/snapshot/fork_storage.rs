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

//! MARF-aware helpers for copying the canonical `__fork_storage` table.
//!
//! These sit apart from the generic SQL utilities in [`super::common`]
//! because they understand MARF leaf semantics: they walk the squashed
//! trie to learn which `value_hash` entries are canonical, then copy only
//! those rows.

use std::collections::HashSet;
use std::time::Instant;

use rusqlite::types::{Value, ValueRef};
use rusqlite::{params, Connection};

use super::common::clone_schemas_from_source;
use crate::chainstate::stacks::index::marf::{MARFOpenOpts, MarfConnection, MARF};
use crate::chainstate::stacks::index::storage::{TrieFileStorage, TrieHashCalculationMode};
use crate::chainstate::stacks::index::{trie_sql, Error, MARFValue, MarfTrieId};
use crate::util_lib::db::quote_sql_identifier;

/// Physical payload columns copied with each content-addressed row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReferencedRowLayout {
    /// Copy only the content-addressed key and value.
    KeyValue,
    /// Also copy one owner-supplied payload column.
    KeyValueAndExtra(&'static str),
}

/// Collect the `MARFValue` of every leaf in the squashed trie.
///
/// Opens the MARF at `db_path` read-only, resolves the tip, and walks the
/// trie via `for_each_leaf`.  Auto-detects external blobs.
///
/// Returns `(tip_block_hash, leaf_value_hashes)`.
pub fn collect_leaf_value_hashes<T: MarfTrieId>(
    db_path: &str,
) -> Result<(T, HashSet<MARFValue>), Error> {
    let external_blobs = std::path::Path::new(&format!("{db_path}.blobs")).exists();
    let open_opts = MARFOpenOpts::new(TrieHashCalculationMode::Deferred, external_blobs);
    let storage = TrieFileStorage::open_readonly(db_path, open_opts)?;
    let mut marf = MARF::<T>::from_storage(storage);
    let tip = trie_sql::get_latest_confirmed_block_hash::<T>(marf.sqlite_conn())?;

    let mut hashes = HashSet::new();
    marf.with_conn(|conn| {
        MARF::for_each_leaf(conn, &tip, |_hash, value| {
            hashes.insert(value);
            Ok(())
        })
    })?;

    Ok((tip, hashes))
}

/// Walk the squashed MARF at `dst_path` read-only and return its canonical
/// leaf value hashes (for [`copy_canonical_fork_storage`]). A dst that was
/// not squashed into a MARF fails at open.
pub fn collect_canonical_leaf_hashes<T: MarfTrieId>(
    dst_path: &str,
) -> Result<HashSet<MARFValue>, Error> {
    let t = Instant::now();
    let (_tip, leaf_hashes) = collect_leaf_value_hashes::<T>(dst_path)?;
    info!(
        "[fork_storage] collected {} leaf hashes in {:?}",
        leaf_hashes.len(),
        t.elapsed()
    );
    Ok(leaf_hashes)
}

/// Copy canonical `__fork_storage` rows from `src` into `main`. i.e.
/// only the rows whose `value_hash` is referenced by a leaf in the
/// squashed MARF.
///
/// An empty `leaf_hashes` results in zero rows copied. the strict
/// `clone_schemas_from_source` ensures the schema is still cloned.
pub fn copy_canonical_fork_storage(
    conn: &Connection,
    leaf_hashes: &HashSet<MARFValue>,
) -> Result<u64, Error> {
    let src_has_table: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM src.sqlite_master WHERE type='table' AND name='__fork_storage'",
            [],
            |row| row.get(0),
        )
        .map_err(Error::SQLError)?;

    if !src_has_table {
        return Err(Error::CorruptionError(
            "src has no __fork_storage; expected on any chainstate that ran the MARF migration"
                .into(),
        ));
    }

    clone_schemas_from_source(conn, &["__fork_storage"])?;
    copy_leaf_referenced_rows(
        conn,
        "__fork_storage",
        "value_hash",
        ReferencedRowLayout::KeyValue,
        leaf_hashes,
    )
}

/// Stream-copy a content-addressed `(key_col, value)` table from the ATTACHed
/// `src`, keeping only rows whose key is in `keep` (the squashed MARF's leaf
/// set). Used for the index `__fork_storage` and Clarity `data_table`; the
/// leaf-set filter is in memory, not SQL, so these can't use `execute_copy_specs`.
///
/// The destination table must exist and be empty - unexpected pre-population errors.
/// Keys may be canonical lowercase hexadecimal or raw 40-byte `MARFValue`s.
/// `table`/`key_col` are interpolated into SQL: pass only trusted fixed identifiers.
/// The payload column is named `value`; an optional owner-supplied extra column is
/// quoted before interpolation.
pub fn copy_leaf_referenced_rows(
    conn: &Connection,
    table: &str,
    key_col: &str,
    layout: ReferencedRowLayout,
    keep: &HashSet<MARFValue>,
) -> Result<u64, Error> {
    let t = Instant::now();
    let extra_column = match layout {
        ReferencedRowLayout::KeyValue => None,
        ReferencedRowLayout::KeyValueAndExtra(column) => Some(quote_sql_identifier(column)),
    };
    let extra_projection = extra_column
        .as_deref()
        .map_or_else(String::new, |column| format!(", {column}"));
    let mut select = conn
        .prepare(&format!(
            "SELECT {key_col}, value{extra_projection} FROM src.{table}"
        ))
        .map_err(Error::SQLError)?;
    let mut insert = conn
        .prepare(&format!(
            "INSERT INTO {table} ({key_col}, value{extra_projection}) \
             VALUES (?1, ?2{})",
            if extra_column.is_some() { ", ?3" } else { "" }
        ))
        .map_err(Error::SQLError)?;
    let mut rows: u64 = 0;
    let mut scanned: u64 = 0;
    let mut rows_iter = select.query([]).map_err(Error::SQLError)?;
    while let Some(row) = rows_iter.next().map_err(Error::SQLError)? {
        scanned += 1;
        let key_ref = row.get_ref(0).map_err(Error::SQLError)?;
        let key = match key_ref {
            ValueRef::Text(bytes) => {
                let key_str = std::str::from_utf8(bytes).map_err(|error| {
                    Error::CorruptionError(format!("src.{table}.{key_col} is not UTF-8: {error}"))
                })?;
                let key = MARFValue::from_hex(key_str).map_err(|error| {
                    Error::CorruptionError(format!(
                        "src.{table}.{key_col} `{key_str}` is not a hex MARFValue: {error:?}"
                    ))
                })?;
                if key.to_hex() != key_str {
                    return Err(Error::CorruptionError(format!(
                        "src.{table}.{key_col} `{key_str}` is not canonical lowercase hex"
                    )));
                }
                key
            }
            ValueRef::Blob(bytes) => MARFValue(bytes.try_into().map_err(|_| {
                Error::CorruptionError(format!("src.{table}.{key_col} is not a 40-byte MARFValue"))
            })?),
            value => {
                return Err(Error::CorruptionError(format!(
                    "src.{table}.{key_col} has invalid SQLite type {:?}",
                    value.data_type()
                )));
            }
        };
        if keep.contains(&key) {
            let key = owned_sql_value(key_ref)?;
            let value = owned_sql_value(row.get_ref(1).map_err(Error::SQLError)?)?;
            if extra_column.is_some() {
                let extra = owned_sql_value(row.get_ref(2).map_err(Error::SQLError)?)?;
                insert
                    .execute(params![key, value, extra])
                    .map_err(Error::SQLError)?;
            } else {
                insert
                    .execute(params![key, value])
                    .map_err(Error::SQLError)?;
            }
            rows += 1;
        }
    }
    info!(
        "[copy] {table} stream-filter: scanned {scanned}, copied {rows} in {:?}",
        t.elapsed()
    );
    Ok(rows)
}

/// Materialize a borrowed SQLite value so it can outlive the source row cursor.
fn owned_sql_value(value: ValueRef<'_>) -> Result<Value, Error> {
    Ok(match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(value) => Value::Integer(value),
        ValueRef::Real(value) => Value::Real(value),
        ValueRef::Text(value) => {
            Value::Text(String::from_utf8(value.to_vec()).map_err(|error| {
                Error::CorruptionError(format!("SQLite TEXT is not UTF-8: {error}"))
            })?)
        }
        ValueRef::Blob(value) => Value::Blob(value.to_vec()),
    })
}
