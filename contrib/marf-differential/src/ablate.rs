//! Committed-read patch-cache ablation (fourth review, part 3).
//!
//! One implementation per process (the run script runs each binary in
//! turn), one workload, a list of patch-cache capacities. The database is
//! built once; key sets are chosen by walk shape with the rewrite's
//! profiler on the shared files (the stored trie is identical for every
//! implementation), including the identity of the patched records each
//! walk meets, so a set's number of distinct patched nodes is known. For
//! every capacity the database is reopened (process caches empty: "cold" =
//! first pass after a fresh open, with the OS page cache and the blobs
//! mapping warm from building the database), the set is read once cold,
//! then `PASSES` more times warm (`DROP_CACHES=1` also drops the OS page
//! cache before every fresh open, root only, making "cold" process-cold and
//! page-cold; the blobs mapping is created by the open either way); values
//! are collected into a preallocated
//! buffer inside the timed region and verified against the model after it.
//! With the `counters` feature the implementation's read counters are
//! reported per set and capacity.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::time::Instant;

use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha20Rng;
use stacks_common::types::chainstate::StacksBlockId;
use stacks_marf_next::ClarityMarfTrieId as _;

use crate::alloc::{environment, measure, repeat, Counters, Direct, Opts, Samples};
use crate::Impl;

/// Workload shapes. `std` is the `alloc` workload (patched nodes: the root
/// and second-level nodes, a few hundred distinct); `big` has enough keys
/// for third-level branch nodes, so its final tip has thousands of distinct
/// patched nodes.
#[derive(Clone, Copy, Debug)]
pub struct Workload {
    pub name: &'static str,
    pub blocks: usize,
    pub inserts: usize,
    pub patch_blocks: usize,
    pub patch_inserts: usize,
}

impl Workload {
    pub fn parse(s: &str) -> Option<Workload> {
        match s {
            "std" => Some(Workload {
                name: "std",
                blocks: 20,
                inserts: 2000,
                patch_blocks: 8,
                patch_inserts: 50,
            }),
            "big" => Some(Workload {
                name: "big",
                blocks: 250,
                inserts: 4000,
                patch_blocks: 40,
                patch_inserts: 500,
            }),
            _ => None,
        }
    }
}

fn bid(n: u32) -> StacksBlockId {
    let mut b = [0u8; 32];
    b[..4].copy_from_slice(&n.to_be_bytes());
    b[31] = 1;
    StacksBlockId(b)
}

/// What building the database leaves behind: the model of every key's
/// value at the final tip, the first block's keys and values (patch-free
/// control at the first tip), and the tips.
struct Built {
    tip1: StacksBlockId,
    bulk_tip: StacksBlockId,
    tip: StacksBlockId,
    /// key -> value at `tip`
    model: HashMap<String, [u8; 40]>,
    /// key -> value at `tip1` (the first block's keys)
    model1: HashMap<String, [u8; 40]>,
    /// key -> value at `bulk_tip`
    model_bulk: HashMap<String, [u8; 40]>,
    all_keys: Vec<String>,
}

