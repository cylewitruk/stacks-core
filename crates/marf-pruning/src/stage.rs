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

//! Buffered temporary node store, removed when dropped.
use std::fs;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use marf_core::{Error, corrupt};

use super::STREAM_BUFFER;
use crate::codec;
use crate::codec::{Decoded, MAX_NODE_LEN};
use crate::node::Node;
use crate::positioned_io::{read_exact_at, write_all_at};

/// Nodes collected for pruning, staged in a temporary file. Each node is
/// written once, in its plain record encoding (`codec::encode_node`, no
/// hash) behind a 4-byte length, and read back by index; indices are handed
/// out in discovery order, so a node's index is always greater than its
/// parent's, and child pointers hold indices. The file is removed on drop.
pub struct Staged {
    path: PathBuf,
    file: File,
    /// Appended records not yet written to the file.
    pending: Vec<u8>,
    /// Bytes written to the file so far.
    flushed: u64,
    /// Start of each node's record in the file; `u64::MAX` until written.
    offsets: Vec<u64>,
    encode: Vec<u8>,
    read: Vec<u8>,
}

impl Staged {
    /// Create an exclusive temporary stage beside the destination.
    pub fn create(dir: &Path) -> Result<Staged, Error> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = dir.join(format!(".prune_nodes_{}_{nanos}.tmp", std::process::id()));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        Ok(Staged {
            path,
            file,
            pending: Vec::with_capacity(STREAM_BUFFER + MAX_NODE_LEN + 4),
            flushed: 0,
            offsets: vec![],
            encode: Vec::with_capacity(MAX_NODE_LEN),
            read: Vec::with_capacity(MAX_NODE_LEN),
        })
    }

    /// Hand out the next index (the node is written later).
    pub fn reserve(&mut self) -> Result<u32, Error> {
        let idx = u32::try_from(self.offsets.len()).map_err(|_| Error::OverflowError)?;
        self.offsets.push(u64::MAX);
        Ok(idx)
    }

    /// Number of reserved node indices.
    pub fn len(&self) -> u32 {
        self.offsets.len() as u32
    }

    /// Encode a node into its reserved stage slot.
    pub fn write(&mut self, idx: u32, node: &Node) -> Result<(), Error> {
        self.encode.clear();
        codec::encode_node(node, false, &mut self.encode);
        let len = u32::try_from(self.encode.len()).map_err(|_| Error::OverflowError)?;
        let slot = self
            .offsets
            .get_mut(idx as usize)
            .ok_or_else(|| corrupt!("stage: index {idx} was never reserved"))?;
        if *slot != u64::MAX {
            return Err(corrupt!("stage: node {idx} written twice"));
        }
        *slot = self.flushed + self.pending.len() as u64;
        self.pending.extend_from_slice(&len.to_le_bytes());
        self.pending.extend_from_slice(&self.encode);
        if self.pending.len() >= STREAM_BUFFER {
            self.flush()?;
        }
        Ok(())
    }

    /// Write pending records so they can be read by index.
    pub fn flush(&mut self) -> Result<(), Error> {
        if !self.pending.is_empty() {
            write_all_at(&self.file, &self.pending, self.flushed)?;
            self.flushed += self.pending.len() as u64;
            self.pending.clear();
        }
        Ok(())
    }

    /// Read node `idx` back; call [`Staged::flush`] once collection is done.
    pub fn read(&mut self, idx: u32) -> Result<Node, Error> {
        let offset = *self
            .offsets
            .get(idx as usize)
            .ok_or_else(|| corrupt!("stage: index {idx} out of range ({})", self.offsets.len()))?;
        if offset == u64::MAX {
            return Err(corrupt!("stage: node {idx} was never written"));
        }
        let mut len = [0u8; 4];
        read_exact_at(&self.file, &mut len, offset)?;
        let len = u32::from_le_bytes(len) as usize;
        if len == 0 || len > MAX_NODE_LEN {
            return Err(corrupt!("stage: node {idx} has record length {len}"));
        }
        self.read.resize(len, 0);
        read_exact_at(&self.file, &mut self.read, offset + 4)?;
        match codec::decode(&self.read, self.read[0])? {
            (Decoded::Node(node), _) => Ok(node),
            (Decoded::Patch(_), _) => Err(corrupt!("stage: node {idx} decoded as a patch")),
        }
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
