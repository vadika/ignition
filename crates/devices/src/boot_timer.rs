//! Boot-timer pseudo device (Firecracker's pseudo/boot_timer.rs). The guest writes
//! a magic byte to offset 0 once at the end of boot; we log the elapsed wall time
//! since VM start. Plain BusDevice: no FDT node, no interrupt, no snapshot state.

use std::time::{Duration, Instant};

use crate::bus::BusDevice;

/// Magic value the guest writes to signal "userspace reached" (matches FC).
const MAGIC_BOOT_COMPLETE: u8 = 123;

pub struct BootTimer {
    start: Instant,
    fired: Option<Duration>,
}

impl BootTimer {
    pub fn new(start: Instant) -> BootTimer {
        BootTimer { start, fired: None }
    }
    /// The recorded boot time, once the guest has signalled (for tests/inspection).
    pub fn boot_time(&self) -> Option<Duration> {
        self.fired
    }
}

impl BusDevice for BootTimer {
    fn write(&mut self, _base: u64, offset: u64, data: &[u8]) {
        if offset != 0 || data.len() != 1 || data[0] != MAGIC_BOOT_COMPLETE {
            return;
        }
        if self.fired.is_none() {
            let elapsed = self.start.elapsed();
            self.fired = Some(elapsed);
            log::info!("Guest-boot-time = {} ms", elapsed.as_millis());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_fires_once() {
        let mut t = BootTimer::new(Instant::now());
        assert!(t.boot_time().is_none());
        t.write(0, 0, &[123]);
        let first = t.boot_time();
        assert!(first.is_some(), "magic write should record boot time");
        t.write(0, 0, &[123]);
        assert_eq!(t.boot_time(), first);
    }

    #[test]
    fn non_magic_ignored() {
        let mut t = BootTimer::new(Instant::now());
        t.write(0, 0, &[1]);
        t.write(0, 4, &[123]);
        t.write(0, 0, &[123, 0]);
        assert!(t.boot_time().is_none());
    }
}
