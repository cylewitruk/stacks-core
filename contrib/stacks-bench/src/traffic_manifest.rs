//! Explicit, signed traffic with preparation and normal state continuity.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail, ensure};
use blockstack_lib::chainstate::burn::db::sortdb::SortitionDB;
use blockstack_lib::chainstate::nakamoto::miner::MinerTenureInfoCause;
use blockstack_lib::chainstate::nakamoto::{NakamotoBlock, NakamotoChainState};
use blockstack_lib::chainstate::stacks::db::StacksChainState;
use blockstack_lib::chainstate::stacks::{
    StacksTransaction, TokenTransferMemo, TransactionAuth, TransactionAuthVerificationMode,
    TransactionPayload, TransactionVersion,
};
use clarity::vm::Value;
use clarity::vm::types::{PrincipalData, QualifiedContractIdentifier, StacksAddressExtensions};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use stacks_common::types::chainstate::StacksPrivateKey;

use super::super::growth::{self, Account, Fixture};
use super::super::{SegmentExecutionInput, TxSegment, execute_segment};

/// A block's role; preparation and warmup are never throughput samples.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    /// Contract calls that create the positions/assets required by later work.
    Preparation,
    /// State-evolving calls excluded from measured throughput.
    Warmup,
    /// One complete physical block included in the load study.
    Measured,
}

/// A public contract call or transfer signed by a benchmark-owned account.
#[derive(Clone, Debug)]
pub enum Action {
    /// Arguments are serialized Clarity Values, including original list shapes.
    Call {
        /// Fully qualified deployed contract identifier.
        contract: String,
        /// Public function to execute.
        function: String,
        /// Owned argument values, supplied to ordinary VM execution.
        args: Vec<Value>,
    },
    /// Reference to a shared literal action; signing and execution remain per transaction.
    Template {
        /// Index into the manifest's nonrecursive action table.
        index: usize,
    },
    /// Non-self native transfer to another generated account.
    Transfer {
        /// Index into the same deterministic account set.
        recipient: usize,
        /// Amount in micro-STX.
        amount: u64,
    },
}

/// Deserialize directly so serde's tagged-enum buffer does not reject Clarity u128/i128.
impl<'de> Deserialize<'de> for Action {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            kind: String,
            contract: Option<String>,
            function: Option<String>,
            args: Option<Vec<Value>>,
            recipient: Option<usize>,
            amount: Option<u64>,
            index: Option<usize>,
        }
        let wire = Wire::deserialize(deserializer)?;
        match (
            wire.kind.as_str(),
            wire.contract,
            wire.function,
            wire.args,
            wire.recipient,
            wire.amount,
            wire.index,
        ) {
            ("call", Some(contract), Some(function), Some(args), None, None, None) => {
                Ok(Self::Call {
                    contract,
                    function,
                    args,
                })
            }
            ("transfer", None, None, None, Some(recipient), Some(amount), None) => {
                Ok(Self::Transfer { recipient, amount })
            }
            ("template", None, None, None, None, None, Some(index)) => Ok(Self::Template { index }),
            _ => Err(serde::de::Error::custom(
                "action must contain exactly the fields of a call, transfer or template",
            )),
        }
    }
}

/// One intended operation, with optional reference provenance.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Operation {
    /// Signing account index; reusing an index preserves state and nonces.
    pub account: usize,
    /// Human-readable workload class reported verbatim in the audit stream.
    pub class: String,
    /// Historical transaction whose shape informed this operation, if any.
    pub reference_txid: Option<String>,
    /// Actual operation executed by the VM.
    pub action: Action,
}

/// One intended physical block, never silently split or reduced.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Batch {
    /// Preparation, warmup or measured work.
    pub phase: Phase,
    /// Ordered transactions in this block.
    pub transactions: Vec<Operation>,
}

/// Versioned finite workload bound to its source block and funding seed.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Supported schema version, currently one.
    pub schema: u32,
    /// Explicit scenario label; no representativeness is inferred from it.
    pub name: String,
    /// Reference dataset and any changes to amounts, accounts or mix.
    pub description: String,
    /// Historical block selected by the capacity harness.
    pub source_block: String,
    /// Historical funding-seed transaction in that block.
    pub seed_txid: String,
    /// Number of deterministic signing accounts to fund.
    pub accounts: usize,
    /// Shared literal actions; references cannot point to another reference.
    #[serde(default)]
    pub templates: Vec<Action>,
    /// Blocks executed serially without restoring state between them.
    pub batches: Vec<Batch>,
}

