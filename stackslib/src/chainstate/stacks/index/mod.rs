// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020-2026 Stacks Open Internet Foundation
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

#![warn(dead_code)]

use std::hash::Hash;
use std::ops::Deref;
use std::sync::{Arc, OnceLock};
use std::{error, fmt, io};

use sha2::{Digest, Sha512_256 as TrieHasher};
#[cfg(test)]
use stacks_common::types::chainstate::BlockHeaderHash;
use stacks_common::types::chainstate::{
    BurnchainHeaderHash, SortitionId, StacksBlockId, TrieHash, TRIEHASH_ENCODED_SIZE,
};

use self::packed_branch::BranchView;
use self::record::NodeRecordFormat;
use crate::chainstate::stacks::index::storage::TrieStorageConnection;
use crate::util_lib::db::Error as db_error;

/// Optional counters for benchmark experiments.
#[cfg(feature = "marf-read-bench-counters")]
pub mod read_bench;

mod ancestry;
pub mod bits;
pub mod blob_layout;
pub mod cache;
pub mod direct_hash_index;
pub mod file;
pub mod inline_value;
use self::inline_value::InlineValue;
mod mapped_file;
mod raw_leaf;
pub use self::mapped_file::FileMapping;
pub mod mapped_node;
pub mod packed_branch;
pub mod marf;
pub mod node;
pub mod proofs;
pub mod record;
pub mod result_cache;
pub mod scratch;
pub mod squash;
pub mod storage;
pub mod trie;
pub mod trie_sql;
pub mod value_relocation;

#[cfg(test)]
pub mod test;

use crate::chainstate::stacks::index::node::{
    clear_backptr, is_backptr, CursorError, ParkedNodeHandle, TrieCursor, TrieLeafRef, TrieNodeID,
    TrieNodePatch, TrieNodeRef, TrieNodeTransientMeta, TrieNodeType, TriePtr,
};

#[derive(Debug)]
pub struct TrieMerkleProof<T: MarfTrieId>(pub Vec<TrieMerkleProofType<T>>);

pub trait ClarityMarfTrieId:
    PartialEq + Clone + std::fmt::Display + std::fmt::Debug + std::convert::From<[u8; 32]>
{
    fn as_bytes(&self) -> &[u8];
    fn to_bytes(self) -> [u8; 32];
    fn from_bytes(from: [u8; 32]) -> Self;
    fn sentinel() -> Self;
}

#[derive(Clone)]
pub enum TrieMerkleProofType<T> {
    Node4((u8, ProofTrieNode<T>, [TrieHash; 3])),
    Node16((u8, ProofTrieNode<T>, [TrieHash; 15])),
    Node48((u8, ProofTrieNode<T>, [TrieHash; 47])),
    Node256((u8, ProofTrieNode<T>, [TrieHash; 255])),
    Leaf((u8, TrieLeaf)),
    Shunt((i64, Vec<TrieHash>)),
}

/// Merkle Proof Trie Pointers have a different structure
///   than the runtime representation --- the proof includes
///   the block header hash for back pointers.
#[derive(Debug, Clone, PartialEq)]
pub struct ProofTrieNode<T> {
    pub id: u8,
    pub path: Vec<u8>,
    pub ptrs: Vec<ProofTriePtr<T>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProofTriePtr<T> {
    pub id: u8,
    pub chr: u8,
    pub back_block: T,
}

/// A compressed trie path segment, stored inline as a fixed-size array.
///
/// Replaces `Vec<u8>` on all trie node types. Maximum length is 32 bytes
/// (the size of a `TrieHash`), enforced at construction time.
///
/// `NodePath` is `Copy`, so cloning nodes no longer heap-allocates for the path field.
#[derive(Clone, Copy, Default)]
pub struct NodePath {
    data: [u8; 32],
    len: u8,
}

impl PartialEq for NodePath {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl Eq for NodePath {}

impl NodePath {
    /// Create a `NodePath` from a byte slice. Returns `None` if `s.len() > 32`.
    #[inline]
    pub fn from_slice(s: &[u8]) -> Option<Self> {
        let len = s.len();
        if len > 32 {
            return None;
        }
        let mut data = [0u8; 32];
        data.get_mut(..len)?.copy_from_slice(s);
        Some(Self {
            data,
            len: len as u8,
        })
    }

    /// The path bytes as a slice.
    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        // `len` is always <= 32: only set by validated paths (`from_slice`, `set_from_slice`, or
        // `read_from` after callers validate against `TRIEHASH_ENCODED_SIZE`).
        self.data
            .get(..self.len as usize)
            .expect("BUG: NodePath len invariant violated")
    }

    /// The length of the path in bytes.
    #[inline]
    pub fn len(&self) -> usize {
        self.len as usize
    }

    /// Returns `true` if the path is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Set the path from a reader.
    ///
    /// `len` must be <= 32 (callers validate via [`TRIEHASH_ENCODED_SIZE`] before calling).
    #[inline]
    pub fn read_from<R: io::Read>(&mut self, len: u8, r: &mut R) -> Result<(), io::Error> {
        self.len = len;
        r.read_exact(
            self.data
                .get_mut(..len as usize)
                .expect("BUG: NodePath::read_from called with len > 32"),
        )
    }

    /// Set the path from a byte slice. Returns `None` if `s.len() > 32`.
    #[inline]
    pub fn set_from_slice(&mut self, s: &[u8]) -> Option<()> {
        let len = s.len();
        if len > 32 {
            return None;
        }
        self.data.get_mut(..len)?.copy_from_slice(s);
        self.len = len as u8;
        Some(())
    }
}

impl Deref for NodePath {
    type Target = [u8];
    #[inline]
    fn deref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl AsRef<[u8]> for NodePath {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl fmt::Debug for NodePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "NodePath({})",
            crate::util::hash::to_hex(self.as_slice())
        )
    }
}

/// Physical address of an immutable value record in a database's extent file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ValueExtent {
    /// Identity binding this locator to one immutable extent-file generation.
    pub store_id: [u8; 16],
    /// Absolute byte offset of the record envelope.
    pub offset: u64,
    /// Complete envelope and payload length.
    pub length: u64,
}

impl ValueExtent {
    /// Fixed physical locator width; never included in a MARF commitment.
    pub const ENCODED_SIZE: usize = 32;

    /// Write the physical locator in its version-one fixed-width representation.
    pub fn write_to<W: io::Write>(&self, writer: &mut W) -> Result<(), io::Error> {
        writer.write_all(&self.store_id)?;
        writer.write_all(&self.offset.to_le_bytes())?;
        writer.write_all(&self.length.to_le_bytes())
    }

    /// Read a complete physical locator without trusting its bounds or file identity.
    pub fn read_from<R: io::Read>(reader: &mut R) -> Result<Self, io::Error> {
        let mut store_id = [0; 16];
        let mut offset = [0; 8];
        let mut length = [0; 8];
        reader.read_exact(&mut store_id)?;
        reader.read_exact(&mut offset)?;
        reader.read_exact(&mut length)?;
        Ok(Self {
            store_id,
            offset: u64::from_le_bytes(offset),
            length: u64::from_le_bytes(length),
        })
    }
}

