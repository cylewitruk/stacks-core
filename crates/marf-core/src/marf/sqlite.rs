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

//! Concrete native handles and storage opening for the built-in SQLite engine.
use rusqlite::{Connection, Transaction};

use super::{Inner, MARF, MARFOpenOpts, MarfTransaction, ReopenedMarf, View};
use crate::archive::PruneInfo;
use crate::observe::{Noop, Observer};
use crate::sqlite::{SqliteAccess, tx_begin_immediate};
use crate::storage::{self, StorageContext, Store};
use crate::{Error, TrieId};

/// Open storage without a MARF wrapper (kept for API compatibility).
pub struct TrieFileStorage<T: TrieId, O: Observer = Noop> {
    db: Connection,
    store: Store<T, O>,
}

impl<T: TrieId, O: Observer> MARF<T, O> {
    pub fn from_storage(storage: TrieFileStorage<T, O>) -> MARF<T, O> {
        MARF {
            db: storage.db,
            inner: Inner {
                store: storage.store,
                building: None,
                current: T::sentinel(),
            },
        }
    }
    pub fn from_path(
        path: impl AsRef<std::path::Path>,
        open_opts: MARFOpenOpts,
    ) -> Result<MARF<T, O>, Error> {
        Ok(MARF::from_storage(TrieFileStorage::open(path, open_opts)?))
    }
    pub fn from_path_unconfirmed(
        path: impl AsRef<std::path::Path>,
        open_opts: MARFOpenOpts,
    ) -> Result<MARF<T, O>, Error> {
        Ok(MARF::from_storage(TrieFileStorage::open_unconfirmed(
            path, open_opts,
        )?))
    }
    pub fn begin_tx(&mut self) -> Result<MarfTransaction<'_, T, O>, Error> {
        if self.inner.store.policy().readonly {
            return Err(Error::ReadOnlyError);
        }
        let tx = tx_begin_immediate::<O>(&mut self.db)?;
        Ok(MarfTransaction {
            tx: Some(tx),
            inner: &mut self.inner,
        })
    }

    /// Make a raw transaction to the underlying storage
    pub fn storage_tx(&mut self) -> Result<Transaction<'_>, rusqlite::Error> {
        tx_begin_immediate::<O>(&mut self.db)
    }
    pub fn into_sqlite_conn(self) -> Connection {
        self.db
    }
}
impl<'a, T: TrieId, O: Observer> MarfTransaction<'a, T, O> {
    pub fn sqlite_tx(&self) -> &Transaction<'a> {
        self.tx.as_ref().expect("transaction still open")
    }
    pub fn sqlite_tx_mut(&mut self) -> &mut Transaction<'a> {
        self.tx.as_mut().expect("transaction still open")
    }
}
impl<T: TrieId, O: Observer> TrieFileStorage<T, O> {
    fn open_opts(
        db_path: impl AsRef<std::path::Path>,
        readonly: bool,
        unconfirmed: bool,
        opts: MARFOpenOpts,
    ) -> Result<Self, Error> {
        let (db, store) = storage::open::<T, O>(
            db_path,
            opts.external_blobs,
            opts.compress,
            readonly,
            unconfirmed,
            opts.force_db_migrate,
            opts.patch_cache_capacity,
        )?;
        Ok(TrieFileStorage { db, store })
    }

    pub fn open(db_path: impl AsRef<std::path::Path>, opts: MARFOpenOpts) -> Result<Self, Error> {
        Self::open_opts(db_path, false, false, opts)
    }

    pub fn open_readonly(
        db_path: impl AsRef<std::path::Path>,
        opts: MARFOpenOpts,
    ) -> Result<Self, Error> {
        Self::open_opts(db_path, true, false, opts)
    }

    pub fn open_unconfirmed(
        db_path: impl AsRef<std::path::Path>,
        opts: MARFOpenOpts,
    ) -> Result<Self, Error> {
        Self::open_opts(db_path, false, true, opts)
    }

    pub fn sqlite_conn(&self) -> &Connection {
        &self.db
    }

    pub fn prune_info(&self) -> Option<&PruneInfo> {
        self.store.prune_info()
    }
}

impl<'a, T: TrieId, O: Observer> ReopenedMarf<'a, T, O> {
    pub fn db_conn(&self) -> &Connection {
        self.db
    }

    pub fn connection(&mut self) -> View<'_, T, O> {
        View {
            db: self.db,
            inner: &mut self.inner,
        }
    }
}

impl<T: TrieId, O: Observer> SqliteAccess for MARF<T, O> {
    fn sqlite_conn(&self) -> &Connection {
        &self.db
    }
}
impl<T: TrieId, O: Observer> SqliteAccess for MarfTransaction<'_, T, O> {
    fn sqlite_conn(&self) -> &Connection {
        self.sqlite_tx()
    }
}
impl<T: TrieId, O: Observer> SqliteAccess for ReopenedMarf<'_, T, O> {
    fn sqlite_conn(&self) -> &Connection {
        self.db
    }
}
impl<T: TrieId, O: Observer> SqliteAccess for View<'_, T, O> {
    fn sqlite_conn(&self) -> &Connection {
        self.db
    }
}
impl<T: TrieId, O: Observer> SqliteAccess for TrieFileStorage<T, O> {
    fn sqlite_conn(&self) -> &Connection {
        &self.db
    }
}

impl<T: TrieId, O: Observer> View<'_, T, O> {
    pub fn sqlite_conn(&self) -> &Connection {
        self.db
    }
}
