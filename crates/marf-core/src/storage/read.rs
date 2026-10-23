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

//! Borrowed record access through SQLite buffers or mapped blobs.
use std::collections::HashMap;

use rusqlite::{Connection, DatabaseName, OptionalExtension, params};

use super::{Blobs, BytesScratch, Cached, Location, Resolved, Store, StoreNode, UNCONFIRMED_SLOT};
use crate::codec::{HASH_LEN, MAX_NODE_LEN, MAX_PATCH_DEPTH, Record, RecordKind};
use crate::node::{Branch, Kind, Node, Path as NodePath, Ptr};
use crate::observe::Observer;
use crate::trie::{Fetched, ROOT_OFFSET};
use crate::{Error, TrieHash, TrieId, codec, corrupt};

impl<'a> StoreNode<'a> {
    pub fn is_leaf(&self) -> Result<bool, Error> {
        match self {
            StoreNode::Record(r) => Ok(r.kind()? == RecordKind::Leaf),
            StoreNode::Patched(_) => Ok(false),
        }
    }

    /// The node's compressed path.
    pub fn path(&self) -> Result<&'a [u8], Error> {
        match self {
            StoreNode::Record(r) => match r.kind()? {
                RecordKind::Leaf => Ok(r.leaf()?.path),
                RecordKind::Branch(_) => r.branch()?.path(),
                RecordKind::Patch => Err(corrupt!("unresolved patch")),
            },
            StoreNode::Patched(b) => Ok(b.path.as_slice()),
        }
    }

    /// The child at `chr` of a branch (`None` for a leaf or an empty slot).
    pub fn child(&self, chr: u8) -> Result<Option<Ptr>, Error> {
        match self {
            StoreNode::Record(r) => match r.kind()? {
                RecordKind::Leaf => Ok(None),
                RecordKind::Branch(_) => r.branch()?.child(chr),
                RecordKind::Patch => Err(corrupt!("unresolved patch")),
            },
            StoreNode::Patched(b) => Ok(b.child(chr).copied()),
        }
    }

    /// One step of a key walk: for a record, [`Record::descend`]; for a
    /// resolved patch chain, the equivalent over the branch.
    pub fn descend(&self, key: &[u8], depth: usize) -> Result<codec::Descent<'a>, Error> {
        match self {
            StoreNode::Record(r) => r.descend(key, depth),
            StoreNode::Patched(b) => Ok(descend_branch(b, key, depth)?),
        }
    }

    /// The leaf's value, if this is a leaf.
    pub fn leaf_value(&self) -> Result<Option<crate::MARFValue>, Error> {
        match self {
            StoreNode::Record(r) if r.kind()? == RecordKind::Leaf => Ok(Some(r.leaf()?.value())),
            _ => Ok(None),
        }
    }

    /// Visit every occupied child slot in slot order.
    pub fn for_each_child(&self, mut visit: impl FnMut(Ptr)) -> Result<(), Error> {
        match self {
            StoreNode::Record(r) => match r.kind()? {
                RecordKind::Leaf => Ok(()),
                RecordKind::Branch(_) => r.branch()?.for_each_slot(|_, p| visit(p)).map(|_| ()),
                RecordKind::Patch => Err(corrupt!("unresolved patch")),
            },
            StoreNode::Patched(b) => {
                b.children.iter().for_each(|p| visit(*p));
                Ok(())
            }
        }
    }

    /// An owned copy.
    pub fn to_node(&self) -> Result<Node, Error> {
        match self {
            StoreNode::Record(r) => match r.decode()?.0 {
                codec::Decoded::Node(n) => Ok(n),
                codec::Decoded::Patch(_) => Err(corrupt!("unresolved patch")),
            },
            StoreNode::Patched(b) => Ok(Node::Branch((*b).clone())),
        }
    }
}

