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

//! Archive construction through historical-read and destination capabilities.
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::time::Instant;

use marf_core::{TrieHash, TrieId};

use super::stage::Staged;
use super::{Progress, PruneOptions, PruneStats, STREAM_BUFFER, child_location, inline_child};
use crate::archive::{
    ArchiveBlob, ArchiveDestination, ArchiveSource, ArchiveTransaction, PruneInfo,
};
use crate::codec::{HASH_LEN, MAX_NODE_LEN};
use crate::hash::{ChildHash, HashScratch, branch_hash, leaf_hash};
use crate::node::{BACKPTR, Node, Ptr, id};
use crate::trie::{HEADER_LEN, ROOT_OFFSET};
use crate::{Error, codec};

/// Construct one archive through independent source/destination capabilities.
/// Destination paths/conflict policy belong to the integrator. A callback
/// failure leaves appended bytes unreachable and rolls back publication.
pub fn prune_with_progress<T, S, D, P>(
    mut src: S,
    dst: &mut D,
    options: &PruneOptions,
    tip: &T,
    squash_height: u32,
    progress: &mut P,
) -> Result<PruneStats, Error<S::Error, D::Error>>
where
    T: TrieId,
    S: ArchiveSource<T>,
    D: ArchiveDestination<T>,
    P: Progress,
{
    let start = Instant::now();
    let label = &options.label;
    let src_squash_height = src.prune_height();
    if let Some(sh) = src_squash_height
        && squash_height <= sh
    {
        return Err(prune_corrupt!(
            "Cannot re-squash at squash_height {squash_height}: source is already squashed at squash_height {sh}; the new squash_height must be strictly greater"
        ));
    }
    let block_at_height = src
        .block_at_height(squash_height, tip)
        .map_err(Error::Source)?
        .ok_or(Error::NotFoundError)?;

    // [1] per-height (block, archival root hash), walking parents from the squash tip.
    let blocks = {
        let floor = src_squash_height.map(|h| h + 1).unwrap_or(0);
        let mut out: Vec<(u32, T, TrieHash)> = Vec::with_capacity(squash_height as usize + 1);
        let mut cur = block_at_height.clone();
        for h in (floor..=squash_height).rev() {
            if cur == T::sentinel() {
                return Err(prune_corrupt!("Block walk hit sentinel at height {h}"));
            }
            let id = src.block_id(&cur).map_err(Error::Source)?;
            let (root, parent) = src.block_metadata(id).map_err(Error::Source)?;
            out.push((h, cur.clone(), root));
            if h == 0 && parent != T::sentinel() {
                return Err(prune_corrupt!(
                    "Block at height 0 ({cur}) has non-sentinel parent {parent}"
                ));
            }
            cur = parent;
        }
        if floor > 0 {
            let side = src.pruned_blocks().map_err(Error::Source)?;
            let boundary = side
                .iter()
                .find(|(h, _, _)| *h == floor - 1)
                .map(|(_, b, _)| b.clone())
                .ok_or_else(|| {
                    prune_corrupt!(
                        "Source squash side table missing row for height {}",
                        floor - 1
                    )
                })?;
            if cur != boundary {
                return Err(prune_corrupt!(
                    "Re-squash boundary mismatch at height {floor}"
                ));
            }
            for (h, b, r) in side {
                if h >= floor {
                    return Err(prune_corrupt!(
                        "Source squash side table has row at height {h} >= {floor}"
                    ));
                }
                out.push((h, b, r));
            }
        }
        out.sort_by_key(|(h, _, _)| *h);
        if out.len() != squash_height as usize + 1 {
            return Err(prune_corrupt!(
                "Build height index: expected {} entries, got {}",
                squash_height + 1,
                out.len()
            ));
        }
        out
    };

    progress.indexed(label, blocks.len());

    // [2] collect every node reachable at the squash tip into the stage,
    // pre-order. A child's index is assigned when its parent is visited,
    // so parents are written with their child pointers already rewritten
    // to inline indices annotated with the child's archival block, and no
    // remap pass is needed.
    let stage_dir = options.staging_dir.as_path();
    let mut stage = Staged::create(stage_dir)?;
    {
        let tip_id = src.block_id(&block_at_height).map_err(Error::Source)?;
        let mut index: HashMap<(u32, u64), u32> = HashMap::new();
        let root_idx = stage.reserve()?;
        index.insert((tip_id, ROOT_OFFSET), root_idx);
        let mut stack: Vec<(u32, u64, u8, u32)> =
            vec![(tip_id, ROOT_OFFSET, id::NODE256, root_idx)];
        let mut discovered: Vec<(u32, u64, u8, u32)> = vec![];
        while let Some((block, offset, kind, idx)) = stack.pop() {
            let mut node = src
                .read_node(block, &Ptr::new(kind, 0, offset))
                .map_err(Error::Source)?;
            if let Node::Branch(b) = &mut node {
                discovered.clear();
                for child in b.children.iter_mut() {
                    let loc = child_location(child, block);
                    let child_idx = match index.entry(loc) {
                        Entry::Occupied(e) => *e.get(),
                        Entry::Vacant(e) => {
                            let i = stage.reserve()?;
                            e.insert(i);
                            discovered.push((loc.0, loc.1, child.kind_id(), i));
                            i
                        }
                    };
                    child.ptr = u64::from(child_idx);
                    child.id &= !BACKPTR;
                    child.back_block = loc.0;
                }
                // first child on top: pre-order, slot order
                stack.extend(discovered.iter().rev().copied());
            }
            stage.write(idx, &node)?;
        }
    }
    stage.flush()?;
    let n = stage.len();
    let node_count = u64::from(n);

    // [3] hash, children before parents (reverse index order).
    let mut hashes: Vec<TrieHash> = vec![TrieHash::ZERO; n as usize];
    let mut scratch = HashScratch::default();
    for idx in (0..n).rev() {
        let node = stage.read(idx)?;
        let hash = match &node {
            Node::Leaf(leaf) => leaf_hash(leaf),
            Node::Branch(b) => branch_hash::<Error<S::Error, D::Error>>(
                b,
                |p| Ok(ChildHash::inline(hashes[inline_child(p, idx, n)? as usize])),
                &mut scratch,
            )?,
        };
        hashes[idx as usize] = hash;
    }
    let prune_root_node_hash = hashes[0];

    // [4] destination: placeholder rows in height order, then the tip.

    let (historical, prune_root_node_hash, blob_len, source_root_hash) = {
        let mut tx = dst.begin_archive().map_err(Error::Destination)?;
        let src_ids: HashMap<T, u32> = src
            .confirmed_blocks()
            .map_err(Error::Source)?
            .into_iter()
            .map(|(id, b)| (b, id))
            .collect();
        let mut archival_to_squashed: HashMap<u32, u32> = HashMap::new();
        let mut historical = 0u64;
        {
            for (_, bhh, _) in blocks.iter() {
                if *bhh == block_at_height {
                    continue;
                }
                let archival = *src_ids.get(bhh).ok_or(Error::NotFoundError)?;
                let squashed = tx.insert_placeholder(bhh).map_err(Error::Destination)?;
                archival_to_squashed.insert(archival, squashed);
                historical += 1;
            }
            let archival_tip = *src_ids.get(&block_at_height).ok_or(Error::NotFoundError)?;
            let squashed_tip = tx
                .insert_placeholder(&block_at_height)
                .map_err(Error::Destination)?;
            archival_to_squashed.insert(archival_tip, squashed_tip);
        }

        drop(src);

        // [5] stream the shared blob: nodes children-first, so every
        // pointer's target offset is known when the pointer is written,
        // then the root into a slot reserved for its widest encoding (the
        // original's `stream_squash_blob` layout). Annotations become
        // destination block ids on the way out.
        let squashed = |archival: u32| -> Result<u32, Error<S::Error, D::Error>> {
            archival_to_squashed
                .get(&archival)
                .copied()
                .ok_or_else(|| prune_corrupt!("block_id {archival} not in block_id_map"))
        };
        let remap =
            |node: &mut Node, idx: u32, offsets: &[u64]| -> Result<(), Error<S::Error, D::Error>> {
                if let Node::Branch(b) = node {
                    for child in b.children.iter_mut() {
                        let c = inline_child(child, idx, n)?;
                        child.ptr = offsets[c as usize];
                        child.back_block = squashed(child.back_block)?;
                    }
                }
                Ok(())
            };
        let (_blob_offset, blob_len) = {
            tx.write_shared_blob_with(|blobs, base| {
                let mut root = stage.read(0)?;
                let widest = |p: &Ptr| -> Ptr {
                    if p.is_back() {
                        *p
                    } else {
                        Ptr {
                            ptr: u64::MAX,
                            ..*p
                        }
                    }
                };
                let reserved = codec::node_len_mapped(&root, false, &widest) as u64;
                let mut offsets: Vec<u64> = vec![0; n as usize];
                let mut out: Vec<u8> = Vec::with_capacity(STREAM_BUFFER + MAX_NODE_LEN + HASH_LEN);
                let mut flushed = ROOT_OFFSET + HASH_LEN as u64 + reserved;
                let mut pos = flushed;
                for idx in (1..n).rev() {
                    let mut node = stage.read(idx)?;
                    remap(&mut node, idx, &offsets)?;
                    offsets[idx as usize] = pos;
                    out.extend_from_slice(hashes[idx as usize].as_bytes());
                    codec::encode_node(&node, false, &mut out);
                    pos = flushed + out.len() as u64;
                    if out.len() >= STREAM_BUFFER {
                        blobs
                            .write_at_unsynced(base + flushed, &out)
                            .map_err(Error::Destination)?;
                        flushed = pos;
                        out.clear();
                    }
                }
                if !out.is_empty() {
                    blobs
                        .write_at_unsynced(base + flushed, &out)
                        .map_err(Error::Destination)?;
                    out.clear();
                }
                // header and root
                remap(&mut root, 0, &offsets)?;
                out.extend_from_slice(T::sentinel().as_bytes());
                out.extend_from_slice(&0u32.to_le_bytes());
                debug_assert_eq!(out.len(), HEADER_LEN);
                out.extend_from_slice(hashes[0].as_bytes());
                codec::encode_node(&root, false, &mut out);
                if out.len() as u64 > ROOT_OFFSET + HASH_LEN as u64 + reserved {
                    return Err(prune_corrupt!(
                        "squash root needs {} bytes, {reserved} reserved",
                        out.len() as u64 - ROOT_OFFSET - HASH_LEN as u64
                    ));
                }
                blobs
                    .write_at_unsynced(base, &out)
                    .map_err(Error::Destination)?;
                Ok(pos)
            })
            .map_err(|e| match e {
                crate::archive::ArchiveWriteError::Destination(e) => Error::Destination(e),
                crate::archive::ArchiveWriteError::Callback(e) => e,
            })?
        };
        drop(stage);

        // [6] the side tables.
        let source_root_hash = blocks
            .iter()
            .find(|(_, b, _)| *b == block_at_height)
            .map(|(_, _, r)| *r)
            .ok_or(Error::NotFoundError)?;
        {
            tx.write_metadata(
                &PruneInfo {
                    archival_marf_root_hash: source_root_hash,
                    prune_root_node_hash,
                    prune_height: squash_height,
                },
                &blocks,
            )
            .map_err(Error::Destination)?;
        }
        tx.commit_archive().map_err(Error::Destination)?;
        (historical, prune_root_node_hash, blob_len, source_root_hash)
    };

    dst.finalize().map_err(Error::Destination)?;

    progress.completed(label, node_count, blob_len);

    Ok(PruneStats {
        node_count,
        prune_height: squash_height,
        blob_size: blob_len,
        historical_placeholder_count: historical,
        source_root_hash,
        prune_root_node_hash,
        total_duration: start.elapsed(),
    })
}
