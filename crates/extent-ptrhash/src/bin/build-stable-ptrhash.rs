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

//! Build a portable immutable commitment-to-stable-ID base on an offline clone.

use std::path::Path;

/// Build and activate the stable-ID index next to its owning Clarity MARF.
fn main() -> extent_ptrhash::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 3 {
        return Err("usage: build-stable-ptrhash CLONE_DB NEW_SIBLING_INDEX_DIRECTORY".into());
    }
    extent_ptrhash::stable::build_and_activate(Path::new(&args[1]), Path::new(&args[2]))
}