/// [`Record::descend`] for an in-memory branch.
pub fn descend_branch<'a>(
    b: &Branch,
    key: &[u8],
    depth: usize,
) -> Result<codec::Descent<'a>, Error> {
    let rest = &key[depth..];
    if !rest.starts_with(b.path.as_slice()) {
        return Ok(codec::Descent::Mismatch);
    }
    let Some(&chr) = rest.get(b.path.len()) else {
        return Err(corrupt!("Path reached a non-leaf"));
    };
    Ok(codec::Descent::Child {
        child: b.child(chr).copied(),
        consumed: b.path.len() + 1,
    })
}
/// Where block `block_id`'s trie bytes live (cached in `locations`).
fn location_in(
    locations: &mut HashMap<u32, Location>,
    external: bool,
    conn: &Connection,
    block_id: u32,
) -> Result<Location, Error> {
    if let Some(loc) = locations.get(&block_id) {
        return Ok(*loc);
    }
    count!(location_sql);
    let (offset, len, unconfirmed): (i64, i64, i64) = conn
        .prepare_cached(
            "SELECT external_offset, external_length, unconfirmed FROM marf_data WHERE block_id = ?1",
        )?
        .query_row(params![block_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?
        .ok_or(Error::NotFoundError)?;
    let loc = if external && len != 0 {
        Location::External {
            offset: offset as u64,
            len: len as u64,
        }
    } else {
        Location::Sqlite {
            unconfirmed: unconfirmed != 0,
        }
    };
    locations.insert(block_id, loc);
    Ok(loc)
}

/// Bytes of block `block_id`'s trie from `offset` for up to `max` bytes
/// (fewer at the end of the trie). Borrows the blobs map or the scratch
/// buffer; takes the fields it needs so callers can hold other parts of the
/// store at the same time.
pub fn trie_bytes<'s, O: Observer>(
    blobs: &'s mut Option<Blobs>,
    scratch: &'s mut BytesScratch,
    conn: &Connection,
    loc: Location,
    block_id: u32,
    offset: u64,
    max: usize,
) -> Result<&'s [u8], Error> {
    match loc {
        Location::External { offset: base, len } => {
            if offset >= len {
                return Err(corrupt!(
                    "offset {offset} beyond trie of {len} bytes (block {block_id})"
                ));
            }
            let want = (max as u64).min(len - offset);
            let blobs = blobs.as_mut().expect("external location implies blobs");
            blobs.slice(base + offset, want)
        }
        // A per-node incremental blob read, like the original: a walk through
        // a trie touches a handful of its nodes and lookups cross many tries,
        // so caching whole tries here only thrashes. The record is not
        // length-prefixed; read the hash and type byte first and then only
        // the bound that type allows, so a leaf costs ~100 bytes of copying
        // rather than the Node256 bound.
        Location::Sqlite { .. } => {
            if scratch.key != Some((block_id, offset)) {
                O::record(crate::observe::Record::DiskRead(block_id));
                let blob = conn.blob_open(
                    DatabaseName::Main,
                    "marf_data",
                    "data",
                    i64::from(block_id),
                    true,
                )?;
                let len = blob.len();
                let off = usize::try_from(offset).map_err(|_| Error::OverflowError)?;
                if off >= len {
                    return Err(corrupt!(
                        "offset {offset} beyond trie of {len} bytes (block {block_id})"
                    ));
                }
                let avail = len - off;
                let head = max.min(avail).min(HASH_LEN + 1);
                scratch.buf.resize(head, 0);
                let n = blob.read_at(&mut scratch.buf, off)?;
                scratch.buf.truncate(n);
                if n == HASH_LEN + 1 && max > HASH_LEN + 1 {
                    let want = max
                        .min(avail)
                        .min(codec::max_record_len(scratch.buf[HASH_LEN]));
                    if want > n {
                        scratch.buf.resize(want, 0);
                        let m = blob.read_at(&mut scratch.buf[n..], off + n)?;
                        scratch.buf.truncate(n + m);
                    }
                }
                scratch.key = Some((block_id, offset));
            }
            Ok(&scratch.buf)
        }
    }
}

/// Parse the record at `ptr` in block `block_id` (located at `loc`).
fn record_in<'s, O: Observer>(
    blobs: &'s mut Option<Blobs>,
    scratch: &'s mut BytesScratch,
    conn: &Connection,
    loc: Location,
    block_id: u32,
    ptr: &Ptr,
) -> Result<Record<'s>, Error> {
    count!(record_reads);
    if matches!(loc, Location::Sqlite { .. }) {
        count!(sqlite_record_reads);
    }
    let bytes = trie_bytes::<O>(blobs, scratch, conn, loc, block_id, ptr.ptr, MAX_NODE_LEN)?;
    Record::parse(bytes, ptr.kind_id()).map_err(|e| match e {
        Error::CorruptionError(m) => {
            Error::CorruptionError(format!("{m} (node at {} in block {block_id})", ptr.ptr))
        }
        e => e,
    })
}
impl<T: TrieId, O: Observer> Store<T, O> {
    fn location(&mut self, conn: &Connection, block_id: u32) -> Result<Location, Error> {
        let _span = observe!(O, StoreLocation);
        location_in(&mut self.locations, self.blobs.is_some(), conn, block_id)
    }

