//! Compile-time opt-in observation of main-file reads during one dedup query.

use std::cell::Cell;
use std::ffi::c_void;
use std::marker::PhantomData;
use std::ptr;
use std::rc::Rc;
use std::sync::Once;
use std::time::Instant;

use rusqlite::{ffi, Connection};
use stacks_profiler::diagnostics::count;

/// Fixed-size measurements accumulated without allocation in SQLite callbacks.
#[derive(Clone, Copy, Default)]
struct Counts {
    reads: u64,
    bytes: u64,
    nanos: u64,
    fetches: u64,
    mapped: u64,
    errors: u64,
    writes: u64,
    write_bytes: u64,
    write_ns: u64,
    syncs: u64,
    sync_ns: u64,
}

/// Forwarding context belongs to the same thread as the borrowed connection.
#[derive(Clone, Copy)]
struct Context {
    file: *mut ffi::sqlite3_file,
    original: *const ffi::sqlite3_io_methods,
    counts: Counts,
}

thread_local! {
    /// Only the synchronous query holding the guard can access the installed callbacks.
    static CONTEXT: Cell<Option<Context>> = const { Cell::new(None) };
}

/// Restore this connection's private method pointer even when a query returns an error.
pub struct QueryProbe<'a> {
    /// Borrow prevents the connection closing before restoration.
    db: &'a Connection,
    /// Live SQLite file whose methods are temporarily intercepted.
    file: *mut ffi::sqlite3_file,
    /// Original immutable method table, retained by SQLite's VFS.
    original: *const ffi::sqlite3_io_methods,
    /// Owned copy is stable until after restoration.
    _methods: Box<ffi::sqlite3_io_methods>,
    /// Starting pager counters.
    cache: (i32, i32),
    /// Callbacks are thread-local; guard must never move to another thread.
    _local: PhantomData<Rc<()>>,
}

/// Read a connection-local pager counter without resetting it.
fn status(db: &Connection, op: i32) -> i32 {
    let (mut current, mut high) = (0, 0);
    // SAFETY: connection and output pointers remain valid for this synchronous call.
    unsafe { ffi::sqlite3_db_status(db.handle(), op, &mut current, &mut high, 0) };
    current
}

/// Current SQLite pager allocation, excluding mapped bytes and OS cache residency.
pub fn pager_bytes(db: &Connection) -> u64 {
    status(db, ffi::SQLITE_DBSTATUS_CACHE_USED).max(0) as u64
}

impl<'a> QueryProbe<'a> {
    /// Observe one main-file query on the bundled version-3 VFS, if diagnostics are enabled.
    pub fn begin(db: &'a Connection) -> Option<Self> {
        if !stacks_profiler::diagnostics::enabled() || CONTEXT.with(|c| c.get().is_some()) {
            return None;
        }
        static CONFIG: Once = Once::new();
        CONFIG.call_once(|| {
            let mut values = Vec::new();
            for pragma in ["page_size", "cache_size", "mmap_size", "journal_mode"] {
                let value: rusqlite::types::Value = db
                    .query_row(&format!("PRAGMA {pragma}"), [], |r| r.get(0))
                    .unwrap_or(rusqlite::types::Value::Null);
                values.push((pragma, format!("{value:?}")));
            }
            eprintln!("DEDUP_IO_CONFIG {values:?}");
        });
        let mut file: *mut ffi::sqlite3_file = ptr::null_mut();
        // SAFETY: FILE_POINTER writes a borrowed file pointer owned by this connection.
        let rc = unsafe {
            ffi::sqlite3_file_control(
                db.handle(),
                c"main".as_ptr(),
                ffi::SQLITE_FCNTL_FILE_POINTER,
                (&mut file as *mut *mut ffi::sqlite3_file).cast(),
            )
        };
        if rc != ffi::SQLITE_OK || file.is_null() {
            return None;
        }
        // SAFETY: a successful FILE_POINTER identifies a live main database file.
        let original = unsafe { (*file).pMethods };
        if original.is_null() || unsafe { (*original).iVersion } != 3 {
            return None;
        }
        // SAFETY: version 3 includes the entire structure; only our copy is modified.
        let mut methods = Box::new(unsafe { *original });
        if methods.xRead.is_none() {
            return None;
        }
        methods.xRead = Some(read);
        if methods.xWrite.is_some() {methods.xWrite=Some(write);}
        if methods.xSync.is_some() {methods.xSync=Some(sync);}
        if methods.xFetch.is_some() {
            methods.xFetch = Some(fetch);
        }
        let cache = (
            status(db, ffi::SQLITE_DBSTATUS_CACHE_HIT),
            status(db, ffi::SQLITE_DBSTATUS_CACHE_MISS),
        );
        CONTEXT.with(|c| {
            c.set(Some(Context {
                file,
                original,
                counts: Counts::default(),
            }))
        });
        // SAFETY: Connection is not Sync; no SQLite operation can concurrently use this file.
        // The guard owns methods and restores the original pointer before releasing the borrow.
        unsafe {
            (*file).pMethods = &*methods;
        }
        Some(Self {
            db,
            file,
            original,
            _methods: methods,
            cache,
            _local: PhantomData,
        })
    }
}

