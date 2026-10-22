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

//! Options shared by the rewrite and upstream benchmark variants.

use stacks_marf::MARFOpenOpts;
use stacks_marf::storage::TrieHashCalculationMode;

/// Configure the backend selected by this workspace.
pub fn marf_opts() -> MARFOpenOpts {
    MARFOpenOpts::new(TrieHashCalculationMode::Deferred, true)
        .with_compression(super::benchmark_compression())
}

/// Describe the selected backend's storage options.
pub fn describe_marf() -> String {
    let o = marf_opts();
    format!(
        "stacks-marf package of this workspace: external_blobs={} compress={} (rewrite: mmap always, patch cache 4096; upstream: no caches)",
        o.external_blobs, o.compress
    )
}
