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

#![allow(unused_variables)]
#![allow(unused_assignments)]

use std::collections::HashMap;
use std::ops::Deref;

use clarity::util::hash::Sha512Trunc256Sum;
use rusqlite::Connection;
use stacks_common::types::chainstate::StacksBlockId;
use stacks_common::util::hash::to_hex;

use crate::chainstate::stacks::index::bits::*;
use crate::chainstate::stacks::index::marf::*;
use crate::chainstate::stacks::index::node::*;
use crate::chainstate::stacks::index::scratch::MarfReadState;
use crate::chainstate::stacks::index::storage::testing::MarfTestStorage;
use crate::chainstate::stacks::index::storage::*;
use crate::chainstate::stacks::index::trie::*;
use crate::chainstate::stacks::index::{
    Error, MARFValue, MarfTrieId, NodePatching, TrieLeaf, TrieReadSession, TrieReadStorage,
};
use crate::chainstate::stacks::{BlockHeaderHash, TrieHash};

pub mod file;
pub mod marf;
pub mod marf_perfs;
pub mod marf_regression;
pub mod node;
pub mod node_patch;
pub mod proofs;
pub mod scratch;
pub mod squash;
pub mod storage;
pub mod trie;

pub trait MarfRootTable<T: MarfTrieId> {
    fn current_root_to_block_table(&mut self) -> HashMap<TrieHash, T>;
}

impl<T: MarfTrieId> MarfRootTable<T> for MARF<T> {
    fn current_root_to_block_table(&mut self) -> HashMap<TrieHash, T> {
        self.with_storage(|storage| {
            storage
                .read_root_to_block_table()
                .expect("Failed to read root-to-block table")
        })
    }
}

impl<T: MarfTrieId> MarfRootTable<T> for MarfTransaction<'_, T> {
    fn current_root_to_block_table(&mut self) -> HashMap<TrieHash, T> {
        self.with_storage(|storage| {
            storage
                .read_root_to_block_table()
                .expect("Failed to read root-to-block table")
        })
    }
}

impl<T: MarfTrieId> MarfRootTable<T> for ReopenedTrieStorageConnection<'_, T> {
    fn current_root_to_block_table(&mut self) -> HashMap<TrieHash, T> {
        self.with_storage(|storage| {
            storage
                .read_root_to_block_table()
                .expect("Failed to read root-to-block table")
        })
    }
}

impl<T: MarfTrieId, S: NodePatching, R> MarfRootTable<T> for MarfReadCtx<'_, T, S, R>
where
    R: TrieReadStorage<T> + MarfTestStorage<T> + ?Sized,
{
    fn current_root_to_block_table(&mut self) -> HashMap<TrieHash, T> {
        self.storage()
            .read_root_to_block_table()
            .expect("Failed to read root-to-block table")
    }
}

/// Walk from the given node to the next node on the path, advancing the cursor.
///
/// * Returns the [`TriePtr`] which was followed, the _next_ node to walk, and the hash of the
///   _current_ node.
/// * Returns `None` if we either didn't find the node, we're out of path, we're at a leaf, or
///   if a backpointer is encountered.
///
/// **NOTE:** This only works if we're walking a trie, not a MARF.
pub fn walk_from<T: MarfTrieId, Db: Deref<Target = Connection>>(
    storage: &mut TrieStorageConnection<T, Db>,
    node: &TrieNodeType,
    cursor: &mut TrieCursor<T>,
    decode_scratch: &mut MarfReadState,
) -> Result<Option<(TriePtr, TrieNodeType, TrieHash)>, Error> {
    match cursor.walk(node, &storage.get_cur_block()) {
        Ok(ptr_opt) => {
            match ptr_opt {
                None => {
                    // end of path
                    Ok(None)
                }
                Some(ptr) => {
                    // end of node path
                    trace!("Walked to {:?}", &ptr);
                    let (node, hash) = read_owned_hashed_node(storage, &ptr, decode_scratch)?;

                    Ok(Some((ptr, node, hash)))
                }
            }
        }
        Err(e) => Err(Error::CursorError(e)),
    }
}

pub fn read_owned_hashed_node<T: MarfTrieId, Db: Deref<Target = Connection>>(
    storage: &mut TrieStorageConnection<T, Db>,
    ptr: &TriePtr,
    decode_scratch: &mut MarfReadState,
) -> Result<(TrieNodeType, TrieHash), Error> {
    let mut read_session = TrieReadSession::new(storage, decode_scratch);
    read_session
        .read_node(ptr)?
        .into_owned_node()
        .and_then(|(node, hash)| {
            hash.map(|hash| (node, hash))
                .ok_or_else(|| Error::CorruptionError("Missing node hash in trie read".to_string()))
        })
}

