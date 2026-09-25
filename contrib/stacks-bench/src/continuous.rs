//! Signed mixed traffic carried through consecutive synthetic blocks.

use std::collections::BTreeMap;
use std::env;
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail, ensure};
use blockstack_lib::burnchains::Txid;
use blockstack_lib::chainstate::burn::db::sortdb::SortitionDB;
use blockstack_lib::chainstate::nakamoto::miner::MinerTenureInfoCause;
use blockstack_lib::chainstate::nakamoto::{NakamotoBlock, NakamotoChainState};
use blockstack_lib::chainstate::stacks::db::StacksChainState;
use blockstack_lib::chainstate::stacks::{
    StacksTransaction, TokenTransferMemo, TransactionAuthVerificationMode, TransactionContractCall,
    TransactionPayload,
};
use blockstack_lib::config::DEFAULT_MAX_TENURE_BYTES;
use clarity::vm::Value;
use clarity::vm::representations::ClarityName;
use clarity::vm::types::{PrincipalData, QualifiedContractIdentifier, TupleData};
use sha2::{Digest, Sha256};
use stacks_common::types::chainstate::{StacksAddress, StacksPrivateKey};
use stacks_common::util::hash::Hash160;

use super::growth::{self, Account, Fixture};
use super::{SegmentExecutionInput, TxSegment, execute_segment};

/// Select the byte allowance for generated continuous benchmark blocks only.
pub fn tenure_byte_limit(capacity: bool, continuous: bool) -> u64 {
    if capacity && continuous {
        DEFAULT_MAX_TENURE_BYTES * 100
    } else {
        DEFAULT_MAX_TENURE_BYTES
    }
}

/// Number of transactions in the deterministic mix cycle.
const CYCLE: usize = 1000;
/// Deployed real batch-transfer and append-log contract.
const BATCH: &str = "SP31DP8F8CF2GXSZBHHHK5J6Y061744E1TNFGYWYV.batchsend";
/// Deployed real public token mint.
const MINT: &str = "SPGDS0Y17973EN5TCHNHGJJ9B31XWQ5YX8A36C9B.hermes-tokenv2";
/// Deployed pixel-map workload.
const PIXEL: &str = "SP1Q7YR67R6WGP28NXDJD1WZ11REPAAXRJJ3V6RKM.stackspix";
/// Deployed per-account/global-counter workload.
const CLICK: &str = "SP1Q7YR67R6WGP28NXDJD1WZ11REPAAXRJJ3V6RKM.clicker";
/// Deployed scalar update workload.
const COUNTER: &str = "SP3R3SX667CWE61113X23CAQ03SZXXZ3D8D3A4NFH.counter";

/// Select one class while interleaving all classes throughout each cycle.
fn class(index: usize, heavy: bool) -> &'static str {
    let slot = (index % CYCLE * 613) % CYCLE;
    match slot {
        0..=233 => "native-transfer",
        234..=235 => "batch-transfer",
        236..=253 => "dlmm-add",
        254..=335 if heavy => "dlmm-add",
        254..=553 => "token-mint",
        554..=733 => "pixel-update",
        734..=903 => "clicker",
        904..=951 => "counter",
        _ => "append-log",
    }
}

/// Construct a public call to a pinned deployed contract.
fn call(identifier: &str, function: &str, args: Vec<Value>) -> Result<TransactionPayload> {
    let id = QualifiedContractIdentifier::parse(identifier)?;
    Ok(TransactionPayload::ContractCall(TransactionContractCall {
        address: StacksAddress::new(id.issuer.version(), Hash160(id.issuer.1))
            .context("contract issuer")?,
        contract_name: id.name,
        function_name: ClarityName::try_from(function)
            .map_err(|e| anyhow!("function name: {e:?}"))?,
        function_args: args,
    }))
}

