//! Diagnostic-only page locality and residency observations; never touches mapped payloads.
use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::OnceLock;

/// Maximum distinct pages retained per diagnostic category.
const PAGE_LIMIT: usize = 131_072;
/// Process page size, distinct from the SQLite page size.
pub fn page_size() -> usize {
    static SIZE: OnceLock<usize> = OnceLock::new();
    *SIZE.get_or_init(|| {
        // SAFETY: sysconf takes no pointers and returns the OS page size.
        let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        assert!(size > 0);
        size as usize
    })
}

/// Bounded page sets for measured calls on the current replay thread.
#[derive(Default)]
struct Pages {
    /// Slot pages, encoded as partition number and in-partition page.
    slots: HashSet<u64>,
    /// Full-digest envelope pages in the value file.
    headers: HashSet<u64>,
    /// Unique sets reached their diagnostic capacity.
    overflows: u64,
    /// Observed measured candidate queries.
    queries: u64,
}
thread_local! {
    /// Only the execution thread records measured accesses.
    static PAGES: RefCell<Pages> = RefCell::new(Pages::default());
}

/// Whether counters belong to measured, opted-in replay work.
pub fn enabled() -> bool {
    stacks_profiler::diagnostics::enabled() && !stacks_profiler::Profiler::is_suppressed()
}

/// Record a slot access without retaining keys, values, or unbounded per-call events.
pub fn slot(partition: usize, offset: usize) {
    if !enabled() { return; }
    PAGES.with(|p| {
        let mut p=p.borrow_mut(); p.queries+=1;
        let page=((partition as u64)<<32) | (offset/page_size()) as u64;
        if p.slots.len()<PAGE_LIMIT || p.slots.contains(&page) { p.slots.insert(page); } else {p.overflows+=1;}
    });
}

/// Record the pages needed to compare a candidate's complete record envelope.
pub fn header(offset: u64) {
    if !enabled() { return; }
    PAGES.with(|p| {
        let mut p=p.borrow_mut();
        for page in offset/page_size() as u64..=(offset+87)/page_size() as u64 {
            if p.headers.len()<PAGE_LIMIT || p.headers.contains(&page) {p.headers.insert(page);} else {p.overflows+=1;}
        }
    });
}

/// Report resident pages for the mapped byte range, without faulting the bytes into memory.
/// This is a mincore snapshot, not attribution of private process RSS or physical I/O.
pub fn residency(bytes: &[u8]) -> std::io::Result<(usize, usize)> {
    if bytes.is_empty() {return Ok((0,0));}
    let size=page_size(); let address=bytes.as_ptr() as usize; let start=address/size*size;
    let length=bytes.len()+address-start; let pages=length.div_ceil(size);
    let mut result=vec![0u8;pages];
    // SAFETY: the byte owner keeps this mapping live; vector has one entry per queried page.
    let rc=unsafe {libc::mincore(start as *mut libc::c_void,length,result.as_mut_ptr().cast())};
    if rc!=0 {return Err(std::io::Error::last_os_error());}
    Ok((pages,result.iter().filter(|v| **v & 1 != 0).count()))
}

/// Emit fixed-size aggregates at the end of a base handle's lifetime.
pub fn report_pages() {
    PAGES.with(|p| {let p=p.borrow();eprintln!("PTRHASH_PAGES {}",serde_json::json!({"queries":p.queries,"slot_pages":p.slots.len(),"header_pages":p.headers.len(),"page_bytes":page_size(),"overflow_observations":p.overflows}));});
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Residency sampling accepts an empty slice and a live page-aligned mapping.
    #[test]
    fn residency_of_live_mapping() {
        assert_eq!(residency(&[]).unwrap(),(0,0));
        let mut map=memmap2::MmapOptions::new().len(page_size()*2).map_anon().unwrap();map[0]=9;
        let (pages,resident)=residency(&map).unwrap();assert_eq!(pages,2);assert!(resident>=1 && resident<=2);
    }
}