pub fn read_root<T: MarfTrieId, Db: Deref<Target = Connection>>(
    storage: &mut TrieStorageConnection<T, Db>,
) -> Result<(TrieNodeType, TrieHash), Error> {
    let mut decode_scratch = MarfReadState::new();
    Trie::read_root(storage, &mut decode_scratch)?
        .into_owned_node()
        .and_then(|(node, hash)| {
            hash.map(|hash| (node, hash))
                .ok_or_else(|| Error::CorruptionError("Missing node hash in trie read".to_string()))
        })
}

pub fn read_nodetype<T: MarfTrieId, Db: Deref<Target = Connection>>(
    storage: &mut TrieStorageConnection<T, Db>,
    ptr: &TriePtr,
) -> Result<(TrieNodeType, TrieHash), Error> {
    let mut decode_scratch = MarfReadState::new();
    storage
        .read_node_with_state(ptr, &mut decode_scratch)?
        .into_owned_node()
        .and_then(|(node, hash)| {
            hash.map(|hash| (node, hash))
                .ok_or_else(|| Error::CorruptionError("Missing node hash in trie read".to_string()))
        })
}

pub fn update_root_hash<T: MarfTrieId, Db: Deref<Target = Connection>>(
    storage: &mut TrieStorageConnection<T, Db>,
    cursor: &TrieCursor<T>,
) -> Result<(), Error> {
    let mut decode_scratch = MarfReadState::new();
    Trie::update_root_hash(storage, cursor, &mut decode_scratch)
}

/// Print out a trie to stderr
pub fn dump_trie<T, Db>(s: &mut TrieStorageConnection<T, Db>)
where
    T: MarfTrieId,
    Db: Deref<Target = Connection>,
{
    test_debug!("\n----- BEGIN TRIE ------");

    fn space(cnt: usize) -> String {
        let mut ret = vec![];
        for _ in 0..cnt {
            ret.push(" ".to_string());
        }
        ret.join("")
    }

    let root_ptr = s.root_ptr();
    let mut frontier: Vec<(TrieNodeType, TrieHash, usize)> = vec![];
    let (root, root_hash) = read_root(s).unwrap();
    frontier.push((root, root_hash, 0));

    while !frontier.is_empty() {
        let (next, next_hash, depth) = frontier.pop().unwrap();
        let (ptrs, path_len) = match next {
            TrieNodeType::Leaf(ref leaf_data) => {
                test_debug!("{}{} {:?}", &space(depth), next_hash, leaf_data);
                (vec![], leaf_data.path.len())
            }
            TrieNodeType::Node4(ref data) => {
                test_debug!("{}{} {:?}", &space(depth), next_hash, data);
                (data.ptrs.to_vec(), data.path.len())
            }
            TrieNodeType::Node16(ref data) => {
                test_debug!("{}{} {:?}", &space(depth), next_hash, data);
                (data.ptrs.to_vec(), data.path.len())
            }
            TrieNodeType::Node48(ref data) => {
                test_debug!("{}{} {:?}", &space(depth), next_hash, data);
                (data.ptrs.to_vec(), data.path.len())
            }
            TrieNodeType::Node256(ref data) => {
                test_debug!("{}{} {:?}", &space(depth), next_hash, data);
                (data.ptrs.to_vec(), data.path.len())
            }
        };
        for ptr in ptrs.iter() {
            if ptr.id() == TrieNodeID::Empty as u8 {
                continue;
            }
            if !is_backptr(ptr.id()) {
                let (child_node, child_hash) = read_nodetype(s, ptr).unwrap();
                frontier.push((child_node, child_hash, depth + path_len + 1));
            }
        }
    }

    test_debug!("----- END TRIE ------\n");
}

pub fn merkle_test<Db: Deref<Target = Connection>>(
    s: &mut TrieStorageConnection<BlockHeaderHash, Db>,
    path: &[u8],
    value: &[u8],
) {
    let block_header = BlockHeaderHash([0u8; 32]);
    merkle_test_marf(s, &block_header, path, value, Some(HashMap::new()));
}

pub fn merkle_test_marf<Db: Deref<Target = Connection>>(
    s: &mut TrieStorageConnection<BlockHeaderHash, Db>,
    header: &BlockHeaderHash,
    path: &[u8],
    value: &[u8],
    root_to_block: Option<HashMap<TrieHash, BlockHeaderHash>>,
) -> HashMap<TrieHash, BlockHeaderHash> {
    MarfReadCtx::with_ephemeral(s, |ctx| {
        verify_marf_merkle_proof(ctx, header, path, value, root_to_block)
    })
}

