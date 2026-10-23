//! Replay the checked-in fixtures against each implementation.

use marf_differential::ops::{Observation, Op};
use marf_differential::{compare, fixtures_dir, handoff, load_fixture, replay, Impl};

fn fixture_paths() -> Vec<std::path::PathBuf> {
    let mut paths: Vec<_> = std::fs::read_dir(fixtures_dir())
        .expect("fixtures dir")
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().map(|e| e == "json").unwrap_or(false))
        .collect();
    paths.sort();
    paths
}

fn replay_all(which: Impl) {
    let mut failures = vec![];
    for path in fixture_paths() {
        let fixture = load_fixture(&path);
        let divergences = replay(&fixture, which);
        if !divergences.is_empty() {
            eprintln!("{}: {} divergences", fixture.name, divergences.len());
            for d in divergences.iter().take(5) {
                eprintln!("{d}");
            }
            failures.push(fixture.name.clone());
        }
    }
    assert!(failures.is_empty(), "fixtures diverged: {failures:?}");
}

#[test]
fn baseline_reproduces_fixtures() {
    replay_all(Impl::Baseline);
}

#[test]
fn rewrite_reproduces_fixtures() {
    replay_all(Impl::Rewrite);
}

/// The four `small_*` fixtures share a seed and differ only in storage
/// options, so every consensus-visible observation must be identical.
#[test]
fn storage_variants_agree_on_consensus_observations() {
    let names = [
        "small_ext_comp",
        "small_ext_plain",
        "small_sql_comp",
        "small_sql_plain_imm",
    ];
    let fixtures: Vec<_> = names
        .iter()
        .map(|n| load_fixture(&fixtures_dir().join(format!("{n}.json"))))
        .collect();
    let consensus = |f: &marf_differential::Fixture| -> Vec<Observation> {
        f.expected
            .iter()
            .filter(|o| {
                matches!(
                    o,
                    Observation::Hash(_) | Observation::Proof { .. } | Observation::Value(_)
                )
            })
            .cloned()
            .collect()
    };
    let first = consensus(&fixtures[0]);
    assert!(first.iter().any(|o| matches!(o, Observation::Hash(_))));
    for f in &fixtures[1..] {
        assert_eq!(
            first,
            consensus(f),
            "{} differs from {}",
            f.name,
            fixtures[0].name
        );
    }
}

/// Observations the rewrite does not produce yet are ignored by the hand-off
/// tests until the corresponding feature lands.
fn unsupported(op: &Op) -> bool {
    let _ = op;
    false
}

/// Run fixtures split at hand-off points (after a `Close`, or after a
/// `Prune`, both of which leave the handle closed): the prefix with `a`, the
/// suffix with `b`, on the same database directory. Both halves must produce
/// the baseline's recorded observations, and every fixture must contribute
/// at least one hand-off.
///
/// Coverage: every `Prune` boundary, plus about a quarter of the `Close`
/// boundaries of each fixture (evenly spaced), 48 of the 132 eligible
/// boundaries per direction at the time of writing. Set
/// `MARF_DIFF_ALL_HANDOFFS=1` to split at every boundary (about four times
/// the runtime).
fn handoff_all(a: Impl, b: Impl) {
    let all = std::env::var_os("MARF_DIFF_ALL_HANDOFFS").is_some_and(|v| v == "1");
    let mut failures = vec![];
    let mut uncovered = vec![];
    for path in fixture_paths() {
        let fixture = load_fixture(&path);
        let points: Vec<usize> = fixture
            .ops
            .iter()
            .enumerate()
            .filter(|(_, op)| matches!(op, Op::Close | Op::Prune { .. }))
            .map(|(i, _)| i + 1)
            .filter(|&i| i < fixture.ops.len())
            .collect();
        // squash boundaries always; otherwise a quarter of the split points
        // per fixture, unless every boundary was asked for
        let step = if all { 1 } else { (points.len() / 4).max(1) };
        let mut splits: Vec<usize> = points.iter().copied().step_by(step).collect();
        for (i, op) in fixture.ops.iter().enumerate() {
            if matches!(op, Op::Prune { .. })
                && i + 1 < fixture.ops.len()
                && !splits.contains(&(i + 1))
            {
                splits.push(i + 1);
            }
        }
        if splits.is_empty() {
            uncovered.push(fixture.name.clone());
        }
        for split in splits {
            let dir = tempfile::Builder::new()
                .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
                .unwrap();
            let (first, second) = handoff(
                dir.path(),
                a,
                &fixture.ops[..split],
                b,
                &fixture.ops[split..],
            );
            let mut actual = first;
            actual.extend(second);
            let divergences: Vec<_> = compare(&fixture.ops, &fixture.expected, &actual)
                .into_iter()
                .filter(|d| !unsupported(&d.op))
                .collect();
            if !divergences.is_empty() {
                eprintln!(
                    "{} split at {split} ({a:?} -> {b:?}): {} divergences",
                    fixture.name,
                    divergences.len()
                );
                for d in divergences.iter().take(3) {
                    eprintln!("{d}");
                }
                failures.push(format!("{}@{split}", fixture.name));
            }
        }
    }
    assert!(
        uncovered.is_empty(),
        "fixtures without any hand-off point: {uncovered:?}"
    );
    assert!(failures.is_empty(), "hand-offs diverged: {failures:?}");
}

#[test]
fn baseline_then_rewrite_handoff() {
    handoff_all(Impl::Baseline, Impl::Rewrite);
}

#[test]
fn rewrite_then_baseline_handoff() {
    handoff_all(Impl::Rewrite, Impl::Baseline);
}

/// Everything except not-yet-implemented ops must already match.
#[test]
fn rewrite_reproduces_supported_fixture_ops() {
    let mut failures = vec![];
    for path in fixture_paths() {
        let fixture = load_fixture(&path);
        let divergences: Vec<_> = replay(&fixture, Impl::Rewrite)
            .into_iter()
            .filter(|d| !unsupported(&d.op))
            .collect();
        if !divergences.is_empty() {
            eprintln!("{}: {} divergences", fixture.name, divergences.len());
            for d in divergences.iter().take(5) {
                eprintln!("{d}");
            }
            failures.push(fixture.name.clone());
        }
    }
    assert!(failures.is_empty(), "fixtures diverged: {failures:?}");
}
