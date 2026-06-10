//! MCP (Model Context Protocol) stdio server for stacks-bench.
//!
//! Launched via `stacks-bench mcp`. Provides tool-based access to benchmark
//! data for LLM agents. The server holds an [`AppDb`] connection pool for the
//! session lifetime and exposes tools that map to the same query layer used by
//! the CLI.

mod resources;
pub mod server;
mod tools;

use rmcp::ServiceExt as _;
use server::StacksBenchServer;
use stacks_bench::db::app::AppDb;

/// Arguments for the `stacks-bench mcp` subcommand.
#[derive(clap::Args, Debug)]
pub struct McpArgs {}

/// Start the MCP stdio server. Called from [`Cli::exec`] as an early return
/// that bypasses all interactive chrome and JSON envelope logic.
pub async fn run_mcp_server(app_db: AppDb) -> anyhow::Result<()> {
    let server = StacksBenchServer::new(app_db);
    let transport = rmcp::transport::stdio();
    let service = server.serve(transport).await?;
    service.waiting().await?;
    Ok(())
}
