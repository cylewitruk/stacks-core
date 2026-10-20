//! Deterministic operation-sequence generators.

use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha20Rng;

use crate::ops::{block_hex, sentinel_hex, value_hex_from_str, BlockHex, Op, OpenOpts, Tamper};

/// Knobs for [`generate`].
#[derive(Debug, Clone)]
pub struct GenConfig {
    pub opts: OpenOpts,
    /// Number of blocks to build.
    pub blocks: usize,
    /// Inserts per block (uniform in `1..=max`).
    pub max_inserts_per_block: usize,
    /// Size of the key pool; smaller pools mean more overwrites.
    pub key_pool: usize,
    /// Probability (percent) that a new block forks off a non-tip ancestor.
    pub fork_pct: u32,
    /// Probability (percent) that a block is abandoned instead of committed.
    pub drop_pct: u32,
    /// Probability (percent) that a block is committed under a different id.
    pub commit_to_pct: u32,
    /// Probability (percent) of a reopen after a commit.
    pub reopen_pct: u32,
    /// Reads issued after every commit.
    pub reads_per_block: usize,
    /// Proof queries issued after every commit.
    pub proofs_per_block: usize,
    /// Use `InsertBatch` instead of single inserts.
    pub batch: bool,
}

impl Default for GenConfig {
    fn default() -> Self {
        GenConfig {
            opts: OpenOpts::default(),
            blocks: 12,
            max_inserts_per_block: 8,
            key_pool: 24,
            fork_pct: 20,
            drop_pct: 10,
            commit_to_pct: 10,
            reopen_pct: 15,
            reads_per_block: 6,
            proofs_per_block: 1,
            batch: false,
        }
    }
}

fn key(i: usize) -> String {
    format!("key::{i}")
}

/// Generate a deterministic op sequence for `seed`.
pub fn generate(seed: u64, cfg: &GenConfig) -> Vec<Op> {
    let mut rng = ChaCha20Rng::seed_from_u64(seed);
    let mut ops = vec![Op::Open(cfg.opts)];
    // committed blocks: (id, height)
    let mut committed: Vec<(BlockHex, u32)> = vec![];
    let mut next_block: u32 = 1;
    let mut version: u64 = 0;

    for _ in 0..cfg.blocks {
        let (parent, height) = if committed.is_empty() {
            (sentinel_hex(), 0u32)
        } else if rng.gen_range(0..100) < cfg.fork_pct {
            let (p, h) = committed.choose(&mut rng).unwrap().clone();
            (p, h + 1)
        } else {
            let (p, h) = committed.last().unwrap().clone();
            (p, h + 1)
        };
        let child = block_hex(next_block);
        next_block += 1;
        ops.push(Op::Begin {
            parent: parent.clone(),
            child: child.clone(),
        });

        let n = rng.gen_range((cfg.max_inserts_per_block / 2).max(1)..=cfg.max_inserts_per_block);
        let mut entries = vec![];
        for _ in 0..n {
            let k = key(rng.gen_range(0..cfg.key_pool));
            version += 1;
            let v = value_hex_from_str(&format!("value-{version}"));
            entries.push((k, v));
        }
        if cfg.batch {
            ops.push(Op::InsertBatch { entries });
        } else {
            for (k, v) in entries {
                ops.push(Op::Insert { key: k, value: v });
            }
        }
        // read our own uncommitted writes through the transaction
        ops.push(Op::Get {
            tip: child.clone(),
            key: key(rng.gen_range(0..cfg.key_pool)),
        });
        ops.push(Op::OpenTip);

        let roll = rng.gen_range(0..100);
        if roll < cfg.drop_pct {
            ops.push(Op::DropCurrent);
            continue;
        }
        let final_id = if roll < cfg.drop_pct + cfg.commit_to_pct {
            let real = block_hex(next_block);
            next_block += 1;
            ops.push(Op::CommitTo {
                block: real.clone(),
            });
            real
        } else {
            ops.push(Op::Commit);
            child
        };
        committed.push((final_id.clone(), height));

        if rng.gen_range(0..100) < cfg.reopen_pct {
            ops.push(Op::Close);
            ops.push(Op::Open(cfg.opts));
        }

        for _ in 0..cfg.reads_per_block {
            let (tip, h) = committed.choose(&mut rng).unwrap().clone();
            match rng.gen_range(0..5) {
                0 => ops.push(Op::RootHash { tip }),
                1 => ops.push(Op::BlockAtHeight {
                    tip,
                    height: rng.gen_range(0..=h + 1),
                }),
                2 => {
                    let (b, _) = committed.choose(&mut rng).unwrap().clone();
                    ops.push(Op::BlockHeight { tip, block: b });
                }
                3 => {
                    let (b, _) = committed.choose(&mut rng).unwrap().clone();
                    ops.push(Op::CheckAncestor { tip, block: b });
                }
                _ => ops.push(Op::Get {
                    tip,
                    key: key(rng.gen_range(0..cfg.key_pool + 2)),
                }),
            }
        }
        for _ in 0..cfg.proofs_per_block {
            let (tip, _) = committed.choose(&mut rng).unwrap().clone();
            ops.push(Op::GetWithProof {
                tip,
                key: key(rng.gen_range(0..cfg.key_pool)),
            });
        }
    }
    // final sweep: the root hash of every committed tip and a deterministic
    // sample of keys at a bounded number of tips
    let sweep_keys = cfg.key_pool.min(48);
    let tip_stride = (committed.len() * sweep_keys / 4000).max(1);
    for (tip, _) in committed.iter() {
        ops.push(Op::RootHash { tip: tip.clone() });
    }
    for (tip, _) in committed.iter().step_by(tip_stride) {
        for i in 0..sweep_keys {
            let k = if cfg.key_pool <= sweep_keys {
                i
            } else {
                rng.gen_range(0..cfg.key_pool)
            };
            ops.push(Op::Get {
                tip: tip.clone(),
                key: key(k),
            });
        }
    }
    ops.push(Op::Close);
    ops
}

