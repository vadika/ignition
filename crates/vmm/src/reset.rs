//! In-memory reset-to-checkpoint: an immutable RAM image plus the saved
//! vcpu/GIC/device state, and the pure helpers that roll live RAM back to it.

/// Copy the entire pristine image over live RAM. Used when no dirty tracker is
/// armed, so every page may have changed.
pub fn rollback_full(pristine: &[u8], live: &mut [u8]) {
    debug_assert_eq!(pristine.len(), live.len(), "pristine and live RAM must match in size");
    live.copy_from_slice(pristine);
}

/// Copy only the listed pages from the pristine image back over live RAM.
/// `page` is the tracking granule (`crate::dirty::PAGE`). Indices past the end
/// of RAM are skipped (defensive — `drain()` never emits them).
pub fn rollback_pages(pristine: &[u8], live: &mut [u8], pages: &[u64], page: usize) {
    debug_assert_eq!(pristine.len(), live.len(), "pristine and live RAM must match in size");
    for &p in pages {
        let start = (p as usize) * page;
        if start >= live.len() {
            continue;
        }
        let end = (start + page).min(live.len());
        live[start..end].copy_from_slice(&pristine[start..end]);
    }
}

use std::io;
use std::os::unix::io::AsRawFd;
use std::path::Path;

use crate::device_manager::DeviceRecord;
use crate::snapshot::VcpuCheckpoint;

/// An in-memory checkpoint the live guest can be rolled back to in place.
/// One at a time; `Ctrl-A c` replaces it. Seeded automatically on `--restore`.
pub struct ResetPoint {
    /// Immutable RAM image (clonefile RO-mmap, or owned copy on fresh boot).
    pub pristine: PristineRam,
    /// Per-vCPU registers/ICC/vtimer, keyed by mpidr.
    pub vcpus: Vec<VcpuCheckpoint>,
    /// The hv_gic distributor/redistributor blob.
    pub gic_blob: Vec<u8>,
    /// Each virtio device's saved state.
    pub devices: Vec<DeviceRecord>,
}

/// The immutable RAM image a `Ctrl-A r` rolls back to.
///
/// `Mapped` is a read-only mmap of an APFS clonefile (O(1), CoW on disk — the
/// disposable-browser fan-out case). `Owned` is a plain heap copy, used for a
/// fresh boot whose guest RAM has no backing file (`MAP_ANON`).
pub enum PristineRam {
    Mapped { ptr: *mut libc::c_void, len: usize },
    Owned(Vec<u8>),
}

// SAFETY: `Mapped` holds a read-only, immutable mmap. The pointer is never
// written through and the mapping outlives every reader (dropped only when the
// ResetPoint is replaced). Sharing the slice across threads is sound.
unsafe impl Send for PristineRam {}
unsafe impl Sync for PristineRam {}

impl PristineRam {
    /// Map an existing file read-only (no clone). Used to point the reset
    /// pristine at the immutable base memory.bin directly — zero copy, and it
    /// shares the base's warm page cache.
    pub fn map_file_ro(path: &Path, len: usize) -> io::Result<PristineRam> {
        let f = std::fs::OpenOptions::new().read(true).open(path)?;
        // SAFETY: mapping `len` bytes of a file expected to be at least `len`.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                f.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::other("mmap of pristine file failed"));
        }
        Ok(PristineRam::Mapped { ptr, len })
    }

    /// Take an owned copy of the current live RAM (fresh-boot fallback).
    pub fn from_copy(live: &[u8]) -> PristineRam {
        PristineRam::Owned(live.to_vec())
    }

    pub fn as_slice(&self) -> &[u8] {
        match self {
            // SAFETY: `ptr`/`len` came from a successful PROT_READ mmap and the
            // mapping is immutable for the lifetime of `self`.
            PristineRam::Mapped { ptr, len } => unsafe {
                std::slice::from_raw_parts(*ptr as *const u8, *len)
            },
            PristineRam::Owned(v) => v.as_slice(),
        }
    }
}

impl Drop for PristineRam {
    fn drop(&mut self) {
        if let PristineRam::Mapped { ptr, len } = self {
            // SAFETY: unmapping exactly the region we mapped.
            unsafe { libc::munmap(*ptr, *len) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PG: usize = 16384;

    #[test]
    fn rollback_full_reverts_everything() {
        let pristine = vec![0xAAu8; PG * 4];
        let mut live = vec![0xFFu8; PG * 4];
        rollback_full(&pristine, &mut live);
        assert_eq!(live, pristine);
    }

    #[test]
    fn rollback_pages_reverts_only_listed_pages() {
        let pristine = vec![0xAAu8; PG * 4];
        let mut live = vec![0xFFu8; PG * 4];
        // Revert pages 1 and 3 only.
        rollback_pages(&pristine, &mut live, &[1, 3], PG);
        assert!(live[0..PG].iter().all(|&b| b == 0xFF), "page 0 untouched");
        assert!(live[PG..2 * PG].iter().all(|&b| b == 0xAA), "page 1 reverted");
        assert!(live[2 * PG..3 * PG].iter().all(|&b| b == 0xFF), "page 2 untouched");
        assert!(live[3 * PG..4 * PG].iter().all(|&b| b == 0xAA), "page 3 reverted");
    }

    #[test]
    fn rollback_pages_skips_out_of_range_index() {
        let pristine = vec![0xAAu8; PG * 2];
        let mut live = vec![0xFFu8; PG * 2];
        // Page 99 is past the end; must not panic and must leave RAM unchanged.
        rollback_pages(&pristine, &mut live, &[99], PG);
        assert!(live.iter().all(|&b| b == 0xFF));
    }

    #[test]
    fn pristine_owned_round_trips_bytes() {
        let src = vec![0x5Au8; PG * 2];
        let p = PristineRam::from_copy(&src);
        assert_eq!(p.as_slice(), &src[..]);
    }

    #[test]
    fn map_file_ro_round_trips_bytes() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("ignition-mapro-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("memory.bin");
        let bytes = vec![0x7Eu8; PG * 3];
        std::fs::File::create(&src).unwrap().write_all(&bytes).unwrap();

        let p = PristineRam::map_file_ro(&src, bytes.len()).unwrap();
        assert_eq!(p.as_slice(), &bytes[..]);

        drop(p);
        std::fs::remove_dir_all(&dir).ok();
    }
}
