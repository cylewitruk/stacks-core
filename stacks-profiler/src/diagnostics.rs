//! Opt-in writeback diagnostics for matched replay experiments.
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::env;
use std::sync::OnceLock;
use std::time::Instant;

/// Whether this process requested additional transaction diagnostics.
pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| env::var_os("STACKS_WRITEBACK_DIAGNOSTICS").is_some_and(|v| v == "1"))
}

thread_local! {
    /// Fixed-name work totals for the current measured transaction.
    static COUNTS: RefCell<BTreeMap<&'static str, u64>> = const { RefCell::new(BTreeMap::new()) };
    /// Inclusive, opt-in wall clocks for the current measured transaction.
    static WALL_NANOS: RefCell<BTreeMap<&'static str, u64>> = const { RefCell::new(BTreeMap::new()) };
    /// Number of observed calls underlying each extrapolated wall estimate.
    static WALL_SAMPLES: RefCell<BTreeMap<&'static str, u64>> = const { RefCell::new(BTreeMap::new()) };
    /// Cheap deterministic per-thread selector for sparse hot-path clocks.
    static SAMPLE_STATE: Cell<u32> = const { Cell::new(0x9e37_79b9) };
}

/// Inclusive wall clock for one named operation in an opt-in diagnostic run.
pub struct WallClock {
    name: &'static str,
    started: Option<Instant>,
    weight: u32,
}

impl Drop for WallClock {
    fn drop(&mut self) {
        if let Some(started) = self.started {
            let elapsed = started
                .elapsed()
                .as_nanos()
                .saturating_mul(u128::from(self.weight))
                .min(u128::from(u64::MAX)) as u64;
            WALL_NANOS.with(|totals| {
                let mut totals = totals.borrow_mut();
                let total = totals.entry(self.name).or_default();
                *total = total.saturating_add(elapsed);
            });
            WALL_SAMPLES.with(|samples| {
                let mut samples = samples.borrow_mut();
                *samples.entry(self.name).or_default() += 1;
            });
        }
    }
}

/// Start a named wall clock only when diagnostic collection is enabled.
#[inline]
pub fn wall_clock(name: &'static str) -> WallClock {
    WallClock {
        name,
        started: (enabled() && !crate::Profiler::is_suppressed()).then(Instant::now),
        weight: 1,
    }
}

/// Sample roughly one in `rate` calls and extrapolate its inclusive wall time.
/// `rate` must be a power of two; sample counts accompany estimates for review.
#[inline]
pub fn wall_clock_sampled(name: &'static str, rate: u32) -> WallClock {
    assert!(rate.is_power_of_two());
    let started = if enabled() && !crate::Profiler::is_suppressed() {
        SAMPLE_STATE.with(|state| {
            let mut next = state.get();
            next ^= next << 13;
            next ^= next >> 17;
            next ^= next << 5;
            state.set(next);
            (next & (rate - 1) == 0).then(Instant::now)
        })
    } else {
        None
    };
    WallClock {
        name,
        started,
        weight: rate,
    }
}

/// Add work without recording keys, values or per-element events.
#[inline]
pub fn count(name: &'static str, amount: u64) {
    if enabled() && !crate::Profiler::is_suppressed() {
        COUNTS.with(|counts| {
            let mut counts = counts.borrow_mut();
            let value = counts.entry(name).or_default();
            *value = value.saturating_add(amount);
        });
    }
}

/// Retain a high-water measurement for the current measured transaction.
pub fn maximum(name: &'static str, amount: u64) {
    if enabled() && !crate::Profiler::is_suppressed() {
        COUNTS.with(|counts| {
            let mut counts = counts.borrow_mut();
            let value = counts.entry(name).or_default();
            *value = (*value).max(amount);
        });
    }
}

/// Clear totals at a transaction boundary outside its timer.
pub fn reset() {
    if enabled() {
        COUNTS.with(|counts| counts.borrow_mut().clear());
        WALL_NANOS.with(|totals| totals.borrow_mut().clear());
        WALL_SAMPLES.with(|samples| samples.borrow_mut().clear());
    }
}

/// Copy fixed-name totals for emission after the transaction timer stops.
pub fn snapshot() -> BTreeMap<&'static str, u64> {
    COUNTS.with(|counts| counts.borrow().clone())
}

/// Return inclusive named wall nanoseconds for the current transaction.
pub fn wall_snapshot() -> BTreeMap<&'static str, u64> {
    WALL_NANOS.with(|totals| totals.borrow().clone())
}

/// Return the number of actual observations behind sampled wall estimates.
pub fn wall_sample_snapshot() -> BTreeMap<&'static str, u64> {
    WALL_SAMPLES.with(|samples| samples.borrow().clone())
}

/// Enter a diagnostic-only timed span; ordinary timing controls leave it disabled.
#[macro_export]
macro_rules! diagnostic_span {
    ($name:literal, rate: $rate:literal) => {{
        if $crate::diagnostics::enabled() {
            $crate::span!($name, rate: $rate)
        } else {
            None
        }
    }};
    ($name:literal) => {{
        if $crate::diagnostics::enabled() {
            $crate::span!($name)
        } else {
            None
        }
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_respect_suppression_and_reset() {
        if !enabled() {
            return;
        }
        reset();
        count("test", 3);
        {
            let _guard = crate::Profiler::begin_suppression();
            count("test", 9);
        }
        assert_eq!(snapshot().get("test"), Some(&3));
        reset();
        assert!(snapshot().is_empty());
    }

    #[test]
    fn wall_clocks_respect_suppression_and_reset() {
        if !enabled() {
            return;
        }
        reset();
        {
            let _clock = wall_clock("test");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(wall_snapshot().get("test").copied().unwrap_or_default() > 0);
        {
            let _suppression = crate::Profiler::begin_suppression();
            let _clock = wall_clock("suppressed");
        }
        assert!(!wall_snapshot().contains_key("suppressed"));
        reset();
        assert!(wall_snapshot().is_empty());
    }

    #[test]
    fn sampled_clocks_report_observations_with_extrapolated_time() {
        if !enabled() {
            return;
        }
        reset();
        for _ in 0..4096 {
            let _clock = wall_clock_sampled("sampled", 256);
        }
        let samples = wall_sample_snapshot()
            .get("sampled")
            .copied()
            .unwrap_or_default();
        assert!(samples > 0 && samples < 128);
        assert!(wall_snapshot().get("sampled").copied().unwrap_or_default() > 0);
        reset();
        assert!(wall_sample_snapshot().is_empty());
    }
}
