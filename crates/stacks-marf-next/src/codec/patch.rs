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

//! Delta application and selection.
use super::{Patch, PatchView};
use crate::node::{BACKPTR, Branch, Ptr, backptr_children};
use crate::{Error, corrupt};

impl PatchView<'_> {
    /// Apply this patch (see [`apply_patch`]).
    pub fn apply(&self, base: &mut Branch, patch_block: u32, cur_block: u32) -> Result<(), Error> {
        let mut fits = true;
        prepare_patch_base(base, self.base.back_block);
        self.for_each_diff(|d| fits &= base.insert(d))?;
        finish_patch(base, fits, patch_block, cur_block)
    }
}
fn prepare_patch_base(base: &mut Branch, base_block: u32) {
    backptr_children(&mut base.children, base_block);
}

fn finish_patch(
    base: &mut Branch,
    fits: bool,
    patch_block: u32,
    cur_block: u32,
) -> Result<(), Error> {
    if !fits {
        return Err(corrupt!("patch does not fit its base node"));
    }
    backptr_children(&mut base.children, patch_block);
    for c in base.children.iter_mut() {
        if c.is_back() && c.back_block == cur_block {
            c.id &= !BACKPTR;
            c.back_block = 0;
        }
    }
    Ok(())
}

/// Apply a patch with base pointer `patch_base` and diff pointers `diff`,
/// read from `patch_block`, onto `base` (the node as resolved so far),
/// producing the node as seen from `cur_block`.
///
/// Mirrors the original `apply_node*`: before the patch the base's inline
/// children become back-pointers into the patch's base block; after it the
/// patch's inline diffs become back-pointers into the patch's own block; and
/// pointers that ended up pointing into `cur_block` become inline again.
pub fn apply_patch(
    base: &mut Branch,
    patch_base: Ptr,
    diff: &[Ptr],
    patch_block: u32,
    cur_block: u32,
) -> Result<(), Error> {
    prepare_patch_base(base, patch_base.back_block);
    let mut fits = true;
    for d in diff {
        fits &= base.insert(*d);
    }
    finish_patch(base, fits, patch_block, cur_block)
}
/// Compute the patch that turns `old` (the node at `base`, as read from its
/// own block) into `new` (the same node as rewritten in the current trie),
/// or `None` if no patch applies (different type or path, or no change).
///
/// `new` must be fully back-pointer-ified relative to committed blocks; its
/// inline children refer to the current trie and always appear in the diff.
pub fn make_patch(base: Ptr, old_children: &[Ptr], new: &Branch) -> Option<Patch> {
    debug_assert!(base.is_back());
    let mut diff = Vec::new();
    for np in &new.children {
        match old_children.iter().find(|op| op.chr == np.chr) {
            None => diff.push(*np),
            Some(op) => {
                let same = if !op.is_back() && np.is_back() && np.back_block == base.back_block {
                    // `np` may be the back-pointer-ified copy of `op`.
                    *op == np.without_back()
                } else {
                    op == np && np.is_back()
                };
                if !same {
                    diff.push(*np);
                }
            }
        }
    }
    if diff.is_empty() {
        None
    } else {
        Some(Patch { base, diff })
    }
}