fn build<M: Direct>(dir: &Path, w: &Workload, opts: &Opts) -> Built {
    let mut rng = ChaCha20Rng::seed_from_u64(11);
    let mut marf = M::open(&dir.join("marf.sqlite"), opts);
    let mut model = HashMap::new();
    let mut model1 = HashMap::new();
    let mut all_keys = vec![];
    let mut parent = StacksBlockId::sentinel();
    let mut tip = parent.clone();
    let mut tip1 = parent.clone();
    for b in 0..w.blocks {
        let child = bid(b as u32 + 1);
        marf.begin(&parent, &child);
        let keys: Vec<String> = (0..w.inserts)
            .map(|i| format!("key::{b}::{i}::{}", rng.gen::<u32>()))
            .collect();
        let values: Vec<[u8; 40]> = (0..w.inserts)
            .map(|_| {
                let mut v = [0u8; 40];
                rng.fill(&mut v[..]);
                v
            })
            .collect();
        marf.insert_batch(&keys, &values);
        marf.seal();
        marf.commit();
        for (k, v) in keys.iter().zip(&values) {
            model.insert(k.clone(), *v);
            if b == 0 {
                model1.insert(k.clone(), *v);
            }
        }
        all_keys.extend(keys);
        if b == 0 {
            tip1 = child.clone();
        }
        tip = child.clone();
        parent = child;
    }
    let bulk_tip = tip.clone();
    let model_bulk = model.clone();
    for p in 0..w.patch_blocks {
        let child = bid(1000 + p as u32);
        marf.begin(&parent, &child);
        let keys: Vec<String> = (0..w.patch_inserts)
            .map(|_| all_keys[rng.gen_range(0..all_keys.len())].clone())
            .collect();
        let values: Vec<[u8; 40]> = (0..w.patch_inserts)
            .map(|_| {
                let mut v = [0u8; 40];
                rng.fill(&mut v[..]);
                v
            })
            .collect();
        marf.insert_batch(&keys, &values);
        marf.seal();
        marf.commit();
        // later entries of a batch win, as in the MARF
        for (k, v) in keys.iter().zip(&values) {
            model.insert(k.clone(), *v);
        }
        tip = child.clone();
        parent = child;
    }
    Built {
        tip1,
        bulk_tip,
        tip,
        model,
        model1,
        model_bulk,
        all_keys,
    }
}

/// Drop the OS page cache if `DROP_CACHES=1` (needs root); says whether it did.
fn maybe_drop_caches() -> bool {
    if std::env::var("DROP_CACHES").is_ok_and(|v| v == "1") {
        std::fs::write("/proc/sys/vm/drop_caches", b"3").is_ok()
    } else {
        false
    }
}

/// A key set to time: keys, the tip they are read at, and its walk shape.
struct KeySet {
    name: String,
    tip: StacksBlockId,
    keys: Vec<String>,
    distinct_patched_nodes: usize,
    avg_hops: f64,
    avg_patched: f64,
    avg_depth: f64,
}

/// Profile every key at `tip` with the rewrite's profiler (read-only open
/// on the shared files).
fn profiles(
    dir: &Path,
    opts: &Opts,
    tip: &StacksBlockId,
    keys: &[String],
) -> Vec<stacks_marf_next::WalkProfile> {
    use stacks_marf_next::marf::TrieHashCalculationMode as Mode;
    let o = stacks_marf_next::MARFOpenOpts::new(Mode::Deferred, opts.external)
        .with_compression(opts.compress);
    let storage = stacks_marf_next::marf::TrieFileStorage::<StacksBlockId>::open_readonly(
        dir.join("marf.sqlite"),
        o,
    )
    .expect("open profiler");
    let mut m = stacks_marf_next::MARF::from_storage(storage);
    keys.iter()
        .map(|k| {
            let p = m.lookup_profile(tip, k).expect("profile");
            assert!(p.found, "{k} not found at profiling");
            p
        })
        .collect()
}

fn make_set(
    name: String,
    tip: &StacksBlockId,
    keys: Vec<String>,
    profs: &[&stacks_marf_next::WalkProfile],
) -> KeySet {
    let n = keys.len().max(1) as f64;
    let mut sites = HashSet::new();
    for p in profs {
        for s in &p.patched_sites[..(p.patched_nodes as usize).min(8)] {
            sites.insert(*s);
        }
    }
    KeySet {
        name,
        tip: tip.clone(),
        distinct_patched_nodes: sites.len(),
        avg_hops: profs.iter().map(|p| p.backptr_hops as f64).sum::<f64>() / n,
        avg_patched: profs.iter().map(|p| p.patched_nodes as f64).sum::<f64>() / n,
        avg_depth: profs.iter().map(|p| p.max_patch_depth as f64).sum::<f64>() / n,
        keys,
    }
}

