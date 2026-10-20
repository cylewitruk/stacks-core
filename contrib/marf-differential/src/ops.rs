//! The operation / observation vocabulary shared by every backend.
//!
//! Everything here is plain data so that a sequence recorded against one
//! implementation can be replayed against another without sharing code.
//! Block ids and values are lower-case hex strings.

use serde::{Deserialize, Serialize};

/// 32-byte block identifier, hex encoded.
pub type BlockHex = String;
/// 40-byte MARF value, hex encoded.
pub type ValueHex = String;

/// How a MARF is opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenOpts {
    pub external_blobs: bool,
    pub compress: bool,
    pub defer_hashing: bool,
    pub unconfirmed: bool,
}

impl Default for OpenOpts {
    fn default() -> Self {
        OpenOpts {
            external_blobs: true,
            compress: true,
            defer_hashing: true,
            unconfirmed: false,
        }
    }
}

/// One externally observable MARF operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Op {
    /// (Re)open the MARF at the backend's path. Closes any open handle first.
    Open(OpenOpts),
    /// Close the handle (drops any open transaction, which rolls it back).
    Close,
    /// Begin building `child` on top of `parent` (the sentinel for genesis)
    /// inside a transaction; following ops run through that transaction
    /// until it is committed or dropped.
    Begin { parent: BlockHex, child: BlockHex },
    /// Begin building `child` on top of `parent` in a transaction that is
    /// committed immediately, leaving the trie open on the handle; following
    /// write ops use the handle-level API (`MARF::insert`, `MARF::commit`, ...).
    BeginHandle { parent: BlockHex, child: BlockHex },
    /// Begin an unconfirmed trie on top of a confirmed `parent`.
    BeginUnconfirmed { parent: BlockHex },
    /// Insert into the trie being built.
    Insert { key: String, value: ValueHex },
    /// Batch insert into the trie being built.
    InsertBatch { entries: Vec<(String, ValueHex)> },
    /// Seal the trie being built; observes the root hash.
    Seal,
    /// Commit the trie being built under the block id it was begun with.
    Commit,
    /// Commit the trie being built under a different block id.
    CommitTo { block: BlockHex },
    /// Commit the trie being built into the mined-blocks table.
    CommitMined { block: BlockHex },
    /// Abandon the trie being built.
    DropCurrent,
    /// Abandon and delete unconfirmed state.
    DropUnconfirmed,
    /// Look up `key` as of `tip`.
    Get { tip: BlockHex, key: String },
    /// Look up `key` as of `tip` and produce a proof; the proof is verified
    /// against the root hash at `tip` and the root-to-block map of all
    /// committed blocks.
    GetWithProof { tip: BlockHex, key: String },
    /// Generate the proof for `key` at `tip`, apply `tamper` to its serialized
    /// bytes, deserialize and verify. Observes `Bool(verified)`, or
    /// `Err("Deserialize")` if the tampered bytes no longer parse.
    VerifyProof {
        tip: BlockHex,
        key: String,
        tamper: Tamper,
    },
    /// MARF root hash at `tip`.
    RootHash { tip: BlockHex },
    /// Block at `height` on the fork ending at `tip`.
    BlockAtHeight { tip: BlockHex, height: u32 },
    /// Height of `block` on the fork ending at `tip`.
    BlockHeight { tip: BlockHex, block: BlockHex },
    /// Whether `block` is an ancestor of `tip`.
    CheckAncestor { tip: BlockHex, block: BlockHex },
    /// Whether the open chain tip (if any) and its height.
    OpenTip,
    /// Prune the MARF at `height` on `tip`'s fork into a sibling directory,
    /// which becomes the active database directory (the handle is closed).
    Prune { tip: BlockHex, height: u32 },
}

/// A mutation applied to a serialized proof before verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tamper {
    None,
    /// XOR the byte at this index (modulo the length) with 0x01.
    FlipByte(usize),
    /// Keep only the first `n` bytes.
    Truncate(usize),
    /// Verify against a different value than the proof carries.
    WrongValue,
    /// Verify against a different root hash.
    WrongRoot,
    /// Verify with an empty root-to-block map.
    EmptyBlockMap,
}

/// The externally observable result of an [`Op`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Observation {
    Ok,
    /// The operation failed; carries a stable error kind name.
    Err(String),
    Value(Option<ValueHex>),
    Hash(String),
    Block(Option<BlockHex>),
    Height(Option<u32>),
    /// Value found, hex-serialized proof, and whether the proof verified.
    Proof {
        value: ValueHex,
        proof: String,
        verified: bool,
    },
    /// The open tip and its height.
    Tip(Option<(BlockHex, u32)>),
    Bool(bool),
}

/// A recorded fixture: the sequence plus what the reference produced.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fixture {
    pub name: String,
    pub seed: u64,
    pub ops: Vec<Op>,
    pub expected: Vec<Observation>,
}

/// The sentinel block id (all 0xff), as every `MarfTrieId` defines it.
pub fn sentinel_hex() -> BlockHex {
    "ff".repeat(32)
}

/// A deterministic block id from a small integer.
pub fn block_hex(n: u32) -> BlockHex {
    let mut bytes = [0u8; 32];
    bytes[..4].copy_from_slice(&n.to_be_bytes());
    bytes[31] = 0x42;
    stacks_common::util::hash::to_hex(&bytes)
}

/// A 40-byte value hex from an arbitrary string, hashed the way
/// `MARFValue::from_value` does (SHA-512/256 of the string, zero padded).
pub fn value_hex_from_str(s: &str) -> ValueHex {
    use stacks_common::util::hash::Sha512Trunc256Sum;
    let h = Sha512Trunc256Sum::from_data(s.as_bytes());
    let mut bytes = [0u8; 40];
    bytes[..32].copy_from_slice(h.as_bytes());
    stacks_common::util::hash::to_hex(&bytes)
}