/// A sequence exercising the unconfirmed-trie lifecycle.
pub fn generate_unconfirmed(seed: u64, opts: OpenOpts) -> Vec<Op> {
    let mut rng = ChaCha20Rng::seed_from_u64(seed);
    let confirmed = OpenOpts {
        unconfirmed: false,
        ..opts
    };
    let unconfirmed = OpenOpts {
        unconfirmed: true,
        ..opts
    };
    let mut ops = vec![Op::Open(confirmed)];
    let b1 = block_hex(1);
    let b2 = block_hex(2);
    ops.push(Op::Begin {
        parent: sentinel_hex(),
        child: b1.clone(),
    });
    for i in 0..5 {
        ops.push(Op::Insert {
            key: key(i),
            value: value_hex_from_str(&format!("c1-{i}")),
        });
    }
    ops.push(Op::Commit);
    ops.push(Op::Begin {
        parent: b1.clone(),
        child: b2.clone(),
    });
    for i in 3..8 {
        ops.push(Op::Insert {
            key: key(i),
            value: value_hex_from_str(&format!("c2-{i}")),
        });
    }
    ops.push(Op::Commit);
    ops.push(Op::Close);
    ops.push(Op::Open(unconfirmed));
    // two rounds of extending the same unconfirmed trie, with a reopen between
    for round in 0..3 {
        ops.push(Op::BeginUnconfirmed { parent: b2.clone() });
        for _ in 0..4 {
            let k = key(rng.gen_range(0..10));
            ops.push(Op::Insert {
                key: k,
                value: value_hex_from_str(&format!("u{round}-{}", rng.gen::<u32>())),
            });
        }
        ops.push(Op::OpenTip);
        ops.push(Op::Commit);
        ops.push(Op::Close);
        ops.push(Op::Open(unconfirmed));
    }
    // reads of unconfirmed and confirmed state
    let utip = stacks_common::util::hash::to_hex(
        stacks_marf::marf::MARF::<
            stacks_common::types::chainstate::StacksBlockId,
        >::make_unconfirmed_chain_tip(&{
            let bytes = stacks_common::util::hash::hex_bytes(&b2).unwrap();
            let mut a = [0u8; 32];
            a.copy_from_slice(&bytes);
            stacks_common::types::chainstate::StacksBlockId(a)
        })
        .as_bytes(),
    );
    for k in 0..10 {
        ops.push(Op::Get {
            tip: utip.clone(),
            key: key(k),
        });
        ops.push(Op::Get {
            tip: b2.clone(),
            key: key(k),
        });
    }
    ops.push(Op::RootHash { tip: utip.clone() });
    ops.push(Op::RootHash { tip: b2.clone() });
    ops.push(Op::BlockHeight {
        tip: utip.clone(),
        block: b1.clone(),
    });
    // drop the unconfirmed state and check it is gone
    ops.push(Op::BeginUnconfirmed { parent: b2.clone() });
    ops.push(Op::DropUnconfirmed);
    ops.push(Op::Get {
        tip: utip,
        key: key(0),
    });
    ops.push(Op::Close);
    ops
}