/// Leaf of a Trie.
#[derive(Clone)]
pub struct TrieLeaf {
    pub path: NodePath,
    /// Logical value, absent until an extent-backed leaf needs hashing or a proof.
    pub data: Option<MARFValue>,
    /// Optional physical locator excluded from logical commitments.
    pub extent: Option<ValueExtent>,
    /// Inline value bytes retaining their immutable backing owner.
    pub inline: Option<InlineValue>,
}

/// Resolves logical commitments for physical leaves without coupling the index to a value codec.
pub trait ValueExtentResolver: Send + Sync {
    /// Reconstruct the canonical value commitment from an immutable extent record.
    fn commitment(&self, extent: ValueExtent) -> Result<MARFValue, Error>;
    /// Reconstruct an inline value commitment only when explicitly requested.
    fn inline_commitment(&self, _value: &InlineValue) -> Result<MARFValue, Error> {
        Err(Error::CorruptionError(
            "Inline value resolver unavailable".into(),
        ))
    }
}

pub trait MarfTrieId:
    ClarityMarfTrieId
    + rusqlite::types::ToSql
    + rusqlite::types::FromSql
    + stacks_common::codec::StacksMessageCodec
    + std::convert::From<MARFValue>
    + PartialEq
    + Eq
    + Hash
{
}

/// One confirmed `marf_data` row, as loaded by
/// [`trie_sql::bulk_read_block_entries`].
#[derive(Debug, Clone)]
pub struct MarfDataEntry<T> {
    /// SQLite rowid of the block in `marf_data`.
    pub block_id: u32,
    pub block_hash: T,
    /// Byte offset of the block's trie blob in external `.blobs` storage.
    pub external_offset: u64,
}

pub const SENTINEL_ARRAY: [u8; 32] = [255u8; 32];

macro_rules! impl_clarity_marf_trie_id {
    ($thing:ident) => {
        impl ClarityMarfTrieId for $thing {
            fn as_bytes(&self) -> &[u8] {
                self.as_ref()
            }
            fn to_bytes(self) -> [u8; 32] {
                self.0
            }
            fn sentinel() -> Self {
                Self(SENTINEL_ARRAY.clone())
            }
            fn from_bytes(bytes: [u8; 32]) -> Self {
                Self(bytes)
            }
        }

        impl From<MARFValue> for $thing {
            fn from(m: MARFValue) -> Self {
                let h = m.0;
                let mut d = [0u8; 32];
                d.copy_from_slice(&h[..32]);
                for x in &h[32..] {
                    if *x != 0 {
                        panic!(
                            "Failed to convert MARF value into BHH: data stored after 32nd byte"
                        );
                    }
                }
                Self(d)
            }
        }
    };
}

impl_clarity_marf_trie_id!(BurnchainHeaderHash);
impl_clarity_marf_trie_id!(StacksBlockId);
impl_clarity_marf_trie_id!(SortitionId);
#[cfg(test)]
impl_clarity_marf_trie_id!(BlockHeaderHash);

impl MarfTrieId for SortitionId {}
impl MarfTrieId for StacksBlockId {}
impl MarfTrieId for BurnchainHeaderHash {}
#[cfg(test)]
impl MarfTrieId for BlockHeaderHash {}

/// Define the maximum node patching depth when MARF compression is enabled
pub const MAX_PATCH_DEPTH: u32 = 4;

/// Structure that holds the actual data in a MARF leaf node.
/// It only stores the hash of some value string, but we add 8 extra bytes for future extensions.
/// If not used (the rule today), then they should all be 0.
pub struct MARFValue(pub [u8; 40]);
impl_array_newtype!(MARFValue, u8, 40);
impl_array_hexstring_fmt!(MARFValue);
impl_byte_array_newtype!(MARFValue, u8, 40);
impl_byte_array_message_codec!(MARFValue, 40);
pub const MARF_VALUE_ENCODED_SIZE: u32 = 40;

impl From<u32> for MARFValue {
    fn from(value: u32) -> MARFValue {
        let h = value.to_le_bytes();
        let mut d = [0u8; MARF_VALUE_ENCODED_SIZE as usize];
        if h.len() > MARF_VALUE_ENCODED_SIZE as usize {
            panic!("Cannot convert a u32 into a MARF Value.");
        }
        d.get_mut(..h.len())
            .expect("Cannot convert a u32 into a MARF Value")
            .copy_from_slice(&h);
        MARFValue(d)
    }
}

impl<T: MarfTrieId> From<T> for MARFValue {
    fn from(bhh: T) -> MARFValue {
        let h = bhh.to_bytes();
        let mut d = [0u8; MARF_VALUE_ENCODED_SIZE as usize];
        if h.len() > MARF_VALUE_ENCODED_SIZE as usize {
            panic!("Cannot convert a BHH into a MARF Value.");
        }
        d.get_mut(..h.len())
            .expect("Cannot convert a BHH into a MARF Value")
            .copy_from_slice(&h);
        MARFValue(d)
    }
}

impl From<MARFValue> for u32 {
    fn from(m: MARFValue) -> u32 {
        let h = m.0;
        let mut d = [0u8; 4];

        d.copy_from_slice(&h[..4]);

        for h_i in &h[4..] {
            if *h_i != 0 {
                panic!("Failed to convert MARF value into u32: data stored after 4th byte");
            }
        }
        u32::from_le_bytes(d)
    }
}

impl MARFValue {
    /// Construct from a TRIEHASH_ENCODED_SIZE-length slice
    pub fn from_value_hash_bytes(h: &[u8; TRIEHASH_ENCODED_SIZE]) -> MARFValue {
        let mut d = [0u8; MARF_VALUE_ENCODED_SIZE as usize];
        d[..TRIEHASH_ENCODED_SIZE].copy_from_slice(&h[..TRIEHASH_ENCODED_SIZE]);
        MARFValue(d)
    }

    /// Construct from a TrieHash
    pub fn from_value_hash(h: &TrieHash) -> MARFValue {
        MARFValue::from_value_hash_bytes(h.as_bytes())
    }

    /// Construct from a String that encodes a value inserted into the underlying data store
    #[inline]
    pub fn from_value(s: &str) -> MARFValue {
        let mut hasher = TrieHasher::new();
        hasher.update(s.as_bytes());
        let tmp = hasher.finalize().into();

        MARFValue::from_value_hash_bytes(&tmp)
    }

    /// Convert to a byte vector
    pub fn to_vec(&self) -> Vec<u8> {
        self.as_bytes().to_vec()
    }

    /// Extract the value hash from the MARF value
    pub fn to_value_hash(&self) -> TrieHash {
        let mut h = [0u8; TRIEHASH_ENCODED_SIZE];
        h.copy_from_slice(&self.0[0..TRIEHASH_ENCODED_SIZE]);
        TrieHash(h)
    }
}

