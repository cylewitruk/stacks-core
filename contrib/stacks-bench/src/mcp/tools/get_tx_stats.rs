//! `get_tx_stats` tool – paginated per-transaction stats for a benchmark run.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::mcp::server::StacksBenchServer;

/// Parameters for the `get_tx_stats` tool.
#[derive(Deserialize, JsonSchema)]
pub struct GetTxStatsParams {
    /// Benchmark run ID.
    run_id: i32,
    /// Optional block index hash (hex) to filter transactions to a single block.
    #[serde(default)]
    block_id: Option<String>,
    /// Page offset (default: 0).
    #[serde(default)]
    offset: Option<i64>,
    /// Page size (default: 50, max: 200).
    #[serde(default)]
    limit: Option<i64>,
}

#[derive(Serialize)]
struct TxStatsJson {
    tx_hash: String,
    tx_type: String,
    block_height: i64,
    duration_us: i32,
    clarity_runtime: i32,
    clarity_read_length: i32,
    clarity_read_count: i32,
    clarity_write_length: i32,
    clarity_write_count: i32,
}

impl StacksBenchServer {
    pub async fn query_tx_stats(&self, params: &GetTxStatsParams) -> anyhow::Result<String> {
        let offset = params.offset.unwrap_or(0);
        let limit = params.limit.unwrap_or(50).min(200);

        let rows = self
            .app_db
            .get_tx_stats(params.run_id, params.block_id.as_deref(), offset, limit)
            .await?;

        let results: Vec<TxStatsJson> = rows
            .into_iter()
            .map(|r| TxStatsJson {
                tx_hash: r.tx_hash,
                tx_type: r.tx_type,
                block_height: r.block_height,
                duration_us: r.duration_us,
                clarity_runtime: r.clarity_runtime,
                clarity_read_length: r.clarity_read_length,
                clarity_read_count: r.clarity_read_count,
                clarity_write_length: r.clarity_write_length,
                clarity_write_count: r.clarity_write_count,
            })
            .collect();

        serde_json::to_string_pretty(&results)
            .map_err(|e| anyhow::anyhow!("Failed to serialize tx stats: {e}"))
    }
}