/// A linear chain with one fork, pruned at `squash_height`, then read and
/// extended: historical roots, height lookups, rejected historical reads,
/// and root-hash equality of blocks built on top of the squashed state.
pub fn generate_prune(seed: u64, opts: OpenOpts, blocks: usize, squash_height: u32) -> Vec<Op> {
    let mut rng = ChaCha20Rng::seed_from_u64(seed);
    let mut ops = vec![Op::Open(opts)];
    let mut chain: Vec<BlockHex> = vec![];
    let mut next_block: u32 = 1;
    let mut version = 0u64;
    for _ in 0..blocks {
        let parent = chain.last().cloned().unwrap_or_else(sentinel_hex);
        let child = block_hex(next_block);
        next_block += 1;
        ops.push(Op::Begin {
            parent,
            child: child.clone(),
        });
        for _ in 0..rng.gen_range(1..=6) {
            version += 1;
            ops.push(Op::Insert {
                key: key(rng.gen_range(0..40)),
                value: value_hex_from_str(&format!("v{version}")),
            });
        }
        ops.push(Op::Commit);
        chain.push(child);
    }
    // a fork off the middle that must not be part of the squash
    let fork_parent = chain[chain.len() / 2].clone();
    let fork = block_hex(next_block);
    next_block += 1;
    ops.push(Op::Begin {
        parent: fork_parent,
        child: fork.clone(),
    });
    ops.push(Op::Insert {
        key: key(0),
        value: value_hex_from_str("fork"),
    });
    ops.push(Op::Commit);
    let tip = chain.last().unwrap().clone();
    // reference roots before squashing
    for b in chain.iter() {
        ops.push(Op::RootHash { tip: b.clone() });
    }
    ops.push(Op::Prune {
        tip: tip.clone(),
        height: squash_height,
    });
    ops.push(Op::Open(OpenOpts {
        external_blobs: true,
        compress: false,
        defer_hashing: true,
        unconfirmed: false,
    }));
    let squashed_tip = chain[squash_height as usize].clone();
    for (h, b) in chain.iter().enumerate().take(squash_height as usize + 1) {
        ops.push(Op::RootHash { tip: b.clone() });
        ops.push(Op::BlockAtHeight {
            tip: squashed_tip.clone(),
            height: h as u32,
        });
        ops.push(Op::BlockHeight {
            tip: squashed_tip.clone(),
            block: b.clone(),
        });
        ops.push(Op::Get {
            tip: b.clone(),
            key: key(1),
        });
    }
    for k in 0..40 {
        ops.push(Op::Get {
            tip: squashed_tip.clone(),
            key: key(k),
        });
    }
    ops.push(Op::GetWithProof {
        tip: squashed_tip.clone(),
        key: key(1),
    });
    ops.push(Op::Get {
        tip: fork.clone(),
        key: key(0),
    });
    // extend the squashed MARF with the same writes the archival chain had
    let mut parent = squashed_tip;
    for _ in 0..4 {
        let child = block_hex(next_block);
        next_block += 1;
        ops.push(Op::Begin {
            parent: parent.clone(),
            child: child.clone(),
        });
        for _ in 0..5 {
            version += 1;
            ops.push(Op::Insert {
                key: key(rng.gen_range(0..40)),
                value: value_hex_from_str(&format!("post{version}")),
            });
        }
        ops.push(Op::Seal);
        ops.push(Op::Commit);
        ops.push(Op::RootHash { tip: child.clone() });
        ops.push(Op::BlockAtHeight {
            tip: child.clone(),
            height: squash_height / 2,
        });
        parent = child;
    }
    ops.push(Op::Close);
    ops
}

