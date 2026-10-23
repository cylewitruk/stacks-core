// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020-2026 Stacks Open Internet Foundation
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

//! Statically dispatched Stacks diagnostics. Span guards own profiler state.
use std::fmt;
#[cfg(feature = "profiler")]
use std::sync::OnceLock;
use std::time::Duration;

#[cfg(feature = "profiler")]
use marf_core::observe::{OPERATION_COUNT, Record};
use marf_core::observe::{Observer, Operation};
use stacks_common::util::db;
#[cfg(feature = "profiler")]
use stacks_profiler::{ProfileGuard, Profiler, SpanId};

#[cfg(feature = "profiler")]
use crate::profiler;

#[cfg(feature = "profiler")]
static SPANS: [OnceLock<SpanId>; OPERATION_COUNT] = [const { OnceLock::new() }; OPERATION_COUNT];
/// Integration observer selected by the facade's concrete engine.
pub struct IntegrationObserver;
impl Observer for IntegrationObserver {
    const ENABLED: bool = cfg!(feature = "profiler");
    #[cfg(feature = "profiler")]
    type Span = Option<ProfileGuard>;
    #[cfg(not(feature = "profiler"))]
    type Span = ();
    #[cfg(feature = "profiler")]
    fn is_suppressed() -> bool {
        Profiler::is_suppressed()
    }
    #[cfg(feature = "profiler")]
    fn enter(operation: Operation, timed: bool) -> Self::Span {
        let id = SPANS[operation.index()].get_or_init(|| {
            Profiler::new_span_id(operation.name()).with_context(operation.context())
        });
        Some(if timed {
            Profiler::begin_timed_span(id, None)
        } else {
            Profiler::begin_count_only_span(id, None)
        })
    }
    #[cfg(not(feature = "profiler"))]
    #[inline(always)]
    fn enter(_: Operation, _: bool) {}
    #[cfg(feature = "profiler")]
    fn record(record: Record<'_>) {
        match record {
            Record::Key(key) => {
                stacks_profiler::record_if!(profiler::capture_marf_keys(), "KEY", key)
            }
            Record::BlockHash(hash) => {
                stacks_profiler::record_if!(profiler::capture_marf_hash_lookups(), "BH", hash)
            }
            Record::DiskRead(id) => stacks_profiler::record_if!(
                profiler::capture_marf_disk_reads(),
                "MARF_NODE_HASH_DISK_READ",
                u64::from(id)
            ),
        }
    }
    fn sqlite_locked(connection: fmt::Arguments<'_>) {
        db::update_lock_table_formatted(connection);
    }
    fn sqlite_busy(run_count: u32, sleep_ms: u64) {
        db::log_sqlite_busy(run_count, sleep_ms);
    }
    fn sqlite_profile(query: &str, elapsed: Duration) {
        let obj = serde_json::json!({"millis":elapsed.as_millis(),"query":query});
        debug!("sqlite trace profile {}", obj.to_string());
    }
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::process::Command;
    use std::time::{Duration, Instant};

    use marf_core::sqlite;
    use rusqlite::Connection;
    use stacks_common::types::chainstate::StacksBlockId;
    use stacks_common::util::db;

    use super::IntegrationObserver;
    use crate::{MARF, MARFOpenOpts};

    /// Both retry paths report the same node-wide lock holders and connection keys.
    #[test]
    fn shared_lock_diagnostics_preserve_connection_keys_and_retry_policy() {
        const CHILD: &str = "MARF_SHARED_LOCK_DIAGNOSTICS_CHILD";
        const NAME: &str =
            "observer::tests::shared_lock_diagnostics_preserve_connection_keys_and_retry_policy";
        if env::var_os(CHILD).is_some() {
            let dir = tempfile::tempdir().unwrap();
            let mut ordinary = Connection::open(dir.path().join("ordinary.sqlite")).unwrap();
            let ordinary_tx = db::tx_begin_immediate(&mut ordinary).unwrap();
            let ordinary_connection: &Connection = &ordinary_tx;
            eprintln!("EXPECTED_KEY:{ordinary_connection:?}");
            let mut marf = MARF::<StacksBlockId>::from_path(
                dir.path().join("marf.sqlite"),
                MARFOpenOpts::default(),
            )
            .unwrap();
            let marf_tx = marf.begin_tx().unwrap();
            let marf_connection: &Connection = marf_tx.sqlite_tx();
            eprintln!("EXPECTED_KEY:{marf_connection:?}");
            eprintln!("COMMON_RETRY_BEGIN");
            let started = Instant::now();
            assert!(db::tx_busy_handler(3000));
            assert!(started.elapsed() >= Duration::from_millis(90));
            eprintln!("COMMON_RETRY_END");
            eprintln!("MARF_RETRY_BEGIN");
            let started = Instant::now();
            assert!(sqlite::tx_busy_handler::<IntegrationObserver>(3000));
            assert!(started.elapsed() >= Duration::from_millis(90));
            eprintln!("MARF_RETRY_END");
            return;
        }
        let output = Command::new(env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture"])
            .env(CHILD, "1")
            .env("STACKS_LOG_CRITONLY", "0")
            .env("STACKS_LOG_JSON", "0")
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.status.success(), "{text}");
        let keys: Vec<_> = text
            .lines()
            .filter_map(|line| line.strip_prefix("EXPECTED_KEY:"))
            .collect();
        assert_eq!(keys.len(), 2, "{text}");
        for key in keys {
            assert!(key.starts_with("Connection"), "{key}");
            assert_eq!(
                text.matches(&format!("Database '{key}' last locked by"))
                    .count(),
                2,
                "both retry dumps must include both lock holders: {text}"
            );
        }
        assert_eq!(text.matches("Deadlock detected.").count(), 2, "{text}");
        let sleeps: Vec<_> = text
            .lines()
            .filter_map(|line| line.split_once("Database is locked; sleeping "))
            .map(|(_, tail)| tail.split_once("ms").unwrap().0.parse::<u64>().unwrap())
            .collect();
        assert_eq!(sleeps.len(), 2, "{text}");
        assert!(sleeps.iter().all(|ms| (90..110).contains(ms)), "{text}");
        println!("{text}");
    }
}