/// Greedily take keys whose patched records stay within `limit` distinct
/// nodes (first come, first served, in `order`).
fn bounded_set(
    name: &str,
    tip: &StacksBlockId,
    keys: &[String],
    profs: &[stacks_marf_next::WalkProfile],
    order: &[usize],
    limit: usize,
    max_keys: usize,
) -> Option<KeySet> {
    let mut sites: HashSet<(u32, u64)> = HashSet::new();
    let mut chosen: Vec<usize> = vec![];
    for &i in order {
        let p = &profs[i];
        let mine: Vec<(u32, u64)> = p.patched_sites[..(p.patched_nodes as usize).min(8)].to_vec();
        let new: usize = mine.iter().filter(|s| !sites.contains(s)).count();
        if sites.len() + new <= limit {
            sites.extend(mine);
            chosen.push(i);
            if chosen.len() >= max_keys {
                break;
            }
        }
    }
    if chosen.len() < 64 {
        return None;
    }
    let ks: Vec<String> = chosen.iter().map(|&i| keys[i].clone()).collect();
    let ps: Vec<&stacks_marf_next::WalkProfile> = chosen.iter().map(|&i| &profs[i]).collect();
    let label = if limit == usize::MAX {
        name.to_string()
    } else {
        format!("{name}(nodes<={limit})")
    };
    Some(make_set(label, tip, ks, &ps))
}

/// Choose the key sets for one built database.
fn key_sets(dir: &Path, opts: &Opts, built: &Built, reads: usize) -> Vec<KeySet> {
    let mut rng = ChaCha20Rng::seed_from_u64(23);
    let mut sets = vec![];
    // patch-free control: the first block's keys at the first tip
    {
        let mut keys: Vec<String> = built.model1.keys().cloned().collect();
        keys.sort();
        keys.truncate(reads);
        let ps = profiles(dir, opts, &built.tip1, &keys);
        assert!(ps.iter().all(|p| p.patched_nodes == 0 && p.backptr_hops == 0));
        let refs: Vec<&_> = ps.iter().collect();
        sets.push(make_set("patchfree@tip1".into(), &built.tip1, keys, &refs));
    }
    // the bulk tip: a random sample of every key, read at the tip of the
    // bulk blocks (full root, second-level nodes as patch chains)
    {
        let keys: Vec<String> = (0..reads)
            .map(|_| built.all_keys[rng.gen_range(0..built.all_keys.len())].clone())
            .collect();
        let ps = profiles(dir, opts, &built.bulk_tip, &keys);
        let refs: Vec<&_> = ps.iter().collect();
        sets.push(make_set("bulk tip (random keys)".into(), &built.bulk_tip, keys, &refs));
    }
    // the final tip: bucket by (hops, depth) as `alloc` does
    let keys = &built.all_keys;
    let ps = profiles(dir, opts, &built.tip, keys);
    let mut buckets: BTreeMap<(u32, u32), Vec<usize>> = BTreeMap::new();
    for (i, p) in ps.iter().enumerate() {
        let hops = p.backptr_hops.min(3);
        let depth = match p.max_patch_depth {
            0 => 0,
            1 | 2 => 1,
            _ => 2,
        };
        buckets.entry((hops, depth)).or_default().push(i);
    }
    // the most populated buckets with patches, and the patch-free one if any
    let mut ranked: Vec<(&(u32, u32), &Vec<usize>)> = buckets.iter().collect();
    ranked.sort_by_key(|(_, v)| std::cmp::Reverse(v.len()));
    let mut taken = 0;
    for (&(hops, depth), idx) in ranked {
        if idx.len() < 64 {
            continue;
        }
        let label = format!(
            "tip hops={} depth={}",
            if hops == 3 { "3+".to_string() } else { hops.to_string() },
            ["none", "1-2", "3-4"][depth as usize]
        );
        let mut idx = idx.clone();
        // random subset of the bucket, up to `reads` keys
        for i in (1..idx.len()).rev() {
            let j = rng.gen_range(0..=i);
            idx.swap(i, j);
        }
        idx.truncate(reads);
        let ks: Vec<String> = idx.iter().map(|&i| keys[i].clone()).collect();
        let refs: Vec<&_> = idx.iter().map(|&i| &ps[i]).collect();
        sets.push(make_set(label, &built.tip, ks, &refs));
        taken += 1;
        if taken >= 3 && depth != 0 {
            break;
        }
    }
    // working sets bounded by distinct patched nodes, from every patched key
    let patched: Vec<usize> = (0..keys.len()).filter(|&i| ps[i].patched_nodes > 0).collect();
    let mut order = patched.clone();
    for i in (1..order.len()).rev() {
        let j = rng.gen_range(0..=i);
        order.swap(i, j);
    }
    for limit in [32usize, 64, 1024, 4096, usize::MAX] {
        let name = if limit == usize::MAX { "patched all" } else { "patched" };
        if let Some(set) = bounded_set(name, &built.tip, keys, &ps, &order, limit, reads.max(8192)) {
            // skip a bound that did not bind (same node count as a smaller one)
            if sets
                .iter()
                .any(|s| s.name.starts_with("patched") && s.distinct_patched_nodes == set.distinct_patched_nodes)
            {
                continue;
            }
            sets.push(set);
        }
    }
    sets
}

