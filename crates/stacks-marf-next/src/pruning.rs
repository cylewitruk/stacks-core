//! Pruning: collapse the canonical history up to height `H` into a single
//! trie that holds exactly the state visible at `H`.
//!
//! The on-disk vocabulary (`marf_squash_info`, `marf_squashed_blocks`) keeps
//! the original's "squash" names; everything in this crate says "prune".
//!
//! The pruned MARF keeps one `marf_data` row per historical block (so block
//! ids stay meaningful), all pointing at one shared trie blob. Every child
//! pointer in that blob is inline and annotated with the storage id of the
//! block the child originally came from; when the squashed MARF is extended,
//! copy-on-write turns those annotations back into back-pointers, so the
//! root hashes of new blocks match what the archival MARF would produce.
//! Per-height archival root hashes and block ids live in `marf_squashed_blocks`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use stacks_common::types::chainstate::TrieHash;

use crate::marf::{MARFOpenOpts, MarfConnection, TrieHashCalculationMode, MARF};
use crate::node::{Node, Ptr, BACKPTR};
use crate::trie::{Trie, ROOT_OFFSET};
use crate::{corrupt, Error, MarfTrieId};

/// Metadata of a pruned MARF, cached at open time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PruneInfo {
    /// Archival MARF root hash committed to the chain at the prune boundary.
    pub archival_marf_root_hash: TrieHash,
    /// Root node hash of the pruned trie.
    pub prune_root_node_hash: TrieHash,
    /// MARF height at the prune boundary.
    pub prune_height: u32,
}

/// Pruning as a capability of a MARF type.
pub trait Prune<T: MarfTrieId>: Sized {
    /// Prune the MARF at `src_path` at `height` (on the fork of `tip`) into a
    /// new database at `dst_path`.
    fn prune_to_path(
        src_path: impl AsRef<Path>,
        dst_path: impl AsRef<Path>,
        src_open_opts: MARFOpenOpts,
        tip: &T,
        height: u32,
        label: &str,
    ) -> Result<PruneStats, Error>;
}

/// Summary statistics from a pruning run.
#[derive(Debug, Clone)]
pub struct PruneStats {
    pub node_count: u64,
    pub prune_height: u32,
    pub dst_db_path: PathBuf,
    pub dst_blobs_path: PathBuf,
    pub blob_size: u64,
    pub historical_placeholder_count: u64,
    pub source_root_hash: TrieHash,
    pub prune_root_node_hash: TrieHash,
    pub total_duration: Duration,
}

/// Resolve a child pointer to the `(block_id, offset)` locating it in the
/// source: back-pointers and squash annotations name the block directly,
/// plain inline pointers inherit the node's block.
fn child_location(ptr: &Ptr, origin_block: u32) -> (u32, u64) {
    if ptr.is_back() || ptr.back_block != 0 {
        (ptr.back_block, ptr.ptr)
    } else {
        (origin_block, ptr.ptr)
    }
}

