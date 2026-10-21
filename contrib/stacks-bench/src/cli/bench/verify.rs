use anyhow::Result;

use crate::cli::common::{CliContext, ExecCommand};
use crate::commands::bench::verify::{VerifyResult, verify_runs};

/// Compare the correctness records of two benchmark runs: the state root and
/// the receipts digest of every replayed segment (warmup and unmeasured
/// segments included). Fails when any segment differs or is missing, so the
/// runs' timings must not be compared.
#[derive(clap::Args, Debug)]
pub struct VerifyArgs {
    /// Baseline benchmark run ID.
    #[arg(long)]
    pub baseline: i32,

    /// Candidate benchmark run ID.
    #[arg(long)]
    pub candidate: i32,

    /// Maximum number of mismatching segments to list.
    #[arg(long, default_value_t = 20)]
    pub limit: usize,
}

impl ExecCommand for VerifyArgs {
    type Output = VerifyResult;

    async fn exec(&self, ctx: &CliContext) -> Result<Self::Output> {
        let result = verify_runs(&ctx.app_db(), self.baseline, self.candidate, self.limit).await?;

        if ctx.interactive() {
            cliclack::log::step(format!(
                "Runs {} (baseline) and {} (candidate): {}",
                self.baseline,
                self.candidate,
                result.describe()
            ))?;
            for m in &result.mismatches {
                cliclack::log::warning(format!(
                    "{} block {} segment {} txs {}..{} repetition {}: baseline {} candidate {}",
                    m.kind,
                    &m.origin_index_hash[..16],
                    m.segment_index,
                    m.tx_start,
                    m.tx_end,
                    m.repetition,
                    m.baseline.as_deref().unwrap_or("-"),
                    m.candidate.as_deref().unwrap_or("-"),
                ))?;
            }
        }

        if !result.ok {
            anyhow::bail!(
                "runs {} and {} are not comparable: {}",
                self.baseline,
                self.candidate,
                result.describe()
            );
        }
        Ok(result)
    }
}
