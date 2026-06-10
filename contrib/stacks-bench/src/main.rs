use clap::Parser as _;

mod cli;
mod commands;
mod mcp;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // SAFETY: This is the first thing we do in the process, before any
    // potential threads are spawned or any FFI into C libraries that might read
    // the environment.
    unsafe {
        std::env::set_var("STACKS_LOG_CRITONLY", "1");
    }

    cli::Cli::parse().exec().await
}