/// Deep patch chains: a tiny key pool rewritten in every block so the same
/// nodes are copied forward and patched block after block, past
/// `MAX_PATCH_DEPTH`.
pub fn generate_patch_chain(seed: u64, opts: OpenOpts, blocks: usize) -> Vec<Op> {
    let mut rng = ChaCha20Rng::seed_from_u64(seed);
    let mut ops = vec![Op::Open(opts)];
    let mut parent = sentinel_hex();
    let mut tips = vec![];
    for b in 0..blocks {
        let child = block_hex(b as u32 + 1);
        ops.push(Op::Begin {
            parent: parent.clone(),
            child: child.clone(),
        });
        // rewrite every key of a 5-key pool plus one fresh key per block
        let mut entries: Vec<(String, ValueHexAlias)> = (0..5)
            .map(|k| (key(k), value_hex_from_str(&format!("b{b}-k{k}"))))
            .collect();
        entries.push((
            format!("fresh::{b}"),
            value_hex_from_str(&format!("fresh{b}")),
        ));
        if rng.gen_bool(0.5) {
            ops.push(Op::InsertBatch { entries });
        } else {
            for (k, v) in entries {
                ops.push(Op::Insert { key: k, value: v });
            }
        }
        ops.push(Op::Commit);
        if b % 3 == 2 {
            ops.push(Op::Close);
            ops.push(Op::Open(opts));
        }
        tips.push(child.clone());
        parent = child;
    }
    for tip in &tips {
        ops.push(Op::RootHash { tip: tip.clone() });
        for k in 0..5 {
            ops.push(Op::Get {
                tip: tip.clone(),
                key: key(k),
            });
        }
        ops.push(Op::GetWithProof {
            tip: tip.clone(),
            key: key(2),
        });
    }
    ops.push(Op::Close);
    ops
}

type ValueHexAlias = String;

