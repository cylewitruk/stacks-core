//! Benchmark-only signed DLMM workload and supply-preserving funding fixtures.

use std::env;
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail, ensure};
use blockstack_lib::burnchains::Txid;
use blockstack_lib::chainstate::burn::db::sortdb::SortitionDB;
use blockstack_lib::chainstate::nakamoto::miner::MinerTenureInfoCause;
use blockstack_lib::chainstate::nakamoto::{NakamotoBlock, NakamotoChainState};
use blockstack_lib::chainstate::stacks::db::{ClarityTx, StacksChainState};
use blockstack_lib::chainstate::stacks::{
    StacksTransaction, StacksTransactionSigner, TransactionAuth, TransactionPayload,
    TransactionPostConditionMode, TransactionVersion,
};
use clarity::vm::Value;
use clarity::vm::representations::ClarityName;
use clarity::vm::types::StacksAddressExtensions;
use clarity::vm::types::{PrincipalData, QualifiedContractIdentifier, SequenceData, TupleData};
use stacks_common::types::chainstate::StacksPrivateKey;

use super::{SegmentExecutionInput, TxSegment, execute_segment};

/// A deterministic test account whose private key never leaves this harness.
pub struct Account {
    /// Local test signing key, unrelated to any historical account.
    pub key: StacksPrivateKey,
    /// Principal derived from this key's mainnet transaction authorization.
    pub principal: PrincipalData,
}

/// Funding moved from the historical sender into generated test principals.
pub struct Fixture {
    /// Historical sender of the seed add-liquidity transaction.
    pub donor: PrincipalData,
    /// Generated recipients, each initially absent from historical state.
    pub recipients: Vec<PrincipalData>,
    /// Exact native USDCX fungible-token contract.
    pub token: QualifiedContractIdentifier,
}

impl Fixture {
    /// Move balances without minting assets or changing circulating supply.
    pub fn apply(&self, clarity_tx: &mut ClarityTx) -> Result<()> {
        clarity_tx.connection().as_transaction(|tx| {
            tx.with_clarity_db(|db| {
                let supply_before = db.get_ft_supply(&self.token, "usdcx-token")?;
                let donor_ft = db.get_ft_balance(&self.token, "usdcx-token", &self.donor, None)?;
                let per_account = donor_ft / (self.recipients.len() as u128 + 1);
                let stx = db.get_stx_balance_snapshot(&self.donor)?.get_available_balance()?;
                let stx_per_account = stx / (self.recipients.len() as u128 + 1);
                for principal in &self.recipients {
                    let balance = db.get_ft_balance(&self.token, "usdcx-token", principal, None)?;
                    assert_eq!(balance, 0, "fixture account already has token balance");
                    assert_eq!(db.get_stx_balance_snapshot(principal)?.get_available_balance()?, 0, "fixture account already funded");
                    assert_eq!(db.get_account_nonce(principal)?, 0, "fixture account already has nonce");
                    db.set_ft_balance(&self.token, "usdcx-token", principal, per_account)?;
                    db.get_stx_balance_snapshot(&self.donor)?.transfer_to(principal, stx_per_account)?;
                }
                db.set_ft_balance(&self.token, "usdcx-token", &self.donor, donor_ft - per_account * self.recipients.len() as u128)?;
                assert_eq!(supply_before, db.get_ft_supply(&self.token, "usdcx-token")?, "fixture changed supply");
                eprintln!("GROWTH_FUNDING {}", serde_json::json!({"donor":self.donor.to_string(),"recipients":self.recipients.iter().map(ToString::to_string).collect::<Vec<_>>(),"usdcx_per_account":per_account.to_string(),"ustx_per_account":stx_per_account.to_string(),"supply_changed":false}));
                Ok(())
            })
        }).map_err(|e| anyhow!("fixture funding failed: {e:?}"))
    }
}

/// Extract the list from an owned historical Clarity argument.
pub fn positions(value: &Value) -> Result<&[Value]> {
    match value {
        Value::Sequence(SequenceData::List(list)) => Ok(&list.data),
        _ => bail!("seed positions is not a list"),
    }
}

/// Build a valid signed transaction using a benchmark-owned key.
pub fn sign(
    account: &Account,
    nonce: u64,
    payload: TransactionPayload,
) -> Result<StacksTransaction> {
    let auth = TransactionAuth::from_p2pkh(&account.key).context("P2PKH auth")?;
    let mut tx = StacksTransaction::new(TransactionVersion::Mainnet, auth, payload);
    tx.chain_id = 1;
    tx.auth.set_origin_nonce(nonce);
    tx.auth.set_tx_fee(10_000);
    tx.post_condition_mode = TransactionPostConditionMode::Allow;
    let mut signer = StacksTransactionSigner::new(&tx);
    signer
        .sign_origin(&account.key)
        .map_err(|e| anyhow!("sign: {e:?}"))?;
    signer.get_tx().context("signed transaction")
}

