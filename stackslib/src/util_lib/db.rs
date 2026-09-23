// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020 Stacks Open Internet Foundation
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

//! Shared SQLite, transaction, and MARF-index database utilities.

use std::io::Error as IOError;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::{error, fmt, fs, io};

use clarity::vm::types::QualifiedContractIdentifier;
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use rusqlite::types::ToSql;
use rusqlite::{
    Connection, Error as sqlite_error, OpenFlags, OptionalExtension, Params, Row, Transaction,
    TransactionBehavior, params,
};
use serde_json::Error as serde_error;
use stacks_common::types::Address;
use stacks_common::types::chainstate::{SortitionId, StacksAddress, StacksBlockId, TrieHash};
use stacks_common::types::sqlite::NO_PARAMS;
use stacks_common::util::db::update_lock_table;
use stacks_common::util::hash::to_hex;
use stacks_common::util::secp256k1::{Secp256k1PrivateKey, Secp256k1PublicKey};

use crate::chainstate::stacks::index::marf::{MARF, MarfConnection, MarfCore, MarfTransaction};
use crate::chainstate::stacks::index::{Error as MARFError, MARFValue, MarfTrieId};

pub type DBConn = rusqlite::Connection;
pub type DBTx<'a> = rusqlite::Transaction<'a>;

// 256MB
pub const SQLITE_MMAP_SIZE: i64 = 256 * 1024 * 1024;

// 32K
pub const SQLITE_MARF_PAGE_SIZE: i64 = 32768;

/// Statement-cache capacity for `sqlite_open` connections. The widest observed
/// working set is ~120 distinct statements (the sortition MARF);
/// rusqlite's default of 16 would LRU-thrash.
pub const SQLITE_STATEMENT_CACHE_CAPACITY: usize = 200;

/// Chars that must be escaped inside a SQLite `file:` URI path.
const SQLITE_URI_PATH_RESERVED: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

/// One user-defined object read from a SQLite schema catalog.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SqliteSchemaObject {
    /// SQLite object kind (`table`, `index`, `view`, or `trigger`).
    pub kind: String,
    /// Object name.
    pub name: String,
    /// Owning table name as recorded by SQLite.
    pub table: String,
    /// Creation SQL stored in `sqlite_master`.
    pub sql: String,
}

/// Quote a trusted SQLite identifier, escaping embedded double quotes.
pub fn quote_sql_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

/// Build a read-only SQLite file URI, optionally disabling locking and WAL handling.
pub fn sqlite_readonly_uri(path: &Path, immutable: bool) -> Result<String, sqlite_error> {
    let path_text = path
        .to_str()
        .ok_or_else(|| sqlite_error::InvalidPath(path.to_path_buf()))?;
    let encoded = utf8_percent_encode(path_text, SQLITE_URI_PATH_RESERVED);
    let immutable = if immutable { "&immutable=1" } else { "" };
    Ok(format!("file:{encoded}?mode=ro{immutable}"))
}

/// Read every user-defined object in `schema` from its SQLite catalog.
pub fn sqlite_schema_objects(
    connection: &Connection,
    schema: &str,
) -> Result<Vec<SqliteSchemaObject>, sqlite_error> {
    if schema.is_empty()
        || !schema
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(sqlite_error::InvalidParameterName(schema.to_owned()));
    }
    let schema = quote_sql_identifier(schema);
    let mut statement = connection.prepare(&format!(
        "SELECT type, name, tbl_name, sql
         FROM {schema}.sqlite_master
         WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%'
         ORDER BY CASE type WHEN 'table' THEN 0 ELSE 1 END, rowid"
    ))?;
    let objects = statement
        .query_map([], |row| {
            Ok(SqliteSchemaObject {
                kind: row.get(0)?,
                name: row.get(1)?,
                table: row.get(2)?,
                sql: row.get(3)?,
            })
        })?
        .collect();
    objects
}

