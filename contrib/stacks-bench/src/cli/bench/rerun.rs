use anyhow::{Context, Result};
use console::style;

use super::run::{RunArgs, RunResult};
use crate::cli::common::{CliContext, ExecCommand, fmt_run_label, fmt_run_name_suffix};

#[derive(clap::Args, Debug)]
pub struct RerunArgs {
    /// The ID of the benchmark run to re-run. If omitted, an interactive
    /// selector is shown with all available runs.
    #[arg(long, alias = "id")]
    pub run_id: Option<u32>,
}

impl ExecCommand for RerunArgs {
    type Output = RunResult;

    async fn exec(&self, ctx: &CliContext) -> Result<Self::Output> {
        let app_db = ctx.app_db();

        let run_id: i32 = if let Some(id) = self.run_id {
            id as i32
        } else if !ctx.interactive() {
            anyhow::bail!("--run-id is required in non-interactive mode");
        } else {
            let runs = app_db.list_benchmark_runs().await?;
            if runs.is_empty() {
                anyhow::bail!("No benchmark runs found.");
            }

            let mut select = cliclack::select(format!(
                "Select a benchmark run to re-run ({} available)",
                runs.len()
            ));
            for run in &runs {
                select = select.item(run.id, format!("Run {}", run.id), fmt_run_label(run));
            }
            select.filter_mode().interact()?
        };

        let run = app_db
            .get_benchmark_run(run_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Benchmark run {} not found", run_id))?;

        if ctx.interactive() {
            cliclack::log::step(format!(
                "Re-running benchmark run {}{} started at {}",
                style(run.id).bold(),
                fmt_run_name_suffix(&run),
                run.start_time.format("%Y-%m-%d %H:%M:%S"),
            ))?;
        }

        let run_args: RunArgs = serde_json::from_str(&run.args_json).with_context(|| {
            format!(
                "Failed to deserialize args for run {} — stored JSON: {}",
                run.id, &run.args_json
            )
        })?;

        ExecCommand::exec(&run_args, ctx).await
    }
}
