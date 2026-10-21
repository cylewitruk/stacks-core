// Copyright (C) 2026 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

//! Committed-read patch-cache ablation (fourth review, part 3; cells
//! isolated in the fifth).
//!
//! One implementation per process (the run script runs each binary in
//! turn), one workload, a list of patch-cache capacities. The database is
//! built once; key sets are chosen by walk shape with the rewrite's
//! profiler on the shared files (the stored trie is identical for every
//! implementation), including the identity of the patched records each
//! walk meets, so a set's number of distinct patched nodes is known.
//!
//! Measurement cells. In the default `isolated` mode every (capacity, key
//! set, repetition) is its own cell: a fresh `MARF` handle is opened for
//! it (every process-local cache empty, a new blobs mapping), its cache
//! occupancy is recorded, the set is read once (the **cold** or
//! **first-pass** sample), then `PASSES` more times (the **warm** samples),
//! occupancy is recorded again and the handle is dropped. Nothing from
//! another set or capacity is in the caches of a cell. With
//! `DROP_CACHES=1` the OS page cache is dropped before every cell (root:
//! writes 3 to `/proc/sys/vm/drop_caches`); the run fails at once if that
//! write fails, so a **page-cold** cell is never a warm one in disguise.
//! Definitions: *process-cold* = fresh handle, page cache as left by
//! building the database (warm); *page-cold* = process-cold plus the drop;
//! *first pass* = the one read of the set in that state; *warmed* = the
//! following passes of the same cell.
//!
//! `ABLATE_MODE=shared` keeps the round-4 scheme as a separately named
//! experiment: one handle per capacity, every set read through it in order,
//! so later sets inherit cache entries and page residency from earlier
//! ones; the JSON records which sets preceded each cell and the occupancy
//! at entry. It is for mixed-history behaviour only and must not be read
//! as isolated cost.
//!
//! Values are collected into a preallocated
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

use crate::alloc::{environment, measure, repeat, write_json, Counters, Direct, Manifest, Opts, Samples};
use crate::Impl;

/// One measurement cell of the ablation, as written to `JSON_OUT`.
#[derive(Clone, Debug, serde::Serialize)]
pub struct CellSample {
    pub provider: &'static str,
    pub workload: &'static str,
    pub plain: bool,
    pub mode: &'static str,
    pub page_cold: bool,
    pub capacity: Option<usize>,
    pub set: String,
    pub set_tip: &'static str,
    pub n_keys: usize,
    pub distinct_patched_nodes: usize,
    pub avg_hops: f64,
    pub avg_patched_per_walk: f64,
    pub avg_patch_depth: f64,
    pub repetition: usize,
    /// Sets read through the same handle before this one (shared mode).
    pub preceding_sets: Vec<String>,
    pub occupancy_entry: Option<u64>,
    pub occupancy_after_cold: Option<u64>,
    pub occupancy_exit: Option<u64>,
    pub cold_ns: u128,
    pub warm_ns: Vec<u128>,
    pub counters_cold: Counters,
    pub counters_warm: Counters,
    pub peak_heap_bytes: u64,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct AblateOutput {
    pub manifest: Manifest,
    pub workload: Workload,
    pub seeds: (u64, u64),
    pub passes: usize,
    pub capacities: Vec<Option<usize>>,
    pub describe: String,
    pub cells: Vec<CellSample>,
}

/// Workload shapes. `std` is the `alloc` workload (patched nodes: the root
/// and second-level nodes, a few hundred distinct); `big` has enough keys
/// for third-level branch nodes, so its final tip has thousands of distinct
/// patched nodes.
#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct Workload {
    pub name: &'static str,
    pub blocks: usize,
    pub inserts: usize,
    pub patch_blocks: usize,
    pub patch_inserts: usize,
}

