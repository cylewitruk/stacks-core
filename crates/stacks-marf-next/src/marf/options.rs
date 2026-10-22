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

//! MARF configuration and test overrides.
use super::{MARFOpenOpts, TrieHashCalculationMode};
use crate::storage;

impl MARFOpenOpts {
    #[allow(clippy::should_implement_trait)]
    pub fn default() -> MARFOpenOpts {
        MARFOpenOpts {
            hash_calculation_mode: TrieHashCalculationMode::Deferred,
            external_blobs: false,
            force_db_migrate: false,
            patch_cache_capacity: storage::PATCH_CACHE_CAPACITY,
            compress: false,
        }
    }

    pub fn new(
        hash_calculation_mode: TrieHashCalculationMode,
        external_blobs: bool,
    ) -> MARFOpenOpts {
        MARFOpenOpts {
            hash_calculation_mode,
            external_blobs,
            force_db_migrate: false,
            patch_cache_capacity: storage::PATCH_CACHE_CAPACITY,
            compress: false,
        }
    }

    pub fn with_compression(mut self, compression: bool) -> Self {
        self.compress = compression;
        self
    }

    /// Benchmark control: size of the resolved-patch cache (0 disables it).
    pub fn with_patch_cache_capacity(mut self, capacity: usize) -> Self {
        self.patch_cache_capacity = capacity;
        self
    }
}

#[cfg(any(test, feature = "testing"))]
thread_local! {
    static TEST_MARF_COMPRESSION_FLAG: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Test hook: force MARF compression on or off for MARFs opened on this
/// thread through [`test_override_marf_compression`].
#[cfg(any(test, feature = "testing"))]
pub fn fault_injection_marf_compression(enabled: bool) {
    TEST_MARF_COMPRESSION_FLAG.set(Some(enabled));
}

/// Apply test-specific overrides to the MARF compression configuration.
#[cfg(any(test, feature = "testing"))]
pub fn test_override_marf_compression(marf_opts: &mut MARFOpenOpts) {
    if let Some(enabled) = TEST_MARF_COMPRESSION_FLAG.get() {
        marf_opts.compress = enabled;
    }
}

#[cfg(not(any(test, feature = "testing")))]
pub fn test_override_marf_compression(_marf_opts: &mut MARFOpenOpts) {}
