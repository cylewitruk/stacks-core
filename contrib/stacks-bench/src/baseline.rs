//! Per-machine block-processing overhead baseline.
//!
//! The baseline measures how long it takes to mine and commit an *empty*
//! Nakamoto block on the host machine. Subtracting this overhead from a
//! measured block's per-phase timings isolates the cost contributed by that
//! block's actual transaction workload.
//!
//! The pipeline has two layers:
//!
//! - [`EmptyBaselineRunner`] — streams one [`EmptyBaselineSample`] per
//!   [`EmptyBaselineRunner::step`] call, holding the sortDB connection and
//!   the current parent header so per-block overhead is invariant across
//!   calls.
//! - [`run_convergent_baseline`] — drives the runner in fixed-size segments
//!   and stops once the rolling mean over the last
//!   [`BASELINE_CONVERGENCE_WINDOW`] segments matches the prior window within
//!   [`BASELINE_CONVERGENCE_THRESHOLD`]. The final result is the mean of the
//!   last `BASELINE_CONVERGENCE_WINDOW` segments.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use blockstack_lib::burnchains::Burnchain;
use blockstack_lib::chainstate::burn::db::sortdb::SortitionDB;
use blockstack_lib::chainstate::nakamoto::NakamotoChainState;
use blockstack_lib::chainstate::nakamoto::miner::{MinerTenureInfoCause, NakamotoBlockBuilder};
use blockstack_lib::chainstate::stacks::db::{StacksChainState, StacksHeaderInfo};
use blockstack_lib::config::DEFAULT_MAX_TENURE_BYTES;
use stacks_common::types::chainstate::StacksBlockId;

use crate::bench_events::{self, BenchEvent, BenchEventSender};
use crate::metrics::BlockProcessingBaseline;

/// Number of empty blocks averaged into each segment.
pub const BASELINE_SEGMENT_SIZE: u32 = 50;
/// Minimum segments collected before convergence checks engage. Combined
/// with [`BASELINE_CONVERGENCE_WINDOW`], the first segments
/// `[0, BASELINE_MIN_SEGMENTS - 2 * BASELINE_CONVERGENCE_WINDOW)` are
/// permanently discarded as warm-up — they never appear in any comparison
/// that can trigger early-stop.
pub const BASELINE_MIN_SEGMENTS: u32 = 15;
/// Cap on total segments; bounds worst-case work at
/// `BASELINE_MAX_SEGMENTS * BASELINE_SEGMENT_SIZE` empty blocks.
pub const BASELINE_MAX_SEGMENTS: u32 = 60;
/// Size of the rolling window used for both convergence comparison and the
/// final reported average.
pub const BASELINE_CONVERGENCE_WINDOW: u32 = 5;
/// Convergence threshold: relative change between the recent and prior
/// rolling-window totals below this fraction declares convergence.
pub const BASELINE_CONVERGENCE_THRESHOLD: f64 = 0.05;

/// Phase timings for one empty-block baseline iteration.
#[derive(Debug, Clone, Copy, Default)]
pub struct EmptyBaselineSample {
    pub setup: Duration,
    pub finalize: Duration,
    pub clarity_commit: Duration,
    pub advance_tip: Duration,
    pub index_commit: Duration,
}

impl EmptyBaselineSample {
    pub fn total(&self) -> Duration {
        self.setup + self.finalize + self.clarity_commit + self.advance_tip + self.index_commit
    }
}

/// Streaming runner that produces one empty-block baseline sample per call to
/// [`Self::step`]. Holds the sortDB connection and current parent header so
/// per-block overhead is invariant across invocations — segmenting the output
/// adds no per-segment cost.
pub struct EmptyBaselineRunner<'a> {
    chainstate: &'a mut StacksChainState,
    sortdb: SortitionDB,
    cur_parent_info: StacksHeaderInfo,
    iter: u32,
}

impl<'a> EmptyBaselineRunner<'a> {
    pub fn new(
        chainstate: &'a mut StacksChainState,
        burnchain: &Burnchain,
        start_parent_block_id: &StacksBlockId,
    ) -> Result<Self> {
        let cur_parent_info =
            NakamotoChainState::get_block_header(chainstate.db(), start_parent_block_id)?
                .ok_or_else(|| anyhow!("Parent header not found: {start_parent_block_id}"))?;
        let sortdb = burnchain
            .open_sortition_db(false)
            .with_context(|| "open sortition db (readonly) for baseline")?;
        Ok(Self {
            chainstate,
            sortdb,
            cur_parent_info,
            iter: 0,
        })
    }

