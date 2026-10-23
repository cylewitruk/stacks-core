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

//! Native opening/session APIs preserve the existing shared SQLite transaction.
use std::path::Path;

use rusqlite::{Connection, Transaction};

use super::{MARF, MARFOpenOpts, MarfTransaction, ReopenedMarf, TrieFileStorage, View};
use crate::archive::PruneInfo;
use crate::observer::IntegrationObserver;
use crate::sqlite::{self, SqliteAccess};
use crate::{Error, Id, MarfTrieId};
impl<T: MarfTrieId> MARF<T> {
    /// Construct the facade around configured storage.
    pub fn from_storage(storage: TrieFileStorage<T>) -> Self {
        Self {
            inner: marf_core::MARF::from_storage(storage.inner),
            info: storage.info,
        }
    }
    /// Open a confirmed SQLite-backed MARF.
    pub fn from_path(path: impl AsRef<Path>, opts: MARFOpenOpts) -> Result<Self, Error> {
        sqlite::configure_vfs()?;
        Self::wrap(marf_core::MARF::from_path(path, opts)?)
    }
    /// Open mutable unconfirmed state in the same storage.
    pub fn from_path_unconfirmed(
        path: impl AsRef<Path>,
        opts: MARFOpenOpts,
    ) -> Result<Self, Error> {
        sqlite::configure_vfs()?;
        Self::wrap(marf_core::MARF::from_path_unconfirmed(path, opts)?)
    }
    /// Begin the transaction shared with Clarity's metadata side store.
    pub fn begin_tx(&mut self) -> Result<MarfTransaction<'_, T>, Error> {
        Ok(MarfTransaction {
            inner: self.inner.begin_tx()?,
            info: &self.info,
        })
    }
    /// Native transaction for side tables; do not replace/control it or mutate MARF records.
    pub fn storage_tx(&mut self) -> Result<Transaction<'_>, rusqlite::Error> {
        self.inner.storage_tx()
    }
    /// Consume the engine and return its native connection.
    pub fn into_sqlite_conn(self) -> Connection {
        self.inner.into_sqlite_conn()
    }
}
impl<'a, T: MarfTrieId> MarfTransaction<'a, T> {
    /// The same native transaction used for MARF publication. Native access preconditions apply.
    pub fn sqlite_tx(&self) -> &Transaction<'a> {
        self.inner.sqlite_tx()
    }
    /// Native transaction access; replacement, transaction control and MARF record mutation are forbidden.
    pub fn sqlite_tx_mut(&mut self) -> &mut Transaction<'a> {
        self.inner.sqlite_tx_mut()
    }
}
impl<T: MarfTrieId> TrieFileStorage<T> {
    /// Open confirmed writable storage.
    pub fn open(path: impl AsRef<Path>, opts: MARFOpenOpts) -> Result<Self, Error> {
        sqlite::configure_vfs()?;
        let inner =
            marf_core::marf::TrieFileStorage::<Id<T>, IntegrationObserver>::open(path, opts)?;
        let info = inner.prune_info().map(PruneInfo::from);
        Ok(Self { inner, info })
    }
    /// Open historical storage without migration or writes.
    pub fn open_readonly(path: impl AsRef<Path>, opts: MARFOpenOpts) -> Result<Self, Error> {
        sqlite::configure_vfs()?;
        let inner = marf_core::marf::TrieFileStorage::<Id<T>, IntegrationObserver>::open_readonly(
            path, opts,
        )?;
        let info = inner.prune_info().map(PruneInfo::from);
        Ok(Self { inner, info })
    }
    /// Open mutable unconfirmed storage.
    pub fn open_unconfirmed(path: impl AsRef<Path>, opts: MARFOpenOpts) -> Result<Self, Error> {
        sqlite::configure_vfs()?;
        let inner =
            marf_core::marf::TrieFileStorage::<Id<T>, IntegrationObserver>::open_unconfirmed(
                path, opts,
            )?;
        let info = inner.prune_info().map(PruneInfo::from);
        Ok(Self { inner, info })
    }
    /// Cached archive boundary metadata.
    pub fn prune_info(&self) -> Option<&PruneInfo> {
        self.info.as_ref()
    }
    /// Native connection for side-table queries.
    pub fn sqlite_conn(&self) -> &Connection {
        self.inner.sqlite_conn()
    }
}
impl<T: MarfTrieId> ReopenedMarf<'_, T> {
    pub fn db_conn(&self) -> &Connection {
        self.inner.db_conn()
    }
}
impl<T: MarfTrieId> SqliteAccess for MARF<T> {
    fn sqlite_conn(&self) -> &Connection {
        self.inner.sqlite_conn()
    }
}
impl<T: MarfTrieId> SqliteAccess for MarfTransaction<'_, T> {
    fn sqlite_conn(&self) -> &Connection {
        self.inner.sqlite_conn()
    }
}
impl<T: MarfTrieId> SqliteAccess for ReopenedMarf<'_, T> {
    fn sqlite_conn(&self) -> &Connection {
        self.inner.sqlite_conn()
    }
}
impl<T: MarfTrieId> SqliteAccess for View<'_, T> {
    fn sqlite_conn(&self) -> &Connection {
        self.inner.sqlite_conn()
    }
}
impl<T: MarfTrieId> SqliteAccess for TrieFileStorage<T> {
    fn sqlite_conn(&self) -> &Connection {
        self.inner.sqlite_conn()
    }
}