#[derive(Debug)]
pub enum Error {
    NotOpenedError,
    IOError(io::Error),
    SQLError(rusqlite::Error),
    RequestedIdentifierForExtensionTrie,
    NotFoundError,
    BackptrNotFoundError,
    ExistsError,
    BadSeekValue,
    CorruptionError(String),
    BlockHashMapCorruptionError(Option<Box<Error>>),
    ReadOnlyError,
    UnconfirmedError,
    NotDirectoryError,
    PartialWriteError,
    InProgressError,
    WriteNotBegunError,
    CursorError(node::CursorError),
    RestoreMarfBlockError(Box<Error>),
    NonMatchingForks([u8; 32], [u8; 32]),
    OverflowError,
    Patch(TrieNodePatch),
    NodeTooDeep,
    /// Read at a block strictly below the squash height of a squashed MARF.
    /// The squashed MARF only retains the canonical state at H, so per-block
    /// historical reads in `0..H` cannot be served.
    HistoricalReadInSquashedRange {
        block_height: u32,
        squash_height: u32,
    },
    /// Operation is not supported on a squashed MARF (e.g. proof generation).
    UnsupportedOnSquashedMarf(&'static str),
    /// Operation requires a different `TrieFile` backing. Carries the
    /// operation name.
    UnsupportedTrieFileType(&'static str),
    /// A destination path required to be empty already exists. Carries the
    /// offending path.
    DestinationExists(String),
}

/// A borrowed slice of serialized trie node bytes (e.g. from an mmap'd blob).
///
/// Carries the node type so the decoder knows how to interpret the bytes.
#[derive(Debug, Clone, Copy)]
pub struct BorrowedNodeBytes<'a> {
    node_type: TrieNodeID,
    format: NodeRecordFormat,
    marker: Option<u8>,
    payload: &'a [u8],
}

impl<'a> BorrowedNodeBytes<'a> {
    /// Create from a legacy marker-plus-payload slice, excluding its stored hash.
    pub fn new(node_type: TrieNodeID, bytes: &'a [u8]) -> Self {
        let (marker, payload) = bytes
            .split_first()
            .map_or((None, &[][..]), |(marker, payload)| {
                (Some(*marker), payload)
            });
        Self {
            node_type,
            format: NodeRecordFormat::Legacy,
            marker,
            payload,
        }
    }

    /// Retain a separately parsed marker and mapped payload without a byte copy.
    pub fn from_record(record: record::NodeRecord<'a>) -> Self {
        Self {
            node_type: record.logical_type(),
            format: record.format,
            marker: Some(record.marker),
            payload: record.payload,
        }
    }

    /// The logical node type used by traversal.
    pub fn node_type(&self) -> TrieNodeID {
        self.node_type
    }

    /// Borrow the path directly from a path-first payload.
    pub fn mapped_path(&self) -> Result<Option<&'a [u8]>, Error> {
        if !self.format.is_type_first() {
            return Ok(None);
        }
        if self.marker == Some(TrieNodeID::RawLeaf as u8) {
            return raw_leaf::path(self.payload).map(Some);
        }
        mapped_node::path_prefix(self.payload).map(Some)
    }

    /// Select a child without decoding the full mapped branch.
    pub fn mapped_child(&self, chr: u8) -> Result<Option<Option<TriePtr>>, Error> {
        if !self.format.is_type_first() {
            return Ok(None);
        }
        if self.node_type == TrieNodeID::Leaf {
            return Ok(Some(None));
        }
        BranchView::parse(self.format, self.node_type, self.payload)?
            .child(chr)
            .map(Some)
    }

    /// Borrow a compact leaf's path and copy only its small physical locator.
    pub fn mapped_leaf(&self) -> Result<Option<TrieLeafRef<'a>>, Error> {
        if !self.format.is_type_first() || self.marker != Some(TrieNodeID::ValueLeaf as u8) {
            return Ok(None);
        }
        let path = mapped_node::path_prefix(self.payload)?;
        let mut remaining = self
            .payload
            .get(1 + path.len()..)
            .ok_or(Error::OverflowError)?;
        let extent = ValueExtent::read_from(&mut remaining)?;
        Ok(Some(TrieLeafRef {
            path,
            data: None,
            extent: Some(extent),
            inline: None,
        }))
    }

    /// Decode a borrowed payload without assembling a contiguous marker-plus-body buffer.
    pub fn decode_node(&self) -> Result<TrieNodeType, Error> {
        let marker = self
            .marker
            .ok_or_else(|| Error::CorruptionError("Missing node marker".into()))?;
        let record = record::NodeRecord {
            format: self.format,
            marker,
            hash: None,
            payload: self.payload,
            prefix_len: 0,
        };
        record
            .decode_node(self.node_type as u8)
            .map(|(node, _)| node)
    }
}

/// An owned copy of serialized trie node bytes (e.g. from a seek+read I/O path).
///
/// Same role as [`BorrowedNodeBytes`] but owns its allocation.
#[derive(Debug, Clone)]
pub struct OwnedNodeBytes {
    node_type: TrieNodeID,
    bytes: Vec<u8>,
}

impl OwnedNodeBytes {
    /// Create from a node type and its serialized bytes.
    pub fn new(node_type: TrieNodeID, bytes: Vec<u8>) -> Self {
        Self { node_type, bytes }
    }

    /// The type of trie node these bytes encode.
    pub fn node_type(&self) -> TrieNodeID {
        self.node_type
    }

    /// The raw serialized bytes.
    pub fn bytes(&self) -> &[u8] {
        self.bytes.as_slice()
    }
}

/// Serialized node bytes — either borrowed from an mmap region or owned from a read buffer.
///
/// Provides uniform access to the node type and raw bytes regardless of ownership.
#[derive(Debug, Clone)]
pub enum BytesBacking<'a> {
    /// Zero-copy reference into mmap'd blob data.
    Borrowed(BorrowedNodeBytes<'a>),
    /// Owned buffer from seek+read I/O.
    Owned(OwnedNodeBytes),
}

impl<'a> BytesBacking<'a> {
    /// The type of trie node these bytes encode.
    pub fn node_type(&self) -> TrieNodeID {
        match self {
            BytesBacking::Borrowed(node) => node.node_type(),
            BytesBacking::Owned(node) => node.node_type(),
        }
    }

    /// Decode the logical node, preserving borrowing until decoding is requested.
    pub fn decode_node(&self) -> Result<TrieNodeType, Error> {
        match self {
            Self::Borrowed(node) => node.decode_node(),
            Self::Owned(node) => bits::decode_stable_node_bytes(node.bytes(), node.node_type()),
        }
    }
}

