//! `run_benchmark` tool – executes a benchmark run via the shared commands layer.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rmcp::model::{Meta, ProgressNotificationParam, ProgressToken, ServerNotification};
use rmcp::{Peer, RoleServer};
use schemars::JsonSchema;
use serde::Deserialize;
use stacks_bench::StacksBlockRef;
use stacks_bench::bench_events::BenchEvent;
use tokio::sync::mpsc;

use crate::commands::bench::run::{BenchRunParams, FilterKind, RunResult};
use crate::commands::common::{
    ContractArg, IndexerUiSpawner, TxIdArg, normalize_contract_args, silent_indexer_ui,
};
use crate::mcp::server::StacksBenchServer;

/// Parameters for the `run_benchmark` tool.
#[derive(Deserialize, JsonSchema)]
pub struct RunBenchmarkParams {
    /// Path to the Stacks node data directory (the directory containing the
    /// `chainstate` folder).
    pub source_dir: String,

    /// Stacks block (height or hex block id) to start at, inclusive.
    /// Defaults to block 1 if omitted. Not allowed when `txid` or `block` is
    /// non-empty.
    #[serde(default)]
    pub start_at: Option<String>,

    /// Stacks block (height or hex block id) to end at, inclusive. Not
    /// allowed when `txid` or `block` is non-empty.
    #[serde(default)]
    pub end_at: Option<String>,

    /// Number of blocks to process, starting from `start_at`. Not allowed
    /// when `txid` or `block` is non-empty.
    #[serde(default)]
    pub count: Option<u32>,

    /// Transaction ids (hex) to benchmark. When non-empty, `start_at`,
    /// `end_at`, `count`, and `filter` must be omitted. Each transaction is
    /// replayed `repetitions` times independently from its own parent block.
    /// Mutually exclusive with `block`.
    #[serde(default)]
    pub txid: Vec<String>,

    /// Stacks blocks (height or hex block id) to benchmark. When non-empty,
    /// `start_at`, `end_at`, `count`, `filter`, and `txid` must be omitted.
    /// Each block is replayed `repetitions` times from its own parent block.
    /// Mutually exclusive with `txid`.
    #[serde(default)]
    pub block: Vec<String>,

    /// Number of measured repetitions per target. Requires `txid` or `block`
    /// to be non-empty. Default when omitted: 10.
    #[serde(default)]
    pub repetitions: Option<u32>,

    /// Number of warmup blocks (block-range mode) or warmup repetitions per
    /// target (txid/block mode) before measurement begins. Default: 0.
    #[serde(default)]
    pub warmup: Option<u32>,

    /// Human-readable name for this benchmark run.
    #[serde(default)]
    pub name: Option<String>,

    /// Transaction filter. Currently only `"contract-call"` is supported.
    /// Not allowed when `txid`, `block`, or `contract` is non-empty.
    #[serde(default)]
    pub filter: Option<String>,

    /// Restrict benchmarking to blocks that call one or more specific
    /// contracts. Each entry has the form `ADDR.CONTRACT[.FUNCTION]`. When
    /// the function suffix is omitted, the filter matches any function call
    /// on that contract. Multiple entries OR-combine. Compatible with range
    /// flags (`start_at`/`end_at`/`count`); mutually exclusive with `txid`,
    /// `block`, and `filter`.
    #[serde(default)]
    pub contract: Vec<String>,

    /// Trust benchmark replay order for contract AST cache lineage checks.
    /// This disables cache clearing when synthetic replay block ids do not
    /// appear to descend from the previous synthetic block. Use only for
    /// canonical block/transaction replay experiments; epoch transitions and
    /// explicit invalidation still clear the cache.
    #[serde(default)]
    pub trust_contract_cache_replay_order: bool,

    /// Network name (e.g. `"mainnet"`, `"testnet"`). Inferred from the
    /// chainstate if omitted.
    #[serde(default)]
    pub network: Option<String>,

    /// Tip block (height or hex block id) to anchor canonical history
    /// resolution. Defaults to the node's current canonical tip.
    #[serde(default)]
    pub tip: Option<String>,

    /// Parent directory under which the shadow (reflink) copy of the source
    /// chainstate is created. Defaults to the source directory's parent.
    /// Override when running in a sandbox where the default parent is not
    /// writable.
    ///
    /// Constraints: must be on the same filesystem as `source_dir` (reflinks
    /// fail across filesystems), and must not resolve inside the source tree
    /// (would recurse). The shadow tempdir is still auto-named and
    /// auto-cleaned.
    #[serde(default)]
    pub shadow_dir_root: Option<String>,
}