/// Error paths and edge cases, written out explicitly.
pub fn generate_edge_cases(opts: OpenOpts) -> Vec<Op> {
    let s = sentinel_hex();
    let b1 = block_hex(1);
    let b2 = block_hex(2);
    let b3 = block_hex(3);
    let b4 = block_hex(4);
    let missing = block_hex(999);
    let v = |t: &str| value_hex_from_str(t);
    let mut ops = vec![Op::Open(opts)];
    // reads on an empty MARF
    ops.push(Op::Get {
        tip: missing.clone(),
        key: key(0),
    });
    ops.push(Op::RootHash {
        tip: missing.clone(),
    });
    ops.push(Op::BlockAtHeight {
        tip: missing.clone(),
        height: 0,
    });
    ops.push(Op::BlockHeight {
        tip: missing.clone(),
        block: missing.clone(),
    });
    ops.push(Op::CheckAncestor {
        tip: missing.clone(),
        block: missing.clone(),
    });
    ops.push(Op::Insert {
        key: key(0),
        value: v("no-begin"),
    });
    ops.push(Op::Commit);
    ops.push(Op::OpenTip);
    // begin on a missing parent
    ops.push(Op::Begin {
        parent: missing.clone(),
        child: b1.clone(),
    });
    // genesis
    ops.push(Op::Begin {
        parent: s.clone(),
        child: b1.clone(),
    });
    ops.push(Op::Begin {
        parent: s.clone(),
        child: b2.clone(),
    }); // in progress
    ops.push(Op::Insert {
        key: key(0),
        value: v("one"),
    });
    ops.push(Op::Get {
        tip: b1.clone(),
        key: key(0),
    }); // own uncommitted write
    ops.push(Op::Get {
        tip: missing.clone(),
        key: key(0),
    });
    ops.push(Op::BlockHeight {
        tip: b1.clone(),
        block: b1.clone(),
    });
    ops.push(Op::BlockHeight {
        tip: b1.clone(),
        block: s.clone(),
    });
    ops.push(Op::BlockAtHeight {
        tip: b1.clone(),
        height: 0,
    });
    ops.push(Op::BlockAtHeight {
        tip: b1.clone(),
        height: 5,
    });
    ops.push(Op::Seal);
    ops.push(Op::RootHash { tip: b1.clone() }); // sealed, uncommitted
    ops.push(Op::Commit);
    // existing block, in both roles
    ops.push(Op::Begin {
        parent: s.clone(),
        child: b1.clone(),
    });
    ops.push(Op::Begin {
        parent: b1.clone(),
        child: b1.clone(),
    });
    // drop releases the block id for reuse
    ops.push(Op::Begin {
        parent: b1.clone(),
        child: b2.clone(),
    });
    ops.push(Op::Insert {
        key: key(1),
        value: v("dropped"),
    });
    ops.push(Op::DropCurrent);
    ops.push(Op::Get {
        tip: b2.clone(),
        key: key(1),
    });
    ops.push(Op::Begin {
        parent: b1.clone(),
        child: b2.clone(),
    });
    ops.push(Op::Insert {
        key: key(1),
        value: v("kept"),
    });
    ops.push(Op::Commit);
    ops.push(Op::Get {
        tip: b2.clone(),
        key: key(1),
    });
    ops.push(Op::Get {
        tip: b2.clone(),
        key: key(0),
    });
    // commit_to an id that already exists, and to a fresh one
    ops.push(Op::Begin {
        parent: b2.clone(),
        child: b3.clone(),
    });
    ops.push(Op::Insert {
        key: key(2),
        value: v("three"),
    });
    ops.push(Op::CommitTo { block: b1.clone() });
    // Consumers always `open_block` a tip before reading; without it the
    // original's reads fail with NotFound after a failed commit because its
    // "current block" cursor still names the abandoned trie.
    ops.push(Op::CheckAncestor {
        tip: b2.clone(),
        block: b2.clone(),
    });
    ops.push(Op::Get {
        tip: b3.clone(),
        key: key(2),
    });
    ops.push(Op::Begin {
        parent: b2.clone(),
        child: b3.clone(),
    });
    ops.push(Op::Insert {
        key: key(2),
        value: v("three-again"),
    });
    ops.push(Op::CommitTo { block: b4.clone() });
    ops.push(Op::Get {
        tip: b3.clone(),
        key: key(2),
    });
    ops.push(Op::Get {
        tip: b4.clone(),
        key: key(2),
    });
    ops.push(Op::BlockHeight {
        tip: b4.clone(),
        block: b4.clone(),
    });
    ops.push(Op::BlockHeight {
        tip: b4.clone(),
        block: b3.clone(),
    });
    ops.push(Op::BlockHeight {
        tip: b4.clone(),
        block: b2.clone(),
    });
    ops.push(Op::BlockAtHeight {
        tip: b4.clone(),
        height: 2,
    });
    ops.push(Op::BlockAtHeight {
        tip: b4.clone(),
        height: 1,
    });
    // a child of the renamed block fixes up the height mapping
    let b5 = block_hex(5);
    ops.push(Op::Begin {
        parent: b4.clone(),
        child: b5.clone(),
    });
    ops.push(Op::Commit); // empty block (only height keys)
    ops.push(Op::BlockHeight {
        tip: b5.clone(),
        block: b4.clone(),
    });
    ops.push(Op::BlockAtHeight {
        tip: b5.clone(),
        height: 2,
    });
    ops.push(Op::RootHash { tip: b5.clone() });
    ops.push(Op::GetWithProof {
        tip: b5.clone(),
        key: key(2),
    });
    ops.push(Op::GetWithProof {
        tip: b5.clone(),
        key: key(0),
    });
    // mined blocks are not readable through the forest
    let mined = block_hex(6);
    ops.push(Op::Begin {
        parent: b5.clone(),
        child: block_hex(7),
    });
    ops.push(Op::Insert {
        key: key(3),
        value: v("mined"),
    });
    ops.push(Op::CommitMined {
        block: mined.clone(),
    });
    ops.push(Op::CheckAncestor {
        tip: b5.clone(),
        block: b5.clone(),
    });
    ops.push(Op::Get {
        tip: mined.clone(),
        key: key(3),
    });
    ops.push(Op::Get {
        tip: block_hex(7),
        key: key(3),
    });
    ops.push(Op::Begin {
        parent: b5.clone(),
        child: block_hex(7),
    }); // id is free again
    ops.push(Op::Commit);
    // ancestry checks
    ops.push(Op::CheckAncestor {
        tip: b5.clone(),
        block: b2.clone(),
    });
    ops.push(Op::CheckAncestor {
        tip: b5.clone(),
        block: b3.clone(),
    });
    ops.push(Op::CheckAncestor {
        tip: b5.clone(),
        block: missing.clone(),
    });
    ops.push(Op::CheckAncestor {
        tip: b2.clone(),
        block: b5.clone(),
    });
    ops.push(Op::CheckAncestor {
        tip: b5.clone(),
        block: b5.clone(),
    });
    // unconfirmed ops on a confirmed handle
    ops.push(Op::BeginUnconfirmed { parent: b5.clone() });
    ops.push(Op::DropUnconfirmed);
    // read-only reopen is not modelled; close
    ops.push(Op::Close);
    // unconfirmed handle: parent must be confirmed
    ops.push(Op::Open(OpenOpts {
        unconfirmed: true,
        ..opts
    }));
    ops.push(Op::BeginUnconfirmed {
        parent: missing.clone(),
    });
    ops.push(Op::Begin {
        parent: b5.clone(),
        child: block_hex(8),
    }); // confirmed begin on unconfirmed handle
    ops.push(Op::BeginUnconfirmed { parent: b5.clone() });
    ops.push(Op::Insert {
        key: key(4),
        value: v("u"),
    });
    ops.push(Op::CommitTo {
        block: block_hex(9),
    }); // not allowed for unconfirmed
    ops.push(Op::Close);
    ops
}

