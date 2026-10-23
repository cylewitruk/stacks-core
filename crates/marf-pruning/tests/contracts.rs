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

//! Failure boundaries preserve typed errors and discard temporary stage resources.
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use marf_core::archive::{
    ArchiveBlob, ArchiveDestination, ArchiveSource, ArchiveTransaction, ArchiveWriteError,
    PruneInfo,
};
use marf_core::{MARF, MARFOpenOpts, MARFValue, TrieHash, TrieId, TrieId32};
use marf_pruning::{PruneOptions, prune};
/// A publication sink that fails inside streaming, before metadata can be written.
struct Destination {
    metadata: AtomicBool,
    source_dropped: Arc<AtomicBool>,
}
struct Transaction<'a>(&'a Destination);
struct FailedBlob<'a>(&'a AtomicBool);
impl ArchiveBlob for FailedBlob<'_> {
    type Error = io::Error;
    fn write_at_unsynced(&self, _: u64, _: &[u8]) -> io::Result<()> {
        assert!(
            self.0.load(Ordering::Relaxed),
            "source must be released before streaming"
        );
        Err(io::Error::from(io::ErrorKind::PermissionDenied))
    }
}
impl<'a> ArchiveTransaction<TrieId32> for Transaction<'a> {
    type Error = io::Error;
    type Blob = FailedBlob<'a>;
    fn insert_placeholder(&mut self, _: &TrieId32) -> io::Result<u32> {
        Ok(1)
    }
    fn write_shared_blob_with<E>(
        &mut self,
        write: impl FnOnce(&Self::Blob, u64) -> Result<u64, E>,
    ) -> Result<(u64, u64), ArchiveWriteError<io::Error, E>> {
        write(&FailedBlob(&self.0.source_dropped), 0)
            .map(|n| (0, n))
            .map_err(ArchiveWriteError::Callback)
    }
    fn write_metadata(&mut self, _: &PruneInfo, _: &[(u32, TrieId32, TrieHash)]) -> io::Result<()> {
        self.0.metadata.store(true, Ordering::Relaxed);
        Ok(())
    }
    fn commit_archive(self) -> io::Result<()> {
        self.0.metadata.store(true, Ordering::Relaxed);
        Ok(())
    }
}
impl ArchiveDestination<TrieId32> for Destination {
    type Error = io::Error;
    type Transaction<'a> = Transaction<'a>;
    fn begin_archive(&mut self) -> io::Result<Self::Transaction<'_>> {
        Ok(Transaction(self))
    }
    fn finalize(&mut self) -> io::Result<()> {
        Ok(())
    }
}
#[test]
fn streaming_failure_preserves_cause_and_removes_stage() {
    let directory = tempfile::tempdir().unwrap();
    let mut source = MARF::<TrieId32>::from_path(":memory:", MARFOpenOpts::default()).unwrap();
    let tip = TrieId32([7; 32]);
    let mut tx = source.begin_tx().unwrap();
    tx.begin(&TrieId32::sentinel(), &tip).unwrap();
    tx.insert("key", MARFValue([9; 40])).unwrap();
    tx.commit().unwrap();
    let dropped = Arc::new(AtomicBool::new(false));
    let source = ReleasingSource {
        engine: source,
        dropped: dropped.clone(),
    };
    let mut destination = Destination {
        metadata: AtomicBool::new(false),
        source_dropped: dropped,
    };
    let result = prune(
        source,
        &mut destination,
        &PruneOptions {
            staging_dir: directory.path().to_owned(),
            label: "failure".into(),
        },
        &tip,
        0,
    );
    std::assert_matches!(result,Err(marf_pruning::Error::Destination(ref e)) if e.kind()==io::ErrorKind::PermissionDenied);
    assert!(!destination.metadata.load(Ordering::Relaxed));
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
}

/// An owned source whose destruction is observable at the output boundary.
struct ReleasingSource {
    engine: MARF<TrieId32>,
    dropped: Arc<AtomicBool>,
}
impl Drop for ReleasingSource {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::Relaxed);
    }
}
impl ArchiveSource<TrieId32> for ReleasingSource {
    type Error = marf_core::Error;
    fn prune_height(&self) -> Option<u32> {
        self.engine.prune_height()
    }
    fn block_at_height(&mut self, h: u32, t: &TrieId32) -> Result<Option<TrieId32>, Self::Error> {
        self.engine.block_at_height(h, t)
    }
    fn block_id(&mut self, b: &TrieId32) -> Result<u32, Self::Error> {
        self.engine.block_id(b)
    }
    fn block_metadata(&mut self, b: u32) -> Result<(TrieHash, TrieId32), Self::Error> {
        self.engine.block_metadata(b)
    }
    fn pruned_blocks(&mut self) -> Result<Vec<(u32, TrieId32, TrieHash)>, Self::Error> {
        self.engine.pruned_blocks()
    }
    fn read_node(
        &mut self,
        b: u32,
        p: &marf_core::node::Ptr,
    ) -> Result<marf_core::node::Node, Self::Error> {
        self.engine.read_node(b, p)
    }
    fn confirmed_blocks(&mut self) -> Result<Vec<(u32, TrieId32)>, Self::Error> {
        self.engine.confirmed_blocks()
    }
}
