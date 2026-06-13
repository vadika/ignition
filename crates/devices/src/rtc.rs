//! ARM PrimeCell PL031 real-time clock. Time-only: RTCDR reports host wall-clock
//! seconds plus a guest-settable offset (RTCLR). The alarm/Match register and the
//! RTC interrupt are not wired. The PrimeCell ID registers let the kernel's amba
//! bus bind the `rtc-pl031` driver.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::bus::BusDevice;
use crate::device::{DeviceMgrError, FdtKind, MmioDevice};

/// Host wall-clock seconds since the Unix epoch.
fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// PL031 time-only RTC.
pub struct Pl031 {
    /// Seconds added to host wall-clock to form RTCDR. 0 = report host time; a
    /// guest write to RTCLR sets `offset = loaded - now_unix()`.
    offset: i64,
    /// Last value written to RTCLR (returned on RTCLR reads).
    load: u32,
    /// Last value written to RTCMR (returned on RTCMR reads; alarm not wired).
    match_reg: u32,
    /// Last value written to RTCCR (reads always report enabled).
    control: u32,
    /// RTCIMSC (interrupt mask); stored for read-back, never acted on.
    imsc: u32,
}

/// Snapshot state: only the offset affects the time the guest reads.
#[derive(Serialize, Deserialize)]
struct Pl031Snapshot {
    offset: i64,
}

impl Pl031 {
    pub fn new() -> Self {
        Pl031 { offset: 0, load: 0, match_reg: 0, control: 0, imsc: 0 }
    }

    fn rtcdr(&self) -> u32 {
        (now_unix() + self.offset) as u32
    }
}

impl Default for Pl031 {
    fn default() -> Self {
        Self::new()
    }
}

impl BusDevice for Pl031 {
    fn read(&mut self, _base: u64, offset: u64, data: &mut [u8]) {
        if data.len() != 4 {
            for b in data.iter_mut() {
                *b = 0;
            }
            return;
        }
        let val: u32 = match offset {
            0x00 => self.rtcdr(),
            0x04 => self.match_reg,
            0x08 => self.load,
            0x0C => 1, // RTCCR: report RTCEN (enabled)
            0x10 => self.imsc,
            0x14 | 0x18 | 0x1C => 0, // RIS / MIS / ICR: no interrupts
            0xFE0 => 0x31,
            0xFE4 => 0x10,
            0xFE8 => 0x04,
            0xFEC => 0x00,
            0xFF0 => 0x0D,
            0xFF4 => 0xF0,
            0xFF8 => 0x05,
            0xFFC => 0xB1,
            _ => 0,
        };
        data.copy_from_slice(&val.to_le_bytes());
    }

    fn write(&mut self, _base: u64, offset: u64, data: &[u8]) {
        if data.len() != 4 {
            return;
        }
        let v = u32::from_le_bytes(data.try_into().unwrap());
        match offset {
            0x04 => self.match_reg = v,
            0x08 => {
                self.load = v;
                self.offset = v as i64 - now_unix();
            }
            0x0C => self.control = v,
            0x10 => self.imsc = v,
            _ => {} // DR/RIS/MIS read-only; ICR has no IRQ to clear; IDs read-only
        }
    }
}

impl MmioDevice for Pl031 {
    fn fdt_kind(&self) -> FdtKind {
        FdtKind::Pl031
    }
    fn snapshot_id(&self) -> &str {
        "rtc"
    }
    fn save(&self) -> serde_json::Value {
        serde_json::to_value(Pl031Snapshot { offset: self.offset }).expect("Pl031Snapshot serializes")
    }
    fn restore(&mut self, v: &serde_json::Value) -> Result<(), DeviceMgrError> {
        let s: Pl031Snapshot = serde_json::from_value(v.clone())
            .map_err(|e| DeviceMgrError::StateInvalid { id: "rtc".into(), reason: e.to_string() })?;
        self.offset = s.offset;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rd(dev: &mut Pl031, off: u64) -> u32 {
        let mut b = [0u8; 4];
        dev.read(0, off, &mut b);
        u32::from_le_bytes(b)
    }
    fn wr(dev: &mut Pl031, off: u64, v: u32) {
        dev.write(0, off, &v.to_le_bytes());
    }

    #[test]
    fn dr_reads_host_wall_clock() {
        let mut rtc = Pl031::new();
        let dr = rd(&mut rtc, 0x00) as i64;
        let now = now_unix();
        assert!((dr - now).abs() <= 5, "RTCDR {dr} not within 5s of now {now}");
    }

    #[test]
    fn load_sets_the_clock() {
        let mut rtc = Pl031::new();
        let t = 2_000_000_000u32; // year 2033, fits in u32 seconds
        wr(&mut rtc, 0x08, t);
        let dr = rd(&mut rtc, 0x00) as i64;
        assert!((dr - t as i64).abs() <= 5, "after Load, RTCDR {dr} not within 5s of {t}");
        assert_eq!(rd(&mut rtc, 0x08), t, "RTCLR reads back the loaded value");
    }

    #[test]
    fn primecell_ids_match_pl031() {
        let mut rtc = Pl031::new();
        assert_eq!(rd(&mut rtc, 0xFE0) & 0xff, 0x31);
        assert_eq!(rd(&mut rtc, 0xFE4) & 0xff, 0x10);
        assert_eq!(rd(&mut rtc, 0xFE8) & 0xff, 0x04);
        assert_eq!(rd(&mut rtc, 0xFEC) & 0xff, 0x00);
        assert_eq!(rd(&mut rtc, 0xFF0) & 0xff, 0x0D);
        assert_eq!(rd(&mut rtc, 0xFF4) & 0xff, 0xF0);
        assert_eq!(rd(&mut rtc, 0xFF8) & 0xff, 0x05);
        assert_eq!(rd(&mut rtc, 0xFFC) & 0xff, 0xB1);
    }

    #[test]
    fn control_reads_enabled() {
        let mut rtc = Pl031::new();
        assert_eq!(rd(&mut rtc, 0x0C), 1);
    }

    #[test]
    fn snapshot_roundtrips_offset() {
        let mut rtc = Pl031::new();
        let t = 2_000_000_000u32;
        wr(&mut rtc, 0x08, t);
        let saved = rtc.save();
        let mut fresh = Pl031::new();
        fresh.restore(&saved).unwrap();
        let dr = rd(&mut fresh, 0x00) as i64;
        assert!((dr - t as i64).abs() <= 5, "restored RTCDR {dr} not within 5s of {t}");
    }

    #[test]
    fn identity() {
        let rtc = Pl031::new();
        assert_eq!(rtc.fdt_kind(), FdtKind::Pl031);
        assert_eq!(rtc.snapshot_id(), "rtc");
    }
}
