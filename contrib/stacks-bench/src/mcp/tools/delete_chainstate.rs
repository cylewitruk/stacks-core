//! `delete_chainstate` tool – deletes a chainstate and all associated data.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use stacks_bench::db::app::CheckpointMode;

use crate::mcp::server::StacksBenchServer;

/// Parameters for the `delete_chainstate` tool.
#[derive(Deserialize, JsonSchema)]
pub struct DeleteChainstateParams {
    /// ID of the chainstate to delete.
    pub chainstate_id: i32,
}

#[derive(Serialize)]
struct DeleteChainstateResult {
    deleted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

impl StacksBenchServer {
    pub async fn exec_delete_chainstate(
        &self,
        params: &DeleteChainstateParams,
    ) -> anyhow::Result<String> {
        let mut db = self.app_db.clone();
        let result = match db.delete_chainstate(params.chainstate_id).await {
            Ok(()) => {
                // Post-delete cleanup: checkpoint + vacuum to reclaim space.
                let _ = db.checkpoint(CheckpointMode::Truncate).await;
                let _ = db.vacuum().await;
                DeleteChainstateResult {
                    deleted: true,
                    message: None,
                }
            }
            Err(e) => {
                let msg = format!("{e:#}");
                if msg.contains("not found") {
                    DeleteChainstateResult {
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
