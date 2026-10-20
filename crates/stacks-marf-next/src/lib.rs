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

//! The Merkleized Adaptive Radix Forest (MARF): the authenticated key/value
//! store backing the Stacks chainstate, sortition and Clarity databases.
//!
//! This crate is an independent reimplementation of the MARF that preserves
//! the consensus hashing rules, the Merkle proof format and the on-disk
//! storage formats of the original implementation (which lives in
//! `stackslib::chainstate::stacks::index` for the time being). See
//! `docs/marf-rewrite/` for the behavioural contract and design notes.
//!
//! Module map:
//!
//! * [`node`]: the logical trie node model (`Leaf`, `Branch`, `Ptr`, `Path`).
//! * [`hash`]: consensus hashing of nodes and MARF roots.
//! * [`codec`]: the legacy on-disk node encoding (plain, compressed, patches).
//! * [`trie`]: the in-memory trie under construction for one block.
//! * [`storage`]: SQLite + blob-file persistence of committed tries.
//! * [`marf`]: the forest: blocks, forks, lookups, commits, ancestry.
//! * [`proofs`]: Merkle proofs of inclusion.
//! * [`pruning`]: collapsing history into a single trie at a height.

#[allow(unused_imports)]
#[macro_use(o, slog_log, slog_trace, slog_debug, slog_info, slog_warn, slog_error)]
extern crate slog;

#[macro_use]
extern crate stacks_common;

#[macro_use]
extern crate serde_derive;

use std::hash::Hash;
use std::{error, fmt, io};

use sha2::{Digest, Sha512_256};
use stacks_common::types::chainstate::{
    BurnchainHeaderHash, SortitionId, StacksBlockId, TrieHash, TRIEHASH_ENCODED_SIZE,
};

/// Bump a [`stats::Counters`] field on the current thread; a no-op unless
/// the `counters` feature is on.
macro_rules! count {
    ($field:ident) => {
        #[cfg(feature = "counters")]
        $crate::stats::update(|c| c.$field += 1);
    };
    ($field:ident, $n:expr) => {
        #[cfg(feature = "counters")]
        $crate::stats::update(|c| c.$field += u64::from($n));
    };
}

pub mod codec;
pub mod compat;
pub mod hash;
pub mod marf;
pub mod node;
pub mod proofs;
pub mod pruning;
pub mod stats;
pub mod storage;
pub mod trie;

pub use crate::compat::{file, trie_sql, MarfDataEntry};
pub use crate::marf::{MARFOpenOpts, MarfConnection, MarfTransaction, MARF};
#[cfg(any(test, feature = "testing"))]
pub use crate::marf::WalkProfile;
pub use crate::proofs::{
    ProofTrieNode, ProofTriePtr, TrieLeaf, TrieMerkleProof, TrieMerkleProofType,
};

/// Types that can identify a block (and therefore a trie) in a MARF.
pub trait ClarityMarfTrieId:
    PartialEq + Clone + std::fmt::Display + std::fmt::Debug + std::convert::From<[u8; 32]>
{
    fn as_bytes(&self) -> &[u8];
    fn to_bytes(self) -> [u8; 32];
    fn from_bytes(from: [u8; 32]) -> Self;
    fn sentinel() -> Self;
}

/// Block identifiers usable as trie identifiers in persistent storage.
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
                Self(SENTINEL_ARRAY)
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

impl MarfTrieId for SortitionId {}
impl MarfTrieId for StacksBlockId {}
impl MarfTrieId for BurnchainHeaderHash {}

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
        let mut d = [0u8; MARF_VALUE_ENCODED_SIZE as usize];
        d[..4].copy_from_slice(&value.to_le_bytes());
        MARFValue(d)
    }
}

impl<T: MarfTrieId> From<T> for MARFValue {
    fn from(bhh: T) -> MARFValue {
        let mut d = [0u8; MARF_VALUE_ENCODED_SIZE as usize];
        d[..32].copy_from_slice(&bhh.to_bytes());
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
        d[..TRIEHASH_ENCODED_SIZE].copy_from_slice(h);
        MARFValue(d)
    }

    /// Construct from a TrieHash
    pub fn from_value_hash(h: &TrieHash) -> MARFValue {
        MARFValue::from_value_hash_bytes(h.as_bytes())
    }

    /// Construct from a String that encodes a value inserted into the underlying data store
    pub fn from_value(s: &str) -> MARFValue {
        let mut hasher = Sha512_256::new();
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
    NotFoundError,
    ExistsError,
    CorruptionError(String),
    ReadOnlyError,
    UnconfirmedError,
    InProgressError,
    WriteNotBegunError,
    RestoreMarfBlockError(Box<Error>),
    NonMatchingForks([u8; 32], [u8; 32]),
    OverflowError,
    NodeTooDeep,
    /// Read at a block strictly below the prune height of a pruned MARF.
    HistoricalReadInPrunedRange {
        block_height: u32,
        prune_height: u32,
    },
    /// Operation is not supported on a pruned MARF (e.g. proof generation).
    UnsupportedOnPrunedMarf(&'static str),
    /// A destination path required to be empty already exists.
    DestinationExists(String),
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

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Error::IOError(ref e) => fmt::Display::fmt(e, f),
            Error::SQLError(ref e) => fmt::Display::fmt(e, f),
            Error::CorruptionError(ref s) => fmt::Display::fmt(s, f),
            Error::NotOpenedError => write!(f, "Tried to read data from unopened storage"),
            Error::NotFoundError => write!(f, "Object not found"),
            Error::ExistsError => write!(f, "Object exists"),
            Error::ReadOnlyError => write!(f, "Storage is in read-only mode"),
            Error::UnconfirmedError => write!(f, "Storage is in unconfirmed mode"),
            Error::InProgressError => write!(f, "Write was in progress"),
            Error::WriteNotBegunError => write!(f, "Write has not begun"),
            Error::RestoreMarfBlockError(_) => write!(
                f,
                "Failed to restore previous open block during block header check"
            ),
            Error::NonMatchingForks(_, _) => {
                write!(f, "The supplied blocks are not in the same fork")
            }
            Error::OverflowError => write!(f, "Overflow"),
            Error::NodeTooDeep => write!(f, "Node is too deeply buried under patches"),
            Error::HistoricalReadInPrunedRange {
                block_height,
                prune_height,
            } => write!(
                f,
                "Historical read at height {block_height} below prune height {prune_height} \
                 is not supported on a pruned MARF"
            ),
            Error::UnsupportedOnPrunedMarf(op) => {
                write!(f, "Operation `{op}` is not supported on a pruned MARF")
            }
            Error::DestinationExists(ref p) => {
                write!(f, "Destination path already exists: {p}")
            }
        }
    }
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match *self {
            Error::IOError(ref e) => Some(e),
            Error::SQLError(ref e) => Some(e),
            Error::RestoreMarfBlockError(ref e) => Some(e),
            _ => None,
        }
    }
}

/// A convenience for `Error::CorruptionError(format!(..))`.
#[macro_export]
macro_rules! corrupt {
    ($($arg:tt)*) => { $crate::Error::CorruptionError(format!($($arg)*)) };
}
