use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use bench::BenchArgs;
use chainstate::ChainstateArgs;
use clap::{Parser, Subcommand};
use console::style;
use explorer::ExplorerArgs;
use metabase::MetabaseArgs;
use stacks_bench::db::app::AppDb;
use stacks_bench::paths::AppDataDir;

use crate::cli::common::{CliContext, CommandResult, ExecCommand, serialize_erased};
use crate::mcp::McpArgs;

#[macro_use]
pub mod common;
pub mod bench;
pub mod chainstate;
pub mod explorer;
pub mod metabase;
mod theme;

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Run the benchmark
    Bench(BenchArgs),
    /// Manage chainstate data
    Chainstate(ChainstateArgs),
    /// Launch a pre-configured Metabase instance to analyze results
    Metabase(MetabaseArgs),
    /// Launch the profiler explorer web UI
    Explorer(ExplorerArgs),
    /// Start an MCP stdio server for agent access to benchmark data.
    Mcp(McpArgs),
}

#[derive(Parser, Debug)]
#[command(name = "stacks-bench", about)]
pub struct Cli {
    /// The path to the application database (SQLite). If not specified, the database
    /// will be created in the same directory as the `stacks-bench` binary.
    #[arg(long = "db", value_name = "APP_DATA_DIR")]
    pub app_data_dir: Option<PathBuf>,

    /// Emit structured JSON to stdout and suppress all interactive/styled output.
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Commands,
}

/// Execute a command and serialize its output to `serde_json::Value`.
/// This is the single point where typed command outputs are converted to JSON.
async fn run_command(
    cmd: &(impl ExecCommand + ?Sized),
    ctx: &CliContext,
) -> Result<serde_json::Value> {
    let output = cmd.exec(ctx).await?;
    serialize_erased(&output)
}

impl Cli {
    pub async fn exec(&self) -> Result<()> {
        // MCP server bypasses all CLI chrome, CliContext, and JSON envelope.
        if let Commands::Mcp(_) = &self.command {
            let app_data = AppDataDir::resolve_from_opt(self.app_data_dir.as_ref())?;
            let app_db = AppDb::open(&app_data.app_db_path()).await?;
            return crate::mcp::run_mcp_server(app_db).await;
        }

        let started_at = Instant::now();
        // Must match CliContext::interactive().
        let interactive = !self.json
            && std::io::stdout().is_terminal()
            && std::io::stdin().is_terminal()
            && std::env::var_os("CI").is_none();

        if interactive {
            cliclack::set_theme(theme::CliTheme);
            cliclack::intro(style(" stacks-bench ").on_cyan().black())?;
        }

        let app_data = AppDataDir::resolve_from_opt(self.app_data_dir.as_ref())?;

        let app_db_path = app_data.app_db_path();
        let app_db = AppDb::open(&app_db_path).await.inspect_err(|e| {
            if interactive {
                let msg = format!(
                    "Failed to open app database at {}: {e}",
                    app_db_path.display()
                );
                cliclack::log::error(msg).ok();
            }
        })?;

        let ctx = CliContext::new(app_data, app_db, self.json);

        let result = match &self.command {
            Commands::Bench(args) => run_command(args, &ctx).await,
            Commands::Chainstate(args) => run_command(args, &ctx).await,
            Commands::Metabase(args) => run_command(args, &ctx).await,
            Commands::Explorer(args) => run_command(args, &ctx).await,
            Commands::Mcp(_) => unreachable!("handled by early return above"),
        };

        let duration_secs = started_at.elapsed().as_secs_f64();

        // JSON envelope is printed only when --json is explicitly set.
        // Non-interactive without --json (e.g. piped) still gets the
        // interactive-style outro (or nothing if chrome is suppressed).
        if self.json {
            let envelope = match result {
                Ok(ref data) => CommandResult::ok(data, duration_secs)?,
                Err(ref e) => CommandResult::err(e, duration_secs),
            };
            envelope.print()?;
            return result.map(|_| ());
        }

        if !interactive {
            // Non-TTY / CI without --json: no chrome, just propagate result
            return result.map(|_| ());
        }

        let secs = duration_secs as u64;
        let hh = secs / 3600;
        let mm = (secs % 3600) / 60;
        let ss = secs % 60;
        let exec_duration_str = format!("{:02}:{:02}:{:02}", hh, mm, ss);

        match result {
            Ok(_) => {
                let finished = style("Finished").green().bold();
                let timing = style(format!("in {exec_duration_str} ({secs}s)"))
                    .dim()
                    .italic();
                cliclack::outro(format!("{finished} {timing}"))?;
                Ok(())
            }
            Err(e) => {
                let failed = style("Failed").red().bold();
                let timing = style(format!("after {exec_duration_str} ({secs}s)"))
                    .dim()
                    .italic();
                cliclack::outro_cancel(format!("{failed} {timing}\n  {e:?}"))?;
                Err(e)
            }
        }
    }
}
