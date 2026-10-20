//! A small write/read benchmark driven through the op DSL, so both
//! implementations run exactly the same workload.

use std::time::Instant;

use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha20Rng;

use crate::ops::{block_hex, sentinel_hex, value_hex_from_str, Op, OpenOpts};
use crate::Impl;

pub fn run(which: Impl, blocks: usize, inserts_per_block: usize, lookups: usize) {
    let mut rng = ChaCha20Rng::seed_from_u64(7);
    let opts = OpenOpts::default();
    let key_pool = inserts_per_block * 4;

    let mut write_ops = vec![Op::Open(opts)];
    let mut parent = sentinel_hex();
    let mut tips = vec![];
    for b in 0..blocks {
        let child = block_hex(b as u32 + 1);
        write_ops.push(Op::Begin {
            parent: parent.clone(),
            child: child.clone(),
        });
        let entries = (0..inserts_per_block)
            .map(|_| {
                let k = format!("key::{}", rng.gen_range(0..key_pool));
                (k, value_hex_from_str(&format!("v{}", rng.gen::<u64>())))
            })
            .collect();
        write_ops.push(Op::InsertBatch { entries });
        write_ops.push(Op::Commit);
        tips.push(child.clone());
        parent = child;
    }
    write_ops.push(Op::Close);

    let mut read_ops = vec![Op::Open(opts)];
    for _ in 0..lookups {
        let tip = tips[rng.gen_range(0..tips.len())].clone();
        read_ops.push(Op::Get {
            tip,
            key: format!("key::{}", rng.gen_range(0..key_pool)),
        });
    }
    for _ in 0..lookups / 100 {
        let tip = tips[rng.gen_range(0..tips.len())].clone();
        read_ops.push(Op::GetWithProof {
            tip,
            key: format!("key::{}", rng.gen_range(0..key_pool)),
        });
    }
    read_ops.push(Op::Close);

    let dir = tempfile::tempdir().expect("tempdir");
    let keep = std::env::var_os("KEEP").is_some();
    let mut backend = which.backend(dir.path());
    let t = Instant::now();
    backend.run(&write_ops);
    let write = t.elapsed();
    let t = Instant::now();
    backend.run(&read_ops);
    let read = t.elapsed();
    let db = std::fs::metadata(dir.path().join("marf.sqlite"))
        .map(|m| m.len())
        .unwrap_or(0);
    let blobs = std::fs::metadata(dir.path().join("marf.sqlite.blobs"))
        .map(|m| m.len())
        .unwrap_or(0);
    println!(
        "{which:?}: {blocks} blocks x {inserts_per_block} inserts: write {:.2?} ({:.0} inserts/s); {lookups} lookups + {} proofs: {:.2?} ({:.0} lookups/s); db {} KiB, blobs {} KiB",
        write,
        (blocks * inserts_per_block) as f64 / write.as_secs_f64(),
        lookups / 100,
        read,
        lookups as f64 / read.as_secs_f64(),
        db / 1024,
        blobs / 1024
    );
    if keep {
        let path = dir.into_path();
        println!("kept {}", path.display());
    }
}
