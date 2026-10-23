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

//! Explicit native access to the configured SQLite implementation.
//! Callers must not replace handles, control transactions or mutate MARF-owned
//! records. Side-table operations share the MARF transaction. Commit detects
//! an externally ended transaction, but cannot undo an external commit or
//! protect intervening reads after external rollback.
pub use marf_core::sqlite::SqliteAccess;
use marf_core::sqlite::{self, Vfs};
use stacks_common::util::db::{self, SqliteVfs};

use crate::Error;
/// Fix the common SQLite VFS selection before opening an engine connection.
pub fn configure_vfs() -> Result<(), Error> {
    let requested = db::sqlite_vfs();
    let actual = match db::set_sqlite_vfs(requested) {
        Ok(()) => requested,
        Err(actual) => actual,
    };
    let vfs = match actual {
        SqliteVfs::Default => Vfs::Default,
        SqliteVfs::UnixExcl => Vfs::UnixExcl,
    };
    sqlite::set_vfs(vfs)
        .map_err(|_| Error::CorruptionError("SQLite VFS was initialized inconsistently".into()))
}
