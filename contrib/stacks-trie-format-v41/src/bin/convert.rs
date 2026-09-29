//! Offline V4-to-V4.1 migration of one immutable MARF source.

use std::env;
use std::error::Error;
use std::path::PathBuf;

use stacks_trie_format_v41::{Config, plan, rewrite};

/// Run one or both resumable phases, never modifying the source MARF.
fn main() -> Result<(), Box<dyn Error>> {
    let arguments: Vec<_> = env::args().collect();
    if arguments.len() != 6 {
        return Err("usage: stacks-trie-format-v41 plan|rewrite|all SOURCE_SQLITE SOURCE_BLOBS SCRATCH DESTINATION".into());
    }
    let config = Config {
        source_db: PathBuf::from(&arguments[2]),
        source_blobs: PathBuf::from(&arguments[3]),
        scratch: PathBuf::from(&arguments[4]),
    };
    let destination = PathBuf::from(&arguments[5]);
    match arguments[1].as_str() {
        "plan" => plan(&config)?,
        "rewrite" => rewrite(&config, &destination)?,
        "all" => {
            plan(&config)?;
            rewrite(&config, &destination)?;
        }
        _ => return Err("phase must be plan, rewrite, or all".into()),
    }
    Ok(())
}
