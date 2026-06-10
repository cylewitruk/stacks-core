//! `list_runs` tool – lists benchmark runs with optional filters.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::mcp::server::StacksBenchServer;

/// Parameters for the `list_runs` tool.
#[derive(Deserialize, JsonSchema)]
pub struct ListRunsParams {
    /// Maximum number of runs to return (default: 50).
    #[serde(default)]
    limit: Option<usize>,
    /// Filter by run name substring (case-insensitive).
    #[serde(default)]
    name: Option<String>,
    /// If true, show only incomplete (in-progress/failed) runs instead of
    /// completed runs.
    #[serde(default)]
    incomplete: Option<bool>,
}

/// JSON shape for a benchmark run returned by `list_runs`.
/// Mirrors the CLI's `bench list --json` output.
#[derive(Serialize)]
struct RunJson {
    id: i32,
    name: Option<String>,
    start_time: String,
    end_time: Option<String>,
    duration_secs: Option<f64>,
    git_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<RunSummaryJson>,
}

/// Inline summary metrics for a benchmark run.
#[derive(Serialize)]
struct RunSummaryJson {
    blocks: u64,
    total_duration_us: u64,
    avg_block_duration_us: u64,
}

impl StacksBenchServer {
    pub async fn query_runs(&self, params: &ListRunsParams) -> anyhow::Result<String> {
        let limit = params.limit.unwrap_or(50);
        let show_incomplete = params.incomplete.unwrap_or(false);

        let mut runs = self.app_db.list_benchmark_runs().await?;

        // Completion status filter (matches CLI default: completed only)
        if show_incomplete {
            runs.retain(|r| r.end_time.is_none());
        } else {
            runs.retain(|r| r.end_time.is_some());
        }

        // Name filter
        if let Some(ref pattern) = params.name {
            let pat = pattern.to_lowercase();
            runs.retain(|r| {
                r.run_name
                    .as_deref()
                    .is_some_and(|n| n.to_lowercase().contains(&pat))
            });
        }

        runs.truncate(limit);

        // Build JSON output with inline summaries
        let mut results = Vec::with_capacity(runs.len());
        for run in &runs {
            let summary = self.app_db.get_run_summary(run.id).await?.and_then(|s| {
                if s.block_count == 0 {
                    None
                } else {
                    Some(RunSummaryJson {
                        blocks: s.block_count,
                        total_duration_us: s.total_duration_us,
                        avg_block_duration_us: s.total_duration_us / s.block_count,
                    })
                }
            });

            let duration_secs = run
                .end_time
                .map(|end| (end - run.start_time).num_milliseconds() as f64 / 1000.0);

            results.push(RunJson {
                id: run.id,
                name: run.run_name.clone(),
                start_time: run.start_time.format("%Y-%m-%dT%H:%M:%S").to_string(),
                end_time: run
                    .end_time
                    .map(|t| t.format("%Y-%m-%dT%H:%M:%S").to_string()),
                duration_secs,
                git_hash: hex::encode(&run.git_commit_hash),
                summary,
            });
        }

        serde_json::to_string_pretty(&results)
            .map_err(|e| anyhow::anyhow!("Failed to serialize runs: {e}"))
    }
}