impl Workload {
    /// A small shape for tests: 6 x 500 keys, then 4 x 50 rewrites.
    pub fn small() -> Workload {
        Workload {
            name: "small",
            blocks: 6,
            inserts: 500,
            patch_blocks: 4,
            patch_inserts: 50,
        }
    }

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

/// Walk shape of a key set: distinct patched records met, and the total
/// number of patched-record visits over one pass (what a fully warm cache
/// would answer).
pub fn set_shape(dir: &Path, opts: &Opts, tip: &StacksBlockId, keys: &[String]) -> (usize, u64, u64) {
    let ps = profiles(dir, opts, tip, keys);
    let mut sites = HashSet::new();
    let mut patched_visits = 0u64;
    let mut visits = 0u64;
    for p in &ps {
        for s in &p.patched_sites[..(p.patched_nodes as usize).min(8)] {
            sites.insert(*s);
        }
        patched_visits += u64::from(p.patched_nodes);
        visits += u64::from(p.nodes);
    }
    (sites.len(), patched_visits, visits)
}

/// Build a small patched database for tests: `blocks` bulk blocks of
/// `inserts` keys then `patch_blocks` blocks rewriting `patch_inserts` of
/// them. Returns the final tip and every key.
pub fn build_small<M: Direct>(dir: &Path, opts: &Opts, w: &Workload) -> (StacksBlockId, Vec<String>) {
    let b = build::<M>(dir, w, opts);
    (b.tip, b.all_keys)
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

/// Whether page-cold cells were requested (`DROP_CACHES=1`).
fn page_cold_requested() -> bool {
    std::env::var("DROP_CACHES").is_ok_and(|v| v == "1")
}

/// Drop the OS page cache (root). A failure aborts the run: a page-cold
/// configuration is never reported from a warm page cache.
fn drop_caches_or_die() {
    // sync first so dirty pages can be dropped too
    let _ = std::process::Command::new("sync").status();
    if let Err(e) = std::fs::write("/proc/sys/vm/drop_caches", b"3") {
        eprintln!("DROP_CACHES=1 requested but writing /proc/sys/vm/drop_caches failed: {e}");
        eprintln!("page-cold measurements are unavailable on this machine; aborting instead of reporting warm figures");
        std::process::exit(3);
    }
}

/// A key set to time: keys, the tip they are read at, and its walk shape.
struct KeySet {
    name: String,
    tip: StacksBlockId,
    tip_label: &'static str,
    keys: Vec<String>,
    distinct_patched_nodes: usize,
    avg_hops: f64,
    avg_patched: f64,
    avg_depth: f64,
}

/// Profile every key at `tip` with the rewrite's profiler (read-only open
/// on the shared files).
pub fn profiles(
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
    tip_label: &'static str,
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
        tip_label,
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
    Some(make_set(label, tip, "final", ks, &ps))
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
        sets.push(make_set("patchfree@tip1".into(), &built.tip1, "tip1", keys, &refs));
    }
    // the bulk tip: a random sample of every key, read at the tip of the
    // bulk blocks (full root, second-level nodes as patch chains)
    {
        let keys: Vec<String> = (0..reads)
            .map(|_| built.all_keys[rng.gen_range(0..built.all_keys.len())].clone())
            .collect();
        let ps = profiles(dir, opts, &built.bulk_tip, &keys);
        let refs: Vec<&_> = ps.iter().collect();
        sets.push(make_set("bulk tip (random keys)".into(), &built.bulk_tip, "bulk", keys, &refs));
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
        sets.push(make_set(label, &built.tip, "final", ks, &refs));
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

/// Read every key of `set` once into `out`, timed; returns the time and the
/// counters of the pass.
/// The cold (first) pass of a cell: the timed loop and nothing else. Kept as
/// a separate, never-inlined function so an instruction profiler can
/// collect inside it alone (`callgrind --toggle-collect`, `scripts/profile.sh`).
#[inline(never)]
pub fn timed_cold_pass<M: Direct>(marf: &mut M, set: &KeySet, out: &mut [Option<[u8; 40]>]) -> crate::alloc::Measure {
    let (_, m) = measure(|| {
        for (i, k) in set.keys.iter().enumerate() {
            out[i] = marf.get_value(&set.tip, k);
        }
    });
    m
}

/// One warm pass of a cell; same body as the cold pass, separate symbol so
/// cold and warm instruction profiles do not mix.
#[inline(never)]
pub fn timed_warm_pass<M: Direct>(marf: &mut M, set: &KeySet, out: &mut [Option<[u8; 40]>]) -> crate::alloc::Measure {
    let (_, m) = measure(|| {
        for (i, k) in set.keys.iter().enumerate() {
            out[i] = marf.get_value(&set.tip, k);
        }
    });
    m
}

fn pass<M: Direct>(marf: &mut M, set: &KeySet, out: &mut [Option<[u8; 40]>]) -> (std::time::Duration, Counters, u64) {
    let _ = marf.counters();
    let m = timed_cold_pass(marf, set, out);
    (m.elapsed, marf.counters(), m.peak)
}

fn run_direct<M: Direct>(w: &Workload, reads: usize, plain: bool) {
    let manifest = Manifest::new("ablate");
    println!("{}", environment());
    let mode: &'static str = match std::env::var("ABLATE_MODE").as_deref() {
        Ok("shared") => "shared",
        _ => "isolated",
    };
    let page_cold = page_cold_requested();
    if page_cold {
        drop_caches_or_die(); // fail early if unavailable
    }
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
    let mut sets = key_sets(dir.path(), &opts, &built, reads);
    // SETS=a,b keeps only the key sets whose name contains one of the
    // substrings (profiling aid: one set per profiled process).
    if let Ok(filter) = std::env::var("SETS") {
        let wanted: Vec<&str> = filter.split(',').filter(|s| !s.is_empty()).collect();
        sets.retain(|s| wanted.iter().any(|w| s.name.contains(w)));
        assert!(!sets.is_empty(), "SETS={filter} matched no key set");
    }
    let describe = M::open(&dir.path().join("marf.sqlite"), &opts).describe(&opts);
    println!("impl {}: {}", M::NAME, describe);
    println!(
        "mode {mode}: {}; cold = {}",
        if mode == "isolated" {
            "fresh handle per capacity x set x repetition, cold pass then warm passes within the cell"
        } else {
            "one handle per capacity, sets read in sequence (mixed history; occupancy and preceding sets recorded)"
        },
        if page_cold {
            "fresh handle + OS page cache dropped before the cell (page-cold)"
        } else {
            "fresh handle, OS page cache as left by building the database (process-cold)"
        }
    );
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
    let mut samples: Vec<CellSample> = vec![];
    let mut out: Vec<Option<[u8; 40]>> = vec![None; sets.iter().map(|s| s.keys.len()).max().unwrap_or(0)];
    let model_for = |set: &KeySet| -> &HashMap<String, [u8; 40]> {
        if set.tip == built.tip1 {
            &built.model1
        } else if set.tip == built.bulk_tip {
            &built.model_bulk
        } else {
            &built.model
        }
    };
    for r in 0..repeat() {
        for ci in 0..caps.len() {
            // rotate the capacity order between repetitions
            let ci = (ci + r) % caps.len();
            let cap = caps[ci];
            let o = Opts {
                patch_cache_capacity: cap,
                ..opts
            };
            // shared mode: one handle for every set of this capacity
            let mut shared: Option<M> = None;
            if mode == "shared" {
                if page_cold {
                    drop_caches_or_die();
                }
                shared = Some(M::open(&dir.path().join("marf.sqlite"), &o));
            }
            let mut preceding: Vec<String> = vec![];
            for (si, set) in sets.iter().enumerate() {
                let mut fresh: Option<M> = None;
                if mode == "isolated" {
                    if page_cold {
                        drop_caches_or_die();
                    }
                    fresh = Some(M::open(&dir.path().join("marf.sqlite"), &o));
                }
                let marf: &mut M = fresh.as_mut().or(shared.as_mut()).expect("a handle");
                if r == 0 && ci == 0 && si == 0 {
                    println!(
                        "capacities: {:?} -> e.g. {}",
                        caps.iter().map(|c| cap_label(*c)).collect::<Vec<_>>(),
                        marf.describe(&o)
                    );
                }
                let model = model_for(set);
                let occupancy_entry = marf.cache_occupancy();
                let (cold_t, counters_cold, peak) = pass(marf, set, &mut out);
                verify(set, &out, model, "cold pass");
                let occupancy_after_cold = marf.cache_occupancy();
                let mut warm_ns = vec![];
                let mut counters_warm = Counters::default();
                let _ = marf.counters();
                let mut warm_peak = 0u64;
                for p in 0..passes {
                    let (d, _, pk) = pass_no_counters(marf, set, &mut out);
                    warm_ns.push(d.as_nanos());
                    warm_peak = warm_peak.max(pk);
                    // every pass is checked, outside its timed region
                    verify(set, &out, model, &format!("warm pass {}", p + 1));
                }
                counters_warm = counters_warm.max_merge(marf.counters());
                let occupancy_exit = marf.cache_occupancy();
                let cell = cells.entry((si, ci)).or_insert_with(|| Cell {
                    cold: Samples::default(),
                    warm: Samples::default(),
                    counters_cold: Counters::default(),
                    counters_warm: Counters::default(),
                    peak_kb: 0,
                });
                cell.cold.push(cold_t);
                for ns in &warm_ns {
                    cell.warm.push(std::time::Duration::from_nanos(*ns as u64));
                }
                cell.peak_kb = cell.peak_kb.max(peak.max(warm_peak) / 1024);
                if r == 0 {
                    cell.counters_cold = counters_cold;
                    cell.counters_warm = counters_warm;
                }
                samples.push(CellSample {
                    provider: M::NAME,
                    workload: w.name,
                    plain,
                    mode,
                    page_cold,
                    capacity: cap,
                    set: set.name.clone(),
                    set_tip: set.tip_label,
                    n_keys: set.keys.len(),
                    distinct_patched_nodes: set.distinct_patched_nodes,
                    avg_hops: set.avg_hops,
                    avg_patched_per_walk: set.avg_patched,
                    avg_patch_depth: set.avg_depth,
                    repetition: r,
                    preceding_sets: preceding.clone(),
                    occupancy_entry,
                    occupancy_after_cold,
                    occupancy_exit,
                    cold_ns: cold_t.as_nanos(),
                    warm_ns,
                    counters_cold,
                    counters_warm,
                    peak_heap_bytes: peak.max(warm_peak),
                });
                if mode == "shared" {
                    preceding.push(set.name.clone());
                }
                drop(fresh);
            }
            drop(shared);
        }
    }
    // table
    println!();
    println!(
        "{:<28} {:>6} {:>8} | {:>12} {:>12} {:>19} {:>10}",
        "set", "n", "capacity", "cold us/op", "warm us/op", "warm min-max", "occupancy"
    );
    for (si, set) in sets.iter().enumerate() {
        for (ci, cap) in caps.iter().enumerate() {
            let cell = &cells[&(si, ci)];
            let per = |d: std::time::Duration| d.as_secs_f64() * 1e6 / set.keys.len() as f64;
            let occ = samples
                .iter()
                .find(|c| c.set == set.name && c.capacity == *cap && c.repetition == 0)
                .and_then(|c| c.occupancy_exit)
                .map(|o| o.to_string())
                .unwrap_or_else(|| "n/a".into());
            println!(
                "{:<28} {:>6} {:>8} | {:>12.2} {:>12.2} {:>19} {:>10}",
                set.name,
                set.keys.len(),
                cap_label(*cap),
                per(cell.cold.median()),
                per(cell.warm.median()),
                format!("{:.2}-{:.2}", per(cell.warm.min()), per(cell.warm.max())),
                occ
            );
        }
    }
    let any = cells.values().any(|c| c.counters_warm.available);
    if any {
        println!();
        println!("counters per set and capacity (first repetition; cold = first pass, warm = {passes} passes):");
        for (si, set) in sets.iter().enumerate() {
            for (ci, cap) in caps.iter().enumerate() {
                let cell = &cells[&(si, ci)];
                for (label, c) in [("cold", &cell.counters_cold), ("warm", &cell.counters_warm)] {
                    println!("  {:<28} cap={:<5} {label}: {}", set.name, cap_label(*cap), c.summary());
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
    write_json(&AblateOutput {
        manifest,
        workload: *w,
        seeds: (11, 23),
        passes,
        capacities: caps,
        describe,
        cells: samples,
    });
}

/// A timed pass without touching the counters (they accumulate across the
/// warm passes and are taken once after them).
fn pass_no_counters<M: Direct>(marf: &mut M, set: &KeySet, out: &mut [Option<[u8; 40]>]) -> (std::time::Duration, (), u64) {
    let m = timed_warm_pass(marf, set, out);
    (m.elapsed, (), m.peak)
}

/// Check one pass's returned values against the model. `Err` names the
/// first wrong key. Called after every pass (cold and each warm pass),
/// outside the timed region, so a wrong intermediate pass cannot be hidden
/// by a correct later one.
pub fn check_pass(
    keys: &[String],
    out: &[Option<[u8; 40]>],
    model: &HashMap<String, [u8; 40]>,
) -> Result<(), String> {
    for (i, k) in keys.iter().enumerate() {
        let expected = model.get(k).copied();
        if out[i] != expected {
            return Err(format!("key {k}: got {:?}, expected {:?}", out[i], expected));
        }
    }
    Ok(())
}

/// Check every pass of a cell in order; `Err((pass, message))` is the first
/// pass that disagreed with the model.
pub fn check_passes(
    keys: &[String],
    passes: &[Vec<Option<[u8; 40]>>],
    model: &HashMap<String, [u8; 40]>,
) -> Result<(), (usize, String)> {
    for (p, out) in passes.iter().enumerate() {
        check_pass(keys, out, model).map_err(|e| (p, e))?;
    }
    Ok(())
}

fn verify(set: &KeySet, out: &[Option<[u8; 40]>], model: &HashMap<String, [u8; 40]>, pass: &str) {
    if let Err(e) = check_pass(&set.keys, out, model) {
        panic!("set {} {pass}: wrong value: {e}", set.name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model_and_passes() -> (Vec<String>, HashMap<String, [u8; 40]>, Vec<Vec<Option<[u8; 40]>>>) {
        let keys: Vec<String> = (0..4).map(|i| format!("k{i}")).collect();
        let mut model = HashMap::new();
        for (i, k) in keys.iter().enumerate() {
            model.insert(k.clone(), [i as u8; 40]);
        }
        let good: Vec<Option<[u8; 40]>> = (0..4).map(|i| Some([i as u8; 40])).collect();
        let mut bad = good.clone();
        bad[2] = Some([0xee; 40]); // one wrong value in the middle pass
        (keys, model, vec![good.clone(), bad, good])
    }

    /// A wrong intermediate warm pass followed by a correct final pass: the
    /// final-only check (the round-5 harness) accepts it; the per-pass check
    /// rejects it and names the pass.
    #[test]
    fn wrong_intermediate_pass_is_not_hidden_by_a_correct_final_pass() {
        let (keys, model, passes) = model_and_passes();
        // what the round-5 harness checked: the buffer after the last pass
        assert!(check_pass(&keys, passes.last().unwrap(), &model).is_ok());
        // what the harness checks now: every pass
        let err = check_passes(&keys, &passes, &model).unwrap_err();
        assert_eq!(err.0, 1, "the second pass (index 1) is the wrong one");
        assert!(err.1.contains("key k2"), "{}", err.1);
    }

    #[test]
    fn all_correct_passes_are_accepted_and_a_missing_value_is_rejected() {
        let (keys, model, mut passes) = model_and_passes();
        passes[1] = passes[0].clone();
        assert!(check_passes(&keys, &passes, &model).is_ok());
        passes[2][0] = None; // absence where the model has a value
        assert_eq!(check_passes(&keys, &passes, &model).unwrap_err().0, 2);
    }
}