    pub fn step(&mut self) -> Result<EmptyBaselineSample> {
        let i = self.iter;
        let setup_start = Instant::now();

        // Empty baseline blocks extend the parent's current tenure. In Nakamoto
        // terms the block header consensus hash is the tenure/election consensus
        // hash, not the parent's current burn view; the burn view may be a
        // no-sortition burn block and therefore have no winning block commit.
        let tenure_id_consensus_hash = self.cur_parent_info.consensus_hash.clone();

        let baseline_sn = SortitionDB::get_block_snapshot_consensus(
            self.sortdb.conn(),
            &tenure_id_consensus_hash,
        )?
        .ok_or_else(|| {
            anyhow!(
                "baseline: missing sortition snapshot for tenure consensus hash \
                 {tenure_id_consensus_hash} (iter {i})"
            )
        })?;

        let burn_dbconn = self.sortdb.index_handle(&baseline_sn.sortition_id);

        let mut builder = NakamotoBlockBuilder::new(
            &self.cur_parent_info,
            &tenure_id_consensus_hash,
            0,
            None,
            None,
            0,
            None,
            None,
            None,
            DEFAULT_MAX_TENURE_BYTES,
        )?;

        let mut miner_tenure_info = builder.load_tenure_info(
            self.chainstate,
            &burn_dbconn,
            MinerTenureInfoCause::NoTenureChange,
        )?;
        let burn_chain_height = miner_tenure_info.burn_tip_height;

        let mut clarity_tx = builder.tenure_begin(&burn_dbconn, &mut miner_tenure_info)?;
        let starting_cost = clarity_tx.cost_so_far();

        let setup = setup_start.elapsed();

        let finalize_start = Instant::now();
        let mined_block = builder.mine_nakamoto_block(&mut clarity_tx, burn_chain_height);
        let finalize = finalize_start.elapsed();

        let total_tenure_cost = clarity_tx.cost_so_far();
        let mut block_execution_cost = clarity_tx.cost_so_far();
        block_execution_cost.sub(&starting_cost)?;

        let clarity_commit_start = Instant::now();
        clarity_tx.commit_to_block(
            &mined_block.header.consensus_hash,
            &mined_block.header.block_hash(),
        );
        let clarity_commit = clarity_commit_start.elapsed();

        let advance_tip_start = Instant::now();

        let burn_view = NakamotoChainState::get_block_burn_view(
            &self.sortdb,
            &mined_block,
            &self.cur_parent_info,
        )?;

        let sn = SortitionDB::get_block_snapshot_consensus(
            self.sortdb.conn(),
            &mined_block.header.consensus_hash,
        )?
        .ok_or_else(|| {
            anyhow!(
                "baseline: snapshot not found for mined header CH {} (iter {i})",
                mined_block.header.consensus_hash
            )
        })?;

        let new_tip_info = NakamotoChainState::advance_tip(
            &mut miner_tenure_info.chainstate_tx.tx,
            &self.cur_parent_info.anchored_header,
            &self.cur_parent_info.consensus_hash,
            &mined_block,
            None,
            &sn.burn_header_hash,
            sn.block_height as u32,
            sn.burn_header_timestamp,
            None,
            None,
            &block_execution_cost,
            &total_tenure_cost,
            builder.get_bytes_so_far(),
            false,
            vec![],
            vec![],
            vec![],
            vec![],
            false,
            0,
            0,
            &burn_view,
        )?;

        let advance_tip = advance_tip_start.elapsed();

        let index_commit_start = Instant::now();
        miner_tenure_info.chainstate_tx.commit()?;
        let index_commit = index_commit_start.elapsed();

        self.cur_parent_info = new_tip_info;
        self.iter += 1;

        Ok(EmptyBaselineSample {
            setup,
            finalize,
            clarity_commit,
            advance_tip,
            index_commit,
        })
    }
}