/// The kind of item returned when reading from persisted trie storage: either a fully-resolved node
/// or an unresolved patch that needs to be applied to its base node.
#[derive(Debug, Clone)]
pub enum ReadTrieItemKind<'a> {
    /// A resolved trie node (possibly decoded lazily from bytes).
    Node(ReadTrieNode<'a>),
    /// A patch node referencing a base node in an ancestor block.
    Patch(&'a TrieNodePatch),
}

/// A single item read from trie storage, wrapping either a resolved node or an unresolved patch
/// along with its hash.
#[derive(Debug, Clone)]
pub struct ReadTrieItem<'a> {
    pub hash: Option<TrieHash>,
    pub kind: ReadTrieItemKind<'a>,
}

/// How a read node's data is backed in memory.
///
/// This enum is the core of the lazy-decode / zero-copy strategy: nodes from different sources
/// carry different backing, and decoding is deferred until the caller actually needs structured
/// access (e.g. `walk()`, `ptrs()`).
#[derive(Debug, Clone)]
pub enum ReadNodeBacking<'a> {
    /// Decoded node borrowed from scratch/parking state (not persisted — e.g. the uncommitted
    /// TrieRAM).
    ///
    /// "Volatile" because the reference is invalidated when the scratch slot is reused.
    VolatileDecoded(TrieNodeRef<'a>),
    /// Decoded node borrowed from persisted storage (e.g. already-decoded cache or SQLite inline
    /// blob).
    PersistedDecoded(TrieNodeRef<'a>),
    /// Raw serialized bytes from persisted storage (mmap or read buffer). Decoded lazily on first
    /// access via [`ReadTrieNode::decoded_bytes`].
    PersistedBytes(BytesBacking<'a>),
    /// Fully owned decoded node (e.g. from patch resolution or TrieRAM clone).
    Owned(TrieNodeType),
}

/// A trie node read from storage, with lazy decoding.
///
/// Wraps a [`ReadNodeBacking`] and provides uniform access to node properties (`walk()`, `ptrs()`,
/// `path_bytes()`, `is_leaf()`, etc.) regardless of how the node is backed. When backed by raw
/// bytes ([`ReadNodeBacking::PersistedBytes`]), decoding is deferred until first structural access
/// and cached in `decoded_bytes` via [`OnceLock`].
#[derive(Debug, Clone)]
pub struct ReadTrieNode<'a> {
    /// The node's Merkle hash, if read.
    pub hash: Option<TrieHash>,
    /// Number of patch layers resolved to produce this node (0 if uncompressed).
    pub patch_depth: usize,
    /// The node data — may be decoded, raw bytes, or owned.
    pub backing: ReadNodeBacking<'a>,
    /// Allocated only if an operation needs the fully decoded node; clones share that result.
    decoded_bytes: Option<OnceLock<Arc<TrieNodeType>>>,
    /// Transient metadata (cowptr, patch state) from the source `TrieNodeType` that
    /// `TrieNodeRef` cannot carry. Applied by `into_owned_node()` to round-trip
    /// without loss.
    transient_meta: Option<TrieNodeTransientMeta>,
}

/// Result of a single step during a trie cursor walk.
///
/// Tells the walker what to do next after visiting a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadTrieNodeCursorStep {
    /// Followed a child pointer — continue walking at the returned ptr.
    Next(TriePtr),
    /// Consumed all path bytes — the walk is complete.
    EndOfPath { is_leaf: bool },
    /// Path diverged from the node's compressed path segment — key not found.
    Diverged,
    /// No child exists for the next path byte — key not found.
    ChrNotFound,
    /// Child pointer is a backpointer — caller must resolve it by opening the ancestor block and
    /// re-reading the node there.
    FollowBackptr(TriePtr),
}

/// Reusable byte-buffer and typed-slot decode workspace.
///
/// Implemented by types that can deserialize trie nodes from byte slices into pre-allocated
/// internal storage, avoiding per-read allocation.
pub trait NodeDecodeScratch {
    /// Take ownership of the internal byte buffer (leaves an empty vec behind).
    fn take_node_bytes(&mut self) -> Vec<u8>;
    /// Return a previously-taken byte buffer for reuse.
    fn restore_node_bytes(&mut self, bytes: Vec<u8>);

    /// Decode a node of the given type from a byte slice into internal storage. Returns the number
    /// of bytes consumed.
    fn decode_node_from_slice(&mut self, id: TrieNodeID, bytes: &[u8]) -> Result<usize, Error>;
    /// Decode a marker and payload separated by the physical record envelope.
    fn decode_node_from_parts(
        &mut self,
        id: TrieNodeID,
        marker: u8,
        payload: &[u8],
    ) -> Result<usize, Error>;
    /// Decode a patch with a separately parsed marker.
    fn decode_patch_from_parts(&mut self, marker: u8, payload: &[u8]) -> Result<usize, Error>;

    /// Decode a patch node from a byte slice into internal storage.
    fn decode_patch_from_slice(&mut self, bytes: &[u8]) -> Result<usize, Error>;

    /// Get a borrowed reference to the currently-decoded node.
    fn get_ref(&self) -> TrieNodeRef<'_>;
    /// Get transient metadata for the currently-decoded node, if any.
    fn transient_meta(&self) -> Option<TrieNodeTransientMeta>;
    /// Get the decoded patch node.
    fn patch(&self) -> &TrieNodePatch;
    /// Take ownership of the decoded patch node, leaving the slot empty. Avoids cloning when the
    /// patch will be moved into a collection.
    fn take_patch(&mut self) -> TrieNodePatch;
    /// Take the reusable patch chain buffer (cleared, ready for use).
    fn take_patch_chain_buf(&mut self) -> Vec<PatchChainEntry>;
    /// Return the patch chain buffer for reuse in subsequent calls.
    fn restore_patch_chain_buf(&mut self, buf: Vec<PatchChainEntry>);
    /// Store an owned node as the current node and return a reference.
    fn store(&mut self, node: TrieNodeType) -> TrieNodeRef<'_>;
    /// Clear the current decoded node slot.
    fn clear_current_node(&mut self);
}

/// One patch encountered while chasing a compressed MARF patch chain.
#[derive(Debug)]
pub struct PatchChainEntry {
    pub block_id: u32,
    pub ptr: TriePtr,
    pub patch: TrieNodePatch,
}

/// Parking capability trait: keep decoded nodes alive across multiple storage reads.
///
/// When walking a trie, we often need to read a node, then read another node
/// (which overwrites the decode scratch), while keeping the first node
/// accessible. Parking moves a node into stable storage with a handle
/// for later retrieval.
///
/// `NodeParking` uses a separate parked-node vec that persists across reads, unlike the scratch
/// trait's current-node slot.
pub trait NodeParking: NodeDecodeScratch {
    /// Move the currently-decoded node into parked storage.
    fn park_current_node(&mut self) -> Result<ParkedNodeHandle, Error>;
    /// Park an already-owned node.
    fn park_owned_node(&mut self, node: TrieNodeType) -> ParkedNodeHandle;
    /// Retrieve a reference to a previously-parked node.
    fn get_parked_ref(&self, handle: ParkedNodeHandle) -> TrieNodeRef<'_>;
    /// Clear all parked nodes (e.g., between top-level operations).
    fn clear_parked_nodes(&mut self);
}