/// Capacities to test: `CAPS` env, comma-separated, `prod` = the
/// implementation's production default. Default `0,64,4096,prod`.
pub fn capacities() -> Vec<Option<usize>> {
    std::env::var("CAPS")
        .unwrap_or_else(|_| "0,64,4096,prod".into())
        .split(',')
        .map(|s| if s == "prod" { None } else { Some(s.parse().expect("capacity")) })
        .collect()
}

struct Cell {
    cold: Samples,
    warm: Samples,
    counters_cold: Counters,
    counters_warm: Counters,
    peak_kb: u64,
}

fn cap_label(c: Option<usize>) -> String {
    c.map(|n| n.to_string()).unwrap_or_else(|| "prod".into())
}

pub fn run(which: Impl, w: &Workload, reads: usize, plain: bool) {
    match which {
        Impl::Baseline => run_direct::<crate::alloc::Baseline>(w, reads, plain),
        Impl::Rewrite => run_direct::<crate::alloc::Rewrite>(w, reads, plain),
    }
}

fn run_direct<M: Direct>(w: &Workload, reads: usize, plain: bool) {
    println!("{}", environment());
    let dir = tempfile::tempdir().expect("tempdir");
    let opts = Opts {
        external: true,
        compress: !plain,
        patch_cache_capacity: None,
        result_cache_capacity: None,
    };
    let t = Instant::now();
    let built = build::<M>(dir.path(), w, &opts);
    println!(
        "workload {} ({} blocks x {} inserts, {} patch blocks x {} rewrites, {}): built in {:.1}s, {} keys",
        w.name,
        w.blocks,
        w.inserts,
        w.patch_blocks,
        w.patch_inserts,
        if plain { "plain" } else { "compressed" },
        t.elapsed().as_secs_f64(),
        built.all_keys.len()
    );
    let sets = key_sets(dir.path(), &opts, &built, reads);
    println!("impl {}: {}", M::NAME, M::open(&dir.path().join("marf.sqlite"), &opts).describe(&opts));
    println!("key sets (walk shape from the rewrite's profiler; identical trie for every implementation):");
    for s in &sets {
        println!(
            "  {:<28} n={:<6} distinct patched nodes={:<6} avg hops={:.2} avg patched/walk={:.2} avg depth={:.2}",
            s.name,
            s.keys.len(),
            s.distinct_patched_nodes,
            s.avg_hops,
            s.avg_patched,
            s.avg_depth
        );
    }
    let caps = capacities();
    let passes: usize = std::env::var("PASSES").ok().and_then(|s| s.parse().ok()).unwrap_or(5);
    let mut cells: BTreeMap<(usize, usize), Cell> = BTreeMap::new();
    let mut out: Vec<Option<[u8; 40]>> = vec![None; sets.iter().map(|s| s.keys.len()).max().unwrap_or(0)];
    for r in 0..repeat() {
        for ci in 0..caps.len() {
            // rotate the capacity order between repetitions
            let ci = (ci + r) % caps.len();
            let cap = caps[ci];
            let o = Opts {
                patch_cache_capacity: cap,
                ..opts
            };
            let dropped = maybe_drop_caches();
            let mut marf = M::open(&dir.path().join("marf.sqlite"), &o);
            if r == 0 && ci == 0 {
                println!(
                    "capacities: {:?} -> e.g. {}; cold = fresh open (process caches empty){}",
                    caps.iter().map(|c| cap_label(*c)).collect::<Vec<_>>(),
                    marf.describe(&o),
                    if dropped { " + OS page cache dropped" } else { ", OS page cache warm" }
                );
            }
            for (si, set) in sets.iter().enumerate() {
                let cell = cells.entry((si, ci)).or_insert_with(|| Cell {
                    cold: Samples::default(),
                    warm: Samples::default(),
                    counters_cold: Counters::default(),
                    counters_warm: Counters::default(),
                    peak_kb: 0,
                });
                let model = if set.tip == built.tip1 {
                    &built.model1
                } else if set.tip == built.bulk_tip {
                    &built.model_bulk
                } else {
                    &built.model
                };
                // cold: first pass after the fresh open (this set's nodes may
                // overlap another set's, so only the first set of a fresh
                // open is fully cold; the table says which)
                let _ = marf.counters();
                let (_, m) = measure(|| {
                    for (i, k) in set.keys.iter().enumerate() {
                        out[i] = marf.get_value(&set.tip, k);
                    }
                });
                let c = marf.counters();
                verify(set, &out, model);
                cell.cold.push(m.elapsed);
                cell.peak_kb = cell.peak_kb.max(m.peak / 1024);
                if r == 0 {
                    cell.counters_cold = c;
                }
                // warm passes
                let _ = marf.counters();
                for _ in 0..passes {
                    let t = Instant::now();
                    for (i, k) in set.keys.iter().enumerate() {
                        out[i] = marf.get_value(&set.tip, k);
                    }
                    cell.warm.push(t.elapsed());
                }
                let c = marf.counters();
                verify(set, &out, model);
                if r == 0 {
                    cell.counters_warm = c;
                }
            }
            drop(marf);
        }
    }
    // table
    println!();
    println!(
        "{:<28} {:>6} {:>8} | {:>12} {:>12} {:>19}",
        "set", "n", "capacity", "cold us/op", "warm us/op", "warm min-max"
    );
    for (si, set) in sets.iter().enumerate() {
        for (ci, cap) in caps.iter().enumerate() {
            let cell = &cells[&(si, ci)];
            let per = |d: std::time::Duration| d.as_secs_f64() * 1e6 / set.keys.len() as f64;
            println!(
                "{:<28} {:>6} {:>8} | {:>12.2} {:>12.2} {:>19}",
                set.name,
                set.keys.len(),
                cap_label(*cap),
                per(cell.cold.median()),
                per(cell.warm.median()),
                format!("{:.2}-{:.2}", per(cell.warm.min()), per(cell.warm.max()))
            );
        }
    }
    let any = cells.values().any(|c| c.counters_warm.available);
    if any {
        println!();
        println!("counters per set and capacity (first repetition; cold = first pass after open, warm = {passes} passes):");
        for (si, set) in sets.iter().enumerate() {
            for (ci, cap) in caps.iter().enumerate() {
                let cell = &cells[&(si, ci)];
                for (label, c) in [("cold", &cell.counters_cold), ("warm", &cell.counters_warm)] {
                    let fields: Vec<String> = c
                        .fields()
                        .into_iter()
                        .filter(|(_, v)| *v != 0)
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect();
                    println!(
                        "  {:<28} cap={:<5} {label}: {}",
                        set.name,
                        cap_label(*cap),
                        fields.join(" ")
                    );
                }
            }
        }
    } else {
        println!("(no read counters in this build; use --features counters for the counter run)");
    }
    if crate::alloc::ALLOC_COUNTING {
        println!();
        for (si, set) in sets.iter().enumerate() {
            for (ci, cap) in caps.iter().enumerate() {
                println!("  peak live heap {:<28} cap={:<5} {} KiB", set.name, cap_label(*cap), cells[&(si, ci)].peak_kb);
            }
        }
    }
}

fn verify(set: &KeySet, out: &[Option<[u8; 40]>], model: &HashMap<String, [u8; 40]>) {
    for (i, k) in set.keys.iter().enumerate() {
        let expected = model.get(k).copied();
        assert_eq!(out[i], expected, "set {} key {k}: wrong value", set.name);
    }
}
