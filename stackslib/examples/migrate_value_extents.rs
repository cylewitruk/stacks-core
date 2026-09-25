//! Offline direct-value migration for an immutable legacy chainstate snapshot.

use blockstack_lib::clarity_vm::database::value_extents_migration::{
    migrate_value_extents, ExtentMigrationConfig,
};
use std::env;
use std::error::Error;
use std::path::PathBuf;
use std::time::Instant;

/// Parse explicit source paths and run the resumable migration.
fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = env::args_os().skip(1).collect();
    if !(3..=4).contains(&args.len()) {
        return Err("usage: migrate_value_extents SOURCE_SQLITE SOURCE_BLOBS NEW_DIRECTORY [INDEX_CACHE_MIB]".into());
    }
    let config = ExtentMigrationConfig {
        index_cache_mib: args
            .get(3)
            .map(|value| {
                value
                    .to_str()
                    .ok_or("invalid cache size")?
                    .parse::<u32>()
                    .map_err(|_| "invalid cache size")
            })
            .transpose()?
            .unwrap_or(512),
        source_db: PathBuf::from(&args[0]),
        source_blobs: PathBuf::from(&args[1]),
        destination: PathBuf::from(&args[2]),
    };
    let mut last = Instant::now();
    let mut phase = String::new();
    migrate_value_extents(&config, &mut |event| {
        if event.phase != phase || last.elapsed().as_secs() >= 10 {
            eprintln!("{} {}", event.phase, event.completed);
            phase = event.phase.into();
            last = Instant::now();
        }
    })
}