/// Patching capability trait: apply compressed patch nodes in-place to decoded nodes.
///
/// MARF compression stores "patch" nodes that record ptr diffs relative to a base node. The
/// patching trait allows the storage layer to resolve a chain of patches by decoding the base node
/// and applying patches in-place.
pub trait NodePatching: NodeDecodeScratch {
    /// Apply a sequence of patches to the currently-decoded node.
    fn apply_patches_in_place(
        &mut self,
        patches: &[PatchChainEntry],
        cur_block_id: u32,
    ) -> Result<(), Error>;
}

/// Read-only access to persisted trie storage.
///
/// Provides block-level positioning (which trie to read from) and node-level I/O (reading node
/// data, hashes, and children). The MARF walk uses this trait to traverse tries across blocks:
/// [`open_block()`](Self::open_block) repositions storage to a specific block's trie, then
/// [`read_node_with_state`](Self::read_node_with_state) reads individual nodes within that trie.
///
/// Backpointer resolution works by calling [`open_block()`](Self::open_block) /
/// [`open_block_known_id()`](Self::open_block_known_id) to jump to an ancestor block, reading the
/// target node there, then continuing the walk. The storage tracks which block is currently open
/// via [`get_cur_block_and_id()`](Self::get_cur_block_and_id).
pub trait TrieReadStorage<T: MarfTrieId>: BlockMap<TrieId = T> {
    /// Read a verified ancestor root directly when an optional fork index supports it.
    fn indexed_ancestor_root(
        &mut self,
        _block: &T,
        _height: u32,
        _target: u32,
    ) -> Result<Option<TrieHash>, Error> {
        Ok(None)
    }

    /// Resolve an ancestor from optional fork metadata; absence requests normal trie traversal.
    fn indexed_ancestor(
        &mut self,
        _block: &T,
        _height: u32,
        _target: u32,
    ) -> Result<Option<T>, Error> {
        Ok(None)
    }

    /// Resolve a leaf only when its logical value is required, leaving locator reads lazy.
    fn resolve_leaf_value(&mut self, leaf: &mut TrieLeaf) -> Result<(), Error> {
        leaf.value().map(|_| ())
    }

    /// Return a complete result only for the owning active mutable trie.
    fn cached_result(&mut self, _block: &T, _path: &TrieHash) -> Option<Option<TrieLeaf>> {
        None
    }

    /// Cache a successful value/absence for the owning active mutable trie.
    fn cache_result(&mut self, _block: &T, _path: TrieHash, _value: Option<TrieLeaf>) {}

    /// Read a node from the currently-open block's trie at the given pointer. The `state` parameter
    /// provides scratch space for node decoding, patch resolution, and parking across reads.
    fn read_node_with_state<'a, S: NodePatching>(
        &'a mut self,
        ptr: &TriePtr,
        state: &'a mut S,
    ) -> Result<ReadTrieNode<'a>, Error>;

    /// Reposition storage to the trie for `bhh`, resolving the block ID from the block-hash-to-ID
    /// mapping. Subsequent node reads will be relative to this block's trie.
    fn open_block(&mut self, bhh: &T) -> Result<(), Error>;

    /// Reposition storage to the trie for `bhh`, using a pre-resolved block ID if available. Falls
    /// back to [`open_block`](Self::open_block) when `id` is `None`.
    fn open_block_maybe_id(&mut self, bhh: &T, id: Option<u32>) -> Result<(), Error> {
        match id {
            Some(id) => self.open_block_known_id(bhh, id),
            None => self.open_block(bhh),
        }
    }

    /// Reposition storage to the trie for `bhh` using a known block ID, avoiding the
    /// block-hash-to-ID lookup.
    ///
    /// Used during backpointer resolution where the `back_block` ID is already available from the
    /// [`TriePtr`].
    fn open_block_known_id(&mut self, bhh: &T, id: u32) -> Result<(), Error>;

    /// Return the block hash of the currently-open trie.
    fn get_cur_block(&self) -> T {
        self.get_cur_block_and_id().0
    }

    /// Return the block hash and numeric ID of the currently-open trie.
    ///
    /// The ID is `None` if the block was opened without a known ID.
    fn get_cur_block_and_id(&self) -> (T, Option<u32>);

    /// Resolve a trie-local numeric block ID to a block hash. Uses the caching [`BlockMap`]
    /// implementation.
    fn get_block_from_local_id(&mut self, local_id: u32) -> Result<T, Error> {
        Ok(self.get_block_hash_caching(local_id)?.clone())
    }

    /// Return a [`TriePtr`] pointing to the root node of the currently-open block's trie.
    ///
    /// This is the starting point for all trie walks.
    fn root_trieptr(&self) -> TriePtr;

    /// Read only the hash of the node at `ptr`, without decoding the node body.
    fn read_node_hash(&mut self, ptr: &TriePtr) -> Result<TrieHash, Error>;

    /// Read the node type ID and hash at `ptr`, without decoding the full node.
    fn read_node_type_id(&mut self, ptr: &TriePtr) -> Result<(TrieNodeID, TrieHash), Error>;

    /// Returns true when this storage represents a squashed MARF.
    fn is_squashed(&self) -> bool {
        false
    }

    /// MARF height at the squash boundary, if this storage is squashed.
    fn squash_height(&self) -> Option<u32> {
        None
    }

    /// Read a historical MARF root hash from squashed side-table metadata.
    fn squashed_block_root_hash_by_height(&self, _height: u32) -> Result<Option<TrieHash>, Error> {
        Ok(None)
    }

    /// Read a historical block height from squashed side-table metadata.
    fn squashed_block_height(&self, _block_hash: &T) -> Result<Option<u32>, Error> {
        Ok(None)
    }

    /// Read a historical block hash from squashed side-table metadata.
    fn squashed_block_hash_by_height(&self, _height: u32) -> Result<Option<T>, Error> {
        Ok(None)
    }

    /// Reject trie traversal below a squashed MARF's retained boundary.
    fn check_historical_read_allowed(&self, _block_hash: &T) -> Result<(), Error> {
        Ok(())
    }

    /// Cache the ancestor hashes for a block (used during proof generation).
    fn set_cached_ancestor_hashes_bytes(&mut self, bhh: &T, bytes: Arc<[TrieHash]>);

    /// Retrieve previously-cached ancestor hashes for a block, if present.
    fn check_cached_ancestor_hashes_bytes(&mut self, bhh: &T) -> Option<Arc<[TrieHash]>>;

    /// Write the hashes of the children pointed to by `ptrs` into `w`.
    ///
    /// Used during Merkle proof construction and root hash computation.
    fn write_children_hashes_by_ptrs<W: io::Write + ?Sized>(
        &mut self,
        ptrs: &[TriePtr],
        w: &mut W,
    ) -> Result<(), Error>;

    /// Return the genesis block hash used in tests, if configured.
    #[cfg(test)]
    fn test_genesis_block(&self) -> Option<T>;
}