#[derive(Debug)]
pub enum Error {
    /// Not implemented
    NotImplemented,
    /// Database doesn't exist
    NoDBError,
    /// Read-only and tried to write
    ReadOnly,
    /// Type error -- can't represent the given data in the database
    TypeError,
    /// Database is corrupt -- we got data that shouldn't be there, or didn't get data when we
    /// should have
    Corruption,
    /// Serialization error -- can't serialize data
    SerializationError(serde_error),
    /// Parse error -- failed to load data we stored directly
    ParseError,
    /// Operation would overflow
    Overflow,
    /// Data not found
    NotFoundError,
    /// Data already exists
    ExistsError,
    /// Data corresponds to a non-canonical PoX sortition
    InvalidPoxSortition,
    /// Sqlite3 error
    SqliteError(sqlite_error),
    /// I/O error
    IOError(IOError),
    /// MARF index error
    IndexError(MARFError),
    /// Old schema error
    OldSchema(u64),
    /// Database is too old for epoch
    TooOldForEpoch,
    /// Block height is out of range
    BlockHeightOutOfRange,
    /// Other error
    Other(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Error::NotImplemented => write!(f, "Not implemented"),
            Error::NoDBError => write!(f, "Database does not exist"),
            Error::ReadOnly => write!(f, "Database is opened read-only"),
            Error::TypeError => write!(f, "Invalid or unrepresentable database type"),
            Error::Corruption => write!(f, "Database is corrupt"),
            Error::SerializationError(ref e) => fmt::Display::fmt(e, f),
            Error::ParseError => write!(f, "Parse error"),
            Error::Overflow => write!(f, "Numeric overflow"),
            Error::NotFoundError => write!(f, "Not found"),
            Error::ExistsError => write!(f, "Already exists"),
            Error::InvalidPoxSortition => write!(f, "Invalid PoX sortition"),
            Error::IOError(ref e) => fmt::Display::fmt(e, f),
            Error::SqliteError(ref e) => fmt::Display::fmt(e, f),
            Error::IndexError(ref e) => fmt::Display::fmt(e, f),
            Error::OldSchema(ref s) => write!(f, "Old database schema: {}", s),
            Error::TooOldForEpoch => {
                write!(f, "Database is not compatible with current system epoch")
            }
            Error::BlockHeightOutOfRange => write!(f, "Block height is out of range"),
            Error::Other(ref s) => fmt::Display::fmt(s, f),
        }
    }
}

impl error::Error for Error {
    fn cause(&self) -> Option<&dyn error::Error> {
        match *self {
            Error::NotImplemented => None,
            Error::NoDBError => None,
            Error::ReadOnly => None,
            Error::TypeError => None,
            Error::Corruption => None,
            Error::SerializationError(ref e) => Some(e),
            Error::ParseError => None,
            Error::Overflow => None,
            Error::NotFoundError => None,
            Error::ExistsError => None,
            Error::InvalidPoxSortition => None,
            Error::SqliteError(ref e) => Some(e),
            Error::IOError(ref e) => Some(e),
            Error::IndexError(ref e) => Some(e),
            Error::OldSchema(ref _s) => None,
            Error::TooOldForEpoch => None,
            Error::BlockHeightOutOfRange => None,
            Error::Other(ref _s) => None,
        }
    }
}

impl From<serde_error> for Error {
    #[cfg_attr(test, mutants::skip)]
    fn from(e: serde_error) -> Self {
        Self::SerializationError(e)
    }
}

impl From<sqlite_error> for Error {
    #[cfg_attr(test, mutants::skip)]
    fn from(e: sqlite_error) -> Self {
        Self::SqliteError(e)
    }
}

impl From<MARFError> for Error {
    #[cfg_attr(test, mutants::skip)]
    fn from(e: MARFError) -> Self {
        Self::IndexError(e)
    }
}

pub trait FromRow<T> {
    fn from_row(row: &Row) -> Result<T, Error>;
}

pub trait FromColumn<T> {
    fn from_column(row: &Row, column_name: &str) -> Result<T, Error>;
}

impl FromRow<u64> for u64 {
    fn from_row(row: &Row) -> Result<u64, Error> {
        let x: i64 = row.get(0)?;
        if x < 0 {
            return Err(Error::ParseError);
        }
        Ok(x as u64)
    }
}

impl FromRow<u32> for u32 {
    fn from_row(row: &Row) -> Result<u32, Error> {
        let x: u32 = row.get(0)?;
        Ok(x)
    }
}

impl FromRow<String> for String {
    fn from_row(row: &Row) -> Result<String, Error> {
        let x: String = row.get(0)?;
        Ok(x)
    }
}

impl FromRow<Vec<u8>> for Vec<u8> {
    fn from_row(row: &Row) -> Result<Vec<u8>, Error> {
        let x: Vec<u8> = row.get(0)?;
        Ok(x)
    }
}

impl FromColumn<u64> for u64 {
    fn from_column(row: &Row, column_name: &str) -> Result<u64, Error> {
        let x: i64 = row.get(column_name)?;
        if x < 0 {
            return Err(Error::ParseError);
        }
        Ok(x as u64)
    }
}

impl FromRow<StacksAddress> for StacksAddress {
    fn from_row(row: &Row) -> Result<StacksAddress, Error> {
        let addr_str: String = row.get(0)?;
        let addr = StacksAddress::from_string(&addr_str).ok_or(Error::ParseError)?;
        Ok(addr)
    }
}

impl FromColumn<Option<u64>> for u64 {
    fn from_column(row: &Row, column_name: &str) -> Result<Option<u64>, Error> {
        let x: Option<i64> = row.get(column_name)?;
        match x {
            Some(x) => {
                if x < 0 {
                    return Err(Error::ParseError);
                }
                Ok(Some(x as u64))
            }
            None => Ok(None),
        }
    }
}

impl FromRow<i64> for i64 {
    fn from_row(row: &Row) -> Result<i64, Error> {
        let x: i64 = row.get(0)?;
        Ok(x)
    }
}

impl FromColumn<i64> for i64 {
    fn from_column(row: &Row, column_name: &str) -> Result<i64, Error> {
        let x: i64 = row.get(column_name)?;
        Ok(x)
    }
}

