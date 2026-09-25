// Copyright (C) 2025-2026 Stacks Open Internet Foundation
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

#[path = "capacity.rs"]
mod capacity;
#[path = "continuous.rs"]
mod continuous;
#[path = "growth.rs"]
mod growth;

use blockstack_lib::chainstate::stacks::address::StacksAddressExtensions;
use std::ops::Range;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use blockstack_lib::burnchains::Txid;
use blockstack_lib::chainstate::burn::db::sortdb::{SortitionDB, get_ancestor_sort_id};
use blockstack_lib::chainstate::nakamoto::NakamotoChainState;
use blockstack_lib::chainstate::nakamoto::miner::{MinerTenureInfoCause, NakamotoBlockBuilder};
use blockstack_lib::chainstate::stacks::db::StacksChainState;
use blockstack_lib::chainstate::stacks::miner::{
    BlockBuilder, BlockLimitFunction, TransactionResourceBudgets, TransactionResult,
};
use blockstack_lib::config::DEFAULT_MAX_TENURE_BYTES;
use clarity::vm::costs::ExecutionCost;
use sha2::{Digest, Sha256};
use stacks_common::types::chainstate::{StacksBlockId, TrieHash};

use crate::context::BenchContext;
use crate::metrics::{BlockMetrics, TransactionMetrics};
use crate::{BlockEra, ResolveEpochFromHeight, StacksBlockHeader};

#[derive(Debug, Clone)]
pub enum ReplayMode {
    Miner,
    Follower,
    Ephemeral,
    /// Execute via replay_nakamoto_by_segments() using build_segments_filtered()
    SegmentedFiltered(crate::filter::TxFilter),
    /// Single-tx mode: segments are built using build_segments_for_txid(),
    /// which produces prefix (unmeasured) + target (measured) only — no
    /// suffix transactions after the target are executed.
    SingleTx(crate::filter::TxFilter),
}

pub struct ReplayBlockRequest<'a> {
    pub mode: &'a ReplayMode,
    pub block_header: &'a StacksBlockHeader,
    pub repetition: u32,
    pub sample_metrics: bool,
}