/// Bundles a [`TrieReadStorage`] reference with node read scratch for convenient node reading.
///
/// Callers use [`read_node()`](Self::read_node) instead of manually passing state to every
/// [`read_node_with_state()`](TrieReadStorage::read_node_with_state) call.
pub struct TrieReadSession<
    'a,
    T: MarfTrieId,
    S: NodePatching,
    R: TrieReadStorage<T> + ?Sized = TrieStorageConnection<'a, T>,
> {
    storage: &'a mut R,
    state: &'a mut S,
    _marker: std::marker::PhantomData<T>,
}

impl<'a, T: MarfTrieId, S: NodePatching, R: TrieReadStorage<T> + ?Sized>
    TrieReadSession<'a, T, S, R>
{
    pub fn new(storage: &'a mut R, state: &'a mut S) -> Self {
        Self {
            storage,
            state,
            _marker: std::marker::PhantomData,
        }
    }

    /// Access the underlying storage (e.g. for [`open_block()`](TrieReadStorage::open_block)
    /// calls).
    pub fn storage(&mut self) -> &mut R {
        self.storage
    }

    /// Read a node at `ptr` from the currently-open block, using the session's decode state for
    /// scratch space and patch resolution.
    pub fn read_node<'b>(&'b mut self, ptr: &TriePtr) -> Result<ReadTrieNode<'b>, Error> {
        self.storage.read_node_with_state(ptr, self.state)
    }

    /// Walk the current trie from the root until the path reaches a leaf or crosses a backptr.
    pub fn walk_to_leaf_or_backptr(
        &mut self,
        path: &TrieHash,
        cursor: &mut TrieCursor<T>,
    ) -> Result<TrieLeafOrBackptr, Error> {
        let root_ptr = self.storage.root_trieptr();
        cursor.reset(path, root_ptr);
        let mut node_ptr = root_ptr;

        for _ in 0..(cursor.path.len() + 1) {
            let cur_block = self.storage.get_cur_block();
            let read = self.read_node(&node_ptr)?;
            match cursor.walk_read(&read, &cur_block) {
                Ok(Some(next_ptr)) => {
                    node_ptr = next_ptr;
                    continue;
                }
                Ok(None) => {
                    if clear_backptr(cursor.ptr().id()) != TrieNodeID::Leaf as u8 {
                        return Err(Error::CorruptionError(
                            "Non-leaf encountered at end of path".to_string(),
                        ));
                    }

                    let leaf = read.as_leaf()?.ok_or_else(|| {
                        Error::CorruptionError("Path reached a non-leaf".to_string())
                    })?;
                    return Ok(TrieLeafOrBackptr::Leaf {
                        ptr: node_ptr,
                        leaf: leaf.to_owned(),
                    });
                }
                Err(Error::CursorError(CursorError::PathDiverged))
                | Err(Error::CursorError(CursorError::ChrNotFound)) => {
                    return Err(Error::NotFoundError);
                }
                Err(Error::CursorError(CursorError::BackptrEncountered(ptr))) => {
                    if !is_backptr(ptr.id()) {
                        return Err(Error::CorruptionError(format!(
                            "Failed to walk 0x{:02x} -- got non-backptr",
                            ptr.chr()
                        )));
                    }

                    return Ok(TrieLeafOrBackptr::Backptr(ptr));
                }
                Err(e) => return Err(e),
            }
        }

        Err(Error::CorruptionError("Trie has a cycle".to_string()))
    }
}

pub enum TrieLeafOrBackptr {
    Leaf { ptr: TriePtr, leaf: TrieLeaf },
    Backptr(TriePtr),
}

impl<'a> ReadTrieItem<'a> {
    /// Wrap a resolved node as a read item.
    pub fn from_node(node: ReadTrieNode<'a>) -> Self {
        Self {
            hash: node.hash,
            kind: ReadTrieItemKind::Node(node),
        }
    }

    /// Wrap an unresolved patch as a read item.
    pub fn from_patch(patch: &'a TrieNodePatch, hash: Option<TrieHash>) -> Self {
        Self {
            hash,
            kind: ReadTrieItemKind::Patch(patch),
        }
    }

    /// Unwrap into a resolved node, or return `Error::Patch` if this is an unresolved patch (caller
    /// must resolve it first).
    pub fn into_node(self) -> Result<ReadTrieNode<'a>, Error> {
        match self.kind {
            ReadTrieItemKind::Node(node) => Ok(node),
            ReadTrieItemKind::Patch(patch) => Err(Error::Patch(patch.clone())),
        }
    }
}

impl<'a> ReadTrieNode<'a> {
    fn new(hash: Option<TrieHash>, patch_depth: usize, backing: ReadNodeBacking<'a>) -> Self {
        let decoded_bytes =
            matches!(&backing, ReadNodeBacking::PersistedBytes(_)).then(OnceLock::new);
        Self {
            hash,
            patch_depth,
            backing,
            decoded_bytes,
            transient_meta: None,
        }
    }

    /// Attach transient metadata (cowptr, patch state) captured from the source TrieNodeType.
    pub fn with_transient_meta(mut self, meta: TrieNodeTransientMeta) -> Self {
        self.transient_meta = Some(meta);
        self
    }

    fn patch_depth_from_owned(node: &TrieNodeType) -> usize {
        node.patch_depth()
    }

    /// Construct from a decoded persisted node (e.g. from SQLite inline blob or cache).
    pub fn from_borrowed(node: TrieNodeRef<'a>, hash: Option<TrieHash>) -> Self {
        Self::new(hash, 0, ReadNodeBacking::PersistedDecoded(node))
    }

    /// Construct from a decoded node in scratch/parking state (volatile — may be overwritten on the
    /// next read).
    pub fn from_state_borrowed(node: TrieNodeRef<'a>, hash: Option<TrieHash>) -> Self {
        Self::new(hash, 0, ReadNodeBacking::VolatileDecoded(node))
    }

    /// Construct from borrowed raw bytes (zero-copy, e.g. mmap).
    pub fn from_stable_bytes(node: BorrowedNodeBytes<'a>, hash: Option<TrieHash>) -> Self {
        Self::new(
            hash,
            0,
            ReadNodeBacking::PersistedBytes(BytesBacking::Borrowed(node)),
        )
    }

    /// Construct from owned raw bytes (e.g. seek+read I/O buffer).
    pub fn from_owned_bytes(node: OwnedNodeBytes, hash: Option<TrieHash>) -> Self {
        Self::new(
            hash,
            0,
            ReadNodeBacking::PersistedBytes(BytesBacking::Owned(node)),
        )
    }

    /// Construct from a fully-owned decoded node (e.g. from patch resolution or TrieRAM clone).
    pub fn from_owned(node: TrieNodeType, hash: Option<TrieHash>) -> Self {
        let patch_depth = Self::patch_depth_from_owned(&node);
        Self::new(hash, patch_depth, ReadNodeBacking::Owned(node))
    }