impl FromColumn<QualifiedContractIdentifier> for QualifiedContractIdentifier {
    fn from_column(row: &Row, column_name: &str) -> Result<QualifiedContractIdentifier, Error> {
        let value: String = row.get(column_name)?;
        QualifiedContractIdentifier::parse(&value).map_err(|_| Error::ParseError)
    }
}

impl FromRow<bool> for bool {
    fn from_row(row: &Row) -> Result<bool, Error> {
        let x: bool = row.get(0)?;
        Ok(x)
    }
}

/// Make public keys loadable from a sqlite database
impl FromColumn<Secp256k1PublicKey> for Secp256k1PublicKey {
    fn from_column(row: &Row, column_name: &str) -> Result<Secp256k1PublicKey, Error> {
        let pubkey_hex: String = row.get(column_name)?;
        let pubkey = Secp256k1PublicKey::from_hex(&pubkey_hex).map_err(|_e| Error::ParseError)?;
        Ok(pubkey)
    }
}

/// Make private keys loadable from a sqlite database
impl FromColumn<Secp256k1PrivateKey> for Secp256k1PrivateKey {
    fn from_column(row: &Row, column_name: &str) -> Result<Secp256k1PrivateKey, Error> {
        let privkey_hex: String = row.get(column_name)?;
        let privkey =
            Secp256k1PrivateKey::from_hex(&privkey_hex).map_err(|_e| Error::ParseError)?;
        Ok(privkey)
    }
}

pub fn u64_to_sql(x: u64) -> Result<i64, Error> {
    if x > (i64::MAX as u64) {
        return Err(Error::ParseError);
    }
    Ok(x as i64)
}

pub fn opt_u64_to_sql(x: Option<u64>) -> Result<Option<i64>, Error> {
    match x {
        Some(num) => {
            if num > (i64::MAX as u64) {
                return Err(Error::ParseError);
            }
            Ok(Some(num as i64))
        }
        None => Ok(None),
    }
}

macro_rules! impl_byte_array_from_column_only {
    ($thing:ident) => {
        impl crate::util_lib::db::FromColumn<$thing> for $thing {
            fn from_column(
                row: &rusqlite::Row,
                column_name: &str,
            ) -> Result<Self, crate::util_lib::db::Error> {
                Ok(row.get::<_, Self>(column_name)?)
            }
        }
    };
}

impl_byte_array_from_column_only!(SortitionId);
impl_byte_array_from_column_only!(StacksBlockId);

macro_rules! impl_byte_array_from_column {
    ($thing:ident) => {
        impl rusqlite::types::FromSql for $thing {
            fn column_result(
                value: rusqlite::types::ValueRef,
            ) -> rusqlite::types::FromSqlResult<Self> {
                let hex_str = value.as_str()?;
                let byte_str = stacks_common::util::hash::hex_bytes(hex_str)
                    .map_err(|_e| rusqlite::types::FromSqlError::InvalidType)?;
                let inst = $thing::from_bytes(&byte_str)
                    .ok_or(rusqlite::types::FromSqlError::InvalidType)?;
                Ok(inst)
            }
        }

        impl crate::util_lib::db::FromColumn<$thing> for $thing {
            fn from_column(
                row: &rusqlite::Row,
                column_name: &str,
            ) -> Result<Self, crate::util_lib::db::Error> {
                Ok(row.get::<_, Self>(column_name)?)
            }
        }

        impl rusqlite::types::ToSql for $thing {
            fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
                let hex_str = self.to_hex();
                Ok(hex_str.into())
            }
        }
    };
}

/// Load the path of the database from the connection
#[cfg(test)]
fn get_db_path(conn: &Connection) -> Result<String, Error> {
    let sql = "PRAGMA database_list";
    let path: Result<Option<String>, sqlite_error> =
        conn.query_row_and_then(sql, NO_PARAMS, |row| row.get(2));
    match path {
        Ok(Some(path)) => Ok(path),
        Ok(None) => Ok("<unknown>".to_string()),
        Err(e) => Err(Error::SqliteError(e)),
    }
}

/// Generate debug output to be fed into an external script to examine query plans.
/// TODO: it uses mocked arguments, which it assumes are strings. This does not always result in a
/// valid query.
#[cfg(test)]
fn log_sql_eqp(conn: &Connection, sql_query: &str) {
    if std::env::var("BLOCKSTACK_DB_TRACE") != Ok("1".to_string()) {
        return;
    }

    let mut parts = sql_query.split(" ");
    let mut full_sql = if let Some(part) = parts.next() {
        part.to_string()
    } else {
        sql_query.to_string()
    };

    while let Some(part) = parts.next() {
        if part.starts_with("?") {
            full_sql = format!("{} \"mock_arg\"", full_sql.trim());
        } else {
            full_sql = format!("{} {}", full_sql.trim(), part.trim());
        }
    }

    let path = get_db_path(conn).unwrap_or("ERROR!".to_string());
    let eqp_sql = format!("\"{}\" EXPLAIN QUERY PLAN {}", &path, full_sql.trim());
    debug!("{}", &eqp_sql);
}

