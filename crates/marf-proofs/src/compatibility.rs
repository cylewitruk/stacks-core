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

//! Existing convenience entry points for MARF callers.
use marf_core::bytes::to_hex;
use marf_core::{MARFValue, TrieHash, TrieId};

use super::{Prove, TrieMerkleProof};
use crate::access::AuthenticatedRead;
use crate::{Error, LegacyCodec, ProofCodec};

impl<T: TrieId> TrieMerkleProof<T> {
    pub fn to_hex(&self) -> String {
        let mut bytes = vec![];
        LegacyCodec
            .encode(self, &mut bytes)
            .expect("Write error on memory buffer");
        to_hex(&bytes)
    }

    /// Make a merkle proof of inclusion from a path.
    pub fn from_path<S: AuthenticatedRead<T> + ?Sized>(
        view: &mut S,
        path: &TrieHash,
        expected_value: &MARFValue,
        root_block_header: &T,
    ) -> Result<TrieMerkleProof<T>, Error<S::Error>> {
        view.prove(path, expected_value, root_block_header)
    }

    /// Make a merkle proof of inclusion from a key/value pair.
    pub fn from_entry<S: AuthenticatedRead<T> + ?Sized>(
        view: &mut S,
        key: &str,
        value: &str,
        root_block_header: &T,
    ) -> Result<TrieMerkleProof<T>, Error<S::Error>> {
        view.prove(
            &TrieHash::from_key(key),
            &MARFValue::from_value(value),
            root_block_header,
        )
    }

    pub fn from_raw_entry<S: AuthenticatedRead<T> + ?Sized>(
        view: &mut S,
        key: &str,
        value: &MARFValue,
        root_block_header: &T,
    ) -> Result<TrieMerkleProof<T>, Error<S::Error>> {
        view.prove(&TrieHash::from_key(key), value, root_block_header)
    }
}
