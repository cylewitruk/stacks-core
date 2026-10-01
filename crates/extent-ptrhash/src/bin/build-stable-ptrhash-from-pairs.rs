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

//! Build a target-native stable PtrHash base from retained sorted commitment/ID pairs.

use std::env;
use std::fs;
use std::path::Path;

use serde_json::Value;

/// Parse the source base's portable shard counts before rebuilding its native functions.
fn shard_counts(path: &Path) -> extent_ptrhash::Result<[u64; 256]> {
    let manifest: Value = serde_json::from_slice(&fs::read(path)?)?;
    let shards = manifest["shards"]
        .as_array()
        .ok_or("missing stable PtrHash shards")?;
    if shards.len() != 256 {
        return Err("stable PtrHash manifest must contain 256 shards".into());
    }
    let mut counts = [0u64; 256];
    for (count, shard) in counts.iter_mut().zip(shards) {
        *count = shard["count"]
            .as_u64()
            .ok_or("invalid stable shard count")?;
    }
    let expected = manifest["count"]
        .as_u64()
        .ok_or("missing stable key count")?;
    if counts
        .iter()
        .try_fold(0u64, |sum, count| sum.checked_add(*count))
        != Some(expected)
    {
        return Err("stable shard counts do not match manifest total".into());
    }
    Ok(counts)
}

/// Build beside an offline disposable Clarity database after its imported base is detached.
fn main() -> extent_ptrhash::Result<()> {
    let args: Vec<_> = env::args().collect();
    if args.len() != 5 {
        return Err("usage: build-stable-ptrhash-from-pairs CLONE_DB NEW_SIBLING_INDEX_DIRECTORY SORTED_PAIRS SOURCE_MANIFEST".into());
    }
    let counts = shard_counts(Path::new(&args[4]))?;
    extent_ptrhash::stable::build_and_activate_from_pairs(
        Path::new(&args[1]),
        Path::new(&args[2]),
        Path::new(&args[3]),
        &counts,
    )
}
