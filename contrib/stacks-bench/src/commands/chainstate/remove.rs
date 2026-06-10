use anyhow::Result;
use serde::Serialize;
use stacks_bench::db::app::{AppDb, CheckpointMode};

#[derive(Serialize)]
pub struct RemoveResult {
    pub deleted_chainstate_ids: Vec<i32>,
    pub message: String,
}

/// Delete the given chainstates and optionally run checkpoint + vacuum.
///
/// All chainstate IDs must correspond to existing chainstates — callers are
/// responsible for validation beforehand (or catching the DB error).
pub async fn delete_chainstates(
    app_db: &mut AppDb,
    chainstate_ids: &[i32],
    cleanup: bool,
) -> Result<RemoveResult> {
    for &id in chainstate_ids {
        app_db.delete_chainstate(id).await?;
    }

    if cleanup {
        let _ = app_db.checkpoint(CheckpointMode::Truncate).await;
        let _ = app_db.vacuum().await;
    }

    Ok(RemoveResult {
        deleted_chainstate_ids: chainstate_ids.to_vec(),
        message: format!("{} chainstate(s) deleted", chainstate_ids.len()),
    })
}
