//! Compare sampled sidecar ancestry answers with authoritative reserved trie mappings.
use std::{env, path::Path};

use blockstack_lib::chainstate::stacks::index::direct_hash_index::DirectHashIndex;
use blockstack_lib::chainstate::stacks::index::marf::{
    MARFOpenOpts, MarfConnection, BLOCK_HEIGHT_TO_HASH_MAPPING_KEY, MARF, OWN_BLOCK_HEIGHT_KEY,
};
use rusqlite::Connection;
use stacks_common::types::chainstate::StacksBlockId;

/// Audit a built, offline experiment clone without changing logical trie contents.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = env::args()
        .nth(1)
        .ok_or("usage: verify_ancestry_index DATABASE")?;
    let mut marf = MARF::<StacksBlockId>::from_path(
        &path,
        MARFOpenOpts {
            external_blobs: true,
            mmap: true,
            ..MARFOpenOpts::default()
        },
    )?;
    let db = Connection::open(&path)?;
    let mut index = DirectHashIndex::open(&db, Path::new(&path))?.ok_or("index unavailable")?;
    db.execute_batch("BEGIN")?;
    let _guard = index.begin(&db)?;
    let max: u32 = db.query_row(
        "SELECT max_id FROM marf_direct_hash_index WHERE singleton=1",
        [],
        |r| r.get(0),
    )?;
    let mut ids: Vec<u32> = (0..64u64)
        .map(|i| 1 + ((u64::from(max.saturating_sub(1)) * i) / 63) as u32)
        .collect();
    ids.sort_unstable();
    ids.dedup();
    let mut checks = 0;
    for id in &ids {
        let block: StacksBlockId = db.query_row(
            "SELECT block_hash FROM marf_data WHERE block_id=?1",
            [id],
            |r| r.get(0),
        )?;
        let height = u32::from(
            marf.get(&block, OWN_BLOCK_HEIGHT_KEY)?
                .ok_or("missing own height")?,
        );
        for target in [0, height / 2, height.saturating_sub(1), height] {
            let direct = index
                .ancestor_hash(&db, *id, &block, height, target)
                .ok_or("ancestry shortcut unavailable")?;
            let expected = if target == height {
                block.clone()
            } else {
                StacksBlockId::from(
                    marf.get(
                        &block,
                        &format!("{BLOCK_HEIGHT_TO_HASH_MAPPING_KEY}::{target}"),
                    )?
                    .ok_or("missing height mapping")?,
                )
            };
            if direct != expected {
                return Err(
                    format!("ancestry mismatch at {id}, height {height}, target {target}").into(),
                );
            }
            checks += 1;
        }
    }
    println!(
        "{{\"passed\":true,\"max_id\":{max},\"sampled_blocks\":{},\"mapping_checks\":{checks}}}",
        ids.len()
    );
    Ok(())
}
