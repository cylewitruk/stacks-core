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

//! Stacks path policy and compatibility pruning convenience.
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use stacks_common::types::chainstate::TrieHash;

use crate::archive::Archive;
pub use crate::archive::PruneInfo;
use crate::marf::{MARF, MARFOpenOpts};
use crate::{Error, Id, MarfTrieId, stacks_hash};
/// Pruning as a capability of a MARF type.
pub trait Prune<T: MarfTrieId>: Sized {
    /// Prune the MARF at `src_path` at `height` (on the fork of `tip`) into a
    /// new database at `dst_path`.
    fn prune_to_path(
        src_path: impl AsRef<Path>,
        dst_path: impl AsRef<Path>,
        src_open_opts: MARFOpenOpts,
        tip: &T,
        height: u32,
        label: &str,
    ) -> Result<PruneStats, Error>;
}

/// Summary statistics from a pruning run.
#[derive(Debug, Clone)]
pub struct PruneStats {
    pub node_count: u64,
    pub prune_height: u32,
    pub dst_db_path: PathBuf,
    pub dst_blobs_path: PathBuf,
    pub blob_size: u64,
    pub historical_placeholder_count: u64,
    pub source_root_hash: TrieHash,
    pub prune_root_node_hash: TrieHash,
    pub total_duration: Duration,
}

struct StacksProgress;
impl marf_pruning::Progress for StacksProgress {
    fn indexed(&mut self, label: &str, heights: usize) {
        info!("[{label}] squash: indexed {heights} heights");
    }
    fn completed(&mut self, label: &str, nodes: u64, bytes: u64) {
        info!("[{label}] squash: complete, {nodes} nodes, {bytes} bytes");
    }
}
impl<T: MarfTrieId> Prune<T> for MARF<T> {
    fn prune_to_path(
        src_path: impl AsRef<Path>,
        dst_path: impl AsRef<Path>,
        opts: MARFOpenOpts,
        tip: &T,
        height: u32,
        label: &str,
    ) -> Result<PruneStats, Error> {
        let start = Instant::now();
        let dst_db_path = dst_path.as_ref().to_owned();
        let dst_blobs_path = Self::blobs_path(&dst_db_path);
        for path in [&dst_db_path, &dst_blobs_path] {
            if path.exists() {
                return Err(Error::DestinationExists(
                    path.to_string_lossy().into_owned(),
                ));
            }
        }
        let src = Self::open_archive_source(src_path.as_ref(), opts)?;
        let mut dst = Self::create_archive_destination(&dst_db_path)?;
        let stage = dst_db_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let options = marf_pruning::PruneOptions {
            staging_dir: stage.to_owned(),
            label: label.to_owned(),
        };
        let s = marf_pruning::prune_with_progress(
            src,
            &mut dst,
            &options,
            &Id(tip.clone()),
            height,
            &mut StacksProgress,
        )
        .map_err(|e| match e {
            marf_pruning::Error::Source(e)
            | marf_pruning::Error::Destination(e)
            | marf_pruning::Error::Format(e) => e,
            marf_pruning::Error::Io(e) => Error::IOError(e),
            marf_pruning::Error::NotFoundError => Error::NotFoundError,
            marf_pruning::Error::CorruptionError(e) => Error::CorruptionError(e),
            marf_pruning::Error::OverflowError => Error::OverflowError,
        })?;
        Ok(PruneStats {
            node_count: s.node_count,
            prune_height: s.prune_height,
            dst_db_path,
            dst_blobs_path,
            blob_size: s.blob_size,
            historical_placeholder_count: s.historical_placeholder_count,
            source_root_hash: stacks_hash(s.source_root_hash),
            prune_root_node_hash: stacks_hash(s.prune_root_node_hash),
            total_duration: start.elapsed(),
        })
    }
}