#[cfg(not(test))]
fn log_sql_eqp(_conn: &Connection, _sql_query: &str) {}

/// boilerplate code for querying rows
pub fn query_rows<T, P>(conn: &Connection, sql_query: &str, sql_args: P) -> Result<Vec<T>, Error>
where
    P: Params,
    T: FromRow<T>,
{
    log_sql_eqp(conn, sql_query);
    let mut stmt = conn.prepare_cached(sql_query)?;
    let result = stmt.query_and_then(sql_args, |row| T::from_row(row))?;

    result.collect()
}

/// boilerplate code for querying a single row
///   if more than 1 row is returned, excess rows are ignored.
pub fn query_row<T, P>(conn: &Connection, sql_query: &str, sql_args: P) -> Result<Option<T>, Error>
where
    P: Params,
    T: FromRow<T>,
{
    log_sql_eqp(conn, sql_query);
    let mut stmt = conn.prepare_cached(sql_query)?;
    let mut result = stmt.query_and_then(sql_args, |row| T::from_row(row))?;
    match result.next() {
        Some(value) => Ok(Some(value?)),
        None => Ok(None),
    }
}

/// boilerplate code for querying a single row
///   if more than 1 row is returned, panic
pub fn query_expect_row<T, P>(
    conn: &Connection,
    sql_query: &str,
    sql_args: P,
) -> Result<Option<T>, Error>
where
    P: Params,
    T: FromRow<T>,
{
    log_sql_eqp(conn, sql_query);
    let mut stmt = conn.prepare_cached(sql_query)?;
    let mut result = stmt.query_and_then(sql_args, |row| T::from_row(row))?;
    let mut return_value = None;
    if let Some(value) = result.next() {
        return_value = Some(value?);
    }
    assert!(
        result.next().is_none(),
        "FATAL: Multiple values returned for query that expected a single result:\n {}",
        sql_query
    );
    Ok(return_value)
}

pub fn query_row_panic<T, P, F>(
    conn: &Connection,
    sql_query: &str,
    sql_args: P,
    panic_message: F,
) -> Result<Option<T>, Error>
where
    P: Params,
    T: FromRow<T>,
    F: FnOnce() -> String,
{
    log_sql_eqp(conn, sql_query);
    let mut stmt = conn.prepare_cached(sql_query)?;
    let mut result = stmt.query_and_then(sql_args, |row| T::from_row(row))?;
    let mut return_value = None;
    if let Some(value) = result.next() {
        return_value = Some(value?);
    }
    if result.next().is_some() {
        panic!("{}", &panic_message());
    }
    Ok(return_value)
}

/// boilerplate code for querying a column out of a sequence of rows
pub fn query_row_columns<T, P>(
    conn: &Connection,
    sql_query: &str,
    sql_args: P,
    column_name: &str,
) -> Result<Vec<T>, Error>
where
    P: Params,
    T: FromColumn<T>,
{
    log_sql_eqp(conn, sql_query);
    let mut stmt = conn.prepare_cached(sql_query)?;
    let mut rows = stmt.query(sql_args)?;

    // gather
    let mut row_data = vec![];
    while let Some(row) = rows.next().map_err(Error::SqliteError)? {
        let next_row = T::from_column(row, column_name)?;
        row_data.push(next_row);
    }

    Ok(row_data)
}

/// boilerplate code for querying a column out of a sequence of rows,
///  expecting exactly 0 or 1 results. panics if more.
pub fn query_one_row_column<T, P>(
    conn: &Connection,
    sql_query: &str,
    sql_args: P,
    column_name: &str,
    panic_msg: &str,
) -> Result<Option<T>, Error>
where
    P: Params,
    T: FromColumn<T>,
{
    log_sql_eqp(conn, sql_query);
    let mut stmt = conn.prepare_cached(sql_query)?;
    let mut rows = stmt.query(sql_args)?;

    // gather
    let mut result = None;
    while let Some(row) = rows.next().map_err(Error::SqliteError)? {
        if result.is_some() {
            panic!("{panic_msg}");
        }
        let next_row = T::from_column(row, column_name)?;
        result = Some(next_row);
    }

    Ok(result)
}

/// Boilerplate for querying a single integer (first and only item of the query must be an int)
pub fn query_int<P>(conn: &Connection, sql_query: &str, sql_args: P) -> Result<i64, Error>
where
    P: Params,
{
    log_sql_eqp(conn, sql_query);
    let mut stmt = conn.prepare_cached(sql_query)?;
    let mut rows = stmt.query(sql_args)?;
    let mut row_data = None;
    while let Some(row) = rows.next().map_err(Error::SqliteError)? {
        if row_data.is_some() {
            return Err(Error::Overflow);
        }
        let i: i64 = row.get(0)?;
        row_data = Some(i);
    }

    row_data.ok_or_else(|| Error::NotFoundError)
}

pub fn query_count<P>(conn: &Connection, sql_query: &str, sql_args: P) -> Result<i64, Error>
where
    P: Params,
{
    query_int(conn, sql_query, sql_args)
}

