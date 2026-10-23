//! Adapter for the rewrite in `stacks-marf-next`.
//!
//! Every hook calls the real API; see `adapter.rs` for the replay loop.

use std::collections::HashMap;
use std::path::Path;

use stacks_common::codec::StacksMessageCodec;
use stacks_common::types::chainstate::{StacksBlockId, TrieHash};
use stacks_common::util::hash::{hex_bytes, to_hex};
use stacks_marf_next::marf::{MARF, MARFOpenOpts, MarfConnection, MarfTransaction};
use stacks_marf_next::pruning::Prune;
use stacks_marf_next::sqlite::SqliteAccess;
use stacks_marf_next::storage::TrieHashCalculationMode;
use stacks_marf_next::{ClarityMarfTrieId, Error, MARFValue, TrieMerkleProof};

use crate::adapter::{MarfApi, ProofCheck, proof_observation, tamper_bytes};
use crate::ops::{Observation, Op, OpenOpts, Tamper};

pub struct Rewrite;

fn bid(hex: &str) -> StacksBlockId {
    let bytes = hex_bytes(hex).expect("bad block hex");
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    StacksBlockId(arr)
}

fn val(hex: &str) -> MARFValue {
    let bytes = hex_bytes(hex).expect("bad value hex");
    let mut arr = [0u8; 40];
    arr.copy_from_slice(&bytes);
    MARFValue(arr)
}

/// Stable error kind names, aligned with `baseline::error_kind`.
pub fn error_kind(e: &Error) -> String {
    let s = match e {
        Error::NotOpenedError => "NotOpened",
        Error::IOError(_) => "IO",
        Error::SQLError(_) => "SQL",
        Error::NotFoundError => "NotFound",
        Error::ExistsError => "Exists",
        Error::CorruptionError(_) => "Corruption",
        Error::ReadOnlyError => "ReadOnly",
        Error::UnconfirmedError => "Unconfirmed",
        Error::InProgressError => "InProgress",
        Error::WriteNotBegunError => "WriteNotBegun",
        Error::RestoreMarfBlockError(_) => "RestoreMarfBlock",
        Error::NonMatchingForks(..) => "NonMatchingForks",
        Error::OverflowError => "Overflow",
        Error::NodeTooDeep => "NodeTooDeep",
        Error::HistoricalReadInPrunedRange { .. } => "HistoricalReadInPrunedRange",
        Error::UnsupportedOnPrunedMarf(_) => "UnsupportedOnPrunedMarf",
        Error::DestinationExists(_) => "DestinationExists",
    };
    s.to_string()
}

fn obs<T>(r: Result<T, Error>, f: impl FnOnce(T) -> Observation) -> Observation {
    match r {
        Ok(v) => f(v),
        Err(e) => Observation::Err(error_kind(&e)),
    }
}

fn unit<T>(r: Result<T, Error>) -> Observation {
    obs(r, |_| Observation::Ok)
}

fn marf_opts(o: &OpenOpts) -> MARFOpenOpts {
    let mode = if o.defer_hashing {
        TrieHashCalculationMode::Deferred
    } else {
        TrieHashCalculationMode::Immediate
    };
    MARFOpenOpts::new(mode, o.external_blobs).with_compression(o.compress)
}

fn entries(e: &[(String, String)]) -> (Vec<String>, Vec<MARFValue>) {
    (
        e.iter().map(|(k, _)| k.clone()).collect(),
        e.iter().map(|(_, v)| val(v)).collect(),
    )
}

fn tip_obs(tip: Option<&StacksBlockId>, height: Option<u32>) -> Observation {
    Observation::Tip(tip.map(|b| (to_hex(b.as_bytes()), height.unwrap_or(0))))
}

/// Root hash -> block id for every confirmed block, used by proof verification.
fn root_to_block<C: MarfConnection<StacksBlockId> + SqliteAccess>(
    c: &mut C,
) -> HashMap<TrieHash, StacksBlockId> {
    let blocks: Vec<StacksBlockId> = {
        let conn = c.sqlite_conn();
        let mut stmt = conn
            .prepare("SELECT block_hash FROM marf_data WHERE unconfirmed = 0 ORDER BY block_id")
            .expect("prepare");
        let rows = stmt
            .query_map([], |row| row.get::<_, StacksBlockId>(0))
            .expect("query");
        rows.map(|r| r.expect("row")).collect()
    };
    let mut map = HashMap::new();
    for b in blocks {
        if b == StacksBlockId::sentinel() {
            continue;
        }
        if let Ok(root) = c.get_root_hash_at(&b) {
            map.insert(root, b);
        }
    }
    map
}

