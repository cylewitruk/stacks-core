//! Read-only file mappings with a stable, incrementally backed virtual range.

use std::fs::File;
use std::io;
use std::ops::Deref;
use std::sync::Arc;
#[cfg(all(unix, target_pointer_width = "64"))]
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Mutex,
};

use memmap2::{Mmap, MmapOptions};
#[cfg(all(unix, target_pointer_width = "64"))]
use nix::libc;
#[cfg(all(unix, target_pointer_width = "64"))]
use std::os::fd::AsRawFd;
#[cfg(all(unix, target_pointer_width = "64"))]
use std::{ptr, slice};

/// Read-only mapping of an append-only file, with a conventional mapping fallback.
#[derive(Clone, Debug)]
pub enum FileMapping {
    /// Contiguous reservation whose complete file pages are mapped incrementally.
    #[cfg(all(unix, target_pointer_width = "64"))]
    Stable(Arc<ReservedMapping>),
    /// Mapping used when reservations are unavailable or exhausted.
    Conventional(Arc<Mmap>),
}

impl FileMapping {
    /// Map an immutable file prefix, retaining room for future appends where supported.
    ///
    /// # Safety
    /// The file must not be truncated or modify bytes borrowed from this mapping.
    pub unsafe fn map(file: &File) -> io::Result<Self> {
        unsafe { Self::map_prefix(file, file.metadata()?.len()) }
    }

    /// Map only a published prefix; an unpublished suffix may subsequently be overwritten.
    ///
    /// # Safety
    /// Bytes below `len` must remain immutable and the file must not shrink below `len`.
    pub unsafe fn map_prefix(file: &File, len: u64) -> io::Result<Self> {
        if len > file.metadata()?.len() {
            return Err(io::Error::other("invalid published mapping length"));
        }
        #[cfg(all(unix, target_pointer_width = "64"))]
        if let Some(capacity) = reservation_capacity(len) {
            if let Ok(mapping) = unsafe { ReservedMapping::new_prefix(file, capacity, len) } {
                return Ok(Self::Stable(Arc::new(mapping)));
            }
        }
        let len = usize::try_from(len).map_err(|_| io::Error::other("mapping too large"))?;
        // SAFETY: Only the caller's immutable published prefix is exposed.
        unsafe {
            MmapOptions::new()
                .len(len)
                .map(file)
                .map(Arc::new)
                .map(Self::Conventional)
        }
    }

    /// Extend coverage after append; already backed pages retain their mappings.
    ///
    /// # Safety
    /// The file must be the original mapped file, without truncation or mutation
    /// of previously borrowed bytes. Shared views may retain slices of the immutable prefix.
    pub unsafe fn refresh(&mut self, file: &File) -> io::Result<()> {
        unsafe { self.refresh_prefix(file, file.metadata()?.len()) }
    }

    /// Extend coverage through a committed prefix without mapping an unpublished suffix.
    ///
    /// # Safety
    /// The same file and immutable-prefix requirements as `map_prefix` apply.
    pub unsafe fn refresh_prefix(&mut self, file: &File, len: u64) -> io::Result<()> {
        if len > file.metadata()?.len() || len < self.len() as u64 {
            return Err(io::Error::other("invalid published mapping length"));
        }
        #[cfg(all(unix, target_pointer_width = "64"))]
        if let Self::Stable(mapping) = self {
            if unsafe { mapping.extend_prefix(file, len) }.is_ok() {
                return Ok(());
            }
        }
        let len = usize::try_from(len).map_err(|_| io::Error::other("mapping too large"))?;
        // SAFETY: Old views keep their mapping; only the immutable prefix is exposed.
        let replacement = unsafe { MmapOptions::new().len(len).map(file)? };
        *self = Self::Conventional(Arc::new(replacement));
        Ok(())
    }
}

impl Deref for FileMapping {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            #[cfg(all(unix, target_pointer_width = "64"))]
            Self::Stable(mapping) => mapping.bytes(),
            Self::Conventional(mapping) => mapping,
        }
    }
}

