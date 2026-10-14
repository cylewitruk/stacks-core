use std::time::Instant;

use anyhow::Result;
use stacks_bench::db::app::{AppDb, CheckpointMode};

/// Run checkpoint + vacuum on the app database.
///
/// When `silent` is false, renders a cliclack multi-progress with a spinner.
/// When `silent` is true, runs the operations quietly.
///
/// Errors are non-fatal: they are rendered on the spinner (interactive) or
/// silently ignored (non-interactive).
pub async fn run_db_cleanup(mut app_db: AppDb, silent: bool) -> Result<()> {
    if silent {
        let _ = app_db.checkpoint(CheckpointMode::Truncate).await;
        let _ = app_db.vacuum().await;
        return Ok(());
    }

    let cleanup = cliclack::multi_progress("Cleaning up");

    let db_spinner = cleanup.add(cliclack::spinner());
    db_spinner.start("Checkpointing + vacuuming database...");
    let db_start = Instant::now();

    let db_handle = tokio::spawn(async move {
        app_db.checkpoint(CheckpointMode::Truncate).await?;
        app_db.vacuum().await?;
        Ok::<_, anyhow::Error>(())
    });

    match db_handle.await {
        Ok(Ok(())) => db_spinner.stop(fmt_success!(
            "Checkpoint + vacuum complete ({:.2}s)",
            db_start.elapsed().as_secs_f32()
        )),
        Ok(Err(e)) => db_spinner.stop(fmt_failure!(
            "Checkpoint/vacuum failed: {e} ({:.2}s)",
            db_start.elapsed().as_secs_f32()
        )),
        Err(e) => db_spinner.stop(fmt_failure!(
            "Checkpoint/vacuum task panicked: {e} ({:.2}s)",
            db_start.elapsed().as_secs_f32()
        )),
    }

    cleanup.stop();

    Ok(())
}