impl Manifest {
    /// Resolve one action without allowing recursive or out-of-range references.
    fn resolve<'a>(&'a self, action: &'a Action) -> Result<&'a Action> {
        let resolved = match action {
            Action::Template { index } => self
                .templates
                .get(*index)
                .context("template index out of range")?,
            literal => literal,
        };
        ensure!(
            !matches!(resolved, Action::Template { .. }),
            "recursive action template"
        );
        Ok(resolved)
    }

    /// Reject incomplete phase order, invalid indices and unbounded workloads.
    fn validate(&self) -> Result<()> {
        ensure!(self.schema == 1, "unsupported traffic schema");
        ensure!(
            !self.name.trim().is_empty() && !self.description.trim().is_empty(),
            "missing provenance"
        );
        ensure!((1..=64).contains(&self.accounts), "accounts must be 1..64");
        for h in [&self.source_block, &self.seed_txid] {
            ensure!(
                hex::decode(h)?.len() == 32,
                "expected 32-byte source identity"
            );
        }
        ensure!(
            !self.batches.is_empty() && self.batches.len() <= 4096,
            "invalid block count"
        );
        ensure!(self.templates.len() <= 16384, "too many action templates");
        for action in &self.templates {
            ensure!(
                !matches!(action, Action::Template { .. }),
                "recursive action template"
            );
        }
        let mut last = 0;
        let mut total = 0usize;
        let mut measured = false;
        for b in &self.batches {
            let rank = match b.phase {
                Phase::Preparation => 0,
                Phase::Warmup => 1,
                Phase::Measured => 2,
            };
            ensure!(
                rank >= last,
                "preparation/warmup cannot follow measured work"
            );
            last = rank;
            measured |= b.phase == Phase::Measured;
            ensure!(
                !b.transactions.is_empty() && b.transactions.len() <= 16384,
                "invalid transactions per block"
            );
            total = total
                .checked_add(b.transactions.len())
                .context("transaction count overflow")?;
            ensure!(total <= 2_000_000, "traffic exceeds transaction bound");
            for op in &b.transactions {
                ensure!(
                    op.account < self.accounts && !op.class.trim().is_empty(),
                    "invalid account or class"
                );
                if let Some(id) = &op.reference_txid {
                    ensure!(hex::decode(id)?.len() == 32, "invalid reference txid");
                }
                match self.resolve(&op.action)? {
                    Action::Call {
                        contract,
                        function,
                        args,
                    } => {
                        ensure!(!function.is_empty() && args.len() <= 64, "invalid call");
                        super::call(contract, function, args.clone())?;
                    }
                    Action::Template { .. } => bail!("unresolved action template"),
                    Action::Transfer { recipient, amount } => {
                        ensure!(
                            *recipient < self.accounts && *recipient != op.account && *amount > 0,
                            "invalid transfer"
                        );
                    }
                }
            }
        }
        ensure!(measured, "manifest has no measured blocks");
        Ok(())
    }
}

/// Generate owned signers without borrowing any historical account's authority.
fn accounts(count: usize) -> Result<Vec<Account>> {
    (0..count)
        .map(|i| {
            let key =
                StacksPrivateKey::from_seed(format!("stacks-bench-traffic-v1-{i}").as_bytes());
            let auth = TransactionAuth::from_p2pkh(&key).context("traffic signer")?;
            let tx = StacksTransaction::new(
                TransactionVersion::Mainnet,
                auth,
                TransactionPayload::TokenTransfer(
                    PrincipalData::Standard(
                        QualifiedContractIdentifier::parse(
                            "SP120SBRBQJ00MCWS7TM5R8WJNTTKD5K0HFRC2CNE.usdcx",
                        )?
                        .issuer,
                    ),
                    1,
                    TokenTransferMemo([0; 34]),
                ),
            );
            Ok(Account {
                principal: tx.origin_address().to_account_principal(),
                key,
            })
        })
        .collect()
}

/// Sign the exact finite operation order; preparation consumes ordinary nonces.
fn transactions(manifest: &Manifest, wallets: &[Account]) -> Result<Vec<StacksTransaction>> {
    let mut nonces = vec![0; wallets.len()];
    let mut txs = Vec::new();
    for batch in &manifest.batches {
        for op in &batch.transactions {
            let payload = match manifest.resolve(&op.action)? {
                Action::Call {
                    contract,
                    function,
                    args,
                } => super::call(contract, function, args.clone())?,
                Action::Template { .. } => bail!("unresolved action template"),
                Action::Transfer { recipient, amount } => TransactionPayload::TokenTransfer(
                    wallets[*recipient].principal.clone(),
                    *amount,
                    TokenTransferMemo([0; 34]),
                ),
            };
            let tx = growth::sign(&wallets[op.account], nonces[op.account], payload)?;
            tx.verify(TransactionAuthVerificationMode::EnforceLowS)
                .map_err(|e| anyhow!("traffic signature: {e:?}"))?;
            nonces[op.account] += 1;
            txs.push(tx);
        }
    }
    Ok(txs)
}

