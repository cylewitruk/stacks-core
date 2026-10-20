//! `marf-differential record-all` records the standard fixtures from the baseline.
//! `marf-differential replay <impl> <fixture.json>...` replays fixtures.
//! `marf-differential record <name> <seed> [blocks] [inserts] [pool]` records an ad-hoc fixture.

use std::path::PathBuf;

use marf_differential::gen::{generate, standard_fixtures, GenConfig};
use marf_differential::{fixtures_dir, load_fixture, record, replay, save_fixture, Impl};

#[global_allocator]
static ALLOC: marf_differential::alloc::Counting = marf_differential::alloc::Counting;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("record-all") => {
            std::fs::create_dir_all(fixtures_dir()).unwrap();
            for (name, seed, ops) in standard_fixtures() {
                let fixture = record(&name, seed, ops);
                let path = fixtures_dir().join(format!("{name}.json"));
                save_fixture(&fixture, &path);
                println!(
                    "recorded {} ({} ops) -> {}",
                    name,
                    fixture.ops.len(),
                    path.display()
                );
            }
        }
        Some("record") => {
            let name = args.get(1).expect("name");
            let seed: u64 = args.get(2).expect("seed").parse().unwrap();
            let mut cfg = GenConfig::default();
            if let Some(b) = args.get(3) {
                cfg.blocks = b.parse().unwrap();
            }
            if let Some(n) = args.get(4) {
                cfg.max_inserts_per_block = n.parse().unwrap();
            }
            if let Some(p) = args.get(5) {
                cfg.key_pool = p.parse().unwrap();
            }
            let fixture = record(name, seed, generate(seed, &cfg));
            let path = fixtures_dir().join(format!("{name}.json"));
            save_fixture(&fixture, &path);
            println!(
                "recorded {} ({} ops) -> {}",
                name,
                fixture.ops.len(),
                path.display()
            );
        }
        Some("replay") => {
            let which = Impl::parse(args.get(1).expect("impl")).expect("baseline|rewrite");
            let mut failed = 0;
            for f in &args[2..] {
                let fixture = load_fixture(&PathBuf::from(f));
                let divergences = replay(&fixture, which);
                if divergences.is_empty() {
                    println!("OK   {} ({} ops)", fixture.name, fixture.ops.len());
                } else {
                    failed += 1;
                    println!("FAIL {} ({} divergences)", fixture.name, divergences.len());
                    let mut kinds: std::collections::BTreeMap<String, usize> = Default::default();
                    for d in &divergences {
                        let name = format!("{:?}", d.op);
                        let name = name
                            .split([' ', '(', '{'])
                            .next()
                            .unwrap_or("?")
                            .to_string();
                        *kinds.entry(name).or_default() += 1;
                    }
                    println!("  by op: {kinds:?}");
                    for d in divergences.iter().take(5) {
                        println!("{d}");
                    }
                }
            }
            if failed > 0 {
                std::process::exit(1);
            }
        }
        Some("open") => {
            // open <impl> <db_dir>: open an existing database and report the result
            let which = Impl::parse(args.get(1).expect("impl")).expect("baseline|rewrite");
            let dir = PathBuf::from(args.get(2).expect("db dir"));
            let mut backend = which.backend(&dir);
            let obs = backend.run(&[marf_differential::Op::Open(marf_differential::OpenOpts {
                external_blobs: dir.join("marf.sqlite.blobs").exists(),
                compress: true,
                defer_hashing: true,
                unconfirmed: false,
            })]);
            println!("{obs:?}");
            if which == Impl::Rewrite {
                let path = dir.join("marf.sqlite");
                let opts = stacks_marf_next::MARFOpenOpts::new(
                    stacks_marf_next::marf::TrieHashCalculationMode::Deferred,
                    dir.join("marf.sqlite.blobs").exists(),
                );
                if let Err(e) = stacks_marf_next::MARF::<
                    stacks_common::types::chainstate::StacksBlockId,
                >::from_path(&path, opts)
                {
                    println!("detail: {e:?}");
                }
            }
        }
        Some("inspect") => {
            marf_differential::inspect::run(args.get(1).expect("db path"));
        }
        Some("bench") => {
            // bench <impl> <blocks> <inserts_per_block> <lookups>
            let which = Impl::parse(args.get(1).expect("impl")).expect("baseline|rewrite");
            let blocks: usize = args.get(2).map(|s| s.parse().unwrap()).unwrap_or(50);
            let inserts: usize = args.get(3).map(|s| s.parse().unwrap()).unwrap_or(2000);
            let lookups: usize = args.get(4).map(|s| s.parse().unwrap()).unwrap_or(20000);
            marf_differential::bench::run(which, blocks, inserts, lookups);
        }
        Some("alloc") => {
            // alloc [external|sqlite] [compressed|plain] [blocks] [inserts] [reads]
            let mut cfg = marf_differential::alloc::Config::default();
            if let Some(s) = args.get(1) {
                cfg.external = s == "external";
            }
            if let Some(s) = args.get(2) {
                cfg.compress = s == "compressed";
            }
            if let Some(b) = args.get(3) {
                cfg.blocks = b.parse().unwrap();
            }
            if let Some(n) = args.get(4) {
                cfg.inserts = n.parse().unwrap();
            }
            if let Some(r) = args.get(5) {
                cfg.reads = r.parse().unwrap();
            }
            marf_differential::alloc::report(&cfg);
        }
        _ => {
            eprintln!("usage: marf-differential record-all | record <name> <seed> [blocks] [inserts] [pool] | replay <baseline|rewrite> <fixture>...");
            std::process::exit(2);
        }
    }
}
