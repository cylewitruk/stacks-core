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
//!
//! Memory: the collected nodes are staged in a temporary file beside the
//! destination ([`Staged`]), and the output blob is streamed into the blobs
//! file; what stays in memory is per-node bookkeeping (a file offset, a
//! hash, a blob offset: 48 bytes a node in arrays, plus the dedup index
//! while collecting and two 1 MiB buffers; about 95 bytes a node measured
//! at peak), never the decoded trie or the serialized blob. The original
//! does the same (`squash::node_store`, `squash::stream`).

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use stacks_common::types::chainstate::TrieHash;

use crate::codec::{self, Decoded, HASH_LEN, MAX_NODE_LEN};
use crate::hash::{branch_hash, leaf_hash, ChildHash, HashScratch};
use crate::marf::{MARFOpenOpts, MarfConnection, TrieHashCalculationMode, MARF};
use crate::node::{id, Node, Ptr, BACKPTR};
use crate::storage::{read_exact_at, write_all_at};
use crate::trie::{HEADER_LEN, ROOT_OFFSET};
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


/// Write buffer size for the stage file and the output blob.
const STREAM_BUFFER: usize = 1 << 20;

/// Nodes collected for pruning, staged in a temporary file. Each node is
/// written once, in its plain record encoding (`codec::encode_node`, no
/// hash) behind a 4-byte length, and read back by index; indices are handed
/// out in discovery order, so a node's index is always greater than its
/// parent's, and child pointers hold indices. The file is removed on drop.
struct Staged {
    path: PathBuf,
    file: File,
    /// Appended records not yet written to the file.
    pending: Vec<u8>,
    /// Bytes written to the file so far.
    flushed: u64,
    /// Start of each node's record in the file; `u64::MAX` until written.
    offsets: Vec<u64>,
    encode: Vec<u8>,
    read: Vec<u8>,
}