/// Run a PRAGMA statement.  This can't always be done via execute(), because it may return a result (and
/// rusqlite does not like this).
pub fn sql_pragma(
    conn: &Connection,
    pragma_name: &str,
    pragma_value: &dyn ToSql,
) -> Result<(), Error> {
    inner_sql_pragma(conn, pragma_name, pragma_value).map_err(Error::SqliteError)
}

fn inner_sql_pragma(
    conn: &Connection,
    pragma_name: &str,
    pragma_value: &dyn ToSql,
) -> Result<(), sqlite_error> {
    conn.pragma_update(None, pragma_name, pragma_value)
}

/// Run a VACUUM command
pub fn sql_vacuum(conn: &Connection) -> Result<(), Error> {
    conn.execute("VACUUM", NO_PARAMS)
        .map_err(Error::SqliteError)
        .map(|_| ())
}

/// Returns true if the database table `table_name` exists in the active
///  database of the provided SQLite connection.
pub fn table_exists(conn: &Connection, table_name: &str) -> Result<bool, sqlite_error> {
    let sql = "SELECT name FROM sqlite_master WHERE type='table' AND name=?";
    conn.query_row(sql, &[table_name], |row| row.get::<_, String>(0))
        .optional()
        .map(|r| r.is_some())
}

/// Set up an on-disk database with a MARF index if they don't exist yet.
/// Either way, returns the MARF path
pub fn db_mkdirs(path_str: &str) -> Result<String, Error> {
    let mut path = PathBuf::from(path_str);
    match fs::metadata(path_str) {
        Ok(md) => {
            if !md.is_dir() {
                error!("Not a directory: {:?}", path);
                return Err(Error::ExistsError);
            }
        }
        Err(e) => {
            if e.kind() != io::ErrorKind::NotFound {
                return Err(Error::IOError(e));
            }
            fs::create_dir_all(path_str).map_err(Error::IOError)?;
        }
    }

    path.push("marf.sqlite");
    let marf_path = path.to_str().ok_or_else(|| Error::ParseError)?.to_string();

    Ok(marf_path)
}

/// Read-only connection to a MARF-indexed DB
pub struct IndexDBConn<'a, C, T: MarfTrieId> {
    pub index: &'a MARF<T>,
    pub context: C,
}

impl<'a, C, T: MarfTrieId> IndexDBConn<'a, C, T> {
    pub fn new(index: &'a MARF<T>, context: C) -> IndexDBConn<'a, C, T> {
        IndexDBConn { index, context }
    }

    /// Get the ancestor block hash of a block of a given height, given a descendent block hash.
    pub fn get_ancestor_block_hash(
        &self,
        block_height: u64,
        tip_block_hash: &T,
    ) -> Result<Option<T>, Error> {
        get_ancestor_block_hash(self.index, block_height, tip_block_hash)
    }

    /// Get the height of an ancestor block, if it is indeed the ancestor.
    pub fn get_ancestor_block_height(
        &self,
        ancestor_block_hash: &T,
        tip_block_hash: &T,
    ) -> Result<Option<u64>, Error> {
        get_ancestor_block_height(&self.index, ancestor_block_hash, tip_block_hash)
    }

    /// Get a value from the fork index
    pub fn get_indexed(&self, header_hash: &T, key: &str) -> Result<Option<String>, Error> {
        let mut connection = self.index.reopen_connection()?;
        get_indexed(&mut connection, header_hash, key)
    }

    pub fn conn(&self) -> &DBConn {
        self.index.sqlite_conn()
    }
}

impl<C, T: MarfTrieId> Deref for IndexDBConn<'_, C, T> {
    type Target = DBConn;
    fn deref(&self) -> &DBConn {
        self.conn()
    }
}

pub struct IndexDBTx<'a, C: Clone, T: MarfTrieId> {
    _index: Option<MarfTransaction<'a, T>>,
    pub context: C,
    block_linkage: Option<(T, T)>,
}

impl<'a, C: Clone, T: MarfTrieId> Deref for IndexDBTx<'a, C, T> {
    type Target = DBTx<'a>;
    fn deref(&self) -> &DBTx<'a> {
        self.tx()
    }
}

impl<'a, C: Clone, T: MarfTrieId> DerefMut for IndexDBTx<'a, C, T> {
    fn deref_mut(&mut self) -> &mut DBTx<'a> {
        self.tx_mut()
    }
}

/// Called by `rusqlite` if we are waiting too long on a database lock
pub fn tx_busy_handler(run_count: i32) -> bool {
    stacks_common::util::db::tx_busy_handler(run_count)
}

/// Begin an immediate-mode transaction, and handle busy errors with exponential backoff.
/// Handling busy errors when the tx begins is preferable to doing it when the tx commits, since
/// then we don't have to worry about any extra rollback logic.
pub fn tx_begin_immediate(conn: &mut Connection) -> Result<DBTx<'_>, Error> {
    tx_begin_immediate_sqlite(conn).map_err(Error::from)
}

