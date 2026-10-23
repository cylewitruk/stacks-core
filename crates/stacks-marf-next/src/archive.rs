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

//! Compatibility factories; archive algorithms consume neutral core capabilities.
use std::path::{Path, PathBuf};

use marf_core::archive::{ArchiveDestination, ArchiveSource};
use rusqlite::Connection;
use stacks_common::types::chainstate::TrieHash;

use crate::marf::{MARF, MARFOpenOpts, TrieHashCalculationMode};
use crate::observer::IntegrationObserver;
use crate::sqlite::SqliteAccess;
use crate::{Error, Id, MarfTrieId, stacks_hash};
/// Cached archival boundary using Stacks hash types.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PruneInfo {
    pub archival_marf_root_hash: TrieHash,
    pub prune_root_node_hash: TrieHash,
    pub prune_height: u32,
}
impl From<&marf_core::archive::PruneInfo> for PruneInfo {
    fn from(p: &marf_core::archive::PruneInfo) -> Self {
        Self {
            archival_marf_root_hash: stacks_hash(p.archival_marf_root_hash),
            prune_root_node_hash: stacks_hash(p.prune_root_node_hash),
            prune_height: p.prune_height,
        }
    }
}
/// Native archival metadata translated at the Stacks boundary.
pub trait ArchiveMetadata {
    fn read_prune_info(&self) -> Result<Option<PruneInfo>, Error>;
}
impl ArchiveMetadata for Connection {
    fn read_prune_info(&self) -> Result<Option<PruneInfo>, Error> {
        marf_core::archive::ArchiveMetadata::read_prune_info(self)
            .map(|p| p.as_ref().map(PruneInfo::from))
    }
}
/// Factory policy kept outside the pruning algorithm.
pub trait Archive<T: MarfTrieId> {
    type Source: ArchiveSource<Id<T>, Error = Error>;
    type Destination: ArchiveDestination<Id<T>, Error = Error>;
    fn blobs_path(path: &Path) -> PathBuf;
    fn open_archive_source(path: &Path, opts: MARFOpenOpts) -> Result<Self::Source, Error>;
    fn create_archive_destination(path: &Path) -> Result<Self::Destination, Error>;
}
/// Lazily open the native destination at the algorithm's publication phase.
/// No destination files are created while source collection/hashing can fail.
pub struct ArchiveTarget<T: MarfTrieId> {
    path: PathBuf,
    engine: Option<marf_core::MARF<Id<T>, IntegrationObserver>>,
}
impl<T: MarfTrieId> Archive<T> for MARF<T> {
    type Source = marf_core::MARF<Id<T>, IntegrationObserver>;
    type Destination = ArchiveTarget<T>;
    fn blobs_path(path: &Path) -> PathBuf {
        marf_core::storage::Blobs::path_for(path)
    }
    fn open_archive_source(path: &Path, opts: MARFOpenOpts) -> Result<Self::Source, Error> {
        crate::sqlite::configure_vfs()?;
        Ok(marf_core::MARF::from_storage(
            marf_core::marf::TrieFileStorage::open_readonly(path, opts)?,
        ))
    }
    fn create_archive_destination(path: &Path) -> Result<Self::Destination, Error> {
        Ok(ArchiveTarget {
            path: path.to_owned(),
            engine: None,
        })
    }
}
impl<T: MarfTrieId> ArchiveDestination<Id<T>> for ArchiveTarget<T> {
    type Error = Error;
    type Transaction<'a>
        = marf_core::MarfTransaction<'a, Id<T>, IntegrationObserver>
    where
        Self: 'a;
    fn begin_archive(&mut self) -> Result<Self::Transaction<'_>, Error> {
        if self.engine.is_none() {
            crate::sqlite::configure_vfs()?;
            let e = marf_core::MARF::from_path(
                &self.path,
                MARFOpenOpts::new(TrieHashCalculationMode::Deferred, true),
            )?;
            e.sqlite_conn().pragma_update(None, "journal_mode", "OFF")?;
            e.sqlite_conn().pragma_update(None, "synchronous", "OFF")?;
            self.engine = Some(e);
        }
        self.engine
            .as_mut()
            .expect("destination initialized")
            .begin_archive()
    }
    fn finalize(&mut self) -> Result<(), Error> {
        self.engine
            .as_mut()
            .expect("archive transaction completed")
            .finalize()
    }
}