impl<T: MarfTrieId> Prune<T> for MARF<T> {
    fn prune_to_path(
        src_path: impl AsRef<Path>,
        dst_path: impl AsRef<Path>,
        src_open_opts: MARFOpenOpts,
        tip: &T,
        squash_height: u32,
        label: &str,
    ) -> Result<PruneStats, Error> {
        let start = Instant::now();
        let src_path = src_path.as_ref();
        let dst_db_path = dst_path.as_ref().to_path_buf();
        let dst_blobs_path = crate::storage::Blobs::path_for(&dst_db_path);
        if dst_db_path.exists() {
            return Err(Error::DestinationExists(
                dst_db_path.to_string_lossy().into_owned(),
            ));
        }
        if dst_blobs_path.exists() {
            return Err(Error::DestinationExists(
                dst_blobs_path.to_string_lossy().into_owned(),
            ));
        }

        let mut src = MARF::<T>::from_storage(crate::marf::TrieFileStorage::open_readonly(
            src_path,
            src_open_opts,
        )?);
        let src_squash_height = src.inner.store.prune_height();
        if let Some(sh) = src_squash_height {
            if squash_height <= sh {
                return Err(corrupt!(
                    "Cannot re-squash at squash_height {squash_height}: source is already squashed at squash_height {sh}; the new squash_height must be strictly greater"
                ));
            }
        }
        let block_at_height = src
            .get_block_at_height(squash_height, tip)?
            .ok_or(Error::NotFoundError)?;

        // [1] per-height (block, archival root hash), walking parents from the squash tip.
        let blocks = src.with_conn(|v| {
            let floor = src_squash_height.map(|h| h + 1).unwrap_or(0);
            let mut out: Vec<(u32, T, TrieHash)> = Vec::with_capacity(squash_height as usize + 1);
            let mut cur = block_at_height.clone();
            for h in (floor..=squash_height).rev() {
                if cur == T::sentinel() {
                    return Err(corrupt!("Block walk hit sentinel at height {h}"));
                }
                let id = v.inner.store.block_id(v.db, &cur)?;
                let root = v.inner.store.read_root_hash(v.db, id)?;
                let parent = v.inner.store.read_parent(v.db, id)?;
                out.push((h, cur.clone(), root));
                if h == 0 && parent != T::sentinel() {
                    return Err(corrupt!(
                        "Block at height 0 ({cur}) has non-sentinel parent {parent}"
                    ));
                }
                cur = parent;
            }
            if floor > 0 {
                let side = v.inner.store.pruned_blocks(v.db)?;
                let boundary = side
                    .iter()
                    .find(|(h, _, _)| *h == floor - 1)
                    .map(|(_, b, _)| b.clone())
                    .ok_or_else(|| {
                        corrupt!(
                            "Source squash side table missing row for height {}",
                            floor - 1
                        )
                    })?;
                if cur != boundary {
                    return Err(corrupt!("Re-squash boundary mismatch at height {floor}"));
                }
                for (h, b, r) in side {
                    if h >= floor {
                        return Err(corrupt!(
                            "Source squash side table has row at height {h} >= {floor}"
                        ));
                    }
                    out.push((h, b, r));
                }
            }
            out.sort_by_key(|(h, _, _)| *h);
            if out.len() != squash_height as usize + 1 {
                return Err(corrupt!(
                    "Build height index: expected {} entries, got {}",
                    squash_height + 1,
                    out.len()
                ));
            }
            Ok(out)
        })?;
        info!("[{label}] squash: indexed {} heights", blocks.len());

        // [2] collect every node reachable at the squash tip, in pre-order.
        let (mut nodes, _origins) = src.with_conn(|v| {
            let tip_id = v.inner.store.block_id(v.db, &block_at_height)?;
            let mut nodes: Vec<Node> = vec![];
            let mut origins: Vec<u32> = vec![];
            let mut index: HashMap<(u32, u64), u32> = HashMap::new();
            let mut stack: Vec<(u32, u64, u8)> =
                vec![(tip_id, ROOT_OFFSET, crate::node::id::NODE256)];
            while let Some((block, offset, kind)) = stack.pop() {
                if index.contains_key(&(block, offset)) {
                    continue;
                }
                let (node, _, _) =
                    v.inner
                        .store
                        .read_node(v.db, block, &Ptr::new(kind, 0, offset))?;
                let idx = nodes.len() as u32;
                index.insert((block, offset), idx);
                if let Node::Branch(b) = &node {
                    for child in b.children.iter().rev() {
                        let (cb, co) = child_location(child, block);
                        stack.push((cb, co, child.kind_id()));
                    }
                }
                nodes.push(node);
                origins.push(block);
            }
            // remap child pointers to arena indices; keep the origin block as annotation
            for (i, node) in nodes.iter_mut().enumerate() {
                let origin = origins[i];
                if let Node::Branch(b) = node {
                    for child in b.children.iter_mut() {
                        let loc = child_location(child, origin);
                        let idx = *index
                            .get(&loc)
                            .ok_or_else(|| corrupt!("remap: child {loc:?} not collected"))?;
                        child.ptr = u64::from(idx);
                        child.id &= !BACKPTR;
                        child.back_block = loc.0;
                    }
                }
            }
            Ok::<_, Error>((nodes, origins))
        })?;
        let node_count = nodes.len() as u64;
        info!("[{label}] squash: collected {node_count} nodes");

        // [3] destination: placeholder rows in height order, then the tip.
        let dst_opts = MARFOpenOpts::new(TrieHashCalculationMode::Deferred, true);
        let mut dst = MARF::<T>::from_path(&dst_db_path, dst_opts)?;
        dst.sqlite_conn()
            .pragma_update(None, "journal_mode", "OFF")?;
        dst.sqlite_conn()
            .pragma_update(None, "synchronous", "OFF")?;
        let (historical, prune_root_node_hash, blob_len, source_root_hash) = {
            let mut tx = dst.begin_tx()?;
            let src_ids: HashMap<T, u32> = src.with_conn(|v| {
                Ok::<_, Error>(
                    v.inner
                        .store
                        .confirmed_blocks(v.db)?
                        .into_iter()
                        .map(|(id, b)| (b, id))
                        .collect(),
                )
            })?;
            let mut archival_to_squashed: HashMap<u32, u32> = HashMap::new();
            let mut historical = 0u64;
            {
                let v = tx.view();
                for (_, bhh, _) in blocks.iter() {
                    if *bhh == block_at_height {
                        continue;
                    }
                    let archival = *src_ids.get(bhh).ok_or(Error::NotFoundError)?;
                    let squashed = v.inner.store.insert_placeholder(v.db, bhh)?;
                    archival_to_squashed.insert(archival, squashed);
                    historical += 1;
                }
                let archival_tip = *src_ids.get(&block_at_height).ok_or(Error::NotFoundError)?;
                let squashed_tip = v.inner.store.insert_placeholder(v.db, &block_at_height)?;
                archival_to_squashed.insert(archival_tip, squashed_tip);
            }
            drop(src);

            // [4] rewrite annotations to destination ids, hash, serialize.
            for node in nodes.iter_mut() {
                if let Node::Branch(b) = node {
                    for child in b.children.iter_mut() {
                        child.back_block =
                            *archival_to_squashed.get(&child.back_block).ok_or_else(|| {
                                corrupt!("block_id {} not in block_id_map", child.back_block)
                            })?;
                    }
                }
            }
            let mut trie = Trie::from_nodes(block_at_height.clone(), T::sentinel(), nodes)?;
            let prune_root_node_hash =
                trie.seal(|_| Err(corrupt!("squash trie has no back-pointers")))?;
            trie.set_root_hash(prune_root_node_hash);
            let blob = trie.serialize(false, |_, _, _| Ok(None))?;
            drop(trie);

            // [5] persist the shared blob and the side tables.
            let source_root_hash = blocks
                .iter()
                .find(|(_, b, _)| *b == block_at_height)
                .map(|(_, _, r)| *r)
                .ok_or(Error::NotFoundError)?;
            {
                let v = tx.view();
                v.inner.store.write_shared_blob(v.db, &blob)?;
                v.inner.store.write_prune_metadata(
                    v.db,
                    &source_root_hash,
                    &prune_root_node_hash,
                    squash_height,
                    &blocks,
                )?;
            }
            tx.into_sqlite_tx().commit()?;
            (
                historical,
                prune_root_node_hash,
                blob.len(),
                source_root_hash,
            )
        };

        dst.sqlite_conn()
            .pragma_update(None, "journal_mode", "WAL")?;
        dst.sqlite_conn()
            .pragma_update(None, "synchronous", "NORMAL")?;
        info!("[{label}] squash: complete, {node_count} nodes, {blob_len} bytes");

        Ok(PruneStats {
            node_count,
            prune_height: squash_height,
            dst_db_path,
            dst_blobs_path,
            blob_size: blob_len as u64,
            historical_placeholder_count: historical,
            source_root_hash,
            prune_root_node_hash,
            total_duration: start.elapsed(),
        })
    }
}