fn verify_marf_merkle_proof<C>(
    ctx: &mut C,
    header: &BlockHeaderHash,
    path: &[u8],
    value: &[u8],
    root_to_block: Option<HashMap<TrieHash, BlockHeaderHash>>,
) -> HashMap<TrieHash, BlockHeaderHash>
where
    C: MarfCore<BlockHeaderHash> + MarfRootTable<BlockHeaderHash>,
{
    ctx.open_block(header, None).unwrap();
    let root_hash = ctx
        .with_read_ctx(|read_ctx| {
            read_ctx.with_read_state(|storage, _cursor, decode_scratch| {
                Trie::read_root(storage, decode_scratch).and_then(|read| {
                    read.into_hash().and_then(|hash| {
                        hash.ok_or_else(|| {
                            Error::CorruptionError("Missing node hash in trie read".to_string())
                        })
                    })
                })
            })
        })
        .expect("Failed to get current trie root");
    let triepath = TrieHash::from_bytes(path).unwrap();

    let mut marf_value = [0u8; 40];
    marf_value.copy_from_slice(&value[0..40]);

    test_debug!("---------");
    test_debug!(
        "MARF merkle prove: verify_marf_merkle_proof({:?}, {:?}, {:?})?",
        header,
        path,
        value
    );
    test_debug!("MARF merkle verify target root hash: {:?}", &root_hash);
    test_debug!("MARF merkle verify source block: {:?}", header);
    test_debug!("---------");

    let proof = ctx
        .prove_path(header, &triepath, &MARFValue(marf_value))
        .unwrap();
    let root_to_block = root_to_block.unwrap_or_else(|| ctx.current_root_to_block_table());

    test_debug!("---------");
    test_debug!("MARF merkle verify: {:?}", &proof);
    test_debug!("MARF merkle verify target root hash: {:?}", &root_hash);
    test_debug!("MARF merkle verify source block: {:?}", header);
    test_debug!("MARF root-to-block: {:?}", &root_to_block);
    test_debug!("---------");

    assert!(proof.verify(
        &triepath,
        &MARFValue(marf_value),
        &root_hash,
        &root_to_block
    ));

    root_to_block
}