/// Reserve a power-of-two range with at least 12.5% or 64 MiB of growth headroom.
#[cfg(all(unix, target_pointer_width = "64"))]
fn reservation_capacity(file_len: u64) -> Option<usize> {
    let len = usize::try_from(file_len).ok()?;
    let headroom = (len / 8).max(64 << 20);
    len.checked_add(headroom)?
        .checked_next_power_of_two()
        .filter(|capacity| *capacity <= isize::MAX as usize)
}

/// Owns a page-aligned reservation and the file-backed prefix inside it.
#[cfg(all(unix, target_pointer_width = "64"))]
#[derive(Debug)]
pub struct ReservedMapping {
    /// Base of the reservation, including the inaccessible suffix.
    base: *mut libc::c_void,
    /// Total reserved virtual address space, in bytes.
    capacity: usize,
    /// Number of file-backed bytes; always a multiple of the host page size.
    mapped_len: AtomicUsize,
    /// Serializes backing of the unexposed suffix; readers never take this lock.
    extension: Mutex<()>,
    /// Host page size, used for both addresses and file offsets.
    page_size: usize,
}

// SAFETY: Ownership transfers do not move the reservation; extension is serialized.
#[cfg(all(unix, target_pointer_width = "64"))]
unsafe impl Send for ReservedMapping {}
// SAFETY: Extension touches only unexposed pages and publishes their length atomically.
// Existing slices remain immutable, and Arc ownership prevents unmapping while borrowed.
#[cfg(all(unix, target_pointer_width = "64"))]
unsafe impl Sync for ReservedMapping {}

#[cfg(all(unix, target_pointer_width = "64"))]
impl ReservedMapping {
    /// Reserve address space and map only complete file pages.
    ///
    /// # Safety
    /// The file must remain untruncated and its exposed bytes immutable.
    #[cfg(test)]
    unsafe fn new(file: &File, capacity: usize) -> io::Result<Self> {
        unsafe { Self::new_prefix(file, capacity, file.metadata()?.len()) }
    }

    /// Reserve address space for an immutable published prefix.
    unsafe fn new_prefix(file: &File, capacity: usize, len: u64) -> io::Result<Self> {
        // SAFETY: sysconf has no pointer arguments.
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if page_size <= 0 {
            return Err(io::Error::last_os_error());
        }
        let page_size = page_size as usize;
        if capacity == 0 || capacity > isize::MAX as usize || capacity % page_size != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid mapping reservation",
            ));
        }
        if len > file.metadata()?.len() || len > capacity as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "file exceeds mapping reservation",
            ));
        }
        // SAFETY: The OS selects an unused range. No backing pages are touched or exposed.
        let base = unsafe {
            libc::mmap(
                ptr::null_mut(),
                capacity,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        if base.is_null() {
            // Rust slices require non-null pointers even for empty mappings.
            unsafe {
                libc::munmap(base, capacity);
            }
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "null mapping reservation",
            ));
        }
        let mapping = Self {
            base,
            capacity,
            mapped_len: AtomicUsize::new(0),
            extension: Mutex::new(()),
            page_size,
        };
        // SAFETY: This reservation is exclusively owned and the file contract is inherited.
        unsafe {
            mapping.extend_prefix(file, len)?;
        }
        Ok(mapping)
    }

    /// Back additional complete pages, leaving the previous mapped prefix intact.
    ///
    /// # Safety
    /// The supplied file must be the original untruncated mapping source.
    #[cfg(test)]
    unsafe fn extend(&self, file: &File) -> io::Result<()> {
        unsafe { self.extend_prefix(file, file.metadata()?.len()) }
    }

    /// Map complete pages only up to the caller's committed boundary.
    unsafe fn extend_prefix(&self, file: &File, len: u64) -> io::Result<()> {
        let _extension = self
            .extension
            .lock()
            .map_err(|_| io::Error::other("mapping extension lock poisoned"))?;
        let mapped_len = self.mapped_len.load(Ordering::Relaxed);
        let file_len = usize::try_from(len).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "file length exceeds address space",
            )
        })?;
        if len > file.metadata()?.len() || file_len < mapped_len || file_len > self.capacity {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "file outside mapping reservation",
            ));
        }
        // Keep the partial EOF page on the positioned-read path: appended bytes
        // in a pre-existing EOF mapping have platform-dependent visibility.
        let end = file_len / self.page_size * self.page_size;
        if end == mapped_len {
            return Ok(());
        }
        // SAFETY: This page-aligned interval is entirely within our own inaccessible
        // reservation and entirely backed by the file. Existing file pages are untouched.
        let address = unsafe { self.base.cast::<u8>().add(mapped_len).cast() };
        let mapped = unsafe {
            libc::mmap(
                address,
                end - mapped_len,
                libc::PROT_READ,
                libc::MAP_SHARED | libc::MAP_FIXED,
                file.as_raw_fd(),
                mapped_len as libc::off_t,
            )
        };
        if mapped == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        self.mapped_len.store(end, Ordering::Release);
        Ok(())
    }

    /// Borrow only the currently file-backed complete-page prefix.
    fn bytes(&self) -> &[u8] {
        // SAFETY: The owned reservation is contiguous, read-only over mapped_len,
        // and constrained below isize::MAX. Acquire observes completed backing operations;
        // extension never replaces these pages, and this borrow keeps the owner alive.
        unsafe { slice::from_raw_parts(self.base.cast(), self.mapped_len.load(Ordering::Acquire)) }
    }
}