impl Staged {
    fn create(dir: &Path) -> Result<Staged, Error> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = dir.join(format!(".prune_nodes_{}_{nanos}.tmp", std::process::id()));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        Ok(Staged {
            path,
            file,
            pending: Vec::with_capacity(STREAM_BUFFER + MAX_NODE_LEN + 4),
            flushed: 0,
            offsets: vec![],
            encode: Vec::with_capacity(MAX_NODE_LEN),
            read: Vec::with_capacity(MAX_NODE_LEN),
        })
    }

    /// Hand out the next index (the node is written later).
    fn reserve(&mut self) -> Result<u32, Error> {
        let idx = u32::try_from(self.offsets.len()).map_err(|_| Error::OverflowError)?;
        self.offsets.push(u64::MAX);
        Ok(idx)
    }

    fn len(&self) -> u32 {
        self.offsets.len() as u32
    }

    fn write(&mut self, idx: u32, node: &Node) -> Result<(), Error> {
        self.encode.clear();
        codec::encode_node(node, false, &mut self.encode);
        let len = u32::try_from(self.encode.len()).map_err(|_| Error::OverflowError)?;
        let slot = self
            .offsets
            .get_mut(idx as usize)
            .ok_or_else(|| corrupt!("stage: index {idx} was never reserved"))?;
        if *slot != u64::MAX {
            return Err(corrupt!("stage: node {idx} written twice"));
        }
        *slot = self.flushed + self.pending.len() as u64;
        self.pending.extend_from_slice(&len.to_le_bytes());
        self.pending.extend_from_slice(&self.encode);
        if self.pending.len() >= STREAM_BUFFER {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), Error> {
        if !self.pending.is_empty() {
            write_all_at(&self.file, &self.pending, self.flushed)?;
            self.flushed += self.pending.len() as u64;
            self.pending.clear();
        }
        Ok(())
    }

    /// Read node `idx` back; call [`Staged::flush`] once collection is done.
    fn read(&mut self, idx: u32) -> Result<Node, Error> {
        let offset = *self
            .offsets
            .get(idx as usize)
            .ok_or_else(|| corrupt!("stage: index {idx} out of range ({})", self.offsets.len()))?;
        if offset == u64::MAX {
            return Err(corrupt!("stage: node {idx} was never written"));
        }
        let mut len = [0u8; 4];
        read_exact_at(&self.file, &mut len, offset)?;
        let len = u32::from_le_bytes(len) as usize;
        if len == 0 || len > MAX_NODE_LEN {
            return Err(corrupt!("stage: node {idx} has record length {len}"));
        }
        self.read.resize(len, 0);
        read_exact_at(&self.file, &mut self.read, offset + 4)?;
        match codec::decode(&self.read, self.read[0])? {
            (Decoded::Node(node), _) => Ok(node),
            (Decoded::Patch(_), _) => Err(corrupt!("stage: node {idx} decoded as a patch")),
        }
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// The stage index an inline child pointer of node `parent` refers to; the
/// pre-order invariant (`parent < child < n`) is what lets hashing and
/// serialization run in reverse index order, children before parents.
fn inline_child(p: &Ptr, parent: u32, n: u32) -> Result<u32, Error> {
    if p.is_back() {
        return Err(corrupt!("squash trie has a back-pointer at node {parent}"));
    }
    let child = u32::try_from(p.ptr).map_err(|_| Error::OverflowError)?;
    if child <= parent || child >= n {
        return Err(corrupt!(
            "node {parent}: child index {child} breaks the pre-order invariant (n = {n})"
        ));
    }
    Ok(child)
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

        // [2] collect every node reachable at the squash tip into the stage,
        // pre-order. A child's index is assigned when its parent is visited,
        // so parents are written with their child pointers already rewritten
        // to inline indices annotated with the child's archival block, and no
        // remap pass is needed.
        let stage_dir = dst_db_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let mut stage = Staged::create(stage_dir)?;
        src.with_conn(|v| {
            let tip_id = v.inner.store.block_id(v.db, &block_at_height)?;
            let mut index: HashMap<(u32, u64), u32> = HashMap::new();
            let root_idx = stage.reserve()?;
            index.insert((tip_id, ROOT_OFFSET), root_idx);
            let mut stack: Vec<(u32, u64, u8, u32)> =
                vec![(tip_id, ROOT_OFFSET, id::NODE256, root_idx)];
            let mut discovered: Vec<(u32, u64, u8, u32)> = vec![];
            while let Some((block, offset, kind, idx)) = stack.pop() {
                let (mut node, _, _) =
                    v.inner
                        .store
                        .read_node(v.db, block, &Ptr::new(kind, 0, offset))?;
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
            Ok::<_, Error>(())
        })?;
        stage.flush()?;
        let n = stage.len();
        let node_count = u64::from(n);
        info!("[{label}] squash: collected {node_count} nodes");

        // [3] hash, children before parents (reverse index order).
        let mut hashes: Vec<TrieHash> = vec![TrieHash::ZERO; n as usize];
        let mut scratch = HashScratch::default();
        for idx in (0..n).rev() {
            let node = stage.read(idx)?;
            let hash = match &node {
                Node::Leaf(leaf) => leaf_hash(leaf),
                Node::Branch(b) => branch_hash::<Error>(
                    b,
                    |p| Ok(ChildHash::inline(hashes[inline_child(p, idx, n)? as usize])),
                    &mut scratch,
                )?,
            };
            hashes[idx as usize] = hash;
        }
        let prune_root_node_hash = hashes[0];
        info!("[{label}] squash: hashed {node_count} nodes");

        // [4] destination: placeholder rows in height order, then the tip.
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

            // [5] stream the shared blob: nodes children-first, so every
            // pointer's target offset is known when the pointer is written,
            // then the root into a slot reserved for its widest encoding (the
            // original's `stream_squash_blob` layout). Annotations become
            // destination block ids on the way out.
            let squashed = |archival: u32| -> Result<u32, Error> {
                archival_to_squashed
                    .get(&archival)
                    .copied()
                    .ok_or_else(|| corrupt!("block_id {archival} not in block_id_map"))
            };
            let remap = |node: &mut Node, idx: u32, offsets: &[u64]| -> Result<(), Error> {
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
                let v = tx.view();
                v.inner.store.write_shared_blob_with(v.db, |blobs, base| {
                    let mut root = stage.read(0)?;
                    let widest = |p: &Ptr| -> Ptr {
                        if p.is_back() {
                            *p
                        } else {
                            Ptr { ptr: u64::MAX, ..*p }
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
                            blobs.write_at_unsynced(base + flushed, &out)?;
                            flushed = pos;
                            out.clear();
                        }
                    }
                    if !out.is_empty() {
                        blobs.write_at_unsynced(base + flushed, &out)?;
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
                        return Err(corrupt!(
                            "squash root needs {} bytes, {reserved} reserved",
                            out.len() as u64 - ROOT_OFFSET - HASH_LEN as u64
                        ));
                    }
                    blobs.write_at_unsynced(base, &out)?;
                    Ok(pos)
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
                let v = tx.view();
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
                blob_len,
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

#[cfg(test)]
mod tests {
    use stacks_common::types::chainstate::StacksBlockId;
    use stacks_common::types::sqlite::NO_PARAMS;

    use super::*;
    use crate::marf::{MARFOpenOpts, MarfConnection, TrieHashCalculationMode, MARF};
    use crate::{ClarityMarfTrieId, MARFValue};

    fn bid(n: u8) -> StacksBlockId {
        let mut b = [0u8; 32];
        b[0] = n;
        b[31] = 1;
        StacksBlockId(b)
    }

    fn opts() -> MARFOpenOpts {
        MARFOpenOpts::new(TrieHashCalculationMode::Deferred, true).with_compression(true)
    }

    /// `PruneStats.blob_size` is the shared blob's length: the length
    /// published in `marf_data`, and for a fresh destination the length of
    /// the whole blobs file. (Regression: the blob's offset was reported,
    /// zero for a fresh destination.) Reads, the root hash and proofs of the
    /// pruned database must match the source.
    #[test]
    fn prune_stats_report_the_blob_length() {
        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("src.sqlite");
        let mut src = MARF::<StacksBlockId>::from_path(&src_path, opts()).unwrap();
        let mut parent = StacksBlockId::sentinel();
        let mut expected: Vec<(String, MARFValue)> = vec![];
        for b in 1..=4u8 {
            let child = bid(b);
            let mut tx = src.begin_tx().unwrap();
            tx.begin(&parent, &child).unwrap();
            for i in 0..50u8 {
                let key = format!("key-{b}-{i}");
                let value = MARFValue([b.wrapping_mul(7).wrapping_add(i); 40]);
                tx.insert(&key, value.clone()).unwrap();
                expected.push((key, value));
            }
            tx.commit().unwrap();
            parent = child;
        }
        let tip = parent;
        let src_root = src.get_root_hash_at(&tip).unwrap();
        drop(src);

        let dst_path = dir.path().join("dst.sqlite");
        let stats = <MARF<StacksBlockId> as Prune<StacksBlockId>>::prune_to_path(
            &src_path, &dst_path, opts(), &tip, 3, "test",
        )
        .unwrap();
        assert!(stats.node_count > 0);
        assert!(stats.blob_size > 0, "a fresh destination reported no bytes");
        let file_len = fs::metadata(crate::storage::Blobs::path_for(&dst_path))
            .unwrap()
            .len();
        assert_eq!(stats.blob_size, file_len, "fresh destination: the blob is the whole file");

        let mut dst = MARF::<StacksBlockId>::from_path(&dst_path, opts()).unwrap();
        let published: i64 = dst
            .sqlite_conn()
            .query_row(
                "SELECT DISTINCT external_length FROM marf_data WHERE unconfirmed = 0",
                NO_PARAMS,
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stats.blob_size, published as u64, "published length differs");
        assert_eq!(dst.get_root_hash_at(&tip).unwrap(), src_root);
        for (key, value) in &expected {
            assert_eq!(dst.get(&tip, key).unwrap().as_ref(), Some(value), "{key}");
        }
        assert!(dst.get(&tip, "absent").unwrap().is_none());
        // proofs need the archival ancestry, which a pruned database no
        // longer has: both implementations refuse them (fixture semantics)
        assert!(matches!(
            dst.get_with_proof(&tip, &expected[7].0),
            Err(Error::UnsupportedOnPrunedMarf(_))
        ));
    }
}
