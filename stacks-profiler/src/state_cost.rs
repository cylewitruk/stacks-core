//! Benchmark-only exclusive wall-time attribution; no value bytes are retained.
use std::cell::{Cell as FlagCell, RefCell};
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::Instant;
use serde::Serialize;

const PHASES: &[&str] = &["other", "setup", "transaction", "receipts", "seal", "clarity_commit", "advance_tip", "headers_commit", "checkpoint"];
const OPS: &[&str] = &["other", "read", "write"];
const CATS: &[&str] = &["other", "state_read", "lookup", "value_fetch", "decode", "projection", "materialize", "encode", "writeback", "dedup", "trie_insert", "node_hash", "ancestor", "trie_serialize", "trie_store", "sync", "mapping", "contract_load", "sql_metadata"];
const SIZE: usize = PHASES.len() * OPS.len() * CATS.len();

/// A non-overlapping cell in the phase/operation/category partition.
#[derive(Clone, Copy, Default)]
struct Cell { ns: u64, calls: u64 }
/// Active classification, restored by a nested guard.
#[derive(Clone, Copy, Default)]
struct Key { phase: usize, op: usize, cat: usize }
impl Key {
    /// Address a fixed-size cell without an allocation or string lookup.
    fn index(self) -> usize { (self.phase * OPS.len() + self.op) * CATS.len() + self.cat }
}
/// Thread-local accumulator for one synchronous measured block.
struct State { start: Instant, last: u64, key: Key, depth: usize, cells: Box<[Cell]> }
impl State {
    /// Credit elapsed time to exactly the currently innermost category.
    fn tick(&mut self, now: u64) {
        self.cells[self.key.index()].ns += now.checked_sub(self.last).expect("monotonic diagnostic clock");
        self.last = now;
    }
}
thread_local! { static STATE: RefCell<Option<State>> = const { RefCell::new(None) }; }
thread_local! { static SUPPRESSED: FlagCell<bool> = const { FlagCell::new(false) }; }

/// Prevents detailed probes while retaining elapsed time in the enclosing phase.
pub struct Suppression { previous: bool, _local: PhantomData<Rc<()>> }

/// Suppress nested probes during benchmark receipt auditing on this thread.
pub fn suppress() -> Suppression {
    Suppression { previous: SUPPRESSED.with(|flag| flag.replace(true)), _local: PhantomData }
}

impl Drop for Suppression {
    fn drop(&mut self) { SUPPRESSED.with(|flag| flag.set(self.previous)); }
}

/// Read the cheap thread-local suppression flag before any diagnostic clock.
#[inline]
fn suppressed() -> bool { SUPPRESSED.with(FlagCell::get) }