/// Execute a manifest on the caller's disposable chainstate through normal blocks.
pub fn run(
    chainstate: &mut StacksChainState,
    sortdb: &SortitionDB,
    block: &NakamotoBlock,
    path: &Path,
) -> Result<()> {
    for key in [
        "STACKS_CAPACITY_GROWTH",
        "STACKS_CONTINUOUS_MIX",
        "STACKS_RELAXED_COSTS",
    ] {
        ensure!(
            env::var(key).as_deref() == Ok("1"),
            "{key}=1 required for audited manifest execution"
        );
    }
    ensure!(
        fs::metadata(path)?.len() <= 256 * 1024 * 1024,
        "traffic manifest exceeds 256 MiB"
    );
    let bytes = fs::read(path)?;
    let manifest: Manifest = serde_json::from_slice(&bytes).context("traffic manifest")?;
    manifest.validate()?;
    ensure!(
        manifest.source_block == block.block_id().to_string(),
        "manifest source block mismatch"
    );
    ensure!(
        env::var("STACKS_GROWTH_SEED")? == manifest.seed_txid,
        "manifest funding seed mismatch"
    );
    let seed_index = block
        .executed_and_skipped_txs()
        .iter()
        .position(|t| t.txid().to_string() == manifest.seed_txid)
        .context("traffic funding seed missing")?;
    ensure!(
        !block.executed_and_skipped_txs()[..seed_index]
            .iter()
            .any(|t| t.try_as_tenure_change().is_some()),
        "tenure prefix unsupported"
    );
    let wallets = accounts(manifest.accounts)?;
    let generated = transactions(&manifest, &wallets)?;
    let mut txhash = Sha256::new();
    for t in &generated {
        txhash.update(t.txid().to_string().as_bytes());
    }
    eprintln!(
        "TRAFFIC_MANIFEST {}",
        serde_json::json!({"schema":1,"name":manifest.name,"description":manifest.description,"sha256":hex::encode(Sha256::digest(&bytes)),"txid_sha256":hex::encode(txhash.finalize()),"accounts":manifest.accounts,"transactions":generated.len(),"source_block":manifest.source_block,"funding":"supply-preserving STX/USDCX fixture; liquidity positions require preparation calls"})
    );
    let fixture = Fixture {
        donor: block.executed_and_skipped_txs()[seed_index]
            .origin_address()
            .to_account_principal(),
        recipients: wallets.iter().map(|a| a.principal.clone()).collect(),
        token: QualifiedContractIdentifier::parse(
            "SP120SBRBQJ00MCWS7TM5R8WJNTTKD5K0HFRC2CNE.usdcx",
        )?,
    };
    let mut parent =
        NakamotoChainState::get_block_header(chainstate.db(), &block.header.parent_block_id)?
            .context("traffic parent missing")?;
    parent = execute_segment(
        chainstate,
        sortdb,
        SegmentExecutionInput {
            cur_parent_info: &parent,
            block,
            seg: &TxSegment {
                range: 0..seed_index,
                sampled: false,
            },
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
    let mut previous_root = None::<String>;
    for (index, batch) in manifest.batches.iter().enumerate() {
        let end = offset + batch.transactions.len();
        let parent_id = parent.index_block_hash();
        let start = Instant::now();
        let result = execute_segment(
            chainstate,
            sortdb,
            SegmentExecutionInput {
                cur_parent_info: &parent,
                block: &template,
                seg: &TxSegment {
                    range: offset..end,
                    sampled: true,
                },
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
        )
        .with_context(|| {
            format!(
                "traffic block {index} ({:?}), transactions {offset}..{end}",
                batch.phase
            )
        })?;
        ensure!(
            result.accepted_end == end,
            "traffic block {index} split: accepted {} of {}; no TPS sample",
            result.accepted_end - offset,
            end - offset
        );
        ensure!(
            result.new_tip_info.stacks_block_height == parent.stacks_block_height + 1,
            "traffic height discontinuity"
        );
        let primary = result.setup_duration + result.execution_duration + result.commit_duration;
        let checkpoint = Instant::now();
        chainstate.checkpoint_sqlite_dbs()?;
        let checkpoint_duration = checkpoint.elapsed();
        let mut mix = BTreeMap::new();
        for op in &batch.transactions {
            *mix.entry(op.class.as_str()).or_insert(0usize) += 1;
        }
        let root = result.state_index_root.to_string();
        eprintln!(
            "TRAFFIC_BLOCK {}",
            serde_json::json!({"index":index,"phase":batch.phase,"transactions":end-offset,"tx_start":offset,"tx_end":end,"mix":mix,"parent_id":parent_id.to_string(),"block_id":result.new_tip_info.index_block_hash().to_string(),"height":result.new_tip_info.stacks_block_height,"previous_state_root":previous_root,"state_root":root,"node_work_us":primary.as_micros(),"setup_us":result.setup_duration.as_micros(),"execution_us":result.execution_duration.as_micros(),"seal_commit_us":result.commit_duration.as_micros(),"audit_us":result.audit_duration.as_micros(),"checkpoint_us":checkpoint_duration.as_micros(),"cost":result.segment_total_clarity_cost})
        );
        previous_root = Some(root);
        parent = result.new_tip_info;
        offset = end;
    }
    eprintln!(
        "TRAFFIC_COMPLETE {}",
        serde_json::json!({"blocks":manifest.batches.len(),"transactions":offset,"measured_transactions":manifest.batches.iter().filter(|b|b.phase==Phase::Measured).map(|b|b.transactions.len()).sum::<usize>()})
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Small executable transfer fixture used to verify ordering rather than mocks.
    fn example() -> Manifest {
        serde_json::from_value(serde_json::json!({"schema":1,"name":"test","description":"nonce continuity","source_block":"00".repeat(32),"seed_txid":"11".repeat(32),"accounts":2,"batches":[{"phase":"preparation","transactions":[{"account":0,"class":"transfer","action":{"kind":"transfer","recipient":1,"amount":1}}]},{"phase":"measured","transactions":[{"account":1,"class":"transfer","action":{"kind":"transfer","recipient":0,"amount":1}},{"account":0,"class":"transfer","action":{"kind":"transfer","recipient":1,"amount":1}}]}]})).unwrap()
    }
    /// Preparation is signed and consumes a nonce; another sender stays independent.
    #[test]
    fn traffic_manifest_nonce_continuity() {
        let m = example();
        m.validate().unwrap();
        let a = accounts(2).unwrap();
        let tx = transactions(&m, &a).unwrap();
        assert_eq!(
            tx.iter().map(|t| t.get_origin_nonce()).collect::<Vec<_>>(),
            vec![0, 0, 1]
        );
        assert_ne!(tx[0].txid(), tx[2].txid());
    }
    /// Shared actions produce exactly the same signed stream as inline actions.
    #[test]
    fn traffic_manifest_templates_preserve_signed_stream() {
        let mut m = example();
        let wallets = accounts(2).unwrap();
        let expected = transactions(&m, &wallets).unwrap();
        m.templates
            .push(m.batches[0].transactions[0].action.clone());
        m.batches[0].transactions[0].action =
            serde_json::from_str(r#"{"kind":"template","index":0}"#).unwrap();
        m.validate().unwrap();
        let actual = transactions(&m, &wallets).unwrap();
        assert_eq!(
            expected
                .iter()
                .map(StacksTransaction::txid)
                .collect::<Vec<_>>(),
            actual
                .iter()
                .map(StacksTransaction::txid)
                .collect::<Vec<_>>()
        );
        m.templates[0] = Action::Template { index: 0 };
        assert!(m.validate().is_err());
        m.templates.clear();
        assert!(m.validate().is_err());
    }

    /// Tagged-enum buffering cannot deserialize Clarity's full-width integers.
    #[test]
    fn traffic_manifest_clarity_integer_arguments() {
        let raw = r#"{"kind":"call","contract":"SP120SBRBQJ00MCWS7TM5R8WJNTTKD5K0HFRC2CNE.usdcx","function":"example","args":[{"Int":-311},{"UInt":340282366920938463463374607431768211455}]}"#;
        let action: Action = serde_json::from_str(raw).unwrap();
        let Action::Call { args, .. } = action else {
            panic!("expected call")
        };
        assert_eq!(args, vec![Value::Int(-311), Value::UInt(u128::MAX)]);
        assert!(
            serde_json::from_str::<Action>(&raw.replace("\"args\":", "\"recipient\":1,\"args\":"))
                .is_err()
        );
        assert!(
            serde_json::from_str::<Action>(&raw.replace("\"kind\":", "\"extra\":1,\"kind\":"))
                .is_err()
        );
    }

    /// Invalid phase ordering and self transfers must fail before mutating a shadow.
    #[test]
    fn traffic_manifest_rejects_invalid_plan() {
        let mut m = example();
        m.batches.swap(0, 1);
        assert!(m.validate().is_err());
        let mut m = example();
        m.batches[0].transactions[0].account = 1;
        assert!(m.validate().is_err());
        let mut m = example();
        m.batches.pop();
        assert!(m.validate().is_err());
    }
}