/// Begin an immediate-mode transaction, and handle busy errors with exponential backoff.
/// Handling busy errors when the tx begins is preferable to doing it when the tx commits, since
/// then we don't have to worry about any extra rollback logic.
/// Sames as `tx_begin_immediate` except that it returns a rusqlite error.
pub fn tx_begin_immediate_sqlite(conn: &mut Connection) -> Result<DBTx<'_>, sqlite_error> {
    conn.busy_handler(Some(tx_busy_handler))?;
    let tx = Transaction::new(conn, TransactionBehavior::Immediate)?;
    update_lock_table(tx.deref());
    Ok(tx)
}

#[cfg(feature = "profile-sqlite")]
fn trace_profile(query: &str, duration: std::time::Duration) {
    use serde_json::json;
    let obj = json!({"millis":duration.as_millis(), "query":query});
    debug!(
        "sqlite trace profile {}",
        serde_json::to_string(&obj).unwrap()
    );
}

#[cfg(feature = "profile-sqlite")]
fn inner_connection_open<P: AsRef<Path>>(
    path: P,
    flags: OpenFlags,
) -> Result<Connection, sqlite_error> {
    let mut db = Connection::open_with_flags(path, flags)?;
    db.profile(Some(trace_profile));
    Ok(db)
}

#[cfg(not(feature = "profile-sqlite"))]
fn inner_connection_open<P: AsRef<Path>>(
    path: P,
    flags: OpenFlags,
) -> Result<Connection, sqlite_error> {
    Connection::open_with_flags(path, flags)
}

/// Open a database connection and set some typically-used pragmas.
/// Connections are always opened in SQLite multi-thread (`NO_MUTEX`) mode;
/// passing `FULL_MUTEX` panics.
pub fn sqlite_open<P: AsRef<Path>>(
    path: P,
    mut flags: OpenFlags,
    foreign_keys: bool,
) -> Result<Connection, sqlite_error> {
    // Without an explicit mutex flag the bundled SQLite defaults to serialized
    // mode, whose per-connection mutex is pure overhead here: `Connection` is
    // `!Sync`, so no thread can ever contend on it.
    assert!(
        !flags.contains(OpenFlags::SQLITE_OPEN_FULL_MUTEX),
        "sqlite_open always opens in multi-thread mode; FULL_MUTEX is not supported"
    );
    flags.insert(OpenFlags::SQLITE_OPEN_NO_MUTEX);
    let db = inner_connection_open(path, flags)?;
    db.busy_handler(Some(tx_busy_handler))?;
    db.set_prepared_statement_cache_capacity(SQLITE_STATEMENT_CACHE_CAPACITY);
    if !flags.contains(OpenFlags::SQLITE_OPEN_READ_ONLY) {
        inner_sql_pragma(&db, "journal_mode", &"WAL")?;
    }
    inner_sql_pragma(&db, "synchronous", &"NORMAL")?;
    if foreign_keys {
        inner_sql_pragma(&db, "foreign_keys", &true)?;
    }
    Ok(db)
}

/// Get the ancestor block hash of a block of a given height, given a descendent block hash.
pub fn get_ancestor_block_hash<T: MarfTrieId>(
    index: &MARF<T>,
    block_height: u64,
    tip_block_hash: &T,
) -> Result<Option<T>, Error> {
    let block_height = block_height
        .try_into()
        .map_err(|_e| Error::BlockHeightOutOfRange)?;
    let mut read_only = index.reopen_connection()?;
    let bh = read_only.get_block_at_height(block_height, tip_block_hash)?;
    Ok(bh)
}

/// Get the height of an ancestor block, if it is indeed the ancestor.
pub fn get_ancestor_block_height<T: MarfTrieId>(
    index: &MARF<T>,
    ancestor_block_hash: &T,
    tip_block_hash: &T,
) -> Result<Option<u64>, Error> {
    let mut read_only = index.reopen_connection()?;
    let height_opt = read_only
        .get_block_height(ancestor_block_hash, tip_block_hash)?
        .map(|height| height as u64);
    Ok(height_opt)
}

/// Load some index data
fn load_indexed(conn: &DBConn, marf_value: &MARFValue) -> Result<Option<String>, Error> {
    let mut stmt = conn
        .prepare_cached("SELECT value FROM __fork_storage WHERE value_hash = ?1 LIMIT 2")
        .map_err(Error::SqliteError)?;
    let mut rows = stmt
        .query(params![marf_value.to_hex()])
        .map_err(Error::SqliteError)?;
    let mut value = None;

    while let Some(row) = rows.next()? {
        let value_str: String = row.get(0)?;
        if value.is_some() {
            // should be impossible
            panic!(
                "FATAL: two or more values for {}",
                &to_hex(&marf_value.to_vec())
            );
        }
        value = Some(value_str);
    }

    Ok(value)
}