impl RunBenchmarkParams {
    /// Convert tool parameters into the shared `BenchRunParams`.
    fn into_bench_params(self) -> Result<BenchRunParams, String> {
        let filter = match self.filter.as_deref() {
            None => None,
            Some("contract-call") => Some(FilterKind::ContractCall),
            Some(other) => {
                return Err(format!(
                    "Unknown filter '{other}'. Supported filters: contract-call"
                ));
            }
        };

        let network = match self.network.as_deref() {
            None => None,
            Some(s) => Some(
                s.parse()
                    .map_err(|_| format!("Unknown network '{s}'. Use mainnet or testnet"))?,
            ),
        };

        let txid: Vec<TxIdArg> = self
            .txid
            .iter()
            .map(|hex| {
                hex.parse::<TxIdArg>()
                    .map_err(|e| format!("Invalid txid '{hex}': {e}"))
            })
            .collect::<Result<_, _>>()?;

        let parse_block_ref = |s: &str| -> Result<StacksBlockRef, String> {
            s.parse()
                .map_err(|e| format!("Invalid block ref '{s}': {e}"))
        };

        let block: Vec<StacksBlockRef> = self
            .block
            .iter()
            .map(|s| parse_block_ref(s))
            .collect::<Result<_, _>>()?;

        let contract: Vec<ContractArg> = self
            .contract
            .iter()
            .map(|s| {
                s.parse::<ContractArg>()
                    .map_err(|e| format!("Invalid contract '{s}': {e}"))
            })
            .collect::<Result<_, _>>()?;
        let contract = normalize_contract_args(contract);

        if !txid.is_empty() && !block.is_empty() {
            return Err("txid and block are mutually exclusive".to_string());
        }
        if !contract.is_empty() {
            if !txid.is_empty() {
                return Err("contract and txid are mutually exclusive".to_string());
            }
            if !block.is_empty() {
                return Err("contract and block are mutually exclusive".to_string());
            }
            if self.filter.is_some() {
                return Err("contract and filter are mutually exclusive".to_string());
            }
        }

        // Mirror the CLI conflict matrix (which clap enforces there): targeted
        // modes reject range/filter flags rather than silently ignoring them.
        // Unlike the CLI's `RunArgs.start_at` (clap default = "1"), our
        // `start_at` here is None unless the caller explicitly passed it, so a
        // simple `is_some()` check matches user intent.
        let targeted_mode = if !txid.is_empty() {
            Some("txid")
        } else if !block.is_empty() {
            Some("block")
        } else {
            None
        };
        if let Some(mode) = targeted_mode {
            for (name, present) in [
                ("start_at", self.start_at.is_some()),
                ("end_at", self.end_at.is_some()),
                ("count", self.count.is_some()),
                ("filter", self.filter.is_some()),
            ] {
                if present {
                    return Err(format!(
                        "{name} is not allowed when {mode} is set (range/filter and targeted modes are mutually exclusive)"
                    ));
                }
            }
        } else {
            // Range mode. CLI's `block_count` arg has
            // `conflicts_with_all = ["end_at", ...]`; mirror that here.
            if self.end_at.is_some() && self.count.is_some() {
                return Err(
                    "end_at and count are mutually exclusive (specify a range either by end_at or by start_at+count)"
                        .to_string(),
                );
            }
            // CLI's `--repetitions` has `requires = "target_mode"`; an explicit
            // value is meaningless in range mode (calibration drives sample
            // count there). Caller-omitted = `None`, which we honor; explicit
            // `Some(_)` is rejected.
            if self.repetitions.is_some() {
                return Err(
                    "repetitions requires txid or block (no effect in range mode)".to_string(),
                );
            }
        }

        Ok(BenchRunParams {
            source_dir: self.source_dir.into(),
            start_at: self.start_at.as_deref().map(parse_block_ref).transpose()?,
            end_at: self.end_at.as_deref().map(parse_block_ref).transpose()?,
            tip: self.tip.as_deref().map(parse_block_ref).transpose()?,
            network,
            block_count: self.count,
            txid,
            block,
            repetitions: self.repetitions.unwrap_or(10),
            calibration: 20,
            warmup: self.warmup.unwrap_or(0) as usize,
            filter,
            contract,
            trust_contract_cache_replay_order: self.trust_contract_cache_replay_order,
            no_profiler_kv: false,
            include_pre_nakamoto_blocks: false,
            shadow_dir_root: self.shadow_dir_root.map(PathBuf::from),
            name: self.name,
        })
    }
}

impl StacksBenchServer {
    pub async fn exec_run_benchmark(
        &self,
        params: RunBenchmarkParams,
        meta: Meta,
        client: Peer<RoleServer>,
        context: rmcp::service::RequestContext<RoleServer>,
    ) -> anyhow::Result<String> {
        let bench_params = params
            .into_bench_params()
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        let (event_tx, event_rx) = mpsc::unbounded_channel();

        // Wire up cancellation: MCP cancellation token OR ctrl-c.
        let interrupted = Arc::new(AtomicBool::new(false));
        {
            let flag = interrupted.clone();
            let ct = context.ct.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = ct.cancelled() => {}
                    _ = tokio::signal::ctrl_c() => {}
                }
                flag.store(true, Ordering::Relaxed);
            });
        }

        // Spawn progress notification forwarder if the client provided a
        // progress token.
        let progress_token = meta.get_progress_token();
        if let Some(token) = progress_token {
            tokio::spawn(forward_bench_events(event_rx, client, token));
        } else {
            // Silently drain events.
            tokio::spawn(async move {
                let mut rx = event_rx;
                while rx.recv().await.is_some() {}
            });
        }

        let indexer_ui: IndexerUiSpawner = silent_indexer_ui();

        let mut app_db = self.app_db.clone();
        let result = crate::commands::bench::run::run_benchmark(
            &mut app_db,
            &bench_params,
            event_tx,
            interrupted,
            indexer_ui,
        )
        .await?;

        Ok(serde_json::to_string(&result)?)
    }
}

