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

//! Runtime profiler configuration for stackslib instrumentation.
//!
//! This module is feature-gated behind `profiler` and provides low-overhead
//! toggles for profiler capture modes.

use std::sync::atomic::{AtomicU64, Ordering};

/// Bitflags that control profiler capture behavior.
pub mod flags {
    /// Capture MARF key strings in `get_by_key`.
    pub const CAPTURE_MARF_KEYS: u64 = 1 << 0;
    /// Capture MARF block-hash lookups in `get_by_hash`.
    pub const CAPTURE_MARF_HASH_LOOKUPS: u64 = 1 << 1;
    /// Count MARF cache hits/misses.
    pub const CAPTURE_MARF_CACHE_COUNTS: u64 = 1 << 2;
    /// Capture MARF disk-read identifiers (block id).
    pub const CAPTURE_MARF_DISK_READS: u64 = 1 << 3;
}

const DEFAULT_FLAGS: u64 = flags::CAPTURE_MARF_KEYS
    | flags::CAPTURE_MARF_HASH_LOOKUPS
    | flags::CAPTURE_MARF_CACHE_COUNTS
    | flags::CAPTURE_MARF_DISK_READS;

static PROFILER_FLAGS: AtomicU64 = AtomicU64::new(DEFAULT_FLAGS);

/// Replace the current profiler flags and return the previous value.
#[inline(always)]
pub fn set_flags(flags: u64) -> u64 {
    PROFILER_FLAGS.swap(flags, Ordering::Relaxed)
}

/// Enable the given flags.
#[inline(always)]
pub fn enable(flags: u64) {
    PROFILER_FLAGS.fetch_or(flags, Ordering::Relaxed);
}

/// Disable the given flags.
#[inline(always)]
pub fn disable(flags: u64) {
    PROFILER_FLAGS.fetch_and(!flags, Ordering::Relaxed);
}

/// Returns true if all given flags are enabled.
#[inline(always)]
pub fn is_enabled(flags: u64) -> bool {
    (PROFILER_FLAGS.load(Ordering::Relaxed) & flags) == flags
}

#[inline(always)]
pub fn capture_marf_keys() -> bool {
    is_enabled(flags::CAPTURE_MARF_KEYS)
}

#[inline(always)]
pub fn capture_marf_hash_lookups() -> bool {
    is_enabled(flags::CAPTURE_MARF_HASH_LOOKUPS)
}

#[inline(always)]
pub fn capture_marf_cache_counts() -> bool {
    is_enabled(flags::CAPTURE_MARF_CACHE_COUNTS)
}

#[inline(always)]
pub fn capture_marf_disk_reads() -> bool {
    is_enabled(flags::CAPTURE_MARF_DISK_READS)
}