    /// Return the node type ID, if determinable.
    pub fn node_type(&self) -> Option<TrieNodeID> {
        match &self.backing {
            ReadNodeBacking::VolatileDecoded(node) => TrieNodeID::from_u8(node.id()),
            ReadNodeBacking::PersistedDecoded(node) => TrieNodeID::from_u8(node.id()),
            ReadNodeBacking::PersistedBytes(node) => Some(node.node_type()),
            ReadNodeBacking::Owned(node) => TrieNodeID::from_u8(node.id()),
        }
    }

    /// Return the node type ID as a raw `u8` (includes control bits).
    pub fn node_type_u8(&self) -> u8 {
        match &self.backing {
            ReadNodeBacking::VolatileDecoded(node) => node.id(),
            ReadNodeBacking::PersistedDecoded(node) => node.id(),
            ReadNodeBacking::PersistedBytes(node) => node.node_type() as u8,
            ReadNodeBacking::Owned(node) => node.id(),
        }
    }

    /// Returns `true` if this is a Node256 (the largest intermediate node type).
    pub fn is_node256(&self) -> bool {
        matches!(self.node_type(), Some(TrieNodeID::Node256))
    }

    /// Decode a [`BytesBacking`] node into an owned [`TrieNodeType`].
    ///
    /// Borrowed bytes (mmap) may be a prefix slice — uses prefix decode. Owned bytes use
    /// exact-length decode with validation.
    fn decode_bytes_to_node(&self, node: &BytesBacking<'_>) -> Result<TrieNodeType, Error> {
        node.decode_node()
    }

    /// Return a reference to the decoded node, decoding from bytes on first call and caching the
    /// result in `decoded_bytes`.
    fn decoded_from_bytes(&self, node: &BytesBacking<'_>) -> Result<&TrieNodeType, Error> {
        let decoded_bytes = self.decoded_bytes.as_ref().ok_or_else(|| {
            Error::CorruptionError(
                "Missing decoded-byte cache for byte-backed trie node".to_string(),
            )
        })?;
        if let Some(decoded) = decoded_bytes.get() {
            return Ok(decoded);
        }

        let decoded = self.decode_bytes_to_node(node)?;
        let _ = decoded_bytes.set(Arc::new(decoded));
        decoded_bytes.get().map(Arc::as_ref).ok_or_else(|| {
            Error::CorruptionError(format!(
                "Failed to cache decoded stable byte-backed {:?} node",
                node.node_type()
            ))
        })
    }

    /// Set the patch depth (number of resolved patch layers) on this node.
    pub fn with_patch_depth(mut self, patch_depth: usize) -> Self {
        self.patch_depth = patch_depth;
        self
    }

    /// Returns `true` if this node is a leaf (end of a key path).
    pub fn is_leaf(&self) -> Result<bool, Error> {
        match &self.backing {
            ReadNodeBacking::VolatileDecoded(node) => Ok(node.is_leaf()),
            ReadNodeBacking::PersistedDecoded(node) => Ok(node.is_leaf()),
            ReadNodeBacking::PersistedBytes(node) => Ok(node.node_type() == TrieNodeID::Leaf),
            ReadNodeBacking::Owned(node) => Ok(node.is_leaf()),
        }
    }

    /// Return the node's compressed path segment bytes.
    pub fn path_bytes(&self) -> Result<&[u8], Error> {
        match &self.backing {
            ReadNodeBacking::VolatileDecoded(node) => Ok(node.path_bytes()),
            ReadNodeBacking::PersistedDecoded(node) => Ok(node.path_bytes()),
            ReadNodeBacking::PersistedBytes(node) => {
                if let BytesBacking::Borrowed(bytes) = node {
                    if let Some(path) = bytes.mapped_path()? {
                        return Ok(path);
                    }
                }
                Ok(self.decoded_from_bytes(node)?.path_bytes())
            }
            ReadNodeBacking::Owned(node) => Ok(node.path_bytes()),
        }
    }

    /// Return the node's child pointer array.
    pub fn ptrs(&self) -> Result<&[TriePtr], Error> {
        match &self.backing {
            ReadNodeBacking::VolatileDecoded(node) => Ok(node.ptrs()),
            ReadNodeBacking::PersistedDecoded(node) => Ok(node.ptrs()),
            ReadNodeBacking::PersistedBytes(node) => Ok(self.decoded_from_bytes(node)?.ptrs()),
            ReadNodeBacking::Owned(node) => Ok(node.ptrs()),
        }
    }

    /// Follow a path byte to the corresponding child pointer, if present.
    pub fn walk(&self, chr: u8) -> Result<Option<TriePtr>, Error> {
        match &self.backing {
            ReadNodeBacking::VolatileDecoded(node) => Ok(node.walk(chr)),
            ReadNodeBacking::PersistedDecoded(node) => Ok(node.walk(chr)),
            ReadNodeBacking::PersistedBytes(node) => {
                if let BytesBacking::Borrowed(bytes) = node {
                    if let Some(child) = bytes.mapped_child(chr)? {
                        return Ok(child);
                    }
                }
                Ok(self.decoded_from_bytes(node)?.walk(chr))
            }
            ReadNodeBacking::Owned(node) => Ok(node.walk(chr)),
        }
    }

