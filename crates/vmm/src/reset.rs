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
}