/// Get a value from the fork index
fn get_indexed<T: MarfTrieId, M: MarfConnection<T>>(
    index: &mut M,
    header_hash: &T,
    key: &str,
) -> Result<Option<String>, Error> {
    match index.get(header_hash, key) {
        Ok(Some(marf_value)) => {
            let value = load_indexed(index.sqlite_conn(), &marf_value)?
                .unwrap_or_else(|| panic!("FATAL: corrupt index: key '{}' from {} is present in the index but missing a value in the DB", &key, &header_hash));
            Ok(Some(value))
        }
        Ok(None) => Ok(None),
        Err(MARFError::NotFoundError) => Ok(None),
        Err(e) => {
            error!(
                "Failed to fetch '{}' off of {}: {:?}",
                key, &header_hash, &e
            );
            Err(Error::Corruption)
        }
    }
}

impl<'a, C: Clone, T: MarfTrieId> IndexDBTx<'a, C, T> {
    pub fn new(index: &'a mut MARF<T>, context: C) -> IndexDBTx<'a, C, T> {
        let tx = index
            .begin_tx()
            .expect("BUG: failure to begin MARF transaction");
        IndexDBTx {
            _index: Some(tx),
            block_linkage: None,
            context,
        }
    }

    pub fn index(&self) -> &MarfTransaction<'a, T> {
        self._index
            .as_ref()
            .expect("BUG: MarfTransaction lost, but IndexDBTx still exists")
    }

    fn index_mut(&mut self) -> &mut MarfTransaction<'a, T> {
        self._index
            .as_mut()
            .expect("BUG: MarfTransaction lost, but IndexDBTx still exists")
    }

    pub fn tx(&self) -> &DBTx<'a> {
        self.index().sqlite_tx()
    }

    pub fn tx_mut(&mut self) -> &mut DBTx<'a> {
        self.index_mut().sqlite_tx_mut()
    }

    pub fn instantiate_index(&mut self) -> Result<(), Error> {
        self.tx()
            .execute(
                r#"
        -- fork-specific key/value storage, indexed via a MARF.
        -- each row is guaranteed to be unique
        CREATE TABLE IF NOT EXISTS __fork_storage(
            value_hash TEXT NOT NULL,
            value TEXT NOT NULL,

            PRIMARY KEY(value_hash)
        );
        "#,
                NO_PARAMS,
            )
            .map_err(Error::SqliteError)?;
        Ok(())
    }

    /// Get the ancestor block hash of a block of a given height, given a descendent block hash.
    pub fn get_ancestor_block_hash(
        &mut self,
        block_height: u64,
        tip_block_hash: &T,
    ) -> Result<Option<T>, Error> {
        self.index_mut()
            .get_block_at_height(
                block_height.try_into().expect("Height > u32::max()"),
                tip_block_hash,
            )
            .map_err(Error::from)
    }

    /// Get the height of an ancestor block, if it is indeed the ancestor.
    pub fn get_ancestor_block_height(
        &mut self,
        ancestor_block_hash: &T,
        tip_block_hash: &T,
    ) -> Result<Option<u64>, Error> {
        let height_opt = self
            .index_mut()
            .get_block_height(ancestor_block_hash, tip_block_hash)?
            .map(|height| height as u64);
        Ok(height_opt)
    }

    /// Store some data to the index storage.
    fn store_indexed(&mut self, value: &String) -> Result<MARFValue, Error> {
        let marf_value = MARFValue::from_value(value);
        #[cfg(feature = "commit-residency-diagnostics")]
        let _io = crate::util_lib::db_io_probe::QueryProbe::begin(self.tx());
        self.tx().execute(
            "INSERT OR REPLACE INTO __fork_storage (value_hash, value) VALUES (?1, ?2)",
            &[&to_hex(&marf_value.to_vec()), value],
        )?;
        Ok(marf_value)
    }

    /// Get a value from the fork index
    pub fn get_indexed(&mut self, header_hash: &T, key: &str) -> Result<Option<String>, Error> {
        get_indexed(self.index_mut(), header_hash, key)
    }

    /// Get a value from the fork index, but with a read-only reference
    pub fn get_indexed_ref(&self, header_hash: &T, key: &str) -> Result<Option<String>, Error> {
        let mut ro_index = self.index().reopen_readonly()?;
        get_indexed(&mut ro_index, header_hash, key)
    }

    /// Put all keys and values in a single MARF transaction, and seal it.
    /// This is a one-time operation; subsequent calls will panic.  You should follow this up with
    /// a commit if you want to save the MARF state.
    pub fn put_indexed_all(
        &mut self,
        parent_header_hash: &T,
        header_hash: &T,
        keys: &[String],
        values: &[String],
    ) -> Result<TrieHash, Error> {
        assert_eq!(keys.len(), values.len());
        match self.block_linkage {
            None => {
                #[cfg(feature = "commit-residency-diagnostics")]
                let _begin = stacks_profiler::diagnostic_span!("Headers index: Begin");
                self.index_mut().begin(parent_header_hash, header_hash)?;
                self.block_linkage = Some((parent_header_hash.clone(), header_hash.clone()));
            }
            Some(_) => panic!("Tried to put_indexed_all twice!"),
        }

        #[cfg(feature = "commit-residency-diagnostics")]
        let _values = stacks_profiler::diagnostic_span!("Headers index: Store values SQL");
        let mut marf_values = Vec::with_capacity(values.len());
        for value in values.iter() {
            let marf_value = self.store_indexed(value)?;
            marf_values.push(marf_value);
        }

        #[cfg(feature = "commit-residency-diagnostics")]
        drop(_values);
        #[cfg(feature = "commit-residency-diagnostics")]
        let _insert = stacks_profiler::diagnostic_span!("Headers index: Insert batch");
        self.index_mut().insert_batch(keys, marf_values)?;
        #[cfg(feature = "commit-residency-diagnostics")]
        drop(_insert);
        #[cfg(feature = "commit-residency-diagnostics")]
        let _seal = stacks_profiler::diagnostic_span!("Headers index: Seal");
        let root_hash = self.index_mut().seal()?;
        Ok(root_hash)
    }

    /// Commit the MARF transaction
    pub fn commit(mut self) -> Result<(), Error> {
        self.block_linkage = None;
        debug!("Indexed-commit: MARF index");
        let index_tx = self
            ._index
            .take()
            .expect("BUG: MarfTransaction lost, but IndexDBTx still exists");
        index_tx.commit()?;
        Ok(())
    }

    /// Get the root hash
    pub fn get_root_hash_at(&mut self, bhh: &T) -> Result<TrieHash, Error> {
        let root_hash = self.index_mut().get_root_hash_at(bhh)?;
        Ok(root_hash)
    }
}

