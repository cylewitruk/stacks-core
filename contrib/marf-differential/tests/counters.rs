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

//! Sanity checks of the read-path counters (fifth review): run with
//! `--features counters`. Each check runs for both implementations the
//! binary links (upstream, which has no counters, or the reference through
//! the overlay) and for the rewrite; an implementation without counters is
//! only checked for reporting itself unavailable.

#![cfg(feature = "counters")]

use std::path::Path;

use marf_differential::ablate::{build_small, set_shape, Workload};
use marf_differential::alloc::{Baseline, Counters, Direct, Opts, Rewrite};
use stacks_common::types::chainstate::StacksBlockId;

fn opts(cap: Option<usize>) -> Opts {
    Opts {
        external: true,
        compress: true,
        patch_cache_capacity: cap,
        result_cache_capacity: None,
    }
}

/// Read every key once; returns the counters of that pass.
fn pass<M: Direct>(m: &mut M, tip: &StacksBlockId, keys: &[String]) -> Counters {
    let _ = m.counters();
    for k in keys {
        assert!(m.get_value(tip, k).is_some(), "{k} missing");
    }
    m.counters()
}

/// A patched database and a key set that walks through patched records.
fn setup<M: Direct>(dir: &Path) -> (StacksBlockId, Vec<String>, usize, u64, u64) {
    let (tip, keys) = build_small::<M>(dir, &opts(None), &Workload::small());
    // the first block's keys read at the final tip go through the patched
    // root and second-level nodes
    let set: Vec<String> = keys[..300].to_vec();
    let (distinct, patched_visits, visits) = set_shape(dir, &opts(None), &tip, &set);
    assert!(distinct > 0 && patched_visits > 0, "test set has no patched records");
    (tip, set, distinct, patched_visits, visits)
}

fn check<M: Direct>() {
    let dir = tempfile::tempdir().unwrap();
    let (tip, set, distinct, patched_visits, visits) = setup::<M>(dir.path());
    let db = dir.path().join("marf.sqlite");

    // unavailable measurements are None, never zero
    let mut m = M::open(&db, &opts(None));
    let c = pass(&mut m, &tip, &set);
    if !c.available {
        assert!(c.fields().iter().all(|(_, v)| v.is_none()), "{}: unavailable counters must all be None", M::NAME);
        eprintln!("{}: no counters in this build; only the unavailability check applies", M::NAME);
        return;
    }
    assert_eq!(c.node_visits, Some(visits), "{}: node visits differ from the profiler's walk", M::NAME);
    drop(m);

    // capacity 0: no probes, no hits, no admissions; resolutions still happen
    let mut m = M::open(&db, &opts(Some(0)));
    let c = pass(&mut m, &tip, &set);
    assert_eq!(c.patch_cache_probes, Some(0), "{}: probes at capacity 0", M::NAME);
    assert_eq!(c.patch_cache_hits, Some(0), "{}: hits at capacity 0", M::NAME);
    assert_eq!(c.patch_cache_admissions, Some(0), "{}: admissions at capacity 0", M::NAME);
    assert_eq!(c.patch_resolutions, Some(patched_visits), "{}: every patched visit resolves at capacity 0", M::NAME);
    assert!(c.patch_applications.unwrap() >= patched_visits, "{}: at least one patch applied per resolution", M::NAME);
    assert!(c.owned_materializations.unwrap() >= patched_visits, "{}: a resolution materializes an owned node", M::NAME);
    assert_eq!(m.cache_occupancy(), Some(0), "{}: nothing retained at capacity 0", M::NAME);
    // a second pass resolves again: nothing was reused
    let c2 = pass(&mut m, &tip, &set);
    assert_eq!(c2.patch_resolutions, Some(patched_visits), "{}: capacity 0 reused a resolution", M::NAME);
    drop(m);

    // cold, fitting capacity: each distinct patched record resolved once and
    // admitted; every probe is a hit, a plain miss or an admission
    assert!(distinct <= 4096);
    let mut m = M::open(&db, &opts(Some(4096)));
    assert_eq!(m.cache_occupancy(), Some(0), "{}: fresh handle has an empty cache", M::NAME);
    let c = pass(&mut m, &tip, &set);
    assert_eq!(c.patch_resolutions, Some(distinct as u64), "{}: cold pass resolves each distinct patched record once", M::NAME);
    assert_eq!(c.patch_cache_admissions, Some(distinct as u64), "{}: each resolution admitted", M::NAME);
    assert_eq!(c.patch_cache_hits, Some(patched_visits - distinct as u64), "{}: the other patched visits hit", M::NAME);
    let probes = c.patch_cache_probes.unwrap();
    assert_eq!(
        probes,
        c.patch_cache_hits.unwrap() + c.patch_cache_probe_miss_plain.unwrap() + c.patch_cache_admissions.unwrap(),
        "{}: probes = hits + plain misses + admissions",
        M::NAME
    );
    if let Some(extra) = c.late_probe_extra_parses {
        // the late-probe diagnostic: mapped plain records are not probed, so
        // only SQLite-path plain records count; a probe on a mapped patch
        // follows the parse of its first patch record
        assert!(
            c.patch_cache_probe_miss_plain.unwrap() <= visits - patched_visits,
            "{}: diagnostic plain-record probes exceed the plain visits",
            M::NAME
        );
        assert!(
            extra <= c.patch_cache_hits.unwrap() + c.patch_cache_admissions.unwrap() && extra > 0,
            "{}: late probes after a patch parse ({extra}) must be among the patched probes",
            M::NAME
        );
    } else {
        assert_eq!(c.patch_cache_probe_miss_plain, Some(visits - patched_visits), "{}: plain-record probes", M::NAME);
    }
    assert_eq!(m.cache_occupancy(), Some(distinct as u64), "{}: occupancy after the cold pass", M::NAME);
    // warm: no further resolutions, every patched visit a hit
    let c = pass(&mut m, &tip, &set);
    assert_eq!(c.patch_resolutions, Some(0), "{}: warm fitting set resolved again", M::NAME);
    assert_eq!(c.patch_cache_hits, Some(patched_visits), "{}: warm hits", M::NAME);
    assert_eq!(c.patch_cache_admissions, Some(0));
    assert_eq!(m.cache_occupancy(), Some(distinct as u64));
}

#[test]
fn rewrite_counters_are_consistent() {
    check::<Rewrite>();
}

#[test]
fn baseline_counters_are_consistent_or_unavailable() {
    check::<Baseline>();
}