pub fn make_node_path<Db: Deref<Target = Connection>>(
    s: &mut TrieStorageConnection<BlockHeaderHash, Db>,
    node_id: u8,
    path_segments: &[(Vec<u8>, u8)],
    leaf_data: Vec<u8>,
) -> (Vec<TrieNodeType>, Vec<TriePtr>, Vec<TrieHash>) {
    // make a fully-fleshed-out path of node's to a leaf
    let root_ptr = u32::try_from(s.root_ptr()).unwrap();
    let root = TrieNode256::new(&path_segments[0].0);
    let root_hash = TrieHash::from_data(&[0u8; 32]); // don't care about this in this test
    s.write_node(root_ptr, &root, root_hash).unwrap();

    let mut parent = TrieNodeType::Node256(Box::new(root));
    let mut parent_ptr = root_ptr;

    let mut nodes = vec![];
    let mut node_ptrs = vec![];
    let mut hashes = vec![];
    let mut seg_id = 0;

    for i in 0..path_segments.len() - 1 {
        let path_segment = &path_segments[i + 1].0;
        let chr = path_segments[i].1;
        let node_ptr = s.last_ptr().unwrap();

        let node = match TrieNodeID::from_u8(node_id).unwrap() {
            TrieNodeID::Node4 => TrieNodeType::Node4(TrieNode4::new(path_segment)),
            TrieNodeID::Node16 => TrieNodeType::Node16(TrieNode16::new(path_segment)),
            TrieNodeID::Node48 => TrieNodeType::Node48(Box::new(TrieNode48::new(path_segment))),
            TrieNodeID::Node256 => TrieNodeType::Node256(Box::new(TrieNode256::new(path_segment))),
            _ => panic!("invalid node ID"),
        };

        s.write_nodetype(
            node_ptr,
            &node,
            TrieHash::from_data(&[(seg_id + 1) as u8; 32]),
        )
        .unwrap();

        // update parent
        match parent {
            TrieNodeType::Node256(ref mut data) => {
                assert!(data.insert(&TriePtr::new(node_id, chr, node_ptr.into())))
            }
            TrieNodeType::Node48(ref mut data) => {
                assert!(data.insert(&TriePtr::new(node_id, chr, node_ptr.into())))
            }
            TrieNodeType::Node16(ref mut data) => {
                assert!(data.insert(&TriePtr::new(node_id, chr, node_ptr.into())))
            }
            TrieNodeType::Node4(ref mut data) => {
                assert!(data.insert(&TriePtr::new(node_id, chr, node_ptr.into())))
            }
            TrieNodeType::Leaf(_) => panic!("can't insert into leaf"),
        };

        s.write_nodetype(
            parent_ptr,
            &parent,
            TrieHash::from_data(&[seg_id as u8; 32]),
        )
        .unwrap();

        nodes.push(parent.clone());
        node_ptrs.push(TriePtr::new(node_id, chr, node_ptr.into()));
        hashes.push(TrieHash::from_data(&[(seg_id + 1) as u8; 32]));

        parent = node;
        parent_ptr = node_ptr;

        seg_id += 1;
    }

    // add a leaf at the end
    let child = TrieLeaf::new(&path_segments[path_segments.len() - 1].0, &leaf_data);
    let child_chr = path_segments[path_segments.len() - 1].1;
    let child_ptr = s.last_ptr().unwrap();
    s.write_node(
        child_ptr,
        &child,
        TrieHash::from_data(&[(seg_id + 1) as u8; 32]),
    )
    .unwrap();

    // update parent
    match parent {
        TrieNodeType::Node256(ref mut data) => {
            assert!(data.insert(&TriePtr::new(
                TrieNodeID::Leaf as u8,
                child_chr,
                child_ptr.into()
            )))
        }
        TrieNodeType::Node48(ref mut data) => {
            assert!(data.insert(&TriePtr::new(
                TrieNodeID::Leaf as u8,
                child_chr,
                child_ptr.into()
            )))
        }
        TrieNodeType::Node16(ref mut data) => {
            assert!(data.insert(&TriePtr::new(
                TrieNodeID::Leaf as u8,
                child_chr,
                child_ptr.into()
            )))
        }
        TrieNodeType::Node4(ref mut data) => {
            assert!(data.insert(&TriePtr::new(
                TrieNodeID::Leaf as u8,
                child_chr,
                child_ptr.into()
            )))
        }
        TrieNodeType::Leaf(_) => panic!("can't insert into leaf"),
    };

    s.write_nodetype(
        parent_ptr,
        &parent,
        TrieHash::from_data(&[(seg_id) as u8; 32]),
    )
    .unwrap();

    nodes.push(parent.clone());
    node_ptrs.push(TriePtr::new(
        TrieNodeID::Leaf as u8,
        child_chr,
        child_ptr.into(),
    ));
    hashes.push(TrieHash::from_data(&[(seg_id + 1) as u8; 32]));

    (nodes, node_ptrs, hashes)
}

pub fn make_node4_path<Db: Deref<Target = Connection>>(
    s: &mut TrieStorageConnection<BlockHeaderHash, Db>,
    path_segments: &[(Vec<u8>, u8)],
    leaf_data: Vec<u8>,
) -> (Vec<TrieNodeType>, Vec<TriePtr>, Vec<TrieHash>) {
    make_node_path(s, TrieNodeID::Node4 as u8, path_segments, leaf_data)
}

/// Generates deterministic test insert data as key–value pairs per block
pub fn make_test_insert_data(
    num_inserts_per_block: u64,
    num_blocks: u64,
) -> Vec<Vec<(String, MARFValue)>> {
    let mut data = vec![0u8; 32];
    let mut ret = vec![];

    for blk in 0..num_blocks {
        let mut block_data = vec![];
        test_debug!("Make block {}", blk);
        for val in 0..num_inserts_per_block {
            let path_bytes = Sha512Trunc256Sum::from_data(&data).as_bytes().to_vec();
            data.copy_from_slice(&path_bytes[0..32]);

            let path = to_hex(&path_bytes);

            let value_bytes = Sha512Trunc256Sum::from_data(&data).as_bytes().to_vec();
            data.copy_from_slice(&value_bytes[0..32]);

            let mut value_bytes_slice = [0u8; 40];
            value_bytes_slice[0..32].copy_from_slice(&value_bytes);

            let value = MARFValue(value_bytes_slice);
            block_data.push((path, value));
        }
        ret.push(block_data);
    }
    ret
}

pub mod opts {
    use std::sync::LazyLock;

    use crate::chainstate::stacks::index::marf::MARFOpenOpts;
    use crate::chainstate::stacks::index::storage::TrieHashCalculationMode;

