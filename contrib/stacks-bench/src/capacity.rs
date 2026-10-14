//! Benchmark-only ordered packing within historical tenure boundaries.

use std::env;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use anyhow::{Context, Result, ensure};
use blockstack_lib::chainstate::burn::db::sortdb::SortitionDB;
use blockstack_lib::chainstate::nakamoto::miner::MinerTenureInfoCause;
use blockstack_lib::chainstate::nakamoto::{NakamotoBlock, NakamotoChainState};
use blockstack_lib::chainstate::stacks::db::{StacksChainState, StacksHeaderInfo};
use blockstack_lib::chainstate::stacks::miner::{BlockBuilderSettings, TransactionResourceBudgets};
use blockstack_lib::chainstate::stacks::{Error as ChainError, StacksTransaction};
use blockstack_lib::config::MinerConfig;

use super::{SegmentExecutionInput, TxSegment, execute_segment};

/// Packing configuration required explicitly for each benchmark invocation.
#[derive(Clone, Copy, Debug)]
pub(super) struct Settings {
    /// Maximum original heights combined before admission splits the candidate.
    pub factor: usize,
    /// Last measured original height, used to drain the final partial candidate.
    pub end_height: u64,
}

impl Settings {
    /// Read and validate the optional benchmark configuration.
    pub fn from_env() -> Result<Option<Self>> {
        let Ok(raw) = env::var("STACKS_CAPACITY_FACTOR") else {
            return Ok(None);
        };
        let factor = raw.parse().context("invalid STACKS_CAPACITY_FACTOR")?;
        ensure!(
            (1..=1000).contains(&factor),
            "packing factor must be 1..=1000"
        );
        let end_height = env::var("STACKS_CAPACITY_END_HEIGHT")
            .context("STACKS_CAPACITY_END_HEIGHT required")?
            .parse()?;
        Ok(Some(Self { factor, end_height }))
    }
}

/// Build miner resource budgets from the pinned source's actual defaults.
pub(super) fn resource_budgets() -> TransactionResourceBudgets {
    let config = MinerConfig::default();
    let mut settings = BlockBuilderSettings::limited();
    settings.max_execution_time = Some(std::time::Duration::from_secs(
        config.max_execution_time_secs,
    ));
    settings.max_analysis_time = Some(std::time::Duration::from_secs(
        config.max_analysis_time_secs,
    ));
    settings.max_assembly_mem_bytes = config.max_assembly_mem_bytes;
    TransactionResourceBudgets::from_settings(&settings)
}

/// Only per-block size/receipt limits can be retried in another block.
pub(super) fn splittable(error: &ChainError) -> bool {
    matches!(
        error,
        ChainError::TxWouldNotFitError | ChainError::BlockCostExceeded
    )
}

/// Determine whether a candidate must end before the next original block.
fn boundary(previous: &NakamotoBlock, next: &NakamotoBlock) -> bool {
    previous.header.consensus_hash != next.header.consensus_hash
        || next
            .executed_and_skipped_txs()
            .iter()
            .any(|tx| tx.try_as_tenure_change().is_some())
}

/// Buffered historical candidate plus its independent synthetic tenure branch.
#[derive(Default)]
struct State {
    /// Original blocks awaiting execution.
    pending: Vec<(u64, NakamotoBlock)>,
    /// Last original block received, including already flushed blocks.
    previous: Option<NakamotoBlock>,
    /// Most recent committed synthetic header in this tenure.
    tip: Option<StacksHeaderInfo>,
    /// Number of synthetic blocks committed in this invocation.
    blocks: usize,
}

/// One serial benchmark stream per process, independent of async worker migration.
static STATE: OnceLock<Mutex<State>> = OnceLock::new();

/// Consume one original block and execute completed packed candidates.
pub(super) fn consume(
    chainstate: &mut StacksChainState,
    sortdb: &SortitionDB,
    height: u64,
    block: NakamotoBlock,
    settings: Settings,
) -> Result<()> {
    {
        let mut state = STATE
            .get_or_init(|| Mutex::new(State::default()))
            .lock()
            .map_err(|_| anyhow::anyhow!("capacity state lock poisoned"))?;
        ensure!(
            height <= settings.end_height,
            "capacity end height precedes input"
        );
        if let Some(previous) = state.previous.as_ref() {
            ensure!(
                block.header.parent_block_id == previous.block_id(),
                "non-contiguous input stream"
            );
            if boundary(previous, &block) {
                state.flush(chainstate, sortdb, settings, "tenure-boundary")?;
                state.tip = None;
                eprintln!(
                    "CAPACITY_BOUNDARY {}",
                    serde_json::json!({"height":height,"action":"restore-historical-parent"})
                );
            }
        }
        eprintln!(
            "CAPACITY_INPUT {}",
            serde_json::json!({"height":height,"block_id":block.block_id().to_string(),"txids":block.executed_and_skipped_txs().iter().map(|tx|tx.txid().to_string()).collect::<Vec<_>>()})
        );
        let special = block
            .executed_and_skipped_txs()
            .iter()
            .any(|tx| tx.try_as_tenure_change().is_some());
        state.previous = Some(block.clone());
        state.pending.push((height, block));
        let reason = if special {
            Some("tenure-change")
        } else if height == settings.end_height {
            Some("end-of-range")
        } else if state.pending.len() >= settings.factor {
            Some("packing-target")
        } else {
            None
        };
        if let Some(reason) = reason {
            state.flush(chainstate, sortdb, settings, reason)?;
        }
        Ok(())
    }
}