/// Execute a finite state-evolving DLMM workload inside one real tenure budget.
pub fn run(
    chainstate: &mut StacksChainState,
    sortdb: &SortitionDB,
    block: &NakamotoBlock,
) -> Result<()> {
    let bins: usize = env::var("STACKS_GROWTH_BINS")?.parse()?;
    let count: usize = env::var("STACKS_GROWTH_TRANSACTIONS")?.parse()?;
    let accounts: usize = env::var("STACKS_GROWTH_ACCOUNTS")?.parse()?;
    ensure!(
        (1..=128).contains(&bins) && (1..=8192).contains(&count) && (1..=64).contains(&accounts),
        "invalid growth bounds"
    );
    let seed_id = Txid::from_hex(&env::var("STACKS_GROWTH_SEED")?)
        .map_err(|e| anyhow!("seed txid: {e:?}"))?;
    let seed_index = block
        .executed_and_skipped_txs()
        .iter()
        .position(|tx| tx.txid() == seed_id)
        .context("seed not in selected block")?;
    let seed = &block.executed_and_skipped_txs()[seed_index];
    let TransactionPayload::ContractCall(call) = &seed.payload else {
        bail!("seed must call contract")
    };
    ensure!(
        call.contract_name.as_str() == "dlmm-liquidity-router-v-1-2"
            && call.function_name.as_str() == "add-liquidity-multi",
        "unexpected seed contract/function"
    );
    let original = positions(&call.function_args[0])?;
    ensure!(bins <= original.len(), "not enough seed bins");
    let token =
        QualifiedContractIdentifier::parse("SP120SBRBQJ00MCWS7TM5R8WJNTTKD5K0HFRC2CNE.usdcx")?;
    let mut selected = Vec::new();
    for value in &original[..bins] {
        let Value::Tuple(tuple) = value else {
            bail!("seed position must be tuple")
        };
        let mut fields = tuple.data_map.clone();
        ensure!(
            fields.get("x-amount") == Some(&Value::UInt(0)),
            "seed must use USDCX side only"
        );
        ensure!(
            fields.get("y-token-trait")
                == Some(&Value::Principal(PrincipalData::Contract(token.clone()))),
            "unexpected funding asset"
        );
        fields.insert(
            ClarityName::try_from("y-amount").unwrap(),
            Value::UInt(10_000),
        );
        fields.insert(ClarityName::try_from("min-dlp").unwrap(), Value::UInt(1));
        fields.insert(
            ClarityName::try_from("max-y-liquidity-fee").unwrap(),
            Value::UInt(10_000),
        );
        selected.push(Value::Tuple(TupleData::from_data(
            fields.into_iter().collect(),
        )?));
    }
    let mut payload = call.clone();
    payload.function_args = vec![Value::cons_list_unsanitized(selected)?, Value::none()];
    let mut wallets = Vec::new();
    for i in 0..accounts {
        let key = StacksPrivateKey::from_seed(
            format!("codex-local-capacity-fixture-20260921-{i}").as_bytes(),
        );
        let auth = TransactionAuth::from_p2pkh(&key).context("fixture auth")?;
        let tx = StacksTransaction::new(
            TransactionVersion::Mainnet,
            auth,
            TransactionPayload::ContractCall(payload.clone()),
        );
        wallets.push(Account {
            key,
            principal: tx.origin_address().to_account_principal(),
        });
    }
    let generated: Vec<_> = (0..count)
        .map(|i| {
            sign(
                &wallets[i % accounts],
                (i / accounts) as u64,
                TransactionPayload::ContractCall(payload.clone()),
            )
        })
        .collect::<Result<_>>()?;
    let fixture = Fixture {
        donor: seed.origin_address().to_account_principal(),
        recipients: wallets.iter().map(|a| a.principal.clone()).collect(),
        token,
    };
    let mut parent =
        NakamotoChainState::get_block_header(chainstate.db(), &block.header.parent_block_id)?
            .context("growth parent missing")?;
    // Execute any real prefix and the funding fixture outside the measured workload.
    let prefix = TxSegment {
        range: 0..seed_index,
        sampled: false,
    };
    ensure!(
        !block.executed_and_skipped_txs()[..seed_index]
            .iter()
            .any(|t| t.try_as_tenure_change().is_some()),
        "choose seed without tenure-change prefix"
    );
    let prepared = execute_segment(
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
    )?;
    parent = prepared.new_tip_info;
    chainstate.checkpoint_sqlite_dbs()?;
    let template = NakamotoBlock::new(block.header.clone(), generated);
    let mut offset = 0;
    let mut index = 0;
    while offset < count {
        let seg = TxSegment {
            range: offset..count,
            sampled: true,
        };
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
                repetition: 0,
                measure: true,
                capacity: true,
                fixture: None,
            },
        )?;
        let elapsed = start.elapsed().saturating_sub(result.audit_duration);
        let checkpoint = Instant::now();
        chainstate.checkpoint_sqlite_dbs()?;
        eprintln!(
            "GROWTH_BLOCK {}",
            serde_json::json!({"index":index,"bins":bins,"accounts":accounts,"seed":seed_id.to_string(),"tx_start":offset,"tx_end":result.accepted_end,"wall_us":elapsed.as_micros(),"checkpoint_us":checkpoint.elapsed().as_micros(),"execution_us":result.execution_duration.as_micros(),"commit_us":result.commit_duration.as_micros(),"setup_us":result.setup_duration.as_micros(),"audit_us":result.audit_duration.as_micros(),"cost":result.segment_total_clarity_cost,"state_root":result.state_index_root.to_string()})
        );
        ensure!(result.accepted_end > offset, "growth admission stalled");
        offset = result.accepted_end;
        parent = result.new_tip_info;
        index += 1;
    }
    eprintln!(
        "GROWTH_COMPLETE {}",
        serde_json::json!({"transactions":count,"blocks":index,"bins":bins,"accounts":accounts,"seed":seed_id.to_string()})
    );
    Ok(())
}