impl<C: Clone, T: MarfTrieId> Drop for IndexDBTx<'_, C, T> {
    fn drop(&mut self) {
        if let Some((ref parent, ref child)) = self.block_linkage {
            let index_tx = self
                ._index
                .take()
                .expect("BUG: MarfTransaction lost, but IndexDBTx still exists");
            debug!("Dropping MARF linkage ({},{})", parent, child);
            index_tx.drop_current();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{assert_matches, fs};

    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::unix_absolute("/tmp/marf-squash/index.sqlite", "/tmp/marf-squash/index.sqlite")]
    #[case::windows_drive_letter("C:/Users/test/index.sqlite", "C:/Users/test/index.sqlite")]
    #[case::unreserved_chars_pass_through("/abc-DEF_123.~", "/abc-DEF_123.~")]
    #[case::space_and_uri_structurals("/tmp/has space/file?x#y", "/tmp/has%20space/file%3Fx%23y")]
    #[case::percent_literal_encoded("/tmp/100%/x", "/tmp/100%25/x")]
    // `é` is U+00E9 = 0xC3 0xA9 in UTF-8; non-ASCII bytes always encode.
    #[case::non_ascii_as_utf8_bytes("/tmp/café", "/tmp/caf%C3%A9")]
    fn sqlite_readonly_uri_encodes_paths(#[case] input: &str, #[case] expected: &str) {
        assert_eq!(
            sqlite_readonly_uri(Path::new(input), false).unwrap(),
            format!("file:{expected}?mode=ro")
        );
    }

    #[test]
    fn sqlite_readonly_uri_supports_immutable_mode() {
        assert_eq!(
            sqlite_readonly_uri(Path::new("/tmp/source.sqlite"), true).unwrap(),
            "file:/tmp/source.sqlite?mode=ro&immutable=1"
        );
    }

    #[test]
    fn sqlite_schema_helpers_quote_and_inventory_objects() {
        assert_eq!(quote_sql_identifier("quoted\"name"), "\"quoted\"\"name\"");

        let connection = Connection::open_in_memory().unwrap();
        let table = quote_sql_identifier("value\"table");
        let index = quote_sql_identifier("value\"index");
        connection
            .execute_batch(&format!(
                "CREATE TABLE {table} (value INTEGER); \
                 CREATE INDEX {index} ON {table}(value);"
            ))
            .unwrap();

        let objects = sqlite_schema_objects(&connection, "main").unwrap();
        assert_eq!(objects.len(), 2);
        assert_eq!(objects[0].kind, "table");
        assert_eq!(objects[0].name, "value\"table");
        assert_eq!(objects[1].kind, "index");
        assert_eq!(objects[1].table, "value\"table");

        assert_matches!(
            sqlite_schema_objects(&connection, "main; ATTACH"),
            Err(sqlite_error::InvalidParameterName(name)) if name == "main; ATTACH"
        );
    }

    #[test]
    fn test_pragma() {
        let path = "/tmp/blockstack_db_test_pragma.db";
        if fs::metadata(path).is_ok() {
            fs::remove_file(path).unwrap();
        }

        // calls pragma_update with both journal_mode and foreign_keys
        let db = sqlite_open(
            path,
            OpenFlags::SQLITE_OPEN_CREATE | OpenFlags::SQLITE_OPEN_READ_WRITE,
            true,
        )
        .unwrap();

        // journal mode must be WAL
        db.pragma_query(None, "journal_mode", |row| {
            let value: String = row.get(0)?;
            assert_eq!(value, "wal");
            Ok(())
        })
        .unwrap();

        // foreign keys must be on
        db.pragma_query(None, "foreign_keys", |row| {
            let value: i64 = row.get(0)?;
            assert_eq!(value, 1);
            Ok(())
        })
        .unwrap();
    }
}