#[cfg(all(unix, target_pointer_width = "64"))]
impl Drop for ReservedMapping {
    fn drop(&mut self) {
        // SAFETY: This object owns the entire original reservation, including its
        // file-backed prefix. All Rust borrows have ended before destruction.
        unsafe {
            libc::munmap(self.base, self.capacity);
        }
    }
}

#[cfg(all(test, unix, target_pointer_width = "64"))]
mod tests {
    use super::*;
    use std::os::unix::fs::FileExt;

    /// Appending within/across pages preserves old addresses and exposes only complete pages.
    #[test]
    fn growing_mapping_preserves_prefix_and_bounds() {
        let file = tempfile::tempfile().unwrap();
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        file.write_all_at(&vec![1; page], 0).unwrap();
        let mapping = unsafe { ReservedMapping::new(&file, page * 8).unwrap() };
        let base = mapping.base;
        assert_eq!(mapping.bytes(), vec![1; page]);
        file.write_all_at(&vec![2; page - 1], page as u64).unwrap();
        unsafe {
            mapping.extend(&file).unwrap();
        }
        assert_eq!(mapping.bytes().len(), page);
        file.write_all_at(&[3], (2 * page - 1) as u64).unwrap();
        unsafe {
            mapping.extend(&file).unwrap();
        }
        assert_eq!(mapping.base, base);
        assert_eq!(mapping.bytes().len(), 2 * page);
        assert_eq!(&mapping.bytes()[..page], vec![1; page]);
        assert_eq!(&mapping.bytes()[page..2 * page - 1], vec![2; page - 1]);
        assert_eq!(mapping.bytes()[2 * page - 1], 3);
        unsafe {
            mapping.extend(&file).unwrap();
        }
        assert_eq!(mapping.base, base);
    }

    /// A physically present unpublished suffix is never mapped, even across full pages.
    #[test]
    fn published_prefix_excludes_reusable_suffix() {
        let file = tempfile::tempfile().unwrap();
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        file.write_all_at(&vec![1; 4 * page], 0).unwrap();
        let mut map = unsafe { FileMapping::map_prefix(&file, (page + 64) as u64).unwrap() };
        let reader = map.clone();
        let retained = &reader[..page];
        let address = retained.as_ptr();
        assert_eq!(map.len(), page);
        file.write_all_at(&vec![2; page], (2 * page) as u64)
            .unwrap();
        unsafe {
            map.refresh_prefix(&file, (3 * page) as u64).unwrap();
        }
        assert_eq!(map.as_ptr(), address);
        assert_eq!(retained, vec![1; page]);
        assert_eq!(&map[2 * page..], vec![2; page]);
        assert_eq!(map.len(), 3 * page);
    }

