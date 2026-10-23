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

//! An integrator-owned source and transport work without an open database.
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::mem;

use marf_core::access::{AuthenticatedRead, NodeRead, TrieLocation};
use marf_core::node::{Leaf, Node, Path, Ptr, id};
use marf_core::{MARFValue, TrieHash, TrieId32};
use marf_proofs::{
    CodecError, DecodeLimits, LegacyCodec, ProofCodec, Prove, TrieMerkleProof, TrieMerkleProofType,
};

/// Minimal immutable authenticated source; errors belong to the integrator.
struct Source {
    leaf: Leaf,
    fail: bool,
}
/// Borrowed node guard demonstrates no owned conversion during descent.
struct Borrowed<'a>(&'a Leaf);
impl NodeRead for Borrowed<'_> {
    type Error = io::Error;
    fn path(&self) -> io::Result<&[u8]> {
        Ok(self.0.path.as_slice())
    }
    fn is_leaf(&self) -> io::Result<bool> {
        Ok(true)
    }
    fn child(&self, _: u8) -> io::Result<Option<Ptr>> {
        Ok(None)
    }
    fn to_node(self) -> io::Result<Node> {
        Ok(Node::Leaf(self.0.clone()))
    }
}
impl AuthenticatedRead<TrieId32> for Source {
    type Error = io::Error;
    fn select_trie(&mut self, _: &TrieId32) -> io::Result<TrieLocation> {
        if self.fail {
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        } else {
            Ok(TrieLocation::Mem)
        }
    }
    fn root_pointer(&self, _: TrieLocation) -> Ptr {
        Ptr::new(id::LEAF, 0, 0)
    }
    fn read_node(&mut self, _: TrieLocation, _: &Ptr) -> io::Result<Node> {
        Ok(Node::Leaf(self.leaf.clone()))
    }
    fn borrow_node(
        &mut self,
        _: TrieLocation,
        _: &Ptr,
    ) -> io::Result<impl NodeRead<Error = io::Error> + '_> {
        Ok(Borrowed(&self.leaf))
    }
    fn child_hash(&mut self, _: TrieLocation, _: &Ptr) -> io::Result<TrieHash> {
        Ok(marf_core::hash::leaf_hash(&self.leaf))
    }
    fn block_hash(&mut self, _: u32) -> io::Result<TrieId32> {
        Ok(TrieId32([7; 32]))
    }
    fn ancestor_hashes(&mut self, _: &TrieId32, _: u32) -> io::Result<Vec<TrieHash>> {
        Ok(vec![])
    }
    fn is_pruned(&self) -> bool {
        false
    }
    fn get_block_height(&mut self, _: &TrieId32, _: &TrieId32) -> io::Result<Option<u32>> {
        Ok(Some(0))
    }
    fn get_root_hash_at(&mut self, _: &TrieId32) -> io::Result<TrieHash> {
        Ok(marf_core::hash::leaf_hash(&self.leaf))
    }
    fn get_block_at_height(&mut self, h: u32, _: &TrieId32) -> io::Result<Option<TrieId32>> {
        Ok((h == 0).then_some(TrieId32([7; 32])))
    }
}
/// Example integrator framing; authenticated hashing is independent of transport.
struct FramedCodec;
impl ProofCodec<TrieId32> for FramedCodec {
    type Error = CodecError;
    fn encode(&self, p: &TrieMerkleProof<TrieId32>, w: &mut impl Write) -> Result<(), CodecError> {
        w.write_all(b"ALT1")?;
        LegacyCodec.encode(p, w)
    }
    fn decode(
        &self,
        r: &mut impl Read,
        l: DecodeLimits,
    ) -> Result<TrieMerkleProof<TrieId32>, CodecError> {
        let mut tag = [0; 4];
        r.read_exact(&mut tag)?;
        if tag != *b"ALT1" {
            return Err(CodecError::Invalid("frame".into()));
        }
        LegacyCodec.decode(r, l)
    }
}
#[test]
fn custom_source_and_alternate_codec_verify_same_root() {
    let path = TrieHash::from_key("key");
    let value = MARFValue([9; 40]);
    let id = TrieId32([7; 32]);
    let mut source = Source {
        leaf: Leaf {
            path: Path::from_slice(path.as_bytes()).unwrap(),
            value: value.clone(),
        },
        fail: false,
    };
    let root = marf_core::hash::leaf_hash(&source.leaf);
    let proof = source.prove(&path, &value, &id).unwrap();
    assert!(proof.verify(&path, &value, &root, &HashMap::new()));
    let mut bytes = vec![];
    FramedCodec.encode(&proof, &mut bytes).unwrap();
    let decoded = FramedCodec
        .decode(&mut bytes.as_slice(), DecodeLimits::legacy())
        .unwrap();
    assert!(decoded.verify(&path, &value, &root, &HashMap::new()));
    source.fail = true;
    let err = source.prove(&path, &value, &id).unwrap_err();
    std::assert_matches!(err,marf_proofs::Error::Source(ref cause) if cause.kind()==io::ErrorKind::PermissionDenied);
    assert!(std::error::Error::source(&err).unwrap().is::<io::Error>());
}
#[test]
fn default_decode_limits_and_trailing_bytes() {
    let size = mem::size_of::<TrieMerkleProofType<TrieId32>>();
    let limit = DecodeLimits::legacy();
    assert_eq!(limit.max_vector_bytes, 16_778_054);
    let above = limit.max_vector_bytes as usize / size + 1;
    let result: Result<TrieMerkleProof<TrieId32>, _> =
        LegacyCodec.decode(&mut (above as u32).to_be_bytes().as_slice(), limit);
    std::assert_matches!(result, Err(CodecError::Invalid(_)));
    // Exactly one component is allowed by allocation but fails on missing payload.
    let input = 1u32.to_be_bytes();
    let allowed = DecodeLimits {
        max_vector_bytes: size as u32,
        max_count: 1,
    };
    let result: Result<TrieMerkleProof<TrieId32>, _> =
        LegacyCodec.decode(&mut input.as_slice(), allowed);
    std::assert_matches!(result, Err(CodecError::Io(_)));
    let result: Result<TrieMerkleProof<TrieId32>, _> = LegacyCodec.decode(
        &mut input.as_slice(),
        DecodeLimits {
            max_vector_bytes: size as u32 - 1,
            max_count: 1,
        },
    );
    std::assert_matches!(result, Err(CodecError::Invalid(_)));
    let mut trailing = [0, 0, 0, 0, 0xaa].as_slice();
    let p: TrieMerkleProof<TrieId32> = LegacyCodec.decode(&mut trailing, limit).unwrap();
    assert!(p.is_empty());
    assert_eq!(trailing, [0xaa]);
    let result: Result<TrieMerkleProof<TrieId32>, _> =
        LegacyCodec.decode(&mut [0, 0, 0, 1, 6].as_slice(), limit);
    std::assert_matches!(result, Err(CodecError::Invalid(_)));
    // A pointer vector has its own fixed element-size limit, not an aggregate budget.
    let count =
        limit.max_vector_bytes / mem::size_of::<marf_proofs::ProofTriePtr<TrieId32>>() as u32 + 1;
    let mut input = vec![0, 0, 0, 1, 0, 0, 2, 0, 0, 0, 0];
    input.extend_from_slice(&count.to_be_bytes());
    let result: Result<TrieMerkleProof<TrieId32>, _> =
        LegacyCodec.decode(&mut input.as_slice(), limit);
    std::assert_matches!(result, Err(CodecError::Invalid(_)));
}
