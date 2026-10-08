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

//! Destination headroom checks for unpublished all-or-nothing conversion.

use std::fs::File;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::Result;

/// Read allocatable bytes available to this process, excluding reserved filesystem blocks.
pub fn available(path: &Path) -> io::Result<u64> {
    let directory = File::open(path)?;
    let mut status = MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: the open directory remains live and status points to writable struct storage.
    if unsafe { libc::fstatvfs(directory.as_raw_fd(), status.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful fstatvfs initialized the complete result.
    let status = unsafe { status.assume_init() };
    (status.f_bavail as u64)
        .checked_mul(status.f_frsize as u64)
        .ok_or_else(|| io::Error::other("filesystem capacity overflow"))
}

/// Periodically stop private work before consuming its publication/scratch reserve.
///
/// This is a best-effort check, not a reservation against other processes. ENOSPC
/// remains an error and leaves the original source and unpublished output intact.
pub struct Guard<'a> {
    /// Existing directory on the destination filesystem.
    path: &'a Path,
    /// Headroom for one worst-case trie rewrite and partition allocation.
    reserve: u64,
    /// Last successful check, limiting filesystem calls on hot conversion loops.
    checked: Instant,
}

impl<'a> Guard<'a> {
    /// Verify initial capacity before creating an unpublished destination.
    pub fn new(path: &'a Path, reserve: u64) -> Result<Self> {
        Self::require(available(path)?, reserve)?;
        Ok(Self {
            path,
            reserve,
            checked: Instant::now(),
        })
    }

    /// Refresh the headroom check at most once per second.
    pub fn check(&mut self) -> Result<()> {
        if self.checked.elapsed() >= Duration::from_secs(1) {
            Self::require(available(self.path)?, self.reserve)?;
            self.checked = Instant::now();
        }
        Ok(())
    }

    /// Keep the capacity comparison independently testable without filling a disk.
    fn require(free: u64, reserve: u64) -> Result<()> {
        if free < reserve {
            return Err(format!(
                "insufficient migration headroom: available={free} required_reserve={reserve}"
            )
            .into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exact capacity is accepted; a smaller capacity fails without filesystem mutation.
    #[test]
    fn headroom_boundary_and_filesystem_query() {
        assert!(Guard::require(9, 10).is_err());
        Guard::require(10, 10).unwrap();
        let directory = tempfile::tempdir().unwrap();
        assert!(available(directory.path()).unwrap() > 0);
        assert!(Guard::new(directory.path(), u64::MAX).is_err());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }
}