/// Format a `RunResult` as a concise summary string suitable for MCP tool
/// output (returned as the tool result text).
#[allow(dead_code)]
pub fn format_run_result(result: &RunResult) -> String {
    serde_json::to_string(result).unwrap_or_else(|_| "Failed to serialize result".into())
}

// ---------------------------------------------------------------------------
// Progress notification forwarder
// ---------------------------------------------------------------------------

pub(super) async fn forward_bench_events(
    mut rx: mpsc::UnboundedReceiver<BenchEvent>,
    client: Peer<RoleServer>,
    token: ProgressToken,
) {
    // Debounce: only send high-frequency progress events when the whole-
    // percent value changes (at most ~100 notifications per phase).
    let mut last_sent_pct: i32 = -1;

    while let Some(event) = rx.recv().await {
        let notification = match &event {
            BenchEvent::ShadowDirStarted => {
                last_sent_pct = -1;
                Some(progress(
                    &token,
                    0.0,
                    None,
                    Some("Creating shadow directory..."),
                ))
            }
            BenchEvent::ShadowDirComplete { duration } => Some(progress(
                &token,
                0.0,
                None,
                Some(&format!(
                    "Shadow directory ready ({:.1}s)",
                    duration.as_secs_f64()
                )),
            )),
            BenchEvent::BaselineStarted {
                warmup_blocks,
                measured_blocks,
            } => {
                last_sent_pct = -1;
                Some(progress(
                    &token,
                    0.0,
                    None,
                    Some(&format!(
                        "Measuring baseline overhead (warmup: {warmup_blocks}, measured: {measured_blocks})"
                    )),
                ))
            }
            BenchEvent::BaselineRoundProgress {
                round,
                completed,
                total,
            } => debounced(&mut last_sent_pct, *completed as f64, *total as f64, || {
                progress(
                    &token,
                    *completed as f64,
                    Some(*total as f64),
                    Some(&format!("Baseline round {round}")),
                )
            }),
            BenchEvent::ReplayStarted {
                total_blocks,
                warmup_blocks,
                ..
            } => {
                last_sent_pct = -1;
                Some(progress(
                    &token,
                    0.0,
                    Some(*total_blocks as f64),
                    Some(&format!(
                        "Replaying blocks (warmup: {warmup_blocks}, measured: {})",
                        total_blocks - warmup_blocks
                    )),
                ))
            }
            BenchEvent::ReplayWarmupProgress { completed, total } => {
                debounced(&mut last_sent_pct, *completed as f64, *total as f64, || {
                    progress(
                        &token,
                        *completed as f64,
                        Some(*total as f64),
                        Some("Warmup"),
                    )
                })
            }
            BenchEvent::ReplayProgress { completed, total } => {
                debounced(&mut last_sent_pct, *completed as f64, *total as f64, || {
                    progress(&token, *completed as f64, Some(*total as f64), None)
                })
            }
            BenchEvent::ReplayComplete {
                measured_blocks,
                duration,
            } => {
                last_sent_pct = -1;
                Some(progress(
                    &token,
                    *measured_blocks as f64,
                    Some(*measured_blocks as f64),
                    Some(&format!("Replay complete ({:.1}s)", duration.as_secs_f64())),
                ))
            }
            BenchEvent::CleanupStarted => Some(progress(&token, 0.0, None, Some("Cleaning up..."))),
            BenchEvent::CleanupComplete => {
                Some(progress(&token, 0.0, None, Some("Cleanup complete")))
            }
            // Other events are not mapped to progress notifications.
            _ => None,
        };

        if let Some(params) = notification {
            let _ = client
                .send_notification(ServerNotification::ProgressNotification(
                    rmcp::model::ProgressNotification::new(params),
                ))
                .await;
        }
    }
}

/// Only produce a notification when the whole-percent value changes (or on
/// the very first and very last tick). This caps high-frequency per-block
/// events to ~100 notifications per phase.
fn debounced(
    last_pct: &mut i32,
    completed: f64,
    total: f64,
    make: impl FnOnce() -> ProgressNotificationParam,
) -> Option<ProgressNotificationParam> {
    if total <= 0.0 {
        return Some(make());
    }
    let pct = ((completed / total) * 100.0) as i32;
    if pct != *last_pct {
        *last_pct = pct;
        Some(make())
    } else {
        None
    }
}

fn progress(
    token: &ProgressToken,
    progress: f64,
    total: Option<f64>,
    message: Option<&str>,
) -> ProgressNotificationParam {
    let mut p = ProgressNotificationParam::new(token.clone(), progress);
    if let Some(t) = total {
        p = p.with_total(t);
    }
    if let Some(m) = message {
        p = p.with_message(m.to_owned());
    }
    p
}