    /// Bytes of block `block_id`'s trie from `offset`, at most `max`.
    fn bytes(
        &mut self,
        conn: &Connection,
        block_id: u32,
        offset: u64,
        max: usize,
    ) -> Result<&[u8], Error> {
        let loc = self.location(conn, block_id)?;
        self.scratch.bytes.key = None;
        trie_bytes::<O>(
            &mut self.blobs,
            &mut self.scratch.bytes,
            conn,
            loc,
            block_id,
            offset,
            max,
        )
    }

    /// Read the whole trie blob of `block_id`.
    pub fn read_trie_blob(&mut self, conn: &Connection, block_id: u32) -> Result<Vec<u8>, Error> {
        let _span = observe!(O, StoreReadTrieBlob);
        match self.location(conn, block_id)? {
            Location::External { offset, len } => {
                let blobs = self
                    .blobs
                    .as_mut()
                    .expect("external location implies blobs");
                Ok(blobs.slice(offset, len)?.to_vec())
            }
            Location::Sqlite { .. } => Ok(conn.query_row(
                "SELECT data FROM marf_data WHERE block_id = ?1",
                params![block_id],
                |r| r.get(0),
            )?),
        }
    }

    /// The hash stored in front of the node at `offset`.
    pub fn read_hash(
        &mut self,
        conn: &Connection,
        block_id: u32,
        offset: u64,
    ) -> Result<TrieHash, Error> {
        let _span = observe!(O, StoreReadHash);
        let bytes = self.bytes(conn, block_id, offset, HASH_LEN)?;
        TrieHash::from_bytes(bytes).ok_or_else(|| corrupt!("Failed to read hash in full"))
    }

    pub fn read_root_hash(&mut self, conn: &Connection, block_id: u32) -> Result<TrieHash, Error> {
        self.read_hash(conn, block_id, ROOT_OFFSET)
    }

    /// The parent block id recorded in the trie blob header of `block_id`.
    pub fn read_parent(&mut self, conn: &Connection, block_id: u32) -> Result<T, Error> {
        let bytes = self.bytes(conn, block_id, 0, 32)?;
        let buf: [u8; 32] = bytes
            .try_into()
            .map_err(|_| corrupt!("Failed to read parent hash in full"))?;
        Ok(T::from_bytes(buf))
    }
    /// Read the record at `ptr` in `block_id`: either a node or a patch.
    /// Resolve the node at `ptr` in `block_id` without materializing it:
    /// a plain record is borrowed from storage; a patch chain is followed to
    /// its base (at most [`crate::codec::MAX_PATCH_DEPTH`] hops) and applied into the
    /// store's scratch branch. Nothing is allocated once the scratch buffers
    /// have grown to their working size.
    pub fn node(
        &mut self,
        conn: &Connection,
        block_id: u32,
        ptr: &Ptr,
    ) -> Result<Resolved<'_>, Error> {
        let _span = observe!(O, StoreNode);
        resolve_in::<O>(
            &mut self.blobs,
            &mut self.scratch.bytes,
            &mut self.scratch.diffs,
            &mut self.scratch.patched,
            self.patch_cache_capacity,
            &mut self.locations,
            conn,
            block_id,
            ptr,
        )
    }

    /// Like [`Store::node`], but always as a `Branch` (decoded into scratch
    /// when the record is a plain branch); `None` for a leaf.
    pub fn branch(
        &mut self,
        conn: &Connection,
        block_id: u32,
        ptr: &Ptr,
    ) -> Result<Option<(&Branch, u32)>, Error> {
        let r = resolve_in::<O>(
            &mut self.blobs,
            &mut self.scratch.bytes,
            &mut self.scratch.diffs,
            &mut self.scratch.patched,
            self.patch_cache_capacity,
            &mut self.locations,
            conn,
            block_id,
            ptr,
        )?;
        match r.node {
            StoreNode::Patched(b) => Ok(Some((b, r.depth))),
            StoreNode::Record(rec) => match rec.kind()? {
                RecordKind::Leaf => Ok(None),
                RecordKind::Patch => Err(corrupt!("unresolved patch")),
                RecordKind::Branch(_) => {
                    let out = self
                        .scratch
                        .decoded
                        .get_or_insert_with(|| Branch::new(Kind::Node256, NodePath::EMPTY));
                    rec.branch()?.fill(out)?;
                    Ok(Some((out, 0)))
                }
            },
        }
    }

    /// Read the node at `ptr` in `block_id` as an owned value, resolving
    /// patch chains. Returns the node, its stored hash, and the patch depth.
    pub fn read_node(
        &mut self,
        conn: &Connection,
        block_id: u32,
        ptr: &Ptr,
    ) -> Result<(Node, TrieHash, u32), Error> {
        let _span = observe!(O, StoreReadNode);
        let r = self.node(conn, block_id, ptr)?;
        count!(owned_materializations);
        Ok((r.node.to_node()?, r.hash, r.depth))
    }

    /// Fetch the target of a back-pointer, for copy-on-write.
    pub fn fetch(&mut self, conn: &Connection, ptr: &Ptr) -> Result<Fetched, Error> {
        let (node, _, depth) = self.read_node(conn, ptr.back_block, ptr)?;
        Ok(Fetched { node, depth })
    }
}