impl Drop for QueryProbe<'_> {
    fn drop(&mut self) {
        // SAFETY: the borrowed connection is live and our method table is still owned.
        unsafe {
            (*self.file).pMethods = self.original;
        }
        let context = CONTEXT.with(|c| c.replace(None));
        if let Some(context) = context {
            let v = context.counts;
            stacks_profiler::diagnostics::maximum(
                "dedup_pager_peak_bytes",
                status(self.db, ffi::SQLITE_DBSTATUS_CACHE_USED).max(0) as u64,
            );
            count("dedup_io_writes", v.writes);
            count("dedup_io_write_bytes", v.write_bytes);
            count("dedup_io_write_ns", v.write_ns);
            count("dedup_io_syncs", v.syncs);
            count("dedup_io_sync_ns", v.sync_ns);
            count("dedup_io_queries", 1);
            count("dedup_io_reads", v.reads);
            count("dedup_io_bytes", v.bytes);
            count("dedup_io_read_ns", v.nanos);
            count("dedup_io_fetches", v.fetches);
            count("dedup_io_mapped_fetches", v.mapped);
            count("dedup_io_errors", v.errors);
            count(
                "dedup_pager_hits",
                status(self.db, ffi::SQLITE_DBSTATUS_CACHE_HIT).saturating_sub(self.cache.0) as u64,
            );
            count(
                "dedup_pager_misses",
                status(self.db, ffi::SQLITE_DBSTATUS_CACHE_MISS).saturating_sub(self.cache.1)
                    as u64,
            );
            count(
                match v.reads {
                    0 => "dedup_io_queries_read0",
                    1 => "dedup_io_queries_read1",
                    2 => "dedup_io_queries_read2",
                    3 => "dedup_io_queries_read3",
                    4 => "dedup_io_queries_read4",
                    _ => "dedup_io_queries_read5plus",
                },
                1,
            );
        }
    }
}

/// Forward the exact read request and return code; measure VFS time, not physical disk I/O.
unsafe extern "C" fn read(
    file: *mut ffi::sqlite3_file,
    buffer: *mut c_void,
    amount: i32,
    offset: i64,
) -> i32 {
    let Some(mut c) = CONTEXT.with(Cell::get) else {
        return ffi::SQLITE_IOERR;
    };
    if c.file != file {
        return ffi::SQLITE_IOERR;
    }
    let Some(original) = (*c.original).xRead else {
        return ffi::SQLITE_IOERR;
    };
    let start = Instant::now();
    let rc = original(file, buffer, amount, offset);
    c.counts.reads += 1;
    c.counts.bytes += amount.max(0) as u64;
    c.counts.nanos += start.elapsed().as_nanos().min(u64::MAX as u128) as u64;
    c.counts.errors += u64::from(rc != ffi::SQLITE_OK);
    CONTEXT.with(|context| context.set(Some(c)));
    rc
}

