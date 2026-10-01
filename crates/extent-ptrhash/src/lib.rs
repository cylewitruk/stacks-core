//! Immutable commitment-to-stable-ID lookup for canonical Clarity generations.
//! Files are locally constructed artifacts; activation is offline and transactional.

use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

use ptr_hash::{bucket_fn::Linear, hash::Xxh3_128, PtrHash};
use serde::{Deserialize as SerdeDeserialize, Serialize as SerdeSerialize};
use sha2::{Digest, Sha256};

#[cfg(feature = "diagnostics")]
pub mod diagnostics;
/// Portable registrations and bounded construction for the canonical value index.
pub mod stable;

/// All commitment bytes participate in hashing; candidate verification remains mandatory.
type Function = PtrHash<[u8; 40], Linear, Vec<u32>, Xxh3_128, Vec<u8>>;

/// Limit construction to one byte-prefix partition at a time.
const PARTITIONS: usize = 256;
/// Reject pathological input partitions rather than exceeding the construction budget.
const MAX_PARTITION_KEYS: usize = 2_000_000;
/// Index operations preserve the underlying storage or validation diagnostic.
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// One completed shard's portable metadata.
#[derive(SerdeSerialize, SerdeDeserialize)]
struct ShardInfo {
    /// Number of unique full commitments.
    count: usize,
    /// Digest of the native PtrHash serialization.
    function_sha256: String,
    /// Digest of the fixed-width little-endian location array.
    slots_sha256: String,
}

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Convert an invariant failure into a storage error.
fn invalid(message: &str) -> Box<dyn std::error::Error + Send + Sync> {
    io::Error::new(io::ErrorKind::InvalidData, message).into()
}

/// Read the independently selected fingerprint bytes; never use it as an identity.
fn fingerprint(key: &[u8; 40]) -> [u8; 4] {
    key[28..32].try_into().unwrap()
}

/// Create and durably write one immutable generation artifact.
fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = File::options().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
