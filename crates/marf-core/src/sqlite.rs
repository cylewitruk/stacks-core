// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020-2026 Stacks Open Internet Foundation
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

//! Explicit native SQLite integration; this is not a backend substitution API.
use std::path::Path;
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

use rand::{Rng, thread_rng};
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior};

use crate::TrieId;
use crate::bytes::{from_hex, to_hex};
use crate::observe::Observer;

/// SQLite mapping size used by the existing engine.
pub const SQLITE_MMAP_SIZE: i64 = 256 * 1024 * 1024;
/// Existing MARF database page size.
pub const SQLITE_MARF_PAGE_SIZE: i64 = 32768;
/// Prepared statement cache capacity.
pub const SQLITE_STATEMENT_CACHE_CAPACITY: usize = 200;
/// Empty SQL parameter list.
pub const NO_PARAMS: &[&dyn ToSql] = &[];
/// Native access shares the engine's active session.
///
/// Callers must not end/replace transactions or mutate MARF-owned records.
/// Ordinary side-store/schema operations are supported. Commit-time checks
/// cannot prevent arbitrary SQL or undo an externally committed transaction.
pub trait SqliteAccess {
    /// The engine's connection, including the active transaction when present.
    fn sqlite_conn(&self) -> &Connection;
}
/// SQL's canonical text encoding of a neutral identity.
pub struct SqlId<'a, I: TrieId>(pub &'a I);
impl<I: TrieId> ToSql for SqlId<'_, I> {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(to_hex(self.0.as_bytes()).into())
    }
}
/// A neutral identity decoded from a legacy text column.
pub struct StoredId<I>(pub I);
impl<I: TrieId> FromSql for StoredId<I> {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let text = value.as_str()?;
        let bytes = from_hex(text).map_err(|_| FromSqlError::InvalidType)?;
        let bytes = bytes
            .as_slice()
            .try_into()
            .map_err(|_| FromSqlError::InvalidType)?;
        Ok(Self(I::from_bytes(bytes)))
    }
}
/// SQLite VFS selection, fixed before the first core connection opens.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Vfs {
    /// Platform default locking.
    #[default]
    Default,
    /// Exclusive Unix locking, with read-only access enforced by query_only.
    UnixExcl,
}
static VFS: OnceLock<Vfs> = OnceLock::new();
/// Select one process-wide VFS; selecting the current value is idempotent.
pub fn set_vfs(vfs: Vfs) -> Result<(), Vfs> {
    let fixed = *VFS.get_or_init(|| vfs);
    if fixed == vfs { Ok(()) } else { Err(fixed) }
}
/// Busy retries preserve the existing exponential/jittered timing policy.
pub fn tx_busy_handler<O: Observer>(run_count: i32) -> bool {
    let run_count = run_count.unsigned_abs();
    let mut sleep_ms = 2u64.saturating_pow(run_count);
    sleep_ms = sleep_ms.saturating_add(thread_rng().gen_range(0..sleep_ms));
    if sleep_ms > 100 {
        sleep_ms = thread_rng().gen_range(90..110);
    }
    O::sqlite_busy(run_count, sleep_ms);
    thread::sleep(Duration::from_millis(sleep_ms));
    true
}
/// Apply a PRAGMA that may return rows.
pub fn sql_pragma(conn: &Connection, name: &str, value: &dyn ToSql) -> rusqlite::Result<()> {
    conn.pragma_update(None, name, value)
}
/// Whether the SQLite schema contains the named table.
pub fn table_exists(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
        [name],
        |r| r.get(0),
    )?;
    Ok(count > 0)
}
/// Open with the existing mutex, retry, cache and durability policy.
pub fn sqlite_open<O: Observer>(
    path: impl AsRef<Path>,
    mut flags: OpenFlags,
    foreign_keys: bool,
) -> rusqlite::Result<Connection> {
    assert!(
        !flags.contains(OpenFlags::SQLITE_OPEN_FULL_MUTEX),
        "sqlite_open always opens in multi-thread mode; FULL_MUTEX is not supported"
    );
    let readonly = flags.contains(OpenFlags::SQLITE_OPEN_READ_ONLY);
    let vfs = *VFS.get_or_init(Vfs::default);
    let emulate_readonly = readonly && vfs == Vfs::UnixExcl;
    if emulate_readonly {
        flags.remove(OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_CREATE);
        flags.insert(OpenFlags::SQLITE_OPEN_READ_WRITE);
    }
    flags.insert(OpenFlags::SQLITE_OPEN_NO_MUTEX);
    let db = match vfs {
        Vfs::Default => Connection::open_with_flags(path, flags),
        Vfs::UnixExcl => Connection::open_with_flags_and_vfs(path, flags, "unix-excl"),
    }?;
    #[cfg(feature = "profile-sqlite")]
    let db = {
        let mut db = db;
        db.profile(Some(O::sqlite_profile));
        db
    };
    if emulate_readonly {
        sql_pragma(&db, "query_only", &true)?;
    }
    db.busy_handler(Some(tx_busy_handler::<O>))?;
    db.set_prepared_statement_cache_capacity(SQLITE_STATEMENT_CACHE_CAPACITY);
    if !readonly {
        sql_pragma(&db, "journal_mode", &"WAL")?;
    }
    sql_pragma(&db, "synchronous", &"NORMAL")?;
    if foreign_keys {
        sql_pragma(&db, "foreign_keys", &true)?;
    }
    Ok(db)
}
/// Begin the same immediate transaction used by MARF and integrator side stores.
pub fn tx_begin_immediate<O: Observer>(conn: &mut Connection) -> rusqlite::Result<Transaction<'_>> {
    conn.busy_handler(Some(tx_busy_handler::<O>))?;
    let tx = Transaction::new(conn, TransactionBehavior::Immediate)?;
    let connection: &Connection = &tx;
    O::sqlite_locked(format_args!("{connection:?}"));
    Ok(tx)
}
