//! Opt-in writeback diagnostics for matched replay experiments.
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::env;
use std::sync::OnceLock;

/// Whether this process requested additional transaction diagnostics.
pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| env::var_os("STACKS_WRITEBACK_DIAGNOSTICS").is_some_and(|v| v == "1"))
}

thread_local! {
    /// Fixed-name work totals for the current measured transaction.
    static COUNTS: RefCell<BTreeMap<&'static str, u64>> = const { RefCell::new(BTreeMap::new()) };
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
    }
}

/// Copy fixed-name totals for emission after the transaction timer stops.
pub fn snapshot() -> BTreeMap<&'static str, u64> {
    COUNTS.with(|counts| counts.borrow().clone())
}

/// Enter a diagnostic-only timed span; ordinary timing controls leave it disabled.
#[macro_export]
macro_rules! diagnostic_span {
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
}
