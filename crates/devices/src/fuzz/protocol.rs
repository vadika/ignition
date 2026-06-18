//! The host/guest fuzz control protocol: register offsets within the control
//! region, doorbell command codes, and default window geometry. This is the
//! single source of truth; `guest/fuzz-harness/ignition_fuzz.h` mirrors it by
//! hand (keep them in sync — Task 7 asserts the values match).

/// Control-register offsets within the trap-MMIO control region.
pub mod reg {
    /// W: guest writes a command code (see `cmd`); traps to the VMM.
    pub const DOORBELL: u64 = 0x00;
    /// RW: length of the current input in the shared window (host writes, guest reads).
    pub const INPUT_LEN: u64 = 0x04;
    /// W: ASan/abort reason class on a CRASH doorbell (guest writes).
    pub const CRASH_CODE: u64 = 0x08;
}

/// Doorbell command codes (guest -> VMM).
pub mod cmd {
    /// One-time setup complete; parked at the parse site. First receipt captures
    /// the snapshot.
    pub const SNAPSHOT_ME: u32 = 0x1;
    /// Input processed cleanly.
    pub const DONE: u32 = 0x2;
    /// Target crashed (from the death/signal handler).
    pub const CRASH: u32 = 0x3;
}

/// Default shared-window size in bytes (2 MiB).
pub const DEFAULT_WINDOW_SIZE: u64 = 0x20_0000;
/// Default coverage-region size in bytes (64 KiB). An array of 8-bit SanCov edge
/// counters, written by the guest's `trace-pc` callback and read by the host
/// observer. A power of two so the callback can mask the hashed edge PC with
/// `DEFAULT_COV_SIZE - 1` (AFL-style hashed coverage map).
pub const DEFAULT_COV_SIZE: u64 = 0x1_0000;
/// Control region size in bytes: one 16 KiB guest page. The four registers
/// occupy only the first 16 bytes, but the region is page-sized so the guest can
/// `mmap` it via `/dev/mem` (mmap offsets must be 16 KiB-aligned on this guest).
pub const CONTROL_SIZE: u64 = 0x4000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_offsets_are_distinct_and_within_control_region() {
        let offsets = [reg::DOORBELL, reg::INPUT_LEN, reg::CRASH_CODE];
        for (i, a) in offsets.iter().enumerate() {
            assert!(*a + 4 <= CONTROL_SIZE, "register {a:#x} must fit in control region");
            for b in &offsets[i + 1..] {
                assert_ne!(a, b, "register offsets must be distinct");
            }
        }
    }

    #[test]
    fn command_codes_are_distinct_and_nonzero() {
        let codes = [cmd::SNAPSHOT_ME, cmd::DONE, cmd::CRASH];
        for (i, a) in codes.iter().enumerate() {
            assert_ne!(*a, 0, "0 is reserved for 'no command'");
            for b in &codes[i + 1..] {
                assert_ne!(a, b, "command codes must be distinct");
            }
        }
    }

    #[test]
    fn cov_size_is_page_aligned_power_of_two() {
        // The coverage region is a host-readable 8-bit-counter map mmap'd into the
        // guest at a 16 KiB-aligned GPA; its size must be 16 KiB-aligned so the
        // guest can /dev/mem-mmap it, and a power of two so the trace-pc callback
        // can mask the hashed PC with (DEFAULT_COV_SIZE - 1).
        assert_eq!(DEFAULT_COV_SIZE % 0x4000, 0, "cov region must be 16 KiB-aligned");
        assert!(DEFAULT_COV_SIZE.is_power_of_two(), "mask trick needs a power of two");
        assert!(DEFAULT_COV_SIZE >= 0x4000, "at least one guest page");
    }
}