/// Select real seed positions, lowering amounts for a bounded funded stream.
fn dlmm_payload(seed: &StacksTransaction, bins: usize) -> Result<TransactionPayload> {
    let TransactionPayload::ContractCall(source) = &seed.payload else {
        bail!("DLMM seed not a call")
    };
    ensure!(
        source.contract_name.as_str() == "dlmm-liquidity-router-v-1-2"
            && source.function_name.as_str() == "add-liquidity-multi",
        "unexpected seed"
    );
    let positions = growth::positions(&source.function_args[0])?;
    ensure!(bins <= positions.len(), "insufficient seed positions");
    let token =
        QualifiedContractIdentifier::parse("SP120SBRBQJ00MCWS7TM5R8WJNTTKD5K0HFRC2CNE.usdcx")?;
    let mut selected = Vec::new();
    for value in &positions[..bins] {
        let Value::Tuple(tuple) = value else {
            bail!("non-tuple position")
        };
        let mut fields = tuple.data_map.clone();
        ensure!(
            fields.get("x-amount") == Some(&Value::UInt(0)),
            "nonzero X side"
        );
        ensure!(
            fields.get("y-token-trait")
                == Some(&Value::Principal(PrincipalData::Contract(token.clone()))),
            "unexpected token"
        );
        for (name, amount) in [
            ("y-amount", 10_000),
            ("min-dlp", 1),
            ("max-y-liquidity-fee", 10_000),
        ] {
            fields.insert(ClarityName::try_from(name).unwrap(), Value::UInt(amount));
        }
        selected.push(Value::Tuple(TupleData::from_data(
            fields.into_iter().collect(),
        )?));
    }
    let mut payload = source.clone();
    payload.function_args = vec![Value::cons_list_unsanitized(selected)?, Value::none()];
    Ok(TransactionPayload::ContractCall(payload))
}

/// Construct changing state writes and non-self transfers for a generated sender.
fn payload(
    kind: &str,
    index: usize,
    sender: usize,
    wallets: &[Account],
    dlmm: &TransactionPayload,
) -> Result<TransactionPayload> {
    let recipient = wallets[(sender + 1) % wallets.len()].principal.clone();
    match kind {
        "native-transfer" => Ok(TransactionPayload::TokenTransfer(
            recipient,
            1_000,
            TokenTransferMemo([0; 34]),
        )),
        "batch-transfer" => {
            let entries = (1..=16)
                .map(|j| {
                    TupleData::from_data(vec![
                        (
                            ClarityName::try_from("to").unwrap(),
                            Value::Principal(
                                wallets[(sender + j) % wallets.len()].principal.clone(),
                            ),
                        ),
                        (ClarityName::try_from("ustx").unwrap(), Value::UInt(1_000)),
                    ])
                    .map(Value::Tuple)
                })
                .collect::<std::result::Result<Vec<_>, _>>()?;
            call(
                BATCH,
                "send-many-stx",
                vec![Value::cons_list_unsanitized(entries)?],
            )
        }
        "dlmm-add" => Ok(dlmm.clone()),
        "token-mint" => call(
            MINT,
            "mint",
            vec![Value::UInt(1), Value::Principal(recipient)],
        ),
        "pixel-update" => call(
            PIXEL,
            "place-pixel",
            vec![
                Value::UInt((index % 50) as u128),
                Value::UInt((index / 50 % 50) as u128),
                Value::string_ascii_from_bytes(format!("{:06X}", index % 0x1000000).into_bytes())?,
            ],
        ),
        "clicker" => call(CLICK, "tap", vec![]),
        "counter" => call(
            COUNTER,
            "increment-by",
            vec![Value::UInt((index % 7 + 1) as u128)],
        ),
        "append-log" => call(
            BATCH,
            "log-config",
            vec![Value::UInt((index % 4) as u128), Value::UInt(index as u128)],
        ),
        _ => bail!("unknown mixed class"),
    }
}

/// Read a bounded workload count.
fn setting(name: &str, min: usize, max: usize) -> Result<usize> {
    let n = env::var(name)?.parse()?;
    ensure!((min..=max).contains(&n), "{name} out of range");
    Ok(n)
}

