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

//! Stacks integration for the SQLite-backed MARF engine and its proof/pruning tools.
#[allow(unused_imports)]
#[macro_use(o, slog_log, slog_trace, slog_debug, slog_info, slog_warn, slog_error)]
extern crate slog;
#[macro_use]
extern crate stacks_common;
use std::hash::Hash;

pub use marf_core::{
    Error, MARF_VALUE_ENCODED_SIZE, MARFValue, access, codec, corrupt, hash, node, stats, trie,
};
use stacks_common::types::chainstate::{BurnchainHeaderHash, SortitionId, StacksBlockId, TrieHash};
pub mod archive;
pub mod compat;
pub mod marf;
mod observer;
#[cfg(feature = "profiler")]
pub mod profiler;
pub mod proofs;
pub mod pruning;
pub mod sqlite;
pub use compat::{MarfDataEntry, file, trie_sql};
#[cfg(any(test, feature = "testing"))]
pub use marf::WalkProfile;
pub use marf::{MARF, MARFOpenOpts, MarfConnection, MarfTransaction};
pub use proofs::{ProofTrieNode, ProofTriePtr, TrieLeaf, TrieMerkleProof, TrieMerkleProofType};
/// Legacy storage names retained for integration consumers.
pub mod storage {
    pub use marf_core::storage::{Blobs, PATCH_CACHE_CAPACITY};

    pub use crate::archive::PruneInfo;
    pub use crate::marf::{TrieFileStorage, TrieHashCalculationMode};
}
/// Existing Stacks identifier integration contract.
pub trait ClarityMarfTrieId:
    PartialEq + Clone + std::fmt::Display + std::fmt::Debug + From<[u8; 32]>
{
    fn as_bytes(&self) -> &[u8];
    fn to_bytes(self) -> [u8; 32];
    fn from_bytes(from: [u8; 32]) -> Self;
    fn sentinel() -> Self;
}
/// Identifiers accepted by the Stacks facade and legacy SQL/transport helpers.
pub trait MarfTrieId:
    ClarityMarfTrieId
    + rusqlite::types::ToSql
    + rusqlite::types::FromSql
    + stacks_common::codec::StacksMessageCodec
    + Eq
    + Hash
{
}
/// Sentinel identifying the parent before the first trie.
pub const SENTINEL_ARRAY: [u8; 32] = [255; 32];
macro_rules! impl_id {
    ($t:ident) => {
        impl ClarityMarfTrieId for $t {
            fn as_bytes(&self) -> &[u8] {
                self.as_ref()
            }
            fn to_bytes(self) -> [u8; 32] {
                self.0
            }
            fn from_bytes(b: [u8; 32]) -> Self {
                Self(b)
            }
            fn sentinel() -> Self {
                Self(SENTINEL_ARRAY)
            }
        }
        impl MarfTrieId for $t {}
    };
}
impl_id!(StacksBlockId);
impl_id!(SortitionId);
impl_id!(BurnchainHeaderHash);
/// Fixed-size identity adapter; does not impose SQL or transport bounds on core.
#[repr(transparent)]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Id<T>(pub T);
impl<T: MarfTrieId> std::fmt::Display for Id<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}
impl<T: MarfTrieId> marf_core::TrieId for Id<T> {
    fn as_bytes(&self) -> &[u8; 32] {
        self.0
            .as_bytes()
            .try_into()
            .expect("MarfTrieId has a fixed 32-byte identity")
    }
    fn from_bytes(b: [u8; 32]) -> Self {
        Self(T::from_bytes(b))
    }
    fn sentinel() -> Self {
        Self(T::sentinel())
    }
}
fn core_hash(hash: &TrieHash) -> marf_core::TrieHash {
    marf_core::TrieHash(hash.0)
}
fn stacks_hash(hash: marf_core::TrieHash) -> TrieHash {
    TrieHash(hash.0)
}
