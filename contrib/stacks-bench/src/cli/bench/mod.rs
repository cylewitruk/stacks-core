use crate::cli::common::{BoxedOutput, CliContext, ExecCommand, boxed};

pub mod bench_ui;
pub mod list;
pub mod remove;
pub mod rerun;
pub mod run;
pub mod show;

#[derive(clap::Subcommand, Debug)]
pub enum BenchCommand {
    Run(run::RunArgs),
    /// Re-run an existing benchmark using its original parameters.
    Rerun(rerun::RerunArgs),
    #[command(alias = "rm")]
    Remove(remove::RemoveArgs),
    #[command(alias = "ls")]
    List(list::ListArgs),
    /// Show details and profiler data for a benchmark run.
    Show(show::ShowArgs),
}

#[derive(clap::Args, Debug)]
pub struct BenchArgs {
    #[command(subcommand)]
    pub command: BenchCommand,
}

impl ExecCommand for BenchArgs {
    type Output = BoxedOutput;

    async fn exec(&self, ctx: &CliContext) -> anyhow::Result<BoxedOutput> {
        match &self.command {
            BenchCommand::Run(args) => Ok(boxed(args.exec(ctx).await?)),
            BenchCommand::Rerun(args) => Ok(boxed(args.exec(ctx).await?)),
            BenchCommand::Remove(args) => Ok(boxed(args.exec(ctx).await?)),
            BenchCommand::List(args) => Ok(boxed(args.exec(ctx).await?)),
            BenchCommand::Show(args) => Ok(boxed(args.exec(ctx).await?)),
        }
    }
}
