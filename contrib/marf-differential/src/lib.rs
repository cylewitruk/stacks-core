//! Differential testing harness for the MARF rewrite.
//!
//! An [`Op`] sequence is replayed by a [`Backend`] which yields one
//! [`Observation`] per op. Fixtures recorded from the baseline are replayed
//! against the rewrite; the two observation lists must be identical.

pub mod ablate;
pub mod adapter;
pub mod alloc;
pub mod baseline;
pub mod bench;
pub mod construct;
pub mod r#gen;
pub mod inspect;
pub mod ops;
pub mod rewrite;

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

pub use ops::{Fixture, Observation, Op, OpenOpts};

/// The baseline mutates the process-wide `TMPDIR` while opening a fresh
/// database, which races with `tempfile` in other threads. Runs are
/// serialized through this lock.
static RUN_LOCK: Mutex<()> = Mutex::new(());

fn run_lock() -> MutexGuard<'static, ()> {
    RUN_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// How a transaction-scoped sub-run ended.
pub enum TxOutcome {
    /// `n` ops consumed, the transaction was committed or dropped.
    Consumed(usize),
    /// `n` ops consumed; op `n` must be re-processed outside the transaction
    /// (the transaction was rolled back by dropping it).
    Abandoned(usize),
}

pub trait Backend {
    /// Replay `ops`, returning one observation per op.
    fn run(&mut self, ops: &[Op]) -> Vec<Observation>;
    /// The directory holding the database currently in use (squashing moves it).
    fn dir(&self) -> std::path::PathBuf;
}

/// Which implementation to drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Impl {
    Baseline,
    Rewrite,
}

impl Impl {
    pub fn parse(s: &str) -> Option<Impl> {
        match s {
            "baseline" => Some(Impl::Baseline),
            "rewrite" => Some(Impl::Rewrite),
            _ => None,
        }
    }

    pub fn backend(self, dir: &Path) -> Box<dyn Backend> {
        match self {
            Impl::Baseline => Box::new(adapter::Driver::new(baseline::Oracle, dir)),
            Impl::Rewrite => Box::new(adapter::Driver::new(rewrite::Rewrite, dir)),
        }
    }
}

/// A single divergence between an expected and an actual observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Divergence {
    pub index: usize,
    pub op: Op,
    pub expected: Observation,
    pub actual: Observation,
}

impl std::fmt::Display for Divergence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "op #{} {:?}\n  expected: {:?}\n  actual:   {:?}",
            self.index, self.op, self.expected, self.actual
        )
    }
}

/// Compare observation lists op by op.
pub fn compare(ops: &[Op], expected: &[Observation], actual: &[Observation]) -> Vec<Divergence> {
    let mut out = vec![];
    for (i, op) in ops.iter().enumerate() {
        let e = expected
            .get(i)
            .cloned()
            .unwrap_or(Observation::Err("<missing>".into()));
        let a = actual
            .get(i)
            .cloned()
            .unwrap_or(Observation::Err("<missing>".into()));
        if e != a {
            out.push(Divergence {
                index: i,
                op: op.clone(),
                expected: e,
                actual: a,
            });
        }
    }
    out
}

/// Record a fixture by running `ops` against the baseline in a fresh temp dir.
impl<A: adapter::MarfApi> Backend for adapter::Driver<A> {
    fn run(&mut self, ops: &[Op]) -> Vec<Observation> {
        adapter::Driver::run(self, ops)
    }
    fn dir(&self) -> std::path::PathBuf {
        adapter::Driver::dir(self)
    }
}

pub fn record(name: &str, seed: u64, ops: Vec<Op>) -> Fixture {
    let _guard = run_lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let mut backend = Impl::Baseline.backend(dir.path());
    let expected = backend.run(&ops);
    Fixture {
        name: name.to_string(),
        seed,
        ops,
        expected,
    }
}

/// Replay a fixture against `which` in a fresh temp dir and return divergences.
pub fn replay(fixture: &Fixture, which: Impl) -> Vec<Divergence> {
    let _guard = run_lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let mut backend = which.backend(dir.path());
    let actual = backend.run(&fixture.ops);
    compare(&fixture.ops, &fixture.expected, &actual)
}

/// Run `first` with implementation `a`, then `second` with `b` on the same
/// database directory (a clean hand-off: `a` is closed before `b` opens).
/// Returns both observation lists.
pub fn handoff(
    dir: &Path,
    a: Impl,
    first: &[Op],
    b: Impl,
    second: &[Op],
) -> (Vec<Observation>, Vec<Observation>) {
    let _guard = run_lock();
    let (first_obs, dir) = {
        let mut backend = a.backend(dir);
        let mut obs = backend.run(first);
        obs.extend(backend.run(&[Op::Close]));
        obs.pop();
        (obs, backend.dir())
    };
    let second_obs = {
        let mut backend = b.backend(&dir);
        backend.run(second)
    };
    (first_obs, second_obs)
}

/// Directory holding checked-in fixtures.
pub fn fixtures_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

pub fn load_fixture(path: &Path) -> Fixture {
    let text = std::fs::read_to_string(path).expect("read fixture");
    serde_json::from_str(&text).expect("parse fixture")
}

pub fn save_fixture(fixture: &Fixture, path: &Path) {
    let text = serde_json::to_string(fixture).expect("serialize fixture");
    std::fs::write(path, text).expect("write fixture");
}
