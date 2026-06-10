//! `delete_run` tool – deletes a benchmark run and all dependent data.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use stacks_bench::db::app::CheckpointMode;

use crate::mcp::server::StacksBenchServer;

/// Parameters for the `delete_run` tool.
#[derive(Deserialize, JsonSchema)]
pub struct DeleteRunParams {
    /// ID of the benchmark run to delete.
    pub run_id: i32,
}

#[derive(Serialize)]
struct DeleteRunResult {
    deleted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

impl StacksBenchServer {
    pub async fn exec_delete_run(&self, params: &DeleteRunParams) -> anyhow::Result<String> {
        let mut db = self.app_db.clone();
        let result = match db.delete_benchmark_run(params.run_id).await {
            Ok(()) => {
                // Post-delete cleanup: checkpoint + vacuum to reclaim space.
                let _ = db.checkpoint(CheckpointMode::Truncate).await;
                let _ = db.vacuum().await;
                DeleteRunResult {
                    deleted: true,
                    message: None,
                }
            }
            Err(e) => {
                let msg = format!("{e:#}");
                if msg.contains("not found") {
                    DeleteRunResult {
                        deleted: false,
                        message: Some(msg),
                    }
                } else {
                    return Err(e);
                }
            }
        };
        Ok(serde_json::to_string(&result)?)
    }
}