/// Execute a queued finite stream without restoring state between candidates.
pub fn run(
    chainstate: &mut StacksChainState,
    sortdb: &SortitionDB,
    block: &NakamotoBlock,
) -> Result<()> {
    let dose = setting("STACKS_MIX_DOSE", 16, 4096)?;
    let blocks = setting("STACKS_MIX_BLOCKS", 2, 40)?;
    let warm_blocks = 2;
    let accounts = 64;
    let profile = env::var("STACKS_MIX_PROFILE")?;
    ensure!(
        profile == "mixed" || profile == "dlmm-heavy",
        "unknown mix profile"
    );
    let heavy = profile == "dlmm-heavy";
    let bins = if heavy { 32 } else { 8 };
    let seed_id = Txid::from_hex(&env::var("STACKS_GROWTH_SEED")?)
        .map_err(|e| anyhow!("seed txid: {e:?}"))?;
    let seed_index = block
        .executed_and_skipped_txs()
        .iter()
        .position(|tx| tx.txid() == seed_id)
        .context("seed missing")?;
    let seed = &block.executed_and_skipped_txs()[seed_index];
    let dlmm = dlmm_payload(seed, bins)?;
    let wallets = (0..accounts)
        .map(|i| {
            let key = StacksPrivateKey::from_seed(
                format!("codex-continuous-mix-20260922-{i}").as_bytes(),
            );
            let auth =
                blockstack_lib::chainstate::stacks::TransactionAuth::from_p2pkh(&key).unwrap();
            let tx = StacksTransaction::new(
                blockstack_lib::chainstate::stacks::TransactionVersion::Mainnet,
                auth,
                dlmm.clone(),
            );
            use clarity::vm::types::StacksAddressExtensions;
            Account {
                key,
                principal: tx.origin_address().to_account_principal(),
            }
        })
        .collect::<Vec<_>>();
    let total = dose * (blocks + warm_blocks);
    let mut nonces = vec![0_u64; accounts];
    let mut types = Vec::with_capacity(total);
    let mut generated = Vec::with_capacity(total);
    let mut digest = Sha256::new();
    for index in 0..total {
        let sender = (index + index / CYCLE) % accounts;
        let kind = class(index, heavy);
        let tx = growth::sign(
            &wallets[sender],
            nonces[sender],
            payload(kind, index, sender, &wallets, &dlmm)?,
        )?;
        tx.verify(TransactionAuthVerificationMode::EnforceLowS)
            .map_err(|e| anyhow!("generated signature: {e:?}"))?;
        nonces[sender] += 1;
        digest.update(tx.txid().to_string().as_bytes());
        types.push(kind);
        generated.push(tx);
    }
    eprintln!(
        "MIX_STREAM {}",
        serde_json::json!({"profile":profile,"dose":dose,"measured_candidates":blocks,"warm_candidates":warm_blocks,"accounts":accounts,"bins":bins,"transactions":total,"txid_sha256":hex::encode(digest.finalize())})
    );
    use clarity::vm::types::StacksAddressExtensions;
    let fixture = Fixture {
        donor: seed.origin_address().to_account_principal(),
        recipients: wallets.iter().map(|a| a.principal.clone()).collect(),
        token: QualifiedContractIdentifier::parse(
            "SP120SBRBQJ00MCWS7TM5R8WJNTTKD5K0HFRC2CNE.usdcx",
        )?,
    };
    let mut parent =
        NakamotoChainState::get_block_header(chainstate.db(), &block.header.parent_block_id)?
            .context("parent missing")?;
    ensure!(
        !block.executed_and_skipped_txs()[..seed_index]
            .iter()
            .any(|t| t.try_as_tenure_change().is_some()),
        "tenure change prefix unsupported"
    );
    let prefix = TxSegment {
        range: 0..seed_index,
        sampled: false,
    };
    parent = execute_segment(
        chainstate,
        sortdb,
        SegmentExecutionInput {
            cur_parent_info: &parent,
            block,
            seg: &prefix,
            seg_ix: 0,
            segment_tenure_change_tx: None,
            segment_coinbase_tx: None,
            segment_cause: MinerTenureInfoCause::NoTenureChange,
            setup_start: None,
            repetition: 0,
            measure: false,
            capacity: false,
            fixture: Some(&fixture),
        },
    )?
    .new_tip_info;
    chainstate.checkpoint_sqlite_dbs()?;
    let template = NakamotoBlock::new(block.header.clone(), generated);
    let mut offset = 0;
    let mut index = 0;
    let mut previous_root = None::<String>;
    while offset < total {
        let candidate = offset / dose;
        let end = ((candidate + 1) * dose).min(total);
        let seg = TxSegment {
            range: offset..end,
            sampled: true,
        };
        let parent_id = parent.index_block_hash();
        let start = Instant::now();
        let result = execute_segment(
            chainstate,
            sortdb,
            SegmentExecutionInput {
                cur_parent_info: &parent,
                block: &template,
                seg: &seg,
                seg_ix: index,
                segment_tenure_change_tx: None,
                segment_coinbase_tx: None,
                segment_cause: MinerTenureInfoCause::NoTenureChange,
                setup_start: Some(start),
                repetition: (index as u32 + 1) * 5,
                measure: true,
                capacity: true,
                fixture: None,
            },
        )?;
        let wall = start.elapsed().saturating_sub(result.audit_duration);
        let checkpoint_window = stacks_profiler::state_cost::Window::new(candidate >= warm_blocks);
        let checkpoint_guard = stacks_profiler::state_cost::phase("checkpoint");
        let checkpoint = Instant::now();
        chainstate.checkpoint_sqlite_dbs()?;
        let checkpoint_us = checkpoint.elapsed().as_micros();
        drop(checkpoint_guard);
        let checkpoint_snapshot = checkpoint_window.finish();
        if let Some(ref snapshot) = result.state_cost {
            eprintln!(
                "STATE_COST {}",
                serde_json::json!({"index":index,"candidate":candidate,"measured":candidate>=warm_blocks,"snapshot":snapshot,"checkpoint":checkpoint_snapshot})
            );
        }
        ensure!(result.accepted_end > offset, "mixed admission stalled");
        ensure!(
            result.new_tip_info.stacks_block_height == parent.stacks_block_height + 1,
            "height continuity"
        );
        let mut mix = BTreeMap::new();
        for kind in &types[offset..result.accepted_end] {
            *mix.entry(*kind).or_insert(0_usize) += 1;
        }
        let root = result.state_index_root.to_string();
        eprintln!(
            "MIX_BLOCK {}",
            serde_json::json!({
                "index":index,"candidate":candidate,"measured":candidate>=warm_blocks,"profile":profile,"dose":dose,
                "tx_start":offset,"tx_end":result.accepted_end,"queued_after":total-result.accepted_end,"mix":mix,
                "parent_id":parent_id.to_string(),"block_id":result.new_tip_info.index_block_hash().to_string(),
                "previous_state_root":previous_root,"state_root":root,"synthetic_height":result.new_tip_info.stacks_block_height,
                "wall_us":wall.as_micros(),"checkpoint_us":checkpoint_us,"execution_us":result.execution_duration.as_micros(),
                "commit_us":result.commit_duration.as_micros(),"setup_us":result.setup_duration.as_micros(),"audit_us":result.audit_duration.as_micros(),
                "cost":result.segment_total_clarity_cost,
            })
        );
        previous_root = Some(root);
        offset = result.accepted_end;
        parent = result.new_tip_info;
        index += 1;
    }
    eprintln!(
        "MIX_COMPLETE {}",
        serde_json::json!({"transactions":total,"measured_transactions":dose*blocks,"physical_blocks":index,"profile":profile,"dose":dose})
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Canonical warmup/funding retain their limit; only continuous generated blocks relax it.
    #[test]
    fn continuous_tenure_byte_scope() {
        for (capacity, continuous) in [(false, false), (false, true), (true, false)] {
            assert_eq!(tenure_byte_limit(capacity, continuous), 10 * 1024 * 1024);
        }
        assert_eq!(tenure_byte_limit(true, true), 1_048_576_000);
    }

    /// The permutation preserves intended mix proportions and includes every class.
    #[test]
    fn continuous_mix_proportions() {
        for heavy in [false, true] {
            let mut counts = BTreeMap::new();
            for i in 0..CYCLE {
                *counts.entry(class(i, heavy)).or_insert(0) += 1;
            }
            assert_eq!(counts.len(), 8);
            assert_eq!(counts["native-transfer"], 234);
            assert_eq!(counts["batch-transfer"], 2);
            assert_eq!(counts["dlmm-add"], if heavy { 100 } else { 18 });
            assert_eq!(counts["token-mint"], if heavy { 218 } else { 300 });
        }
    }
}