/// Forward mapping requests without touching or faulting the returned bytes.
unsafe extern "C" fn fetch(
    file: *mut ffi::sqlite3_file,
    offset: i64,
    amount: i32,
    output: *mut *mut c_void,
) -> i32 {
    let Some(mut c) = CONTEXT.with(Cell::get) else {
        return ffi::SQLITE_IOERR;
    };
    if c.file != file {
        return ffi::SQLITE_IOERR;
    }
    let Some(original) = (*c.original).xFetch else {
        return ffi::SQLITE_IOERR;
    };
    let rc = original(file, offset, amount, output);
    c.counts.fetches += 1;
    c.counts.mapped += u64::from(rc == ffi::SQLITE_OK && !output.is_null() && !(*output).is_null());
    c.counts.errors += u64::from(rc != ffi::SQLITE_OK);
    CONTEXT.with(|context| context.set(Some(c)));
    rc
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real pager reads are forwarded, failed SQL restores the table, and nesting is rejected.
    #[test]
    fn forwards_reads_and_restores_after_errors() {
        if !stacks_profiler::diagnostics::enabled() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let db = Connection::open(dir.path().join("test.sqlite")).unwrap();
        db.execute_batch("PRAGMA mmap_size=0; PRAGMA cache_size=1; CREATE TABLE test(v BLOB); INSERT INTO test VALUES(zeroblob(65536)); PRAGMA shrink_memory").unwrap();
        let probe = QueryProbe::begin(&db).unwrap();
        let original = probe.original;
        let file = probe.file;
        assert!(QueryProbe::begin(&db).is_none());
        let value: Vec<u8> = db
            .query_row("SELECT v FROM test", [], |r| r.get(0))
            .unwrap();
        assert_eq!(value, vec![0; 65536]);
        assert!(CONTEXT.with(Cell::get).unwrap().counts.reads > 0);
        assert!(db.execute_batch("SELECT nonexistent FROM test").is_err());
        drop(probe);
        assert_eq!(unsafe { (*file).pMethods }, original);
        assert!(CONTEXT.with(Cell::get).is_none());
        assert!(QueryProbe::begin(&db).is_some());
    }

    /// Read-only and mapped connections retain their normal results and ownership.
    #[test]
    fn observes_mapped_fetches() {
        if !stacks_profiler::diagnostics::enabled() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mapped.sqlite");
        let db = Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE t(v); INSERT INTO t VALUES(zeroblob(65536))")
            .unwrap();
        drop(db);
        let db =
            Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        db.execute_batch("PRAGMA mmap_size=1048576").unwrap();
        let probe = QueryProbe::begin(&db).unwrap();
        let bytes: Vec<u8> = db.query_row("SELECT v FROM t", [], |r| r.get(0)).unwrap();
        assert_eq!(bytes.len(), 65536);
        let observed = CONTEXT.with(Cell::get).unwrap().counts;
        assert!(observed.reads + observed.mapped > 0);
        drop(probe);
    }
}

/// Forward a main-file write and retain its elapsed time without changing durability.
unsafe extern "C" fn write(file: *mut ffi::sqlite3_file, buffer: *const c_void, amount:i32, offset:i64)->i32 {
    let Some(mut c)=CONTEXT.with(Cell::get) else {return ffi::SQLITE_IOERR;};
    if c.file != file {return ffi::SQLITE_IOERR;}
    let Some(original)=(*c.original).xWrite else {return ffi::SQLITE_IOERR;};
    let start=Instant::now();let rc=original(file,buffer,amount,offset);
    c.counts.writes+=1;c.counts.write_bytes+=amount.max(0) as u64;c.counts.write_ns+=start.elapsed().as_nanos() as u64;c.counts.errors+=u64::from(rc!=ffi::SQLITE_OK);
    CONTEXT.with(|v|v.set(Some(c)));rc
}

/// Forward a main-file sync; WAL/journal files are outside this connection-local probe.
unsafe extern "C" fn sync(file:*mut ffi::sqlite3_file,flags:i32)->i32 {
    let Some(mut c)=CONTEXT.with(Cell::get) else {return ffi::SQLITE_IOERR;};
    if c.file!=file {return ffi::SQLITE_IOERR;}
    let Some(original)=(*c.original).xSync else {return ffi::SQLITE_IOERR;};
    let start=Instant::now();let rc=original(file,flags);c.counts.syncs+=1;c.counts.sync_ns+=start.elapsed().as_nanos() as u64;c.counts.errors+=u64::from(rc!=ffi::SQLITE_OK);
    CONTEXT.with(|v|v.set(Some(c)));rc
}
