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

//! Best-effort access-pattern advice, without changing mapping ownership or durability.

#[cfg(any(target_os = "linux", test))]
use std::io::{self, Write};
#[cfg(any(target_os = "linux", test))]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(target_os = "linux")]
use memmap2::Advice;
use memmap2::Mmap;

/// Expected page access order for a file mapping.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AccessPattern {
    /// Retain the operating system's default read-ahead behavior.
    #[default]
    Normal,
    /// Avoid speculative neighboring reads on Linux; other platforms keep OS defaults.
    Random,
}

/// Number of successfully advised mappings or appended intervals.
static ADVISED: AtomicU64 = AtomicU64::new(0);
/// Number of failed advice requests; mapping access remains valid.
static FAILED: AtomicU64 = AtomicU64::new(0);
/// Bounds advice failure messages to one per process.
#[cfg(any(target_os = "linux", test))]
static WARNED: AtomicBool = AtomicBool::new(false);

/// Process-wide advice outcomes, useful when validating mapping coverage.
#[derive(Clone, Copy, Debug)]
pub struct AdviceStats {
    /// Successful random-access advice calls.
    pub advised: u64,
    /// Failed random-access advice calls.
    pub failed: u64,
}

/// Read cumulative advice outcomes without resetting other callers' counters.
pub fn advice_stats() -> AdviceStats {
    AdviceStats {
        advised: ADVISED.load(Ordering::Relaxed),
        failed: FAILED.load(Ordering::Relaxed),
    }
}

impl AccessPattern {
    /// Advise an existing mapping on Linux; other platforms keep OS defaults.
    pub fn apply(self, mapping: &Mmap) {
        #[cfg(target_os = "linux")]
        if self == Self::Random && !mapping.is_empty() {
            record(mapping.advise(Advice::Random));
        }
        #[cfg(not(target_os = "linux"))]
        let _ = mapping;
    }

    /// Advise a newly backed, page-aligned interval before publishing it to readers.
    ///
    /// # Safety
    /// `address` must identify a live file-backed mapping covering `length` bytes
    /// and be aligned to the host page size. The mapping must remain live for this call.
    pub unsafe fn apply_raw(self, address: *mut libc::c_void, length: usize) {
        #[cfg(target_os = "linux")]
        if self == Self::Random && length != 0 {
            // SAFETY: the caller owns the aligned, live mapped interval.
            let result = unsafe { libc::madvise(address, length, libc::MADV_RANDOM) };
            record(if result == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            });
        }
        #[cfg(not(target_os = "linux"))]
        let _ = (address, length);
    }
}

/// Record a best-effort hint without making a usable mapping fail to open.
#[cfg(any(target_os = "linux", test))]
fn record(result: io::Result<()>) {
    match result {
        Ok(()) => {
            ADVISED.fetch_add(1, Ordering::Relaxed);
        }
        Err(error) => {
            FAILED.fetch_add(1, Ordering::Relaxed);
            if !WARNED.swap(true, Ordering::Relaxed) {
                let _ = writeln!(
                    io::stderr().lock(),
                    "Random mmap advice failed; continuing with OS defaults: {error}"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Advice does not change bytes or prevent access to the final partial page.
    #[test]
    fn advice_preserves_mapped_bytes() {
        let mut file = tempfile::tempfile().unwrap();
        let bytes = vec![73; 32769];
        file.write_all(&bytes).unwrap();
        // SAFETY: this test does not modify or truncate the mapped file.
        let mapping = unsafe { Mmap::map(&file).unwrap() };
        AccessPattern::Normal.apply(&mapping);
        AccessPattern::Random.apply(&mapping);
        assert_eq!(&mapping[..], bytes);
        #[cfg(target_os = "linux")]
        assert!(advice_stats().advised > 0);
    }

    /// Both advice entry points leave non-Linux mappings and counters unchanged.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn non_linux_retains_default_advice() {
        let mut file = tempfile::tempfile().unwrap();
        let bytes = vec![91; 32769];
        file.write_all(&bytes).unwrap();
        // SAFETY: the mapped file is neither modified nor truncated during the test.
        let mapping = unsafe { Mmap::map(&file).unwrap() };
        AccessPattern::Random.apply(&mapping);
        // SAFETY: mmap provides a page-aligned, live file-backed interval.
        unsafe {
            AccessPattern::Random.apply_raw(mapping.as_ptr().cast_mut().cast(), mapping.len());
        }
        assert_eq!(advice_stats().advised, 0);
        assert_eq!(&mapping[..], bytes);
    }

    /// A failed performance hint remains observable without returning a storage failure.
    #[test]
    fn failed_advice_is_counted() {
        let before = advice_stats().failed;
        record(Err(io::Error::other("injected advice failure")));
        assert!(advice_stats().failed > before);
    }
}