/// Whether this process requested the optional attribution instrumentation.
#[inline]
pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("STACKS_STATE_DIAGNOSTICS").as_deref() == Ok("1"))
}
/// A serializable exclusive wall-time cell.
#[derive(Serialize)]
pub struct Row {
    /// Enclosing block-processing phase.
    pub phase: &'static str,
    /// State read/write context, preserved through nested calls.
    pub operation: &'static str,
    /// Innermost instrumented work category.
    pub category: &'static str,
    /// Exclusive elapsed nanoseconds, including descheduling and page faults.
    pub ns: u64,
    /// Number of entered instrumented scopes.
    pub calls: u64,
}
/// One complete measured synchronous window, excluding report serialization.
#[derive(Serialize)]
pub struct Snapshot {
    /// Wall time against which the exclusive partition is checked.
    pub elapsed_ns: u64,
    /// Nonzero cells; their elapsed times must sum exactly to elapsed_ns.
    pub rows: Vec<Row>,
}
/// A synchronous window owner that clears thread-local state on early return.
pub struct Window { active: bool, _local: PhantomData<Rc<()>> }
impl Window {
    /// Start only an explicitly measured window when diagnostics are enabled.
    pub fn new(measured: bool) -> Self { Self::start(measured && enabled()) }
    /// Initialize a window; exposed to local deterministic tests through this module.
    fn start(active: bool) -> Self {
        if active { STATE.with(|slot| {
            let mut slot = slot.borrow_mut();
            assert!(slot.is_none(), "nested diagnostic windows");
            *slot = Some(State { start: Instant::now(), last: 0, key: Key::default(), depth: 0, cells: vec![Cell::default(); SIZE].into_boxed_slice() });
        }); }
        Self { active, _local: PhantomData }
    }
    /// Stop clocks before allocating/serializing the result.
    pub fn finish(mut self) -> Option<Snapshot> {
        if !self.active { return None; }
        let mut state = STATE.with(|slot| slot.borrow_mut().take().expect("active window"));
        assert_eq!(state.depth, 0, "diagnostic guard outlived its window");
        state.tick(state.start.elapsed().as_nanos() as u64);
        self.active = false;
        let mut rows = Vec::new();
        for (i, cell) in state.cells.iter().enumerate() {
            if cell.ns == 0 && cell.calls == 0 { continue; }
            rows.push(Row { phase: PHASES[i / (OPS.len() * CATS.len())], operation: OPS[(i / CATS.len()) % OPS.len()], category: CATS[i % CATS.len()], ns: cell.ns, calls: cell.calls });
        }
        assert_eq!(rows.iter().map(|r| r.ns).sum::<u64>(), state.last);
        Some(Snapshot { elapsed_ns: state.last, rows })
    }
}
impl Drop for Window {
    fn drop(&mut self) { if self.active { STATE.with(|slot| *slot.borrow_mut() = None); } }
}
/// Restores the parent classification on ordinary and early returns.
pub struct Guard { previous: Key, depth: usize, _local: PhantomData<Rc<()>> }
impl Drop for Guard {
    fn drop(&mut self) { STATE.with(|slot| {
        let mut slot = slot.borrow_mut();
        if let Some(state) = slot.as_mut() {
            assert_eq!(state.depth, self.depth, "diagnostic scopes must be LIFO");
            state.tick(state.start.elapsed().as_nanos() as u64);
            state.key = self.previous;
            state.depth -= 1;
        }
    }); }
}
/// Enter a category or phase without allocating on the hot path.
#[inline]
fn enter(phase: Option<usize>, cat: usize, op: Option<usize>) -> Option<Guard> {
    if !enabled() { return None; }
    enter_active(phase, cat, op)
}
/// Enter the active window independently of the environment, for isolated tests.
fn enter_active(phase: Option<usize>, cat: usize, op: Option<usize>) -> Option<Guard> {
    if suppressed() { return None; }
    STATE.with(|slot| {
        let mut slot = slot.borrow_mut();
        let state = slot.as_mut()?;
        state.tick(state.start.elapsed().as_nanos() as u64);
        let previous = state.key;
        if let Some(phase) = phase { state.key.phase = phase; }
        if let Some(op) = op { state.key.op = op; }
        state.key.cat = cat;
        state.depth += 1;
        state.cells[state.key.index()].calls += 1;
        Some(Guard { previous, depth: state.depth, _local: PhantomData })
    })
}
/// Enter a named outer phase, resetting read/write context.
pub fn phase(name: &str) -> Option<Guard> {
    if !enabled() { return None; }
    let phase = PHASES.iter().position(|v| *v == name).expect("known phase");
    enter(Some(phase), 0, Some(0))
}
/// Enter a fixed-name core probe; unknown legacy probes remain disabled.
#[inline]
pub fn named(name: &str) -> Option<Guard> {
    if suppressed() || !enabled() { return None; }
    let (cat, op) = match name {
        "State: Read" => (1, Some(1)),
        "MARF lookup key" | "MARF lookup hash" => (2, None),
        "Clarity side-store SQLite get" | "Clarity value extent fetch" => (3, None),
        "Value: Canonical decode" | "State: Decode" => (4, None),
        "State: Projection" => (5, None),
        "Value: Materialize" => (6, None),
        "Value: Prepare write serialization" | "Writeback: Packed encoding" | "Writeback: Value commitments" | "Writeback: Value commitment" | "State: Encode" => (7, None),
        "VM: Rollback commit" | "Writeback: Apply entries" | "State: Write" => (8, Some(2)),
        "Writeback: Dedup SQLite lookup" | "Writeback: Dedup SQLite insert" | "Writeback: PtrHash lookup" | "Writeback: Dedup PtrHash lookup" | "PtrHash: Header verification" | "Writeback: Dedup read-ahead wait" | "State: Dedup" | "Writeback: Dedup and encode" => (9, None),
        "Writeback: MARF insert batch" => (10, None),
        "Seal: Node hashes" => (11, None),
        "Seal: Ancestor hashes" | "State: Ancestor" => (12, None),
        "Flush: Serialize trie" => (13, None),
        "Flush: Store trie" | "Writeback: Append bytes" => (14, None),
        "Commit: Blob sync" | "Writeback: Value file sync" => (15, None),
        "Commit: Blob mapping" | "State: Mapping" => (16, None),
        "VM: Contract load" => (17, None),
        "State: Metadata" => (18, None),
        _ => return None,
    };
    enter(None, cat, op)
}
#[cfg(test)]
mod tests {
    use super::*;
    /// Synthetic ticks prove exclusive cells partition nested read/lookup/decode work.
    #[test]
    fn state_cost_partition_ticks() {
        let mut s = State { start: Instant::now(), last: 0, key: Key::default(), depth: 0, cells: vec![Cell::default(); SIZE].into_boxed_slice() };
        s.tick(10); s.key = Key { phase: 2, op: 1, cat: 1 }; s.tick(30);
        s.key.cat = 2; s.tick(80); s.key.cat = 4; s.tick(100);
        assert_eq!(s.cells.iter().map(|v| v.ns).sum::<u64>(), 100);
        assert_eq!(s.cells[Key { phase: 2, op: 1, cat: 2 }.index()].ns, 50);
    }
    /// Nested scopes restore parents and finish without counting an interval twice.
    #[test]
    fn state_cost_nested_restore() {
        let window = Window::start(true);
        { let _outer = enter_active(Some(2), 1, Some(1));
          { let _inner = enter_active(None, 2, None); }
          STATE.with(|s| { let s = s.borrow(); assert_eq!(s.as_ref().unwrap().key.cat, 1); }); }
        let result = window.finish().unwrap();
        assert_eq!(result.rows.iter().map(|r| r.ns).sum::<u64>(), result.elapsed_ns);
        assert_eq!(result.rows.iter().map(|r| r.calls).sum::<u64>(), 2);
    }
    /// Dropping an unfinished window clears state for the next block.
    #[test]
    fn state_cost_early_return_reset() {
        { let _window = Window::start(true); let _guard = enter_active(None, 1, Some(1)); }
        assert!(Window::start(true).finish().is_some());
    }
    /// Disabled windows emit no measurement and leave no active thread state.
    #[test]
    fn state_cost_disabled() { assert!(Window::start(false).finish().is_none()); }
    /// Nested suppression preserves the outer phase and restores later probes.
    #[test]
    fn state_cost_suppression_nested_restore() {
        let window = Window::start(true);
        {
            let _phase = enter_active(Some(3), 0, Some(0));
            let outer = suppress();
            assert!(enter_active(None, 7, None).is_none());
            { let _inner = suppress(); assert!(enter_active(None, 2, None).is_none()); }
            assert!(enter_active(None, 7, None).is_none());
            drop(outer);
        }
        { let _read = enter_active(Some(2), 1, Some(1)); }
        let snapshot = window.finish().unwrap();
        assert_eq!(snapshot.rows.iter().map(|row| row.calls).sum::<u64>(), 2);
        assert!(snapshot.rows.iter().all(|row| row.category != "encode"));
        assert_eq!(snapshot.rows.iter().map(|row| row.ns).sum::<u64>(), snapshot.elapsed_ns);
    }

    /// Unwinding a suppressed operation restores instrumentation on this thread.
    #[test]
    fn state_cost_suppression_unwind() {
        let window = Window::start(true);
        let result = std::panic::catch_unwind(|| {
            let _suppression = suppress();
            assert!(enter_active(None, 7, None).is_none());
            panic!("test suppressed unwind");
        });
        assert!(result.is_err());
        { let _probe = enter_active(Some(2), 2, Some(1)); }
        let snapshot = window.finish().unwrap();
        assert_eq!(snapshot.rows.iter().map(|row| row.calls).sum::<u64>(), 1);
    }

    /// Suppression does not disable probes on a separate replay thread.
    #[test]
    fn state_cost_suppression_thread_local() {
        let _suppression = suppress();
        let count = std::thread::spawn(|| {
            let window = Window::start(true);
            { let _probe = enter_active(Some(2), 2, Some(1)); }
            window.finish().unwrap().rows.iter().map(|row| row.calls).sum::<u64>()
        }).join().unwrap();
        assert_eq!(count, 1);
    }

}
