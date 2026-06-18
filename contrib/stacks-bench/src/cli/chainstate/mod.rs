pub mod index;
pub mod list;
pub mod remove;

use clap::Subcommand;

use crate::cli::common::{BoxedOutput, CliContext, ExecCommand, boxed};

#[derive(Subcommand, Debug)]
pub enum ChainstateCommand {
    /// Index a range of blocks from the node database
    Index(index::IndexArgs),
    /// List indexed chainstates
    #[command(alias = "ls")]
    List(list::ListArgs),
    /// Delete one or more chainstates and all associated data
    #[command(alias = "rm")]
    Remove(remove::RemoveArgs),
}

#[derive(clap::Args, Debug)]
pub struct ChainstateArgs {
    #[command(subcommand)]
    pub command: ChainstateCommand,
}

impl ExecCommand for ChainstateArgs {
    type Output = BoxedOutput;

    async fn exec(&self, ctx: &CliContext) -> anyhow::Result<BoxedOutput> {
        match &self.command {
            ChainstateCommand::Index(args) => Ok(boxed(args.exec(ctx).await?)),
            ChainstateCommand::List(args) => Ok(boxed(args.exec(ctx).await?)),
            ChainstateCommand::Remove(args) => Ok(boxed(args.exec(ctx).await?)),
        }
    }
}
