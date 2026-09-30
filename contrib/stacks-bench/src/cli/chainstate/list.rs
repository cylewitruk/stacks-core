use anyhow::Result;

use crate::cli::common::{Align, CliContext, ExecCommand, Table};
use crate::commands::chainstate::list::{ChainstateJson, query_chainstates};

#[derive(clap::Args, Debug)]
pub struct ListArgs {
    /// Maximum number of chainstates to display.
    #[arg(long, default_value_t = 50)]
    pub limit: usize,
}

impl ListArgs {
    fn print_table(&self, rows: &[ChainstateJson]) -> Result<()> {
        if rows.is_empty() {
            cliclack::log::info("No chainstates found.")?;
            return Ok(());
        }

        let mut table = Table::new()
            .col("ID", Align::Right)
            .col("Network", Align::Left)
            .col("Chain ID", Align::Right)
            .col("Tip Height", Align::Right)
            .col_with("Tip Hash", Align::Left, 16, Some(16))
            .col("Runs", Align::Right);

        for r in rows {
            let short_hash = r.tip_hash[..r.tip_hash.len().min(16)].to_string();
            table.row(vec![
                r.id.to_string(),
                r.network.clone(),
                r.chain_id.to_string(),
                r.tip_height.to_string(),
                short_hash,
                r.runs.to_string(),
            ]);
        }

        table.print_with_footer("chainstate", self.limit)?;
        Ok(())
    }
}

impl ExecCommand for ListArgs {
    type Output = Vec<ChainstateJson>;

    async fn exec(&self, ctx: &CliContext) -> Result<Self::Output> {
        let rows = query_chainstates(&ctx.app_db(), self.limit).await?;

        if !ctx.json() {
            self.print_table(&rows)?;
        }

        Ok(rows)
    }
}
