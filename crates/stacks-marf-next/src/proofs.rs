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

//! Stacks transport and fixed-hash adapters over neutral proof tooling.
use std::collections::HashMap;
use std::io::{Read, Write};
use std::ops::Deref;

use marf_core::access::AuthenticatedRead;
use marf_proofs::{DecodeLimits, LegacyCodec, ProofCodec};
pub use marf_proofs::{
    DecodeLimits as ProofDecodeLimits, ProofCodec as ConfigurableProofCodec, TrieLeaf,
    TrieMerkleProofTypeIndicator,
};
use stacks_common::codec::{Error as CodecError, StacksMessageCodec};
use stacks_common::types::chainstate::TrieHash;

use crate::{Error, Id, MARFValue, MarfTrieId, core_hash};
/// Proof pointer carrying a facade identity adapter.
pub type ProofTriePtr<T> = marf_proofs::ProofTriePtr<Id<T>>;
/// Proof branch carrying facade identity adapters.
pub type ProofTrieNode<T> = marf_proofs::ProofTrieNode<Id<T>>;
/// Legacy proof component layout; hashes are neutral fixed-size values.
pub type TrieMerkleProofType<T> = marf_proofs::TrieMerkleProofType<Id<T>>;
/// Owned proof vector; wrapping and unwrapping never clones its elements.
#[derive(Debug)]
pub struct TrieMerkleProof<T: MarfTrieId>(pub Vec<TrieMerkleProofType<T>>);
impl<T: MarfTrieId> Deref for TrieMerkleProof<T> {
    type Target = Vec<TrieMerkleProofType<T>>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl<T: MarfTrieId> StacksMessageCodec for TrieMerkleProof<T> {
    fn consensus_serialize<W: Write>(&self, w: &mut W) -> Result<(), CodecError> {
        LegacyCodec
            .encode_components(&self.0, w)
            .map_err(|e| match e {
                marf_proofs::CodecError::Io(e) => CodecError::WriteError(e),
                marf_proofs::CodecError::Invalid(e) => CodecError::SerializeError(e),
            })
    }
    fn consensus_deserialize<R: Read>(r: &mut R) -> Result<Self, CodecError> {
        LegacyCodec
            .decode(r, DecodeLimits::legacy())
            .map(|p: marf_proofs::TrieMerkleProof<Id<T>>| Self(p.0))
            .map_err(|e| match e {
                marf_proofs::CodecError::Io(e) => CodecError::ReadError(e),
                marf_proofs::CodecError::Invalid(e) => CodecError::DeserializeError(e),
            })
    }
}
/// Preserve legacy source and logical error classifications at the integration boundary.
pub fn source_error(e: marf_proofs::Error<Error>) -> Error {
    match e {
        marf_proofs::Error::Source(e) => e,
        marf_proofs::Error::NotFoundError => Error::NotFoundError,
        marf_proofs::Error::CorruptionError(e) => Error::CorruptionError(e),
        marf_proofs::Error::UnsupportedOnPrunedMarf(op) => Error::UnsupportedOnPrunedMarf(op),
    }
}
impl<T: MarfTrieId> TrieMerkleProof<T> {
    /// Legacy transport bytes in lowercase hexadecimal.
    pub fn to_hex(&self) -> String {
        let mut b = vec![];
        self.consensus_serialize(&mut b).expect("memory writer");
        marf_core::bytes::to_hex(&b)
    }
    /// Build a proof from a logical source, without a native SQL requirement.
    pub fn from_path<S: AuthenticatedRead<Id<T>, Error = Error> + ?Sized>(
        view: &mut S,
        path: &TrieHash,
        value: &MARFValue,
        block: &T,
    ) -> Result<Self, Error> {
        marf_proofs::Prove::prove(view, &core_hash(path), value, &Id(block.clone()))
            .map(|p| Self(p.0))
            .map_err(source_error)
    }
    /// Build a proof from a key and unhashed side-store value.
    pub fn from_entry<S: AuthenticatedRead<Id<T>, Error = Error> + ?Sized>(
        view: &mut S,
        key: &str,
        value: &str,
        block: &T,
    ) -> Result<Self, Error> {
        Self::from_raw_entry(view, key, &MARFValue::from_value(value), block)
    }
    /// Build a proof from a key and already hashed value.
    pub fn from_raw_entry<S: AuthenticatedRead<Id<T>, Error = Error> + ?Sized>(
        view: &mut S,
        key: &str,
        value: &MARFValue,
        block: &T,
    ) -> Result<Self, Error> {
        Self::from_path(view, &TrieHash::from_key(key), value, block)
    }
    /// Verify without an open database; only the root-to-block map is adapted.
    pub fn verify_proof(
        proof: &[TrieMerkleProofType<T>],
        path: &TrieHash,
        value: &MARFValue,
        root: &TrieHash,
        root_to_block: &HashMap<TrieHash, T>,
    ) -> bool {
        let ids = root_to_block
            .iter()
            .map(|(h, id)| (core_hash(h), Id(id.clone())))
            .collect();
        marf_proofs::TrieMerkleProof::verify_proof(
            proof,
            &core_hash(path),
            value,
            &core_hash(root),
            &ids,
        )
    }
    /// Verify this owned proof against the expected root.
    pub fn verify(
        &self,
        path: &TrieHash,
        value: &MARFValue,
        root: &TrieHash,
        ids: &HashMap<TrieHash, T>,
    ) -> bool {
        Self::verify_proof(&self.0, path, value, root, ids)
    }
}
/// Legacy proof convenience over a neutral authenticated source.
pub trait Prove<T: MarfTrieId>: AuthenticatedRead<Id<T>, Error = Error> {
    fn prove(
        &mut self,
        path: &TrieHash,
        value: &MARFValue,
        block: &T,
    ) -> Result<TrieMerkleProof<T>, Error> {
        TrieMerkleProof::from_path(self, path, value, block)
    }
}
impl<T: MarfTrieId, S: AuthenticatedRead<Id<T>, Error = Error> + ?Sized> Prove<T> for S {}