/// Handle-level lifecycle (`MARF::begin` / `insert` / `commit` / `drop_current`
/// on the handle rather than through a transaction), including the
/// begin -> drop_current -> begin-again sequence that must release the lock.
pub fn generate_lifecycle(opts: OpenOpts) -> Vec<Op> {
    let s = sentinel_hex();
    let v = |t: &str| value_hex_from_str(t);
    let b = block_hex;
    let mut ops = vec![Op::Open(opts)];
    ops.push(Op::Commit); // nothing open: both implementations succeed
    ops.push(Op::Insert {
        key: key(0),
        value: v("no-block"),
    });
    ops.push(Op::DropCurrent);
    ops.push(Op::OpenTip);
    ops.push(Op::BeginHandle {
        parent: s.clone(),
        child: b(1),
    });
    ops.push(Op::OpenTip);
    ops.push(Op::Insert {
        key: key(0),
        value: v("one"),
    });
    ops.push(Op::InsertBatch {
        entries: vec![(key(1), v("two")), (key(2), v("three"))],
    });
    ops.push(Op::Get {
        tip: b(1),
        key: key(1),
    });
    ops.push(Op::BeginHandle {
        parent: s.clone(),
        child: b(2),
    }); // in progress
    ops.push(Op::Begin {
        parent: s.clone(),
        child: b(2),
    }); // in progress
    ops.push(Op::Seal);
    ops.push(Op::RootHash { tip: b(1) });
    ops.push(Op::Commit);
    ops.push(Op::RootHash { tip: b(1) });
    // begin, drop on the handle, begin the same block again
    ops.push(Op::BeginHandle {
        parent: b(1),
        child: b(2),
    });
    ops.push(Op::Insert {
        key: key(3),
        value: v("dropped"),
    });
    ops.push(Op::DropCurrent);
    ops.push(Op::OpenTip);
    ops.push(Op::Get {
        tip: b(2),
        key: key(3),
    });
    ops.push(Op::BeginHandle {
        parent: b(1),
        child: b(2),
    });
    ops.push(Op::Insert {
        key: key(3),
        value: v("kept"),
    });
    ops.push(Op::CommitTo { block: b(3) });
    ops.push(Op::Get {
        tip: b(3),
        key: key(3),
    });
    ops.push(Op::Get {
        tip: b(2),
        key: key(3),
    });
    // the dropped lock survives a reopen too
    ops.push(Op::BeginHandle {
        parent: b(3),
        child: b(4),
    });
    ops.push(Op::Insert {
        key: key(4),
        value: v("x"),
    });
    ops.push(Op::DropCurrent);
    ops.push(Op::Close);
    ops.push(Op::Open(opts));
    ops.push(Op::BeginHandle {
        parent: b(3),
        child: b(4),
    });
    ops.push(Op::Insert {
        key: key(4),
        value: v("y"),
    });
    ops.push(Op::CommitMined { block: b(5) });
    ops.push(Op::OpenTip);
    ops.push(Op::BeginHandle {
        parent: b(3),
        child: b(4),
    });
    ops.push(Op::Commit);
    ops.push(Op::Get {
        tip: b(4),
        key: key(4),
    });
    ops.push(Op::Get {
        tip: b(4),
        key: key(3),
    });
    ops.push(Op::GetWithProof {
        tip: b(4),
        key: key(0),
    });
    ops.push(Op::DropUnconfirmed); // not an unconfirmed handle: no-op
    ops.push(Op::Close);
    ops
}