impl std::fmt::Display for ReplayMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReplayMode::Miner => write!(f, "Miner"),
            ReplayMode::Follower => write!(f, "Follower"),
            ReplayMode::Ephemeral => write!(f, "Ephemeral"),
            ReplayMode::SegmentedFiltered(_) => write!(f, "SegmentedFiltered"),
            ReplayMode::SingleTx(_) => write!(f, "SingleTx"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SegmentReplayInfo {
    /// The index of the segment, in relation to other segments which may have
    /// been created/executed from the origin block.
    pub seg_ix: usize,
    /// The range of transactions from the origin block's transaction list which
    /// were executed as part of this segment.
    pub tx_range: Range<usize>,
    /// Whether this segment is sampled or not.
    pub sampled: bool,
}

#[derive(Clone, Debug)]
struct TxSegment {
    /// Contiguous tx range in `block.executed_and_skipped_txs()`
    range: Range<usize>,

    /// Whether to sample per-tx metrics and include in totals
    sampled: bool,
}

fn build_segments_full(
    block: &blockstack_lib::chainstate::nakamoto::NakamotoBlock,
) -> Vec<TxSegment> {
    vec![TxSegment {
        range: 0..block.executed_and_skipped_txs().len(),
        sampled: true,
    }]
}

fn build_segments_filtered(
    block: &blockstack_lib::chainstate::nakamoto::NakamotoBlock,
    filter: &crate::filter::TxFilter,
) -> Vec<TxSegment> {
    let n = block.executed_and_skipped_txs().len();
    if n == 0 {
        return vec![];
    }

    let mut out = Vec::new();
    let mut run_start = 0usize; // start of current "unmeasured run"

    for i in 0..n {
        let is_match = filter.matches(&block.executed_and_skipped_txs()[i]);
        if !is_match {
            continue;
        }

        // segment 1: unmeasured run [run_start..i) (may be empty)
        if run_start < i {
            out.push(TxSegment {
                range: run_start..i,
                sampled: false,
            });
        }

        // segment 2: measured singleton [i..i+1)
        out.push(TxSegment {
            range: i..(i + 1),
            sampled: true,
        });

        run_start = i + 1;
    }

    // trailing unmeasured run after last match
    if run_start < n {
        out.push(TxSegment {
            range: run_start..n,
            sampled: false,
        });
    }

    out
}

/// Build segments for single-tx replay mode: prefix (unmeasured) + target
/// (measured). Unlike `build_segments_filtered()`, no suffix transactions
/// after the target are executed since they are irrelevant to the measurement.
fn build_segments_for_txid(
    block: &blockstack_lib::chainstate::nakamoto::NakamotoBlock,
    filter: &crate::filter::TxFilter,
) -> Vec<TxSegment> {
    let n = block.executed_and_skipped_txs().len();
    if n == 0 {
        return vec![];
    }

    // Find the first matching transaction
    let match_idx = block
        .executed_and_skipped_txs()
        .iter()
        .position(|tx| filter.matches(tx));
    let Some(idx) = match_idx else {
        return vec![];
    };

    let mut out = Vec::new();

    // Prefix: unmeasured transactions before the target
    if idx > 0 {
        out.push(TxSegment {
            range: 0..idx,
            sampled: false,
        });
    }

    // Target: the single measured transaction
    out.push(TxSegment {
        range: idx..(idx + 1),
        sampled: true,
    });

    // No suffix — we don't execute transactions after the target
    out
}

fn compute_synthetic_id(
    origin: &StacksBlockId,
    seg_ix: usize,
    range: &std::ops::Range<usize>,
    repetition: u32,
) -> StacksBlockId {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(b"stacks-bench:synth-block:v1");
    hasher.update(origin.as_bytes());
    hasher.update((seg_ix as u64).to_le_bytes());
    hasher.update((range.start as u64).to_le_bytes());
    hasher.update((range.end as u64).to_le_bytes());
    hasher.update(repetition.to_le_bytes());

    let digest = hasher.finalize();
    let bytes = digest[..32].to_vec();
    StacksBlockId::from_vec(&bytes).expect("sha256 yields 32 bytes")
}

/// Re-execute all transactions in a block to measure execution performance.
///
/// Segmented mode returns 0..N measurement units (one per recorded segment).
///
/// `sample_metrics = false` disables profiler spans, per-phase timing,
/// metrics construction, and per-segment SQLite checkpoints.
pub fn replay_block<F>(
    context: &mut BenchContext,
    chainstate: &mut StacksChainState,
    sortdb: &SortitionDB,
    request: ReplayBlockRequest<'_>,
    on_segment: Option<&mut F>,
) -> Result<Option<Vec<BlockMetrics>>>
where
    F: FnMut(&SegmentReplayInfo, Option<&mut BlockMetrics>) -> Result<()>,
{
    let ReplayBlockRequest {
        mode,
        block_header,
        repetition,
        sample_metrics,
    } = request;
    let block_height = block_header.height;
    let epoch = context
        .resolve_stacks_epoch(block_height)
        .ok_or_else(|| anyhow!("Failed to resolve epoch for height {}", block_height))?;

    let metrics: Option<Vec<BlockMetrics>> = match context.resolve_block_era(epoch) {
        BlockEra::Nakamoto => {
            let (naka_block, _) = chainstate
                .nakamoto_blocks_db()
                .get_nakamoto_block(&block_header.id)?
                .ok_or_else(|| anyhow!("Nakamoto block not found"))?;

            if sample_metrics {
                if let Some(settings) = capacity::Settings::from_env()? {
                    ensure_capacity_mode(mode, repetition)?;
                    if std::env::var("STACKS_CAPACITY_GROWTH").as_deref() == Ok("1") {
                        anyhow::ensure!(
                            block_height == settings.end_height,
                            "growth requires a single selected source block"
                        );
                        if std::env::var("STACKS_CONTINUOUS_MIX").as_deref() == Ok("1") {
                            continuous::run(chainstate, sortdb, &naka_block)?;
                        } else {
                            growth::run(chainstate, sortdb, &naka_block)?;
                        }
                    } else {
                        capacity::consume(chainstate, sortdb, block_height, naka_block, settings)?;
                    }
                    return Ok(None);
                }
            }

            match mode {
                ReplayMode::Miner => bail!("Nakamoto Miner replay not implemented"),
                ReplayMode::Ephemeral => bail!("Nakamoto Ephemeral replay not implemented"),

                ReplayMode::Follower => {
                    let segments = build_segments_full(&naka_block);

                    let seg_metrics = replay_nakamoto_by_segments(
                        chainstate,
                        sortdb,
                        &naka_block,
                        &segments,
                        repetition,
                        sample_metrics,
                        on_segment,
                    )?;

                    if seg_metrics.is_empty() {
                        None
                    } else {
                        Some(seg_metrics)
                    }
                }

                ReplayMode::SegmentedFiltered(filter) => {
                    let segments = build_segments_filtered(&naka_block, filter);

                    // No recorded segments => no metrics.
                    if segments.is_empty() || segments.iter().all(|s| !s.sampled) {
                        return Ok(None);
                    }

                    // Do not wrap this in stacks_profiler::measure!: this path
                    // clears and drains profiler results per recorded segment.
                    let seg_metrics = replay_nakamoto_by_segments(
                        chainstate,
                        sortdb,
                        &naka_block,
                        &segments,
                        repetition,
                        sample_metrics,
                        on_segment,
                    )?;

                    if seg_metrics.is_empty() {
                        None
                    } else {
                        Some(seg_metrics)
                    }
                }

                ReplayMode::SingleTx(filter) => {
                    let segments = build_segments_for_txid(&naka_block, filter);

                    if segments.is_empty() || segments.iter().all(|s| !s.sampled) {
                        return Ok(None);
                    }

                    let seg_metrics = replay_nakamoto_by_segments(
                        chainstate,
                        sortdb,
                        &naka_block,
                        &segments,
                        repetition,
                        sample_metrics,
                        on_segment,
                    )?;

                    if seg_metrics.is_empty() {
                        None
                    } else {
                        Some(seg_metrics)
                    }
                }
            }
        }

        BlockEra::PreNakamoto => None,
    };

    Ok(metrics)
}

fn replay_nakamoto_by_segments<F>(
    chainstate: &mut StacksChainState,
    sortdb: &SortitionDB,
    block: &blockstack_lib::chainstate::nakamoto::NakamotoBlock,
    segments: &[TxSegment],
    repetition: u32,
    sample_metrics: bool,
    mut on_segment: Option<&mut F>,
) -> Result<Vec<BlockMetrics>>
where
    F: FnMut(&SegmentReplayInfo, Option<&mut BlockMetrics>) -> Result<()>,
{
    let origin_id = block.block_id();

    if segments.is_empty() {
        return Ok(vec![]);
    }

    let parent_block_id = block.header.parent_block_id.clone();
    let parent_info = NakamotoChainState::get_block_header(chainstate.db(), &parent_block_id)?
        .ok_or_else(|| anyhow!("Parent header not found"))?;

    let mut cur_parent_info = parent_info.clone();

    let mut out: Vec<BlockMetrics> = Vec::new();
    let mut last_state_index_root = None;

    for (seg_ix, seg) in segments.iter().enumerate() {
        if seg.range.is_empty() && !seg.sampled {
            continue;
        }

        let measure = sample_metrics && seg.sampled;

        assert!(
            block.header.problematic_txs.is_empty(),
            "benchmark requires pre-Epoch-4.0 transactions"
        );
        let segment_txs = &block.executed_and_skipped_txs()[seg.range.clone()];

        let _suppression = if sample_metrics && !seg.sampled {
            Some(stacks_profiler::Profiler::begin_suppression())
        } else {
            None
        };

        if measure {
            stacks_profiler::Profiler::clear();
        }

        let _segment_root = if measure {
            stacks_profiler::span!("Segment", seg_ix)
        } else {
            None
        };

        // Setup
        let setup_start = if measure { Some(Instant::now()) } else { None };

        let _setup_guard = if measure {
            stacks_profiler::span!("Segment: Setup", seg_ix)
        } else {
            None
        };

        let segment_tenure_change_tx: Option<
            &blockstack_lib::chainstate::stacks::StacksTransaction,
        > = segment_txs
            .iter()
            .find(|tx| tx.try_as_tenure_change().is_some());

        let segment_coinbase_tx: Option<&blockstack_lib::chainstate::stacks::StacksTransaction> =
            segment_txs.iter().find(|tx| tx.try_as_coinbase().is_some());

        let segment_cause = if let Some(tc_tx) = segment_tenure_change_tx {
            let tc_payload = tc_tx.try_as_tenure_change().expect("checked above");
            MinerTenureInfoCause::from(tc_payload.cause)
        } else {
            MinerTenureInfoCause::NoTenureChange
        };

        drop(_setup_guard);

        let exec_result = execute_segment(
            chainstate,
            sortdb,
            SegmentExecutionInput {
                cur_parent_info: &cur_parent_info,
                block,
                seg,
                seg_ix,
                segment_tenure_change_tx,
                segment_coinbase_tx,
                segment_cause,
                setup_start,
                repetition,
                measure,
                capacity: false,
                fixture: None,
            },
        )
        .with_context(|| {
            format!("Failed to replay segment {seg_ix} from origin block {origin_id}")
        })?;

        drop(_segment_root);
        if let Some(ref snapshot) = exec_result.state_cost {
            eprintln!(
                "STATE_COST {}",
                serde_json::json!({"origin":origin_id.to_string(),"segment":seg_ix,"snapshot":snapshot})
            );
        }

        last_state_index_root = Some(exec_result.state_index_root);

        let segment_profiler_roots = if measure {
            stacks_profiler::Profiler::take_results()
                .with_context(|| {
                    format!(
                        "Failed to drain profiler results for segment {seg_ix} from origin block {origin_id}"
                    )
                })?
        } else {
            vec![]
        };

        // Warmup skips per-segment checkpoints. The caller flushes once before
        // measurement so warmup WAL writes do not skew the first measured block.
        let clarity_db_checkpoint_duration = if sample_metrics {
            let checkpoint_start = Instant::now();
            chainstate.checkpoint_sqlite_dbs()?;
            checkpoint_start.elapsed()
        } else {
            Duration::ZERO
        };

        // Advance parent for burn_view inheritance in the next segment.
        cur_parent_info = exec_result.new_tip_info;

        let info = SegmentReplayInfo {
            seg_ix,
            tx_range: seg.range.clone(),
            sampled: seg.sampled,
        };

        if measure {
            let synthetic_id = compute_synthetic_id(&origin_id, seg_ix, &seg.range, repetition);

            let mut m = BlockMetrics::new_default(origin_id.clone(), synthetic_id.clone());
            m.setup_duration = exec_result.setup_duration;
            m.execution_duration = exec_result.execution_duration;
            m.commit_duration = exec_result.commit_duration;
            m.total_duration = exec_result.setup_duration
                + exec_result.execution_duration
                + exec_result.commit_duration;
            m.total_clarity_cost = exec_result.segment_total_clarity_cost;
            m.profiler_roots = segment_profiler_roots;
            m.clarity_db_checkpoint_duration = clarity_db_checkpoint_duration;

            for (txid, dur, cost) in exec_result.segment_tx_metrics {
                m.transactions.push(TransactionMetrics {
                    txid,
                    duration: dur,
                    cost,
                    profiler_roots: vec![],
                });
            }

            if let Some(cb) = on_segment.as_deref_mut() {
                cb(&info, Some(&mut m))?;
            }

            out.push(m);
        } else if let Some(cb) = on_segment.as_deref_mut() {
            cb(&info, None)?;
        }
    }

    // Validate state root only for canonical-equivalent replays — i.e. a
    // single segment covering every transaction in the block, executed at
    // `repetition == 0`. Two cases break the comparison even though Clarity
    // execution is bit-identical:
    //   * Multi-segment replays commit intermediate state under synthetic
    //     block IDs, altering the MARF trie structure.
    //   * `repetition > 0` synthesizes a shifted timestamp (see
    //     `execute_segment`) to avoid header-table collisions, which changes
    //     the block hash and therefore the MARF entries committed under it.
    if repetition == 0
        && segments.len() == 1
        && segments[0].range == (0..block.executed_and_skipped_txs().len())
        && let Some(replayed_root) = last_state_index_root
        && replayed_root != block.header.state_index_root
    {
        let tenure_tx = block.get_tenure_tx_payload();
        let tenure_cause = tenure_tx.as_ref().map(|t| format!("{:?}", t.cause));
        bail!(
            "State root mismatch for block {origin_id} \
             (height={}, consensus_hash={}, parent={}, \
             tenure={}, txs={}): \
             expected {}, got {replayed_root}",
            block.header.chain_length,
            block.header.consensus_hash,
            block.header.parent_block_id,
            tenure_cause.as_deref().unwrap_or("none"),
            block.executed_and_skipped_txs().len(),
            block.header.state_index_root,
        );
    }

    Ok(out)
}

struct SegmentExecResult {
    new_tip_info: blockstack_lib::chainstate::stacks::db::StacksHeaderInfo,
    commit_duration: Duration,
    setup_duration: Duration,
    execution_duration: Duration,
    segment_total_clarity_cost: ExecutionCost,
    segment_tx_metrics: Vec<(Txid, Duration, ExecutionCost)>,
    /// Exclusive end of transactions actually admitted to this segment.
    accepted_end: usize,
    /// Receipt serialization/audit time excluded from capacity execution time.
    audit_duration: Duration,
    state_index_root: TrieHash,
    /// Exclusive diagnostic partition, captured before report serialization.
    state_cost: Option<stacks_profiler::state_cost::Snapshot>,
}

struct SegmentExecutionInput<'a> {
    cur_parent_info: &'a blockstack_lib::chainstate::stacks::db::StacksHeaderInfo,
    block: &'a blockstack_lib::chainstate::nakamoto::NakamotoBlock,
    seg: &'a TxSegment,
    seg_ix: usize,
    segment_tenure_change_tx: Option<&'a blockstack_lib::chainstate::stacks::StacksTransaction>,
    segment_coinbase_tx: Option<&'a blockstack_lib::chainstate::stacks::StacksTransaction>,
    segment_cause: MinerTenureInfoCause,
    setup_start: Option<Instant>,
    repetition: u32,
    measure: bool,
    /// Apply production miner policy and emit capacity records.
    capacity: bool,
    /// Optional supply-preserving test funding, used only in an unmeasured preparation block.
    fixture: Option<&'a growth::Fixture>,
}