/// See [`Store::node`]; takes the store's fields apart so the result can
/// borrow the record bytes or the cached branch while the caller keeps the
/// rest of the store.
#[allow(clippy::too_many_arguments)]
fn resolve_in<'s, O: Observer>(
    blobs: &'s mut Option<Blobs>,
    bytes: &'s mut BytesScratch,
    diffs: &mut Vec<Ptr>,
    patched: &'s mut HashMap<(u32, u64), Cached>,
    capacity: usize,
    locations: &mut HashMap<u32, Location>,
    conn: &Connection,
    block_id: u32,
    ptr: &Ptr,
) -> Result<Resolved<'s>, Error> {
    let external = blobs.is_some();
    let cur_block = block_id;
    let mut block = block_id;
    let mut ptr = ptr.without_back();
    let key = (block_id, ptr.ptr);
    // capacity 0: no lookup, and nothing is retained beyond the one scratch
    // slot a resolved chain is returned from
    let cache = capacity > 0;
    if cache {
        count!(patch_cache_lookups);
        if patched.len() >= capacity {
            patched.clear();
            count!(patch_cache_clears);
        }
        // (two lookups on a hit: a borrow returned from one branch of an `if`
        // would hold `patched` for the rest of the function)
        if patched.contains_key(&key) {
            count!(patch_cache_hits);
            let c = &patched[&key];
            return Ok(Resolved {
                node: StoreNode::Patched(&c.node),
                hash: c.hash,
                depth: c.depth,
            });
        }
    }
    bytes.key = None;
    diffs.clear();
    // patches met on the way down: (block read from, base pointer, diff range)
    let mut chain: [(u32, Ptr, usize, usize); MAX_PATCH_DEPTH as usize] =
        [(0, Ptr::default(), 0, 0); MAX_PATCH_DEPTH as usize];
    let mut depth = 0usize;
    let mut hash: Option<TrieHash> = None;
    let mut cacheable = true;
    loop {
        let loc = location_in(locations, external, conn, block)?;
        cacheable &= !matches!(loc, Location::Sqlite { unconfirmed: true });
        let rec = record_in::<O>(blobs, bytes, conn, loc, block, &ptr)?;
        if hash.is_none() {
            hash = Some(rec.hash());
        }
        if !rec.is_patch() {
            break;
        }
        if depth == MAX_PATCH_DEPTH as usize {
            return Err(Error::NodeTooDeep);
        }
        let patch = rec.patch()?;
        let start = diffs.len();
        patch.for_each_diff(|d| diffs.push(d))?;
        chain[depth] = (block, patch.base, start, diffs.len());
        depth += 1;
        block = patch.base.back_block;
        ptr = patch.base.without_back();
    }
    let hash = hash.expect("set");
    let loc = location_in(locations, external, conn, block)?;
    let rec = record_in::<O>(blobs, bytes, conn, loc, block, &ptr)?;
    if depth == 0 {
        if cache {
            count!(patch_cache_probe_miss_plain);
        }
        return Ok(Resolved {
            node: StoreNode::Record(rec),
            hash,
            depth: 0,
        });
    }
    count!(patch_resolutions);
    count!(patch_applications, depth as u32);
    count!(owned_materializations);
    let view = rec
        .branch()
        .map_err(|_| corrupt!("patch base at {} in block {block} is not a branch", ptr.ptr))?;
    let mut out = Branch::new(view.kind(), NodePath::EMPTY);
    view.fill(&mut out)?;
    // apply base-first: the innermost patch (closest to the base) first
    for (patch_block, base, start, end) in chain[..depth].iter().rev() {
        codec::apply_patch(
            &mut out,
            *base,
            &diffs[*start..*end],
            *patch_block,
            cur_block,
        )?;
    }
    let depth = depth as u32;
    // an unconfirmed trie is rewritten in place, so its nodes go to a slot
    // that the next such read overwrites instead of the cache proper; so
    // does everything when the cache is disabled
    let key = if cacheable && cache {
        count!(patch_cache_misses);
        key
    } else {
        UNCONFIRMED_SLOT
    };
    patched.insert(
        key,
        Cached {
            node: out,
            hash,
            depth,
        },
    );
    let c = &patched[&key];
    Ok(Resolved {
        node: StoreNode::Patched(&c.node),
        hash,
        depth,
    })
}