    /// Borrow leaf data if this node is a leaf, without taking ownership.
    pub fn as_leaf(&self) -> Result<Option<TrieLeafRef<'_>>, Error> {
        match &self.backing {
            ReadNodeBacking::VolatileDecoded(node) => Ok(node.as_leaf()),
            ReadNodeBacking::PersistedDecoded(node) => Ok(node.as_leaf()),
            ReadNodeBacking::PersistedBytes(node) => {
                if node.node_type() != TrieNodeID::Leaf {
                    return Ok(None);
                }
                if let BytesBacking::Borrowed(bytes) = node {
                    if let Some(leaf) = bytes.mapped_leaf()? {
                        return Ok(Some(leaf));
                    }
                }
                Ok(match self.decoded_from_bytes(node)? {
                    TrieNodeType::Leaf(leaf) => Some(TrieLeafRef {
                        path: leaf.path.as_slice(),
                        data: leaf.data.as_ref(),
                        extent: leaf.extent,
                        inline: leaf.inline.as_ref(),
                    }),
                    _ => None,
                })
            }
            ReadNodeBacking::Owned(node) => Ok(TrieNodeRef::from(node).as_leaf()),
        }
    }

    /// Borrow as a `TrieNodeRef` + hash pair (triggers decode if byte-backed).
    pub fn as_node_ref(&self) -> Result<(TrieNodeRef<'_>, Option<TrieHash>), Error> {
        match &self.backing {
            ReadNodeBacking::VolatileDecoded(node) => Ok((*node, self.hash)),
            ReadNodeBacking::PersistedDecoded(node) => Ok((*node, self.hash)),
            ReadNodeBacking::PersistedBytes(node) => {
                Ok((TrieNodeRef::from(self.decoded_from_bytes(node)?), self.hash))
            }
            ReadNodeBacking::Owned(node) => Ok((TrieNodeRef::from(node), self.hash)),
        }
    }

    /// Consume this read node and return an owned [`TrieNodeType`] + hash.
    ///
    /// If transient metadata (cowptr, patch state) was captured when the `ReadTrieNode` was
    /// constructed, it is applied to the owned node so that round-tripping through [`TrieNodeRef`]
    /// doesn't lose COW/patch state.
    pub fn into_owned_node(self) -> Result<(TrieNodeType, Option<TrieHash>), Error> {
        let meta = self.transient_meta;
        let mut node = match self.backing {
            ReadNodeBacking::VolatileDecoded(node) => node.to_owned_node(),
            ReadNodeBacking::PersistedDecoded(node) => node.to_owned_node(),
            ReadNodeBacking::PersistedBytes(ref node) => self.decode_bytes_to_node(node)?,
            ReadNodeBacking::Owned(node) => node,
        };
        if let Some(meta) = meta {
            meta.apply_to(&mut node);
        }
        Ok((node, self.hash))
    }

    /// Consume this read node and return only the hash, discarding node data.
    pub fn into_hash(self) -> Result<Option<TrieHash>, Error> {
        match self.backing {
            ReadNodeBacking::VolatileDecoded(_)
            | ReadNodeBacking::PersistedDecoded(_)
            | ReadNodeBacking::PersistedBytes(_)
            | ReadNodeBacking::Owned(_) => Ok(self.hash),
        }
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Error::IOError(err)
    }
}

impl From<rusqlite::Error> for Error {
    fn from(err: rusqlite::Error) -> Self {
        if let rusqlite::Error::QueryReturnedNoRows = err {
            Error::NotFoundError
        } else {
            Error::SQLError(err)
        }
    }
}

impl From<db_error> for Error {
    fn from(e: db_error) -> Error {
        match e {
            db_error::SqliteError(se) => Error::SQLError(se),
            db_error::NotFoundError => Error::NotFoundError,
            _ => Error::CorruptionError(format!("{}", &e)),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Error::IOError(ref e) => fmt::Display::fmt(e, f),
            Error::SQLError(ref e) => fmt::Display::fmt(e, f),
            Error::CorruptionError(ref s) => fmt::Display::fmt(s, f),
            Error::CursorError(ref e) => fmt::Display::fmt(e, f),
            Error::BlockHashMapCorruptionError(ref opt_e) => {
                f.write_str("Corrupted MARF BlockHashMap")?;
                match opt_e {
                    Some(e) => write!(f, ": {}", e),
                    None => Ok(()),
                }
            }
            Error::NotOpenedError => write!(f, "Tried to read data from unopened storage"),
            Error::NotFoundError => write!(f, "Object not found"),
            Error::BackptrNotFoundError => write!(f, "Object not found from backptrs"),
            Error::ExistsError => write!(f, "Object exists"),
            Error::BadSeekValue => write!(f, "Bad seek value"),
            Error::ReadOnlyError => write!(f, "Storage is in read-only mode"),
            Error::UnconfirmedError => write!(f, "Storage is in unconfirmed mode"),
            Error::NotDirectoryError => write!(f, "Not a directory"),
            Error::PartialWriteError => {
                write!(f, "Data is partially written and not yet recovered")
            }
            Error::InProgressError => write!(f, "Write was in progress"),
            Error::WriteNotBegunError => write!(f, "Write has not begun"),
            Error::RestoreMarfBlockError(_) => write!(
                f,
                "Failed to restore previous open block during block header check"
            ),
            Error::NonMatchingForks(_, _) => {
                write!(f, "The supplied blocks are not in the same fork")
            }
            Error::RequestedIdentifierForExtensionTrie => {
                write!(f, "BUG: MARF requested the identifier for a RAM trie")
            }
            Error::OverflowError => write!(f, "Overflow"),
            Error::Patch(ref p) => {
                write!(f, "Read patch node instead of expected node: {p:?}")
            }
            Error::NodeTooDeep => write!(f, "Node is too deeply buried under patches"),
            Error::HistoricalReadInSquashedRange {
                block_height,
                squash_height,
            } => write!(
                f,
                "Historical read at height {block_height} below squash height {squash_height} \
                 is not supported on a squashed MARF"
            ),
            Error::UnsupportedOnSquashedMarf(op) => {
                write!(f, "Operation `{op}` is not supported on a squashed MARF")
            }
            Error::UnsupportedTrieFileType(op) => {
                write!(
                    f,
                    "Operation `{op}` is not supported by this TrieFile backing"
                )
            }
            Error::DestinationExists(ref p) => {
                write!(f, "Destination path already exists: {p}")
            }
        }
    }
}

impl error::Error for Error {
    fn cause(&self) -> Option<&dyn error::Error> {
        match *self {
            Error::IOError(ref e) => Some(e),
            Error::SQLError(ref e) => Some(e),
            Error::RestoreMarfBlockError(ref e) => Some(e),
            Error::BlockHashMapCorruptionError(Some(ref e)) => Some(e),
            _ => None,
        }
    }
}

pub trait BlockMap {
    type TrieId: MarfTrieId;
    fn get_block_hash(&self, id: u32) -> Result<Self::TrieId, Error>;
    fn get_block_hash_caching(&mut self, id: u32) -> Result<&Self::TrieId, Error>;
    fn is_block_hash_cached(&self, id: u32) -> bool;
    fn get_block_id(&self, bhh: &Self::TrieId) -> Result<u32, Error>;
    fn get_block_id_caching(&mut self, bhh: &Self::TrieId) -> Result<u32, Error>;
}

#[cfg(test)]
impl BlockMap for () {
    type TrieId = BlockHeaderHash;
    fn get_block_hash(&self, _id: u32) -> Result<BlockHeaderHash, Error> {
        Err(Error::NotFoundError)
    }
    fn get_block_hash_caching(&mut self, _id: u32) -> Result<&BlockHeaderHash, Error> {
        Err(Error::NotFoundError)
    }
    fn is_block_hash_cached(&self, _id: u32) -> bool {
        false
    }
    fn get_block_id(&self, _bhh: &BlockHeaderHash) -> Result<u32, Error> {
        Err(Error::NotFoundError)
    }
    fn get_block_id_caching(&mut self, _bhh: &BlockHeaderHash) -> Result<u32, Error> {
        Err(Error::NotFoundError)
    }
}

/// Tables owned by the MARF, excluded from side-store copies.
pub const MARF_SQLITE_TABLES: &[&str] = &[
    "marf_data",
    "marf_record_format",
    "__fork_storage",
    "marf_squash_info",
    "marf_squashed_blocks",
    "mined_blocks",
    "block_extension_locks",
    "schema_version",
    "migrated_version",
];