fn check_proof<C: MarfConnection<StacksBlockId> + SqliteAccess>(
    c: &mut C,
    tip: &str,
    key: &str,
    tamper: &Tamper,
) -> ProofCheck {
    let tip = bid(tip);
    match c.get_with_proof(&tip, key) {
        Err(e) => ProofCheck::Err(error_kind(&e)),
        Ok(None) => ProofCheck::Missing,
        Ok(Some((value, proof))) => {
            let mut bytes = vec![];
            proof
                .consensus_serialize(&mut bytes)
                .expect("serialize proof");
            tamper_bytes(&mut bytes, tamper);
            let Ok(proof) =
                TrieMerkleProof::<StacksBlockId>::consensus_deserialize(&mut &bytes[..])
            else {
                return ProofCheck::Deserialize;
            };
            let mut root = c
                .get_root_hash_at(&tip)
                .expect("root hash of committed tip");
            let mut map = root_to_block(c);
            let mut value = value;
            match tamper {
                Tamper::WrongValue => value.0[0] ^= 0xff,
                Tamper::WrongRoot => root.0[0] ^= 0xff,
                Tamper::EmptyBlockMap => map.clear(),
                _ => {}
            }
            ProofCheck::Verified(proof.verify(&TrieHash::from_key(key), &value, &root, &map))
        }
    }
}

/// Read-side ops that work on anything implementing `MarfConnection`.
fn read_op<C: MarfConnection<StacksBlockId> + SqliteAccess>(
    c: &mut C,
    op: &Op,
) -> Option<Observation> {
    Some(match op {
        Op::Get { tip, key } => obs(c.get(&bid(tip), key), |v| {
            Observation::Value(v.map(|m| to_hex(m.as_bytes())))
        }),
        Op::RootHash { tip } => obs(c.get_root_hash_at(&bid(tip)), |h| {
            Observation::Hash(to_hex(h.as_bytes()))
        }),
        Op::BlockAtHeight { tip, height } => obs(c.get_block_at_height(*height, &bid(tip)), |b| {
            Observation::Block(b.map(|b| to_hex(b.as_bytes())))
        }),
        Op::BlockHeight { tip, block } => obs(
            c.get_block_height(&bid(block), &bid(tip)),
            Observation::Height,
        ),
        Op::CheckAncestor { tip, block } => match c.with_conn(|v| v.open_block(&bid(tip))) {
            Err(e) => Observation::Err(error_kind(&e)),
            Ok(()) => unit(c.check_ancestor_block_hash(&bid(block))),
        },
        Op::GetWithProof { tip, key } => {
            let tip = bid(tip);
            match c.get_with_proof(&tip, key) {
                Err(e) => Observation::Err(error_kind(&e)),
                Ok(None) => Observation::Value(None),
                Ok(Some((value, proof))) => {
                    let root = c
                        .get_root_hash_at(&tip)
                        .expect("root hash of committed tip");
                    let map = root_to_block(c);
                    let verified = proof.verify(&TrieHash::from_key(key), &value, &root, &map);
                    let mut bytes = vec![];
                    proof
                        .consensus_serialize(&mut bytes)
                        .expect("serialize proof");
                    Observation::Proof {
                        value: to_hex(value.as_bytes()),
                        proof: to_hex(&bytes),
                        verified,
                    }
                }
            }
        }
        Op::VerifyProof { tip, key, tamper } => proof_observation(check_proof(c, tip, key, tamper)),
        _ => return None,
    })
}

impl MarfApi for Rewrite {
    type Handle = MARF<StacksBlockId>;
    type Tx<'a> = MarfTransaction<'a, StacksBlockId>;

    fn open(&self, path: &str, o: &OpenOpts) -> Result<Self::Handle, String> {
        let r = if o.unconfirmed {
            MARF::from_path_unconfirmed(path, marf_opts(o))
        } else {
            MARF::from_path(path, marf_opts(o))
        };
        r.map_err(|e| error_kind(&e))
    }