impl State {
    /// Execute a candidate, carrying unconsumed transactions into further blocks.
    fn flush(
        &mut self,
        chainstate: &mut StacksChainState,
        sortdb: &SortitionDB,
        settings: Settings,
        reason: &str,
    ) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let first = &self.pending[0].1;
        let first_height = self.pending[0].0;
        let last_height = self.pending.last().unwrap().0;
        let txs: Vec<StacksTransaction> = self
            .pending
            .iter()
            .flat_map(|(_, block)| block.executed_and_skipped_txs().iter().cloned())
            .collect();
        let source_blocks: Vec<_> = self.pending.iter().map(|(height, block)| serde_json::json!({"height":height,"id":block.block_id().to_string(),"transactions":block.tx_count()})).collect();
        let template = NakamotoBlock::new(first.header.clone(), txs);
        if self.tip.is_none() {
            self.tip = Some(
                NakamotoChainState::get_block_header(
                    chainstate.db(),
                    &first.header.parent_block_id,
                )?
                .context("capacity parent header missing")?,
            );
        }
        let mut offset = 0;
        loop {
            let seg = TxSegment {
                range: offset..template.tx_count(),
                sampled: true,
            };
            let txs = &template.executed_and_skipped_txs()[seg.range.clone()];
            let tenure_change = txs.iter().find(|tx| tx.try_as_tenure_change().is_some());
            let coinbase = txs.iter().find(|tx| tx.try_as_coinbase().is_some());
            let cause = tenure_change
                .map(|tx| MinerTenureInfoCause::from(tx.try_as_tenure_change().unwrap().cause))
                .unwrap_or(MinerTenureInfoCause::NoTenureChange);
            let start = Instant::now();
            let result = execute_segment(
                chainstate,
                sortdb,
                SegmentExecutionInput {
                    cur_parent_info: self.tip.as_ref().unwrap(),
                    block: &template,
                    seg: &seg,
                    seg_ix: self.blocks,
                    segment_tenure_change_tx: tenure_change,
                    segment_coinbase_tx: coinbase,
                    segment_cause: cause,
                    setup_start: Some(start),
                    repetition: 0,
                    measure: true,
                    capacity: true,
                    fixture: None,
                },
            )
            .with_context(|| {
                format!("capacity candidate {first_height}..{last_height}, tx offset {offset}")
            })?;
            let elapsed = start.elapsed();
            let checkpoint = Instant::now();
            chainstate.checkpoint_sqlite_dbs()?;
            let checkpoint_us = checkpoint.elapsed().as_micros();
            eprintln!(
                "CAPACITY_BLOCK {}",
                serde_json::json!({
                    "index": self.blocks,"factor": settings.factor,"first_height":first_height,"last_height":last_height,
                    "source_blocks":source_blocks,"flush_reason":reason,"tx_start":offset,"tx_end":result.accepted_end,
                    "execution_us":result.execution_duration.as_micros(),"setup_us":result.setup_duration.as_micros(),
                    "commit_us":result.commit_duration.as_micros(),"audit_us":result.audit_duration.as_micros(),
                    "wall_us":elapsed.saturating_sub(result.audit_duration).as_micros(),"checkpoint_us":checkpoint_us,
                    "state_root":result.state_index_root.to_string(),"cost":result.segment_total_clarity_cost,
                    "synthetic_height":result.new_tip_info.stacks_block_height,
                })
            );
            ensure!(
                result.accepted_end > offset || template.tx_count() == 0,
                "admission made no progress"
            );
            offset = result.accepted_end;
            self.tip = Some(result.new_tip_info);
            self.blocks += 1;
            if offset == template.tx_count() {
                break;
            }
        }
        self.pending.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_only_per_block_limits_are_splittable() {
        assert!(splittable(&ChainError::TxWouldNotFitError));
        assert!(splittable(&ChainError::BlockCostExceeded));
        assert!(!splittable(&ChainError::TenureTooBigError));
        assert!(!splittable(&ChainError::BlockCostLimitError));
        assert!(!splittable(&ChainError::InvalidFee));
    }
}