/// Proof verification with tampered proofs, values, roots and block maps.
pub fn generate_proofs(seed: u64, opts: OpenOpts) -> Vec<Op> {
    let mut rng = ChaCha20Rng::seed_from_u64(seed);
    let mut ops = vec![Op::Open(opts)];
    let mut parent = sentinel_hex();
    let mut tips = vec![];
    for i in 0..12u32 {
        let child = block_hex(i + 1);
        ops.push(Op::Begin {
            parent: parent.clone(),
            child: child.clone(),
        });
        for _ in 0..rng.gen_range(2..=8) {
            ops.push(Op::Insert {
                key: key(rng.gen_range(0..30)),
                value: value_hex_from_str(&format!("p{}", rng.gen::<u64>())),
            });
        }
        ops.push(Op::Commit);
        tips.push(child.clone());
        parent = child;
    }
    ops.push(Op::Close);
    ops.push(Op::Open(opts));
    for _ in 0..40 {
        let tip = tips[rng.gen_range(0..tips.len())].clone();
        let k = key(rng.gen_range(0..30));
        let tamper = match rng.gen_range(0..6) {
            0 => Tamper::None,
            1 => Tamper::FlipByte(rng.gen_range(0..4000)),
            2 => Tamper::Truncate(rng.gen_range(0..600)),
            3 => Tamper::WrongValue,
            4 => Tamper::WrongRoot,
            _ => Tamper::EmptyBlockMap,
        };
        ops.push(Op::VerifyProof {
            tip,
            key: k,
            tamper,
        });
    }
    ops.push(Op::Close);
    ops
}

/// Fixtures mirroring the original crate's `marf_regression` parameters.
fn regression(seed: u64, blocks: usize, inserts: usize, pool: usize, batch: bool) -> Vec<Op> {
    let cfg = GenConfig {
        blocks,
        max_inserts_per_block: inserts,
        key_pool: pool,
        fork_pct: 0,
        drop_pct: 0,
        commit_to_pct: 0,
        reopen_pct: if blocks <= 8 { 100 } else { 5 },
        reads_per_block: 2,
        proofs_per_block: if blocks > 200 { 0 } else { 1 },
        batch,
        ..GenConfig::default()
    };
    generate(seed, &cfg)
}

