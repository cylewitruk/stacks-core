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

//! Production options for the optimized reference benchmark variant.
//! Copied to src/marf_config.rs by build-variants.sh in reference worktrees.

use stacks_marf::MARFOpenOpts;
use stacks_marf::storage::TrieHashCalculationMode;

/// Configure the optimized reference's production caches and mapping.
pub fn marf_opts() -> MARFOpenOpts {
    let mut o = MARFOpenOpts::new(TrieHashCalculationMode::Deferred, true)
        .with_compression(super::benchmark_compression())
        .with_mmap(true);
    o.root_node_cache = false;
    o.resolved_patch_cache_capacity = 64;
    o.result_cache_capacity = 2048;
    o
}

/// Describe the optimized reference's effective storage options.
pub fn describe_marf() -> String {
    let o = marf_opts();
    format!(
        "optimized stacks-marf (29ef093d): external_blobs={} compress={} mmap={} root_node_cache={} resolved_patch_cache={} result_cache={}",
        o.external_blobs,
        o.compress,
        o.mmap,
        o.root_node_cache,
        o.resolved_patch_cache_capacity,
        o.result_cache_capacity
    )
}