    /// Empty mappings, reservation exhaustion, and fresh maps remain readable.
    #[test]
    fn empty_mapping_and_exhaustion_fallback() {
        let file = tempfile::tempfile().unwrap();
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        let reserved = unsafe { ReservedMapping::new(&file, page).unwrap() };
        assert!(reserved.bytes().is_empty());
        let mut mapping = FileMapping::Stable(Arc::new(reserved));
        file.write_all_at(&vec![7; 2 * page + 3], 0).unwrap();
        unsafe {
            mapping.refresh(&file).unwrap();
        }
        std::assert_matches!(mapping, FileMapping::Conventional(_));
        assert_eq!(&*mapping, vec![7; 2 * page + 3]);
        assert!(unsafe { ReservedMapping::new(&file, page) }.is_err());
    }
    /// Capacity tracks file size without overflowing the slice address-space limit.
    #[test]
    fn reservation_headroom_is_bounded() {
        assert_eq!(reservation_capacity(0), Some(64 << 20));
        assert_eq!(reservation_capacity(233_691_445_978), Some(256usize << 30));
        assert_eq!(reservation_capacity(256u64 << 30), Some(512usize << 30));
        assert_eq!(reservation_capacity(u64::MAX), None);
        assert_eq!(reservation_capacity(isize::MAX as u64), None);
    }

    /// Concurrent extension publishes complete pages while existing slices remain readable.
    #[test]
    fn shared_mapping_survives_growth_and_parent_drop() {
        let file = tempfile::tempfile().unwrap();
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        file.write_all_at(&vec![3; page], 0).unwrap();
        let mapping = Arc::new(unsafe { ReservedMapping::new(&file, 64 * page).unwrap() });
        let reader = Arc::clone(&mapping);
        let retained = reader.bytes();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for n in 1..32 {
                    file.write_all_at(&vec![n as u8; page], (n * page) as u64)
                        .unwrap();
                    unsafe {
                        mapping.extend(&file).unwrap();
                    }
                }
            });
            scope.spawn(|| {
                for _ in 0..1000 {
                    let bytes = reader.bytes();
                    assert_eq!(bytes.len() % page, 0);
                    assert_eq!(&bytes[..page], retained);
                    for (n, chunk) in bytes.chunks_exact(page).enumerate().skip(1) {
                        assert!(chunk.iter().all(|b| *b == n as u8));
                    }
                }
            });
        });
        assert_eq!(reader.bytes().len(), 32 * page);
        let weak = Arc::downgrade(&mapping);
        drop(mapping);
        assert!(weak.upgrade().is_some());
        assert_eq!(retained, vec![3; page]);
        drop(reader);
        assert!(weak.upgrade().is_none());
    }

    /// A fallback replaces only the refreshing view, leaving borrowed sibling pages alive.
    #[test]
    fn exhausted_mapping_keeps_shared_reader_alive() {
        let file = tempfile::tempfile().unwrap();
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        file.write_all_at(&vec![1; page], 0).unwrap();
        let mut writer = FileMapping::Stable(Arc::new(unsafe {
            ReservedMapping::new(&file, page).unwrap()
        }));
        let reader = writer.clone();
        let retained = &reader[..];
        file.write_all_at(&vec![2; page], page as u64).unwrap();
        unsafe {
            writer.refresh(&file).unwrap();
        }
        std::assert_matches!(writer, FileMapping::Conventional(_));
        assert_eq!(&writer[page..], vec![2; page]);
        drop(writer);
        assert_eq!(retained, vec![1; page]);
    }
}
