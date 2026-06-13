// Thread-safe dirty-page bitmap for diff snapshots.
//
// Multiple vCPU threads call `mark(ipa)` when a write-protect fault fires; at
// snapshot the leader calls `drain()` to get the sorted dirty page indices and
// clear the set. The tracking granule is 16 KiB (the host page size validated
// by the feasibility gate in Task 1).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub const PAGE: usize = 16384; // tracking granule (feasibility gate Task 1: 16 KiB host page)

#[derive(Clone)]
pub struct DirtyTracker {
    base: u64,
    page_count: u64,
    bits: Arc<Vec<AtomicU64>>,
}

impl DirtyTracker {
    pub fn new(base: u64, size: u64) -> Self {
        let page_count = size.div_ceil(PAGE as u64);
        let words = (page_count as usize).div_ceil(64);
        let bits = Arc::new((0..words).map(|_| AtomicU64::new(0)).collect());
        Self {
            base,
            page_count,
            bits,
        }
    }

    pub fn page_count(&self) -> u64 {
        self.page_count
    }

    pub fn mark(&self, ipa: u64) {
        if ipa < self.base {
            return;
        }
        let p = (ipa - self.base) / PAGE as u64;
        if p >= self.page_count {
            return;
        }
        self.bits[(p / 64) as usize].fetch_or(1u64 << (p % 64), Ordering::Relaxed);
    }

    pub fn drain(&self) -> Vec<u64> {
        let mut out = Vec::new();
        for (wi, w) in self.bits.iter().enumerate() {
            let v = w.swap(0, Ordering::Relaxed);
            if v == 0 {
                continue;
            }
            for b in 0..64 {
                if (v >> b) & 1 == 1 {
                    out.push(wi as u64 * 64 + b);
                }
            }
        }
        out // ascending by construction
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mark_and_drain_sorted_unique() {
        let base = 0x4000_0000u64;
        let t = DirtyTracker::new(base, 4 * PAGE as u64);
        t.mark(base + 2 * PAGE as u64 + 7); // page 2
        t.mark(base + 7); // page 0
        t.mark(base + 2 * PAGE as u64); // page 2 again
        assert_eq!(t.drain(), vec![0, 2]);
        assert_eq!(t.drain(), Vec::<u64>::new()); // cleared after drain
    }

    #[test]
    fn last_partial_page_counts() {
        let base = 0x4000_0000u64;
        let t = DirtyTracker::new(base, 3 * PAGE as u64 + 1); // 4 pages
        assert_eq!(t.page_count(), 4);
        t.mark(base + 3 * PAGE as u64);
        assert_eq!(t.drain(), vec![3]);
    }

    #[test]
    fn out_of_range_ignored() {
        let base = 0x4000_0000u64;
        let t = DirtyTracker::new(base, 2 * PAGE as u64);
        t.mark(base - 1); // below base
        t.mark(base + 2 * PAGE as u64); // past end (page 2 of a 2-page region)
        assert_eq!(t.drain(), Vec::<u64>::new());
    }
}