    pub static OPTS_IMM: LazyLock<MARFOpenOpts> =
        LazyLock::new(|| MARFOpenOpts::new(TrieHashCalculationMode::Immediate, false));
    pub static OPTS_IMM_EXT: LazyLock<MARFOpenOpts> =
        LazyLock::new(|| MARFOpenOpts::new(TrieHashCalculationMode::Immediate, true));
    pub static OPTS_IMM_COMP: LazyLock<MARFOpenOpts> =
        LazyLock::new(|| OPTS_IMM.clone().with_compression(true));
    pub static OPTS_IMM_EXT_COMP: LazyLock<MARFOpenOpts> =
        LazyLock::new(|| OPTS_IMM_EXT.clone().with_compression(true));
    pub static OPTS_DEF: LazyLock<MARFOpenOpts> =
        LazyLock::new(|| MARFOpenOpts::new(TrieHashCalculationMode::Deferred, false));
    pub static OPTS_DEF_EXT: LazyLock<MARFOpenOpts> =
        LazyLock::new(|| MARFOpenOpts::new(TrieHashCalculationMode::Deferred, true));
    pub static OPTS_DEF_COMP: LazyLock<MARFOpenOpts> =
        LazyLock::new(|| OPTS_DEF.clone().with_compression(true));
    pub static OPTS_DEF_EXT_COMP: LazyLock<MARFOpenOpts> =
        LazyLock::new(|| OPTS_DEF_EXT.clone().with_compression(true));

    // Mmap variants of the external-blob configs (for rstest parameterized tests).
    pub static OPTS_IMM_EXT_MMAP: LazyLock<MARFOpenOpts> =
        LazyLock::new(|| OPTS_IMM_EXT.clone().with_mmap(true));
    pub static OPTS_DEF_EXT_MMAP: LazyLock<MARFOpenOpts> =
        LazyLock::new(|| OPTS_DEF_EXT.clone().with_mmap(true));
    pub static OPTS_IMM_EXT_COMP_MMAP: LazyLock<MARFOpenOpts> =
        LazyLock::new(|| OPTS_IMM_EXT_COMP.clone().with_mmap(true));
    pub static OPTS_DEF_EXT_COMP_MMAP: LazyLock<MARFOpenOpts> =
        LazyLock::new(|| OPTS_DEF_EXT_COMP.clone().with_mmap(true));

    /// All meaningful combinations: the 8 base configurations plus the 4 external-blob mmap
    /// variants. Mmap has no effect without external blob storage, so those duplicates are omitted.
    pub static ALL_OPTS: LazyLock<Vec<MARFOpenOpts>> = LazyLock::new(|| {
        vec![
            OPTS_IMM.clone(),
            OPTS_IMM_EXT.clone(),
            OPTS_IMM_COMP.clone(),
            OPTS_IMM_EXT_COMP.clone(),
            OPTS_DEF.clone(),
            OPTS_DEF_EXT.clone(),
            OPTS_DEF_COMP.clone(),
            OPTS_DEF_EXT_COMP.clone(),
            OPTS_IMM_EXT_MMAP.clone(),
            OPTS_IMM_EXT_COMP_MMAP.clone(),
            OPTS_DEF_EXT_MMAP.clone(),
            OPTS_DEF_EXT_COMP_MMAP.clone(),
        ]
    });

    #[template]
    #[rstest]
    #[case::imm(&opts::OPTS_IMM)]
    #[case::imm_ext(&opts::OPTS_IMM_EXT)]
    #[case::imm_comp(&opts::OPTS_IMM_COMP)]
    #[case::imm_ext_comp(&opts::OPTS_IMM_EXT_COMP)]
    #[case::def(&opts::OPTS_DEF)]
    #[case::def_ext(&opts::OPTS_DEF_EXT)]
    #[case::def_comp(&opts::OPTS_DEF_COMP)]
    #[case::def_ext_comp(&opts::OPTS_DEF_EXT_COMP)]
    #[case::imm_ext_mmap(&opts::OPTS_IMM_EXT_MMAP)]
    #[case::imm_ext_comp_mmap(&opts::OPTS_IMM_EXT_COMP_MMAP)]
    #[case::def_ext_mmap(&opts::OPTS_DEF_EXT_MMAP)]
    #[case::def_ext_comp_mmap(&opts::OPTS_DEF_EXT_COMP_MMAP)]
    pub fn tpl_all_opts(#[case] marf_opts: &MARFOpenOpts) {}
}

mod cache;

mod record_format;
