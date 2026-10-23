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

//! Explicit proof serialization with the legacy per-vector allocation guard.
use std::io::{self, Read, Write};
use std::mem;

use marf_core::{MARFValue, TrieHash, TrieId};

use crate::{ProofTrieNode, ProofTriePtr, TrieLeaf, TrieMerkleProof, TrieMerkleProofType};
/// Compatibility limits copied from the oracle, frozen numerically after the prototype.
#[derive(Clone, Copy)]
pub struct DecodeLimits {
    /// Maximum allocated bytes for each individual decoded vector.
    pub max_vector_bytes: u32,
    /// Maximum element count in each individual vector.
    pub max_count: u32,
}
impl DecodeLimits {
    /// Match the historical Stacks allocation and count limits.
    pub fn legacy() -> Self {
        Self {
            max_vector_bytes: 16_778_054,
            max_count: u32::MAX,
        }
    }
}
/// Explicit transport selected by an integrator independently of proof hashing.
pub trait ProofCodec<I: TrieId> {
    /// Transport or malformed-input failure retained by the codec.
    type Error: std::error::Error;
    /// Encode one proof into a byte writer.
    fn encode(
        &self,
        proof: &TrieMerkleProof<I>,
        writer: &mut impl Write,
    ) -> Result<(), Self::Error>;
    /// Decode one proof, leaving trailing bytes for the caller.
    fn decode(
        &self,
        reader: &mut impl Read,
        limits: DecodeLimits,
    ) -> Result<TrieMerkleProof<I>, Self::Error>;
}
/// Byte-compatible legacy proof transport with per-vector allocation checks.
pub struct LegacyCodec;
fn exact<const N: usize>(r: &mut (impl Read + ?Sized)) -> Result<[u8; N], CodecError> {
    let mut b = [0; N];
    r.read_exact(&mut b)?;
    Ok(b)
}
fn vec_read<T, R: Read + ?Sized>(
    r: &mut R,
    l: DecodeLimits,
    mut read: impl FnMut(&mut R) -> Result<T, CodecError>,
) -> Result<Vec<T>, CodecError> {
    let n = u32::from_be_bytes(exact(r)?);
    if n > l.max_count || (mem::size_of::<T>() as u128) * (n as u128) > l.max_vector_bytes as u128 {
        return Err(CodecError::Invalid("legacy vector allocation limit".into()));
    }
    let mut v = Vec::with_capacity(n as usize);
    for _ in 0..n {
        v.push(read(r)?);
    }
    Ok(v)
}
fn bytes(r: &mut (impl Read + ?Sized), l: DecodeLimits) -> Result<Vec<u8>, CodecError> {
    vec_read(r, l, |r| Ok(exact::<1>(r)?[0]))
}
fn write_len(n: usize, w: &mut impl Write) -> Result<(), CodecError> {
    Ok(w.write_all(&(n as u32).to_be_bytes())?)
}
fn write_node<I: TrieId>(w: &mut impl Write, n: &ProofTrieNode<I>) -> Result<(), CodecError> {
    w.write_all(&[n.id])?;
    write_len(n.path.len(), w)?;
    w.write_all(&n.path)?;
    write_len(n.ptrs.len(), w)?;
    for p in &n.ptrs {
        w.write_all(&[p.id, p.chr])?;
        w.write_all(p.back_block.as_bytes())?;
    }
    Ok(())
}
fn read_node<I: TrieId>(
    r: &mut (impl Read + ?Sized),
    l: DecodeLimits,
) -> Result<ProofTrieNode<I>, CodecError> {
    let id = exact::<1>(r)?[0];
    let path = bytes(r, l)?;
    let ptrs = vec_read(r, l, |r| {
        Ok(ProofTriePtr {
            id: exact::<1>(r)?[0],
            chr: exact::<1>(r)?[0],
            back_block: I::from_bytes(exact(r)?),
        })
    })?;
    Ok(ProofTrieNode { id, path, ptrs })
}
fn read_component<I: TrieId>(
    r: &mut (impl Read + ?Sized),
    l: DecodeLimits,
) -> Result<TrieMerkleProofType<I>, CodecError> {
    // A sized reference supports the same parser for both vector callbacks and ordinary readers.
    let mut r = r;
    let tag = exact::<1>(&mut r)?[0];
    macro_rules! branch {
        ($v:ident,$n:expr) => {{
            let previous = exact::<1>(&mut r)?[0];
            let node = read_node(&mut r, l)?;
            let mut hashes = [TrieHash([0; 32]); $n];
            for h in &mut hashes {
                *h = TrieHash(exact(&mut r)?);
            }
            TrieMerkleProofType::$v((previous, node, hashes))
        }};
    }
    Ok(match tag {
        0 => branch!(Node4, 3),
        1 => branch!(Node16, 15),
        2 => branch!(Node48, 47),
        3 => branch!(Node256, 255),
        4 => {
            let previous = exact::<1>(&mut r)?[0];
            let path = bytes(&mut r, l)?;
            let data = MARFValue(exact(&mut r)?);
            TrieMerkleProofType::Leaf((previous, TrieLeaf { path, data }))
        }
        5 => {
            let index = i64::from_be_bytes(exact(&mut r)?);
            let hashes = vec_read(&mut r, l, |r| Ok(TrieHash(exact(r)?)))?;
            TrieMerkleProofType::Shunt((index, hashes))
        }
        _ => {
            return Err(CodecError::Invalid(
                "Bad type byte in Trie Merkle Proof".into(),
            ));
        }
    })
}
impl<I: TrieId> ProofCodec<I> for LegacyCodec {
    type Error = CodecError;
    fn encode(&self, p: &TrieMerkleProof<I>, w: &mut impl Write) -> Result<(), CodecError> {
        self.encode_components(&p.0, w)
    }
    fn decode(
        &self,
        r: &mut (impl Read + ?Sized),
        l: DecodeLimits,
    ) -> Result<TrieMerkleProof<I>, CodecError> {
        Ok(TrieMerkleProof(vec_read(r, l, |r| read_component(r, l))?))
    }
}

