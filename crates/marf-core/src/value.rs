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

//! Fixed-width vocabulary and checked ancestry values.
use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::ops::{Deref, Index};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha512_256};

use crate::bytes::{from_hex, to_hex};
use crate::{Error, corrupt};

/// Fixed-width trie identity, independent of SQL and transport codecs.
pub trait TrieId: Clone + Eq + Hash + fmt::Debug + fmt::Display {
    /// The canonical 32 identity bytes.
    fn as_bytes(&self) -> &[u8; 32];
    /// Construct an identity without interpreting its bytes.
    fn from_bytes(bytes: [u8; 32]) -> Self;
    /// The root-parent identity.
    fn sentinel() -> Self {
        Self::from_bytes(SENTINEL_ARRAY)
    }
    /// Consume the identity as canonical bytes.
    fn to_bytes(self) -> [u8; 32] {
        *self.as_bytes()
    }
}
/// The parent of a height-zero trie.
pub const SENTINEL_ARRAY: [u8; 32] = [255; 32];
/// Hash width committed by the MARF format.
pub const TRIEHASH_ENCODED_SIZE: usize = 32;
/// Leaf-value width, including reserved padding.
pub const MARF_VALUE_ENCODED_SIZE: u32 = 40;

/// Shared byte behavior, preserving the existing little-endian ordering.
macro_rules! fixed_bytes {
    ($name:ident,$len:expr) => {
        impl $name {
            /// All-zero bytes.
            pub const ZERO: Self = Self([0; $len]);
            /// Borrow the fixed-size bytes.
            #[inline]
            pub fn as_bytes(&self) -> &[u8; $len] {
                &self.0
            }
            /// Copy the fixed-size bytes.
            #[inline]
            pub fn to_bytes(&self) -> [u8; $len] {
                self.0
            }
            /// Consume as fixed-size bytes.
            pub fn into_bytes(self) -> [u8; $len] {
                self.0
            }
            /// Decode a fixed-width byte slice.
            pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
                bytes.try_into().ok().map(Self)
            }
            /// Decode a byte slice without hashing.
            pub fn from_vec(bytes: &[u8]) -> Option<Self> {
                Self::from_bytes(bytes)
            }
            /// Decode big-endian bytes into the historical internal order.
            pub fn from_bytes_be(bytes: &[u8]) -> Option<Self> {
                Self::from_vec_be(bytes)
            }
            /// Decode big-endian bytes into the historical internal order.
            pub fn from_vec_be(bytes: &[u8]) -> Option<Self> {
                let mut value = Self::from_bytes(bytes)?;
                value.0.reverse();
                Some(value)
            }
            /// Encode as canonical lowercase hex.
            pub fn to_hex(&self) -> String {
                to_hex(&self.0)
            }
            /// Decode case-insensitive hexadecimal bytes.
            pub fn from_hex(text: &str) -> Result<Self, Error> {
                Self::from_bytes(&from_hex(text)?)
                    .ok_or_else(|| corrupt!("Invalid {} hex length", stringify!($name)))
            }
            /// Width in bytes.
            pub fn len(&self) -> usize {
                $len
            }
            /// Fixed-size values are never empty.
            pub fn is_empty(&self) -> bool {
                false
            }
            /// Pointer to the fixed-size bytes.
            pub fn as_ptr(&self) -> *const u8 {
                self.0.as_ptr()
            }
            /// Mutable pointer to the fixed-size bytes.
            pub fn as_mut_ptr(&mut self) -> *mut u8 {
                self.0.as_mut_ptr()
            }
        }
        impl PartialEq for $name {
            fn eq(&self, other: &Self) -> bool {
                self.0 == other.0
            }
        }
        impl Eq for $name {}
        impl PartialOrd for $name {
            fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
                Some(self.cmp(other))
            }
        }
        impl Ord for $name {
            fn cmp(&self, other: &Self) -> Ordering {
                self.0.iter().rev().cmp(other.0.iter().rev())
            }
        }
        impl Hash for $name {
            fn hash<H: Hasher>(&self, h: &mut H) {
                self.0.as_slice().hash(h);
            }
            fn hash_slice<H: Hasher>(values: &[Self], h: &mut H) {
                for value in values {
                    value.0.as_slice().hash(h);
                }
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                for b in self.0 {
                    write!(f, "{b:02x}")?;
                }
                Ok(())
            }
        }
        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(self, f)
            }
        }
        impl fmt::LowerHex for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(self, f)
            }
        }
        impl AsRef<[u8]> for $name {
            fn as_ref(&self) -> &[u8] {
                &self.0
            }
        }
        impl From<[u8; $len]> for $name {
            fn from(b: [u8; $len]) -> Self {
                Self(b)
            }
        }
        impl From<&[u8]> for $name {
            fn from(b: &[u8]) -> Self {
                Self::from_bytes(b).expect("fixed-width bytes")
            }
        }
        impl Deref for $name {
            type Target = [u8; $len];
            fn deref(&self) -> &Self::Target {
                &self.0
            }
        }
        impl<I: std::slice::SliceIndex<[u8]>> Index<I> for $name {
            type Output = I::Output;
            fn index(&self, i: I) -> &Self::Output {
                &self.0[i]
            }
        }
    };
}
/// SHA-512/256 hash committed by trie nodes and forest roots.
#[derive(Default, Copy, Clone)]
pub struct TrieHash(pub [u8; 32]);
fixed_bytes!(TrieHash, 32);
impl TrieHash {
    /// SHA-512/256 of the empty byte string.
    pub const EMPTY: Self = Self([
        0xc6, 0x72, 0xb8, 0xd1, 0xef, 0x56, 0xed, 0x28, 0xab, 0x87, 0xc3, 0x62, 0x2c, 0x51, 0x14,
        0x06, 0x9b, 0xdd, 0x3a, 0xd7, 0xb8, 0xf9, 0x73, 0x74, 0x98, 0xd0, 0xc0, 0x1e, 0xce, 0xf0,
        0x96, 0x7a,
    ]);
    /// Hash a key's UTF-8 bytes.
    #[inline]
    pub fn from_key(key: &str) -> Self {
        Self::from_data(key.as_bytes())
    }
    /// Hash bytes with the fixed consensus hash.
    #[inline]
    pub fn from_data(data: &[u8]) -> Self {
        if data.is_empty() {
            return Self::EMPTY;
        }
        Self(Sha512_256::digest(data).into())
    }
    /// Hash concatenated byte slices without joining them.
    #[inline]
    pub fn from_data_array<B: AsRef<[u8]>>(data: &[B]) -> Self {
        if data.is_empty() {
            return Self::EMPTY;
        }
        let mut h = Sha512_256::new();
        for b in data {
            h.update(b);
        }
        Self(h.finalize().into())
    }
}
impl Serialize for TrieHash {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}
impl<'de> Deserialize<'de> for TrieHash {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::from_hex(&String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}
/// A neutral concrete identity for applications without a separate identifier type.
#[derive(Copy, Clone)]
pub struct TrieId32(pub [u8; 32]);
fixed_bytes!(TrieId32, 32);
impl TrieId for TrieId32 {
    fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
    fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}
/// A 32-byte value hash or identifier plus eight reserved bytes.
#[derive(Clone)]
pub struct MARFValue(pub [u8; 40]);
fixed_bytes!(MARFValue, 40);
impl From<u32> for MARFValue {
    fn from(height: u32) -> Self {
        let mut bytes = [0; 40];
        bytes[..4].copy_from_slice(&height.to_le_bytes());
        Self(bytes)
    }
}
impl From<MARFValue> for u32 {
    /// Decode a compatibility height value.
    ///
    /// # Panics
    /// Panics on non-zero padding. Persisted reads use `decode_height` instead.
    fn from(value: MARFValue) -> Self {
        value
            .decode_height()
            .expect("Failed to convert MARF value into u32: data stored after 4th byte")
    }
}
impl MARFValue {
    /// Store an identity with zeroed padding.
    pub fn from_id<I: TrieId>(id: &I) -> Self {
        let mut bytes = [0; 40];
        bytes[..32].copy_from_slice(id.as_bytes());
        Self(bytes)
    }
    /// Decode an ancestry identifier, rejecting non-zero reserved bytes.
    pub fn decode_id<I: TrieId>(&self) -> Result<I, Error> {
        if self.0[32..].iter().any(|b| *b != 0) {
            return Err(corrupt!("Non-zero identifier padding in MARF value"));
        }
        let mut bytes = [0; 32];
        bytes.copy_from_slice(&self.0[..32]);
        Ok(I::from_bytes(bytes))
    }
    /// Decode an ancestry height, rejecting non-zero reserved bytes.
    pub fn decode_height(&self) -> Result<u32, Error> {
        if self.0[4..].iter().any(|b| *b != 0) {
            return Err(corrupt!("Non-zero height padding in MARF value"));
        }
        let mut bytes = [0; 4];
        bytes.copy_from_slice(&self.0[..4]);
        Ok(u32::from_le_bytes(bytes))
    }
    /// Store a fixed-size value hash with zeroed padding.
    pub fn from_value_hash_bytes(hash: &[u8; 32]) -> Self {
        let mut bytes = [0; 40];
        bytes[..32].copy_from_slice(hash);
        Self(bytes)
    }
    /// Store a value hash supplied by the engine or an integrator.
    pub fn from_value_hash(hash: &impl AsRef<[u8]>) -> Self {
        Self::from_value_hash_bytes(hash.as_ref().try_into().expect("32-byte value hash"))
    }
    /// Hash the UTF-8 side-store value.
    pub fn from_value(value: &str) -> Self {
        Self::from_value_hash(&TrieHash::from_data(value.as_bytes()))
    }
    /// Copy the value bytes.
    pub fn to_vec(&self) -> Vec<u8> {
        self.0.to_vec()
    }
    /// Extract the hash without interpreting reserved bytes.
    pub fn to_value_hash(&self) -> TrieHash {
        let mut bytes = [0; 32];
        bytes.copy_from_slice(&self.0[..32]);
        TrieHash(bytes)
    }
}