    fn prune(&self, src: &str, dst: &str, tip: &str, height: u32) -> Result<(), String> {
        // like the real callers, detect the source's blob layout from disk
        let external = Path::new(&format!("{src}.blobs")).exists();
        let opts = MARFOpenOpts::new(TrieHashCalculationMode::Deferred, external);
        <MARF<StacksBlockId> as Prune<StacksBlockId>>::prune_to_path(
            src,
            dst,
            opts,
            &bid(tip),
            height,
            "diff",
        )
        .map(|_| ())
        .map_err(|e| error_kind(&e))
    }

    fn h_begin_tx<'a>(h: &'a mut Self::Handle) -> Self::Tx<'a> {
        h.begin_tx().expect("begin_tx")
    }
    fn h_begin(h: &mut Self::Handle, parent: &str, child: &str) -> Observation {
        unit(h.begin(&bid(parent), &bid(child)))
    }
    fn h_read(h: &mut Self::Handle, op: &Op) -> Option<Observation> {
        read_op(h, op)
    }
    fn h_insert(h: &mut Self::Handle, e: &[(String, String)]) -> Observation {
        let (k, v) = entries(e);
        unit(h.insert_batch(&k, v))
    }
    fn h_seal(h: &mut Self::Handle) -> Observation {
        // same shape as the oracle adapter: seal through a transaction
        let mut tx = h.begin_tx().expect("begin_tx");
        obs(tx.seal(), |h| Observation::Hash(to_hex(h.as_bytes())))
    }
    fn h_commit(h: &mut Self::Handle) -> Observation {
        unit(h.commit())
    }
    fn h_commit_to(h: &mut Self::Handle, block: &str) -> Observation {
        unit(h.commit_to(&bid(block)))
    }
    fn h_commit_mined(h: &mut Self::Handle, block: &str) -> Observation {
        unit(h.commit_mined(&bid(block)))
    }
    fn h_drop_current(h: &mut Self::Handle) -> Observation {
        h.drop_current();
        Observation::Ok
    }
    fn h_drop_unconfirmed(h: &mut Self::Handle) -> Observation {
        h.drop_unconfirmed();
        Observation::Ok
    }
    fn h_open_tip(h: &Self::Handle) -> Observation {
        tip_obs(h.get_open_chain_tip(), h.get_open_chain_tip_height())
    }

    fn t_begin(tx: &mut Self::Tx<'_>, parent: &str, child: &str) -> Observation {
        unit(tx.begin(&bid(parent), &bid(child)))
    }
    fn t_begin_unconfirmed(tx: &mut Self::Tx<'_>, parent: &str) -> Observation {
        obs(tx.begin_unconfirmed(&bid(parent)), |b| {
            Observation::Block(Some(to_hex(b.as_bytes())))
        })
    }
    fn t_read(tx: &mut Self::Tx<'_>, op: &Op) -> Option<Observation> {
        read_op(tx, op)
    }
    fn t_insert(tx: &mut Self::Tx<'_>, e: &[(String, String)]) -> Observation {
        let (k, v) = entries(e);
        unit(tx.insert_batch(&k, v))
    }
    fn t_seal(tx: &mut Self::Tx<'_>) -> Observation {
        obs(tx.seal(), |h| Observation::Hash(to_hex(h.as_bytes())))
    }
    fn t_commit(tx: Self::Tx<'_>) -> Observation {
        unit(tx.commit())
    }
    fn t_commit_to(tx: Self::Tx<'_>, block: &str) -> Observation {
        unit(tx.commit_to(&bid(block)))
    }
    fn t_commit_mined(tx: Self::Tx<'_>, block: &str) -> Observation {
        unit(tx.commit_mined(&bid(block)))
    }
    fn t_drop_current(tx: Self::Tx<'_>) -> Observation {
        tx.drop_current();
        Observation::Ok
    }
    fn t_drop_unconfirmed(tx: Self::Tx<'_>) -> Observation {
        tx.drop_unconfirmed();
        Observation::Ok
    }
    fn t_open_tip(tx: &Self::Tx<'_>) -> Observation {
        tip_obs(tx.get_open_chain_tip(), tx.get_open_chain_tip_height())
    }
}