/// Outcome of [`run_convergent_baseline`] — used both for emitting the
/// `BaselineComplete` event and for persisting the run-level baseline row.
pub struct BaselineOutcome {
    pub baseline: BlockProcessingBaseline,
    pub converged: bool,
    pub segments_used: u32,
    pub total_blocks: u32,
    pub duration: Duration,
    /// Size of the rolling window averaged into the final [`Self::baseline`],
    /// in segments. Equal to `BASELINE_CONVERGENCE_WINDOW.min(segments_used)`.
    pub measurement_window: u32,
}

impl BaselineOutcome {
    /// Blocks not in the final measurement window — i.e. segments that ran
    /// before convergence but were discarded from the final mean. Stored in
    /// the DB's `warmup_blocks` column so reviewers can see how long the
    /// machine took to settle.
    pub fn discarded_blocks(&self) -> u32 {
        self.total_blocks.saturating_sub(self.measured_blocks())
    }

    pub fn measured_blocks(&self) -> u32 {
        self.measurement_window * BASELINE_SEGMENT_SIZE
    }
}

/// Running sum of per-block samples; finalizes to a per-phase mean.
#[derive(Default)]
struct SegmentAccumulator {
    setup: Duration,
    finalize: Duration,
    clarity_commit: Duration,
    advance_tip: Duration,
    index_commit: Duration,
    count: u32,
}

impl SegmentAccumulator {
    fn add(&mut self, s: &EmptyBaselineSample) {
        self.setup += s.setup;
        self.finalize += s.finalize;
        self.clarity_commit += s.clarity_commit;
        self.advance_tip += s.advance_tip;
        self.index_commit += s.index_commit;
        self.count += 1;
    }

    fn finalize(self) -> BlockProcessingBaseline {
        debug_assert!(self.count > 0);
        let n = self.count;
        BlockProcessingBaseline {
            avg_setup_duration: self.setup / n,
            avg_finalize_duration: self.finalize / n,
            avg_clarity_state_commit_duration: self.clarity_commit / n,
            avg_advance_tip_duration: self.advance_tip / n,
            avg_index_commit_duration: self.index_commit / n,
        }
    }
}

/// Mean of the most-recent `window` per-segment averages.
fn rolling_window_average(
    segments: &[BlockProcessingBaseline],
    window: usize,
) -> BlockProcessingBaseline {
    debug_assert!(window > 0 && segments.len() >= window);
    let slice = &segments[segments.len() - window..];
    let n = slice.len() as u32;
    let sum = slice
        .iter()
        .fold(BlockProcessingBaseline::default(), |acc, s| {
            BlockProcessingBaseline {
                avg_setup_duration: acc.avg_setup_duration + s.avg_setup_duration,
                avg_finalize_duration: acc.avg_finalize_duration + s.avg_finalize_duration,
                avg_clarity_state_commit_duration: acc.avg_clarity_state_commit_duration
                    + s.avg_clarity_state_commit_duration,
                avg_advance_tip_duration: acc.avg_advance_tip_duration + s.avg_advance_tip_duration,
                avg_index_commit_duration: acc.avg_index_commit_duration
                    + s.avg_index_commit_duration,
            }
        });
    BlockProcessingBaseline {
        avg_setup_duration: sum.avg_setup_duration / n,
        avg_finalize_duration: sum.avg_finalize_duration / n,
        avg_clarity_state_commit_duration: sum.avg_clarity_state_commit_duration / n,
        avg_advance_tip_duration: sum.avg_advance_tip_duration / n,
        avg_index_commit_duration: sum.avg_index_commit_duration / n,
    }
}

fn baseline_total(b: &BlockProcessingBaseline) -> Duration {
    b.avg_setup_duration
        + b.avg_finalize_duration
        + b.avg_clarity_state_commit_duration
        + b.avg_advance_tip_duration
        + b.avg_index_commit_duration
}

