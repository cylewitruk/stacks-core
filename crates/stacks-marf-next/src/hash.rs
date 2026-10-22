//! Consensus hashing of trie nodes and MARF roots. All hashes are SHA-512/256.
//!
//! * A leaf hashes its encoded body: `H(0x01 || path_len || path || value)`.
//! * A branch hashes `id`, then for every slot `ptr_id || chr || block[32]`,
//!   then `path_len || path`, then one 32-byte hash per slot. For an inline
//!   child the block bytes are zero and the slot hash is the child's hash; for
//!   a back-pointer both are the referenced block's id; empty slots use zero
//!   block bytes and [`TrieHash::EMPTY`].
//! * The MARF root hash of a trie at height `h > 0` additionally mixes in the
//!   root hashes of the ancestors at heights `h - 2^i`.

use sha2::{Digest, Sha512_256};
use stacks_common::types::chainstate::TrieHash;

use crate::node::{Branch, Leaf, Ptr, id};

/// What a branch's child contributes to its parent's hash.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChildHash {
    /// The 32 block-id bytes hashed alongside the pointer (zero when inline).
    pub block: [u8; 32],
    /// The slot hash (the child's node hash, or the block id for back-pointers).
    pub hash: TrieHash,
}

impl ChildHash {
    pub fn inline(hash: TrieHash) -> ChildHash {
        ChildHash {
            block: [0u8; 32],
            hash,
        }
    }

    pub fn back(block_id: [u8; 32]) -> ChildHash {
        ChildHash {
            block: block_id,
            hash: TrieHash(block_id),
        }
    }
}

pub fn leaf_hash(leaf: &Leaf) -> TrieHash {
    let mut h = Sha512_256::new();
    h.update([id::LEAF, leaf.path.len() as u8]);
    h.update(leaf.path.as_slice());
    h.update(leaf.value.0);
    TrieHash(h.finalize().into())
}

/// Reusable buffers for [`branch_hash`]: the pointer section and the slot
/// hash section of a branch preimage (at most 1 + 256*34 + 33 and 256*32
/// bytes), so a whole trie is hashed with two buffers and one `update` call
/// per section.
pub struct HashScratch {
    head: Vec<u8>,
    tails: Vec<u8>,
}

impl Default for HashScratch {
    fn default() -> HashScratch {
        HashScratch {
            head: Vec::with_capacity(1 + 256 * 34 + 33),
            tails: Vec::with_capacity(256 * 32),
        }
    }
}

/// Hash `branch`, asking `child` once for each occupied slot's contribution.
pub fn branch_hash<E>(
    branch: &Branch,
    mut child: impl FnMut(&Ptr) -> Result<ChildHash, E>,
    scratch: &mut HashScratch,
) -> Result<TrieHash, E> {
    let head = &mut scratch.head;
    let tails = &mut scratch.tails;
    head.clear();
    tails.clear();
    head.push(branch.kind.id());
    for slot in branch.slots() {
        match slot {
            Some(p) => {
                let c = child(p)?;
                head.extend_from_slice(&[p.id, p.chr]);
                head.extend_from_slice(&c.block);
                tails.extend_from_slice(c.hash.as_bytes());
            }
            None => {
                head.extend_from_slice(&[0u8; 34]);
                tails.extend_from_slice(TrieHash::EMPTY.as_bytes());
            }
        }
    }
    head.push(branch.path.len() as u8);
    head.extend_from_slice(branch.path.as_slice());
    let mut h = Sha512_256::new();
    h.update(&*head);
    h.update(&*tails);
    Ok(TrieHash(h.finalize().into()))
}

/// The MARF root hash: the root node's hash mixed with the ancestor root
/// hashes at heights `h-1, h-2, h-4, ...` (empty at height 0).
pub fn marf_root_hash(root_node_hash: &TrieHash, ancestors: &[TrieHash]) -> TrieHash {
    if ancestors.is_empty() {
        return *root_node_hash;
    }
    let mut h = Sha512_256::new();
    h.update(root_node_hash.as_bytes());
    for a in ancestors {
        h.update(a.as_bytes());
    }
    TrieHash(h.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MARFValue;
    use crate::node::{Kind, Path};

    #[test]
    fn empty_root_hash_is_stable() {
        // The hash of an empty Node256 root with an empty path, as the
        // original implementation computes it: H(0x05 || 256*34 zero bytes || 0x00 || 256*EMPTY).
        let root = Branch::new(Kind::Node256, Path::EMPTY);
        let h = branch_hash::<()>(&root, |_| unreachable!(), &mut HashScratch::default()).unwrap();
        let mut expected = Sha512_256::new();
        expected.update([id::NODE256]);
        expected.update([0u8; 34 * 256]);
        expected.update([0u8]);
        for _ in 0..256 {
            expected.update(TrieHash::EMPTY.as_bytes());
        }
        assert_eq!(h.0, <[u8; 32]>::from(expected.finalize()));
    }

    #[test]
    fn leaf_hash_matches_encoding() {
        let leaf = Leaf {
            path: Path::from_slice(&[1, 2]).unwrap(),
            value: MARFValue([3u8; 40]),
        };
        let mut body = vec![];
        crate::codec::encode_node(&crate::node::Node::Leaf(leaf.clone()), false, &mut body);
        assert_eq!(
            leaf_hash(&leaf).0,
            <[u8; 32]>::from(Sha512_256::digest(&body))
        );
    }
}