/// The standard fixture set: (name, seed, ops).
pub fn standard_fixtures() -> Vec<(String, u64, Vec<Op>)> {
    let mut out = vec![];
    let variants: [(&str, OpenOpts); 4] = [
        (
            "ext_comp",
            OpenOpts {
                external_blobs: true,
                compress: true,
                defer_hashing: true,
                unconfirmed: false,
            },
        ),
        (
            "ext_plain",
            OpenOpts {
                external_blobs: true,
                compress: false,
                defer_hashing: true,
                unconfirmed: false,
            },
        ),
        (
            "sql_comp",
            OpenOpts {
                external_blobs: false,
                compress: true,
                defer_hashing: true,
                unconfirmed: false,
            },
        ),
        (
            "sql_plain_imm",
            OpenOpts {
                external_blobs: false,
                compress: false,
                defer_hashing: false,
                unconfirmed: false,
            },
        ),
    ];
    // Every storage variant replays the same seed, so consensus observations
    // (root hashes, proofs) must agree across variants.
    for (name, opts) in variants.iter() {
        let seed = 1000;
        let cfg = GenConfig {
            opts: *opts,
            ..GenConfig::default()
        };
        out.push((format!("small_{name}"), seed, generate(seed, &cfg)));
    }
    // a larger, denser sequence to force node promotions and many patches
    let cfg = GenConfig {
        blocks: 40,
        max_inserts_per_block: 60,
        key_pool: 400,
        fork_pct: 15,
        drop_pct: 5,
        commit_to_pct: 5,
        reopen_pct: 10,
        reads_per_block: 4,
        proofs_per_block: 1,
        batch: true,
        ..GenConfig::default()
    };
    out.push(("dense_ext_comp".into(), 2000, generate(2000, &cfg)));
    let cfg = GenConfig {
        opts: OpenOpts {
            compress: false,
            ..OpenOpts::default()
        },
        ..cfg
    };
    out.push(("dense_ext_plain".into(), 2001, generate(2001, &cfg)));
    // deep forks: many blocks, tiny key pool, frequent forking
    let cfg = GenConfig {
        blocks: 64,
        max_inserts_per_block: 3,
        key_pool: 6,
        fork_pct: 50,
        drop_pct: 10,
        commit_to_pct: 10,
        reopen_pct: 5,
        reads_per_block: 8,
        proofs_per_block: 1,
        batch: false,
        ..GenConfig::default()
    };
    out.push(("forks_ext_comp".into(), 3000, generate(3000, &cfg)));
    out.push((
        "unconfirmed_ext_comp".into(),
        4000,
        generate_unconfirmed(4000, OpenOpts::default()),
    ));
    // wide tries: enough keys per block to promote second-level nodes to Node48/Node256
    let cfg = GenConfig {
        blocks: 3,
        max_inserts_per_block: 6000,
        key_pool: 12000,
        fork_pct: 0,
        drop_pct: 0,
        commit_to_pct: 0,
        reopen_pct: 100,
        reads_per_block: 16,
        proofs_per_block: 2,
        batch: true,
        ..GenConfig::default()
    };
    out.push(("wide_ext_comp".into(), 6000, generate(6000, &cfg)));
    out.push((
        "lifecycle_ext_comp".into(),
        9000,
        generate_lifecycle(OpenOpts::default()),
    ));
    out.push((
        "lifecycle_sql_plain".into(),
        9001,
        generate_lifecycle(OpenOpts {
            external_blobs: false,
            compress: false,
            ..OpenOpts::default()
        }),
    ));
    out.push((
        "proofs_ext_comp".into(),
        9100,
        generate_proofs(9100, OpenOpts::default()),
    ));
    out.push((
        "regress_128x128".into(),
        9200,
        regression(9200, 128, 128, 4096, false),
    ));
    out.push((
        "regress_8x256_batch".into(),
        9201,
        regression(9201, 8, 256, 1024, true),
    ));
    out.push((
        "regress_1x256".into(),
        9202,
        regression(9202, 1, 256, 256, false),
    ));
    out.push((
        "regress_2048x1".into(),
        9203,
        regression(9203, 2048, 1, 64, false),
    ));
    out.push((
        "patch_chain_ext_comp".into(),
        7000,
        generate_patch_chain(7000, OpenOpts::default(), 14),
    ));
    out.push((
        "edge_cases_ext_comp".into(),
        8000,
        generate_edge_cases(OpenOpts::default()),
    ));
    out.push((
        "edge_cases_sql_plain".into(),
        8001,
        generate_edge_cases(OpenOpts {
            external_blobs: false,
            compress: false,
            ..OpenOpts::default()
        }),
    ));
    out.push((
        "prune_ext_comp".into(),
        5000,
        generate_prune(5000, OpenOpts::default(), 24, 16),
    ));
    out.push((
        "prune_sql_plain".into(),
        5001,
        generate_prune(
            5001,
            OpenOpts {
                external_blobs: false,
                compress: false,
                ..OpenOpts::default()
            },
            12,
            9,
        ),
    ));
    out.push((
        "unconfirmed_sql_plain".into(),
        4001,
        generate_unconfirmed(
            4001,
            OpenOpts {
                external_blobs: false,
                compress: false,
                ..OpenOpts::default()
            },
        ),
    ));
    out
}