fn execute_segment(
    chainstate: &mut StacksChainState,
    sortdb: &SortitionDB,
    input: SegmentExecutionInput<'_>,
) -> Result<SegmentExecResult> {
    let _setup_diagnostic = stacks_profiler::diagnostic_span!("Replay: Full setup");
    let SegmentExecutionInput {
        cur_parent_info,
        block,
        seg,
        seg_ix,
        segment_tenure_change_tx,
        segment_coinbase_tx,
        segment_cause,
        setup_start,
        repetition,
        measure,
        capacity,
        fixture,
    } = input;
    let state_window = stacks_profiler::state_cost::Window::new(measure);
    let state_setup = stacks_profiler::state_cost::phase("setup");

    // Keep repeated synthetic blocks from colliding in MARF/header tables.
    // Clarity reads block/burn height from parent state, not this timestamp.
    let synth_timestamp = block
        .header
        .timestamp
        .checked_add(repetition as u64)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "timestamp overflow: base {} + repetition {repetition} exceeds u64::MAX",
                block.header.timestamp
            )
        })?;

    let max_tenure_bytes = continuous::tenure_byte_limit(
        capacity,
        std::env::var("STACKS_CONTINUOUS_MIX").as_deref() == Ok("1"),
    );
    let mut builder = NakamotoBlockBuilder::new(
        cur_parent_info,
        &block.header.consensus_hash,
        block.header.burn_spent,
        segment_tenure_change_tx,
        segment_coinbase_tx,
        block.header.pox_treatment.len(),
        None,
        None,
        Some(synth_timestamp),
        max_tenure_bytes,
    )?;

    let cur_parent_block_id = StacksBlockId::new(
        &cur_parent_info.consensus_hash,
        &cur_parent_info.anchored_header.block_hash(),
    );

    // Tenure-change blocks execute against the burn view named by the payload.
    let burn_dbconn = if let Some(tenure_change_tx) = segment_tenure_change_tx {
        let tenure_change = tenure_change_tx
            .try_as_tenure_change()
            .expect("tenure change tx checked by caller");

        if let Some(ref parent_burn_view) = cur_parent_info.burn_view {
            let parent_burn_view_sn =
                SortitionDB::get_block_snapshot_consensus(sortdb.conn(), parent_burn_view)?
                    .ok_or_else(|| {
                        anyhow!(
                            "parent block burn view {parent_burn_view} was not found while replaying tenure-change block {}",
                            block.block_id()
                        )
                    })?;
            let handle = sortdb.index_handle_at_ch(&tenure_change.burn_view_consensus_hash)?;
            let connected_sort_id = get_ancestor_sort_id(
                &handle,
                parent_burn_view_sn.block_height,
                &handle.context.chain_tip,
            )?
            .ok_or_else(|| {
                anyhow!(
                    "tenure-change burn view {} does not descend from parent burn view {parent_burn_view} while replaying block {}",
                    tenure_change.burn_view_consensus_hash,
                    block.block_id()
                )
            })?;
            if connected_sort_id != parent_burn_view_sn.sortition_id {
                bail!(
                    "tenure-change burn view {} is not connected to parent burn view {parent_burn_view} while replaying block {}",
                    tenure_change.burn_view_consensus_hash,
                    block.block_id()
                );
            }

            handle
        } else {
            sortdb.index_handle_at_ch(&tenure_change.burn_view_consensus_hash)?
        }
    } else {
        sortdb.index_handle_at_block(chainstate, &cur_parent_block_id)?
    };

    let mut miner_tenure_info =
        builder.load_tenure_info(chainstate, &burn_dbconn, segment_cause)?;

    let burn_chain_height = miner_tenure_info.burn_tip_height;
    let coinbase_height = miner_tenure_info.coinbase_height;
    let is_new_tenure = segment_cause.is_new_tenure();
    let mut clarity_tx = builder.tenure_begin(&burn_dbconn, &mut miner_tenure_info)?;
    let mut miner_config = blockstack_lib::config::MinerConfig::default();
    if capacity {
        if let Ok(raw) = std::env::var("STACKS_GROWTH_SOFT_PERCENT") {
            let percent: u8 = raw.parse().context("invalid soft limit percentage")?;
            anyhow::ensure!(
                (1..=100).contains(&percent),
                "soft limit must be 1..=100 percent"
            );
            miner_config.tenure_cost_limit_per_block_percentage = Some(percent);
        }
    }
    if capacity && std::env::var("STACKS_RELAXED_COSTS").as_deref() == Ok("1") {
        let original = clarity_tx
            .block_limit()
            .context("relaxed costs require a metered tracker")?;
        let mut raised = original.clone();
        raised
            .multiply(100)
            .map_err(|e| anyhow!("cost limit overflow: {e:?}"))?;
        let connection = clarity_tx.connection();
        let mut tracker =
            connection.set_cost_tracker(clarity::vm::costs::LimitedCostTracker::new_free());
        let previous_total = tracker.get_total();
        let previous_memory = tracker.get_memory();
        if std::env::var("STACKS_CONTINUOUS_MIX").as_deref() == Ok("1") {
            raised
                .add(&previous_total)
                .map_err(|e| anyhow!("continuous allowance overflow: {e:?}"))?;
        }
        tracker.benchmark_set_limit(raised.clone());
        assert_eq!(tracker.get_total(), previous_total);
        assert_eq!(tracker.get_memory(), previous_memory);
        connection.set_cost_tracker(tracker);
        miner_config.tenure_cost_limit_per_block_percentage = None;
        eprintln!(
            "RELAXED_BUDGET {}",
            serde_json::json!({"original_tenure_bytes": DEFAULT_MAX_TENURE_BYTES, "tenure_bytes": max_tenure_bytes, "parent_tenure_bytes": cur_parent_info.total_tenure_size, "original": original, "raised": raised, "spent": previous_total, "multiplier": 100, "metered": true, "per_block_allowance": std::env::var("STACKS_CONTINUOUS_MIX").as_deref() == Ok("1")})
        );
    }
    let resource_budgets = if capacity {
        capacity::resource_budgets()
    } else {
        TransactionResourceBudgets::unlimited()
    };
    let mut capacity_soft_limit = None;
    if capacity {
        if let Some(percentage) = miner_config.tenure_cost_limit_per_block_percentage {
            let mut limit = clarity_tx
                .block_limit()
                .context("capacity requires consensus cost limit")?;
            let spent = clarity_tx.cost_so_far();
            limit
                .sub(&spent)
                .map_err(|e| anyhow!("tenure budget exhausted: {e:?}"))?;
            limit.divide(100).map_err(|e| anyhow!("divide: {e:?}"))?;
            limit
                .multiply(percentage.into())
                .map_err(|e| anyhow!("multiply: {e:?}"))?;
            limit.add(&spent).map_err(|e| anyhow!("add: {e:?}"))?;
            capacity_soft_limit = Some(limit);
        }
    }

    drop(state_setup);
    drop(_setup_diagnostic);
    let setup_duration = setup_start.map(|s| s.elapsed()).unwrap_or(Duration::ZERO);

    // Transaction execution
    let exec_start = if measure { Some(Instant::now()) } else { None };

    let _exec_guard = if measure {
        stacks_profiler::span!("Segment: Tx Execution", seg_ix)
    } else {
        None
    };

    let mut segment_tx_metrics: Vec<(Txid, Duration, ExecutionCost)> = Vec::new();
    let mut segment_total_clarity_cost = ExecutionCost::ZERO;
    let mut total_receipts_size = 0u64;

    let mut accepted_end = seg.range.start;
    let mut audit_duration = Duration::ZERO;
    let mut capacity_receipts = Vec::new();
    let mut stop_reason = "candidate-exhausted";
    let mut receipt_digest = Sha256::new();
    let mut receipt_count = 0usize;
    let audit_receipts =
        capacity || std::env::var("STACKS_REPLAY_AUDIT_RECEIPTS").as_deref() == Ok("1");
    let starting_cost = clarity_tx.cost_so_far();

    for i in seg.range.clone() {
        let tx = &block.executed_and_skipped_txs()[i];
        let tx_len = tx.tx_len();

        if measure {
            stacks_profiler::diagnostics::reset();
        }
        let tx_start = if measure { Some(Instant::now()) } else { None };

        let rel_i = i - seg.range.start;

        let _tx_guard = if measure {
            stacks_profiler::span!("Transaction", rel_i)
        } else {
            None
        };

        let state_tx = stacks_profiler::state_cost::phase("transaction");
        let res = builder.try_mine_tx_with_len(
            &mut clarity_tx,
            tx,
            tx_len,
            &BlockLimitFunction::NO_LIMIT_HIT,
            &resource_budgets,
            &mut total_receipts_size,
        );

        drop(state_tx);
        drop(_tx_guard);

        let dur = tx_start.map(|s| s.elapsed()).unwrap_or(Duration::ZERO);

        if measure && stacks_profiler::diagnostics::enabled() {
            eprintln!(
                "WRITEBACK_COUNTERS {}",
                serde_json::json!({"txid": tx.txid().to_string(), "block_id": block.block_id().to_string(), "counts": stacks_profiler::diagnostics::snapshot()})
            );
        }
        let success = match res {
            TransactionResult::Success(ref s) => s,
            TransactionResult::ProcessingError(ref error)
                if capacity && i > seg.range.start && capacity::splittable(&error.error) =>
            {
                stop_reason = "block-size-or-receipts";
                break;
            }
            TransactionResult::Skipped(ref skip)
                if capacity && i > seg.range.start && capacity::splittable(&skip.error) =>
            {
                stop_reason = "block-size-or-receipts";
                break;
            }
            _ => {
                clarity_tx.rollback_block();
                return Err(anyhow!(
                    "Tx #{i} (0x{}) failed while executing segment #{seg_ix} ({:?}): {res:?}",
                    tx.txid(),
                    seg.range
                ));
            }
        };

        accepted_end = i + 1;
        receipt_count += 1;
        if audit_receipts {
            let audit_start = Instant::now();
            let state_receipt = stacks_profiler::state_cost::phase("receipts");
            let receipt_probe_suppression = stacks_profiler::state_cost::suppress();
            let _receipt_diagnostic =
                stacks_profiler::diagnostic_span!("Replay: Receipt validation");
            // These checks run after the transaction timer and profiler span have stopped.
            let receipt = &success.receipt;
            let events = receipt
                .events
                .iter()
                .enumerate()
                .map(|(index, event)| {
                    event.json_serialize(index, &tx.txid(), !receipt.post_condition_aborted)
                })
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let semantics = serde_json::to_vec(&serde_json::json!({
                "txid": tx.txid().to_string(),
                "result": hex::encode(receipt.result.serialize_to_vec()?),
                "events": events,
                "post_condition_aborted": receipt.post_condition_aborted,
                "stx_burned": receipt.stx_burned.to_string(),
                "cost": receipt.execution_cost,
                "problematic_skipped": receipt.problematic_skipped,
                "vm_error": receipt.vm_error.as_ref().map(|error| format!("{error:?}")),
            }))?;
            if capacity {
                let is_call = matches!(
                    &tx.payload,
                    blockstack_lib::chainstate::stacks::TransactionPayload::ContractCall(_)
                );
                let application_success = receipt.vm_error.is_none()
                    && !receipt.post_condition_aborted
                    && match &receipt.result {
                        clarity::vm::Value::Response(response) => response.committed,
                        _ => true,
                    };
                if std::env::var("STACKS_CAPACITY_GROWTH").as_deref() == Ok("1") {
                    anyhow::ensure!(
                        application_success,
                        "generated transaction aborted: {:?}",
                        receipt.result
                    );
                }
                capacity_receipts.push(serde_json::json!({
                    "txid":tx.txid().to_string(),"contract_call":is_call,"application_success":application_success,
                    "payload":format!("{:?}",tx.payload),"wall_us":dur.as_micros(),
                    "receipt_sha256":hex::encode(Sha256::digest(&semantics)),"cost":receipt.execution_cost,
                }));
            }
            receipt_digest.update((semantics.len() as u64).to_le_bytes());
            receipt_digest.update(semantics);
            drop(receipt_probe_suppression);
            drop(state_receipt);
            drop(_receipt_diagnostic);
            audit_duration += audit_start.elapsed();
        }

        if measure {
            let cost = success.receipt.execution_cost.clone();
            segment_total_clarity_cost
                .add(&cost)
                .map_err(|e| anyhow!("cost addition: {e:?}"))?;
            segment_tx_metrics.push((tx.txid(), dur, cost));
        }
        if capacity
            && capacity_soft_limit.as_ref().is_some_and(|limit| {
                let non_boot_call = match &tx.payload {
                    blockstack_lib::chainstate::stacks::TransactionPayload::ContractCall(call) => {
                        !call.address.is_boot_code_addr()
                    }
                    blockstack_lib::chainstate::stacks::TransactionPayload::SmartContract(..) => {
                        true
                    }
                    _ => false,
                };
                non_boot_call && clarity_tx.cost_so_far().exceeds(limit)
            })
        {
            stop_reason = "miner-soft-cost-limit";
            break;
        }
    }

    drop(_exec_guard);
    if let Some(fixture) = fixture {
        fixture.apply(&mut clarity_tx)?;
    }

    let execution_duration = exec_start.map(|s| s.elapsed()).unwrap_or(Duration::ZERO);
    let execution_duration = if capacity {
        execution_duration.saturating_sub(audit_duration)
    } else {
        execution_duration
    };

    #[cfg(feature = "commit-residency-diagnostics")]
    stacks_profiler::diagnostics::reset();
    // Commit
    let commit_start = if measure { Some(Instant::now()) } else { None };

    let total_tenure_cost = clarity_tx.cost_so_far();
    let mut block_execution_cost = clarity_tx.cost_so_far();
    block_execution_cost
        .sub(&starting_cost)
        .map_err(|e| anyhow!("cost subtraction: {e:?}"))?;

    let segment_block_size = builder.get_bytes_so_far();

    let state_seal = stacks_profiler::state_cost::phase("seal");
    let _finalize_guard = if measure {
        stacks_profiler::span!("Segment: Finalize (merkle+seal)", seg_ix)
    } else {
        None
    };

    let mined_block = builder.mine_nakamoto_block(&mut clarity_tx, burn_chain_height);
    let mined_block_hash = mined_block.header.block_hash();
    let mined_state_index_root = mined_block.header.state_index_root;
    let mined_consensus_hash = mined_block.header.consensus_hash.clone();
    let evaluated_epoch = clarity_tx.get_epoch();

    drop(state_seal);
    drop(_finalize_guard);

    let state_clarity_commit = stacks_profiler::state_cost::phase("clarity_commit");
    let _clarity_commit_guard = if measure {
        stacks_profiler::span!("Segment: Clarity State Commit", seg_ix)
    } else {
        None
    };

    clarity_tx.commit_to_block(&mined_consensus_hash, &mined_block_hash);

    drop(state_clarity_commit);
    drop(_clarity_commit_guard);

    let burn_view = NakamotoChainState::get_block_burn_view(sortdb, &mined_block, cur_parent_info)?;

    let sn = SortitionDB::get_block_snapshot_consensus(sortdb.conn(), &mined_consensus_hash)?
        .ok_or_else(|| anyhow!("Snapshot not found for {}", mined_consensus_hash))?;

    let block_fees: u128 = (seg.range.start..accepted_end)
        .map(|i| block.executed_and_skipped_txs()[i].get_tx_fee() as u128)
        .sum();

    // Tenure-start blocks need scheduled rewards for later matured-reward lookups.
    let scheduled_miner_reward = if is_new_tenure {
        let parent_coinbase_height = coinbase_height
            .checked_sub(1)
            .expect("coinbase_height underflow on tenure-start block");
        let (commit_burn, sortition_burn) = {
            let block_commit = SortitionDB::get_block_commit(
                sortdb.conn(),
                &sn.winning_block_txid,
                &sn.sortition_id,
            )?
            .ok_or_else(|| {
                anyhow!(
                    "No block-commit for tenure-start snapshot {}",
                    sn.sortition_id,
                )
            })?;
            let sort_burn = SortitionDB::get_block_burn_amount(sortdb.conn(), &sn)?;
            (block_commit.burn_fee, sort_burn)
        };
        Some(NakamotoChainState::calculate_scheduled_tenure_reward(
            &mut miner_tenure_info.chainstate_tx,
            &burn_dbconn,
            &mined_block,
            evaluated_epoch,
            parent_coinbase_height,
            burn_chain_height.into(),
            commit_burn,
            sortition_burn,
        )?)
    } else {
        None
    };

    let state_advance_tip = stacks_profiler::state_cost::phase("advance_tip");
    let _advance_chain_tip_guard = if measure {
        stacks_profiler::span!("Segment: Advance Chain Tip", seg_ix)
    } else {
        None
    };

    let new_tip_info = NakamotoChainState::advance_tip(
        &mut miner_tenure_info.chainstate_tx.tx,
        &cur_parent_info.anchored_header,
        &cur_parent_info.consensus_hash,
        &mined_block,
        None,
        &sn.burn_header_hash,
        sn.block_height as u32,
        sn.burn_header_timestamp,
        scheduled_miner_reward.as_ref(),
        None,
        &block_execution_cost,
        &total_tenure_cost,
        segment_block_size,
        false,
        vec![],
        vec![],
        vec![],
        vec![],
        is_new_tenure,
        coinbase_height,
        block_fees,
        &burn_view,
    )?;

    drop(state_advance_tip);
    drop(_advance_chain_tip_guard);

    let state_headers_commit = stacks_profiler::state_cost::phase("headers_commit");
    let _index_commit_guard = if measure {
        stacks_profiler::span!("Segment: Index Commit", seg_ix)
    } else {
        None
    };

    let blockstack_lib::chainstate::nakamoto::miner::MinerTenureInfo { chainstate_tx, .. } =
        miner_tenure_info;
    chainstate_tx.commit()?;

    drop(builder);
    drop(state_headers_commit);
    drop(_index_commit_guard);

    let commit_duration = commit_start.map(|s| s.elapsed()).unwrap_or(Duration::ZERO);
    let state_cost = state_window.finish();

    #[cfg(feature = "commit-residency-diagnostics")]
    if measure {
        eprintln!(
            "COMMIT_COUNTERS {}",
            serde_json::json!({"counts": stacks_profiler::diagnostics::snapshot()})
        );
    }
    if capacity {
        let audit_start = Instant::now();
        eprintln!(
            "CAPACITY_RECEIPTS {}",
            serde_json::json!({"index":seg_ix,"stop_reason":stop_reason,"transactions":capacity_receipts,"tenure_cost":total_tenure_cost,"soft_percent":miner_config.tenure_cost_limit_per_block_percentage})
        );
        audit_duration += audit_start.elapsed();
    }
    // Retain warmup and unselected segments too: their state feeds measured execution.
    eprintln!(
        "REPLAY_SEMANTICS {}",
        serde_json::json!({
            "block_id": block.block_id().to_string(),
            "segment": seg_ix,
            "range_start": seg.range.start,
            "range_end": accepted_end,
            "measured": measure,
            "transactions": receipt_count,
            "receipts_sha256": hex::encode(receipt_digest.finalize()),
            "state_root": mined_state_index_root.to_string(),
        })
    );

    Ok(SegmentExecResult {
        new_tip_info,
        commit_duration,
        setup_duration,
        execution_duration,
        segment_total_clarity_cost,
        segment_tx_metrics,
        state_index_root: mined_state_index_root,
        state_cost,
        accepted_end,
        audit_duration,
    })
}

/// Require a single ordered full-block replay for capacity packing.
fn ensure_capacity_mode(mode: &ReplayMode, repetition: u32) -> Result<()> {
    if !matches!(mode, ReplayMode::Follower) || repetition != 0 {
        bail!("capacity mode requires full-block replay without repetitions");
    }
    Ok(())
}
