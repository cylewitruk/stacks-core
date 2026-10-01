//! Build the experimental direct-addressed hash snapshot on an offline database.
use blockstack_lib::chainstate::stacks::index::direct_hash_index;
use std::{env, path::Path, time::Instant};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = env::args()
        .nth(1)
        .ok_or("usage: build_direct_hash_index DATABASE")?;
    let start = Instant::now();
    let (max_id, bytes) = direct_hash_index::build(Path::new(&path))?;
    println!(
        "{{\"max_id\":{max_id},\"bytes\":{bytes},\"seconds\":{}}}",
        start.elapsed().as_secs_f64()
    );
    Ok(())
}
