//! Bounded speculative index reads. Results never replace the writer's authoritative lookup.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use rusqlite::{Connection, OpenFlags, OptionalExtension};

/// Maximum look-ahead keys in a work item (1,280 bytes).
pub const WINDOW: usize = 32;

/// Observation-only totals returned once the reader releases its last statement.
#[derive(Debug, Default)]
pub struct Statistics {
    /// Queries attempted, including absent values.
    pub queries: u64,
    /// Failed speculative queries; the writer still determines authoritative errors.
    pub errors: u64,
    /// Elapsed worker time, overlapping the writer's first half-window.
    pub nanos: u64,
    /// Query-window sample of the worker's SQLite pager allocation.
    #[cfg(feature = "dedup-io-diagnostics")]
    pub pager_bytes: u64,
}

/// One bounded request and its completion barrier.
struct Request {
    /// Full content hashes used only to warm file pages.
    keys: Vec<[u8; 40]>,
    /// Return after all statements release read locks.
    done: SyncSender<Statistics>,
}

/// Persistent single reader, owned by the shared extent store across related reopens.
#[derive(Debug)]
pub struct ReadAhead {
    /// At most one queued request in addition to the active request.
    sender: Option<SyncSender<Request>>,
    /// Joined before the extent store is released.
    worker: Option<JoinHandle<()>>,
}

/// A completion barrier that also drains on early return or unwinding.
pub struct Flight {
    /// At most one completion record; carries no index result or value bytes.
    done: Option<Receiver<Statistics>>,
}

impl ReadAhead {
    /// Spawn one read-only worker with a 2 MiB pager target and 256 KiB stack.
    pub fn open(path: PathBuf) -> std::io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel::<Request>(1);
        let worker = thread::Builder::new()
            .name("clarity-dedup-prefetch".into())
            .stack_size(256 * 1024)
            .spawn(move || {
                let db = Connection::open_with_flags(
                    path,
                    OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
                )
                .and_then(|db| {
                    db.pragma_update(None, "cache_size", -2048)?;
                    db.pragma_update(None, "mmap_size", 0)?;
                    db.busy_timeout(std::time::Duration::ZERO)?;
                    Ok(db)
                });
                while let Ok(request) = receiver.recv() {
                    let start = Instant::now();
                    let mut stats = Statistics::default();
                    if let Ok(db) = &db {
                        if let Ok(mut stmt) = db.prepare_cached(
                            "SELECT offset,length FROM clarity_extent_index WHERE hash=?1",
                        ) {
                            for key in request.keys {
                                let result = stmt
                                    .query_row([key.as_slice()], |r| {
                                        Ok((r.get::<_, u64>(0)?, r.get::<_, u64>(1)?))
                                    })
                                    .optional();
                                stats.queries += 1;
                                stats.errors += u64::from(result.is_err());
                            }
                        } else {
                            stats.errors += 1;
                        }
                    } else {
                        stats.errors += 1;
                    }
                    #[cfg(feature = "dedup-io-diagnostics")]
                    if let Ok(db) = &db {
                        stats.pager_bytes = super::io_probe::pager_bytes(db);
                    }
                    stats.nanos = start.elapsed().as_nanos().min(u64::MAX as u128) as u64;
                    // All rows and statements have been dropped before publishing completion.
                    let _ = request.done.send(stats);
                }
            })?;
        Ok(Self {
            sender: Some(sender),
            worker: Some(worker),
        })
    }

    /// Start a bounded speculative window, without retaining a SQLite read transaction.
    pub fn start(&self, keys: &[[u8; 40]]) -> Option<Flight> {
        if keys.is_empty() || keys.len() > WINDOW {
            return None;
        }
        let (sender, done) = mpsc::sync_channel(1);
        self.sender
            .as_ref()?
            .send(Request {
                keys: keys.to_vec(),
                done: sender,
            })
            .ok()?;
        Some(Flight { done: Some(done) })
    }
}

impl Drop for ReadAhead {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Flight {
    /// Wait until speculative statements release their read locks; failures are advisory only.
    pub fn finish(mut self) -> Statistics {
        self.wait()
    }

    /// Drain once, including on an early authoritative lookup error.
    fn wait(&mut self) -> Statistics {
        self.done
            .take()
            .and_then(|r| r.recv().ok())
            .unwrap_or_default()
    }
}

impl Drop for Flight {
    fn drop(&mut self) {
        self.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Missing/uncommitted entries and rollback cannot turn prefetch into authoritative data.
    #[test]
    fn speculative_reads_release_locks_and_observe_no_uncommitted_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.sqlite");
        let db = Connection::open(&path).unwrap();
        db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE clarity_extent_index(hash BLOB PRIMARY KEY, offset INTEGER, length INTEGER) WITHOUT ROWID;").unwrap();
        let worker = ReadAhead::open(path).unwrap();
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        db.execute(
            "INSERT INTO clarity_extent_index VALUES(?1,48,99)",
            [[1_u8; 40].as_slice()],
        )
        .unwrap();
        let stats = worker.start(&[[1; 40], [2; 40]]).unwrap().finish();
        assert_eq!(stats.queries, 2);
        assert_eq!(stats.errors, 0);
        db.execute_batch("ROLLBACK; BEGIN IMMEDIATE").unwrap();
        // Early-return cleanup must release any active statement before a checkpoint/commit.
        drop(worker.start(&[[2; 40]]).unwrap());
        db.execute_batch("COMMIT; PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        let count: u64 = db
            .query_row("SELECT count(*) FROM clarity_extent_index", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 0);
        assert!(worker.start(&[[0; 40]; WINDOW + 1]).is_none());
    }

    /// Speculative storage failures are returned as diagnostics, never invented positive hits.
    #[test]
    fn unavailable_reader_is_advisory_and_joined() {
        let worker =
            ReadAhead::open(PathBuf::from("/does-not-exist/readonly-index.sqlite")).unwrap();
        let stats = worker.start(&[[1; 40]]).unwrap().finish();
        assert_eq!(stats.errors, 1);
        drop(worker);
    }
}