/// Run empty-block samples in fixed-size segments and stop once the rolling
/// mean over the last [`BASELINE_CONVERGENCE_WINDOW`] segments matches the
/// prior window within [`BASELINE_CONVERGENCE_THRESHOLD`].
///
/// Convergence is checked on the sum of all five phase means — individual
/// phases have very different variance scales (e.g. `advance_tip` is noisier
/// than `index_commit`), so per-phase tolerances would either be too tight
/// for the noisy phases or too loose for the quiet ones.
pub fn run_convergent_baseline(
    chainstate: &mut StacksChainState,
    burnchain: &Burnchain,
    start_parent: &StacksBlockId,
    interrupted: &Arc<AtomicBool>,
    ev: &BenchEventSender,
) -> Result<BaselineOutcome> {
    let is_interrupted = || interrupted.load(Ordering::Relaxed);

    bench_events::emit(
        ev,
        BenchEvent::BaselineStarted {
            segment_size: BASELINE_SEGMENT_SIZE,
            min_segments: BASELINE_MIN_SEGMENTS,
            max_segments: BASELINE_MAX_SEGMENTS,
            convergence_window: BASELINE_CONVERGENCE_WINDOW,
            convergence_threshold: BASELINE_CONVERGENCE_THRESHOLD,
        },
    );

    let max_blocks = BASELINE_MAX_SEGMENTS * BASELINE_SEGMENT_SIZE;
    let window = BASELINE_CONVERGENCE_WINDOW as usize;

    let mut runner = EmptyBaselineRunner::new(chainstate, burnchain, start_parent)?;

    let segment_capacity = BASELINE_MAX_SEGMENTS as usize;
    let mut segments: Vec<BlockProcessingBaseline> = Vec::with_capacity(segment_capacity);
    let mut current = SegmentAccumulator::default();
    let mut blocks_completed: u32 = 0;
    let mut converged = false;

    let start = Instant::now();

    'outer: while blocks_completed < max_blocks {
        if is_interrupted() {
            break;
        }

        let sample = runner.step()?;
        current.add(&sample);
        blocks_completed += 1;

        bench_events::emit(
            ev,
            BenchEvent::BaselineProgress {
                blocks_completed,
                max_blocks,
            },
        );

        if current.count == BASELINE_SEGMENT_SIZE {
            let segment_average = std::mem::take(&mut current).finalize();
            segments.push(segment_average.clone());
            let segment_index = segments.len() as u32;

            // Once we have two full windows we can compare them. Until then
            // emit the segment-level numbers only, with no convergence pct.
            let (rolling_avg, convergence_pct) = if segments.len() >= 2 * window {
                let recent = rolling_window_average(&segments, window);
                let prior_slice_end = segments.len() - window;
                let prior = rolling_window_average(&segments[..prior_slice_end], window);
                let recent_total = baseline_total(&recent).as_secs_f64();
                let prior_total = baseline_total(&prior).as_secs_f64();
                let pct = if prior_total > 0.0 {
                    (recent_total - prior_total).abs() / prior_total
                } else {
                    0.0
                };
                (Some(recent), Some(pct))
            } else if segments.len() >= window {
                (Some(rolling_window_average(&segments, window)), None)
            } else {
                (None, None)
            };

            bench_events::emit(
                ev,
                BenchEvent::BaselineSegmentComplete {
                    segment_index,
                    segment_average,
                    rolling_window_average: rolling_avg,
                    convergence_pct,
                },
            );

            if segment_index >= BASELINE_MIN_SEGMENTS
                && let Some(pct) = convergence_pct
                && pct < BASELINE_CONVERGENCE_THRESHOLD
            {
                converged = true;
                break 'outer;
            }
        }
    }

    let duration = start.elapsed();

    // If interrupted (or stopped before any segment finished), return defaults
    // without checkpointing — the caller bails immediately on interrupt and
    // discards this run, so a chainstate-DB checkpoint would just delay the
    // exit on an already-doomed run.
    if is_interrupted() || segments.is_empty() {
        return Ok(BaselineOutcome {
            baseline: BlockProcessingBaseline::default(),
            converged: false,
            segments_used: segments.len() as u32,
            total_blocks: blocks_completed,
            duration,
            measurement_window: 0,
        });
    }

    let measurement_segments = window.min(segments.len());
    let baseline = rolling_window_average(&segments, measurement_segments);

    // Checkpoint the chainstate/clarity dbs.
    bench_events::emit(ev, BenchEvent::BaselineCheckpointStarted);
    let checkpoint_start = Instant::now();
    chainstate.checkpoint_sqlite_dbs()?;
    bench_events::emit(
        ev,
        BenchEvent::BaselineCheckpointComplete {
            duration: checkpoint_start.elapsed(),
        },
    );

    Ok(BaselineOutcome {
        baseline,
        converged,
        segments_used: segments.len() as u32,
        total_blocks: blocks_completed,
        duration,
        measurement_window: measurement_segments as u32,
    })
}