/// Typed wire errors distinguish transport I/O from malformed input.
#[derive(Debug)]
pub enum CodecError {
    /// Reader/writer failure, retaining its typed cause.
    Io(io::Error),
    /// Invalid indicator or declared vector allocation.
    Invalid(String),
}
impl From<io::Error> for CodecError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}
impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => e.fmt(f),
            Self::Invalid(e) => f.write_str(e),
        }
    }
}
impl std::error::Error for CodecError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Invalid(_) => None,
        }
    }
}

impl LegacyCodec {
    /// Encode a borrowed component slice without cloning or changing its allocation.
    pub fn encode_components<I: TrieId>(
        &self,
        components: &[TrieMerkleProofType<I>],
        w: &mut impl Write,
    ) -> Result<(), CodecError> {
        write_len(components.len(), w)?;
        macro_rules! branch {
            ($tag:expr,$data:ident) => {{
                let (previous, node, hashes) = $data;
                w.write_all(&[$tag, *previous])?;
                write_node(w, node)?;
                for h in hashes {
                    w.write_all(&h.0)?;
                }
            }};
        }
        for c in components {
            match c {
                TrieMerkleProofType::Node4(d) => branch!(0, d),
                TrieMerkleProofType::Node16(d) => branch!(1, d),
                TrieMerkleProofType::Node48(d) => branch!(2, d),
                TrieMerkleProofType::Node256(d) => branch!(3, d),
                TrieMerkleProofType::Leaf((previous, leaf)) => {
                    w.write_all(&[4, *previous])?;
                    write_len(leaf.path.len(), w)?;
                    w.write_all(&leaf.path)?;
                    w.write_all(&leaf.data.0)?;
                }
                TrieMerkleProofType::Shunt((index, hashes)) => {
                    w.write_all(&[5])?;
                    w.write_all(&index.to_be_bytes())?;
                    write_len(hashes.len(), w)?;
                    for h in hashes {
                        w.write_all(&h.0)?;
                    }
                }
            }
        }
        Ok(())
    }
}
