//! The `ignition-fuzz` control-register device. Holds the trap-MMIO scalars the
//! host and guest exchange each iteration: INPUT_LEN (host->guest), CRASH_CODE
//! (guest->host), STATUS (host->guest). The DOORBELL register carries no state
//! here — a store to it traps and is handled by the fuzz loop directly.

use crate::bus::BusDevice;
use crate::device::{DeviceMgrError, FdtKind, MmioDevice};
use crate::fuzz::protocol::reg;

pub struct FuzzDevice {
    input_len: u32,
    crash_code: u32,
    status: u32,
}

impl FuzzDevice {
    pub fn new() -> FuzzDevice {
        FuzzDevice { input_len: 0, crash_code: 0, status: 0 }
    }
    /// Host: set the input length the guest will read this iteration.
    pub fn set_input_len(&mut self, len: u32) {
        self.input_len = len;
    }
    /// Host: read the crash reason class the guest wrote on a CRASH doorbell.
    pub fn crash_code(&self) -> u32 {
        self.crash_code
    }
    /// Host: set the STATUS handshake value.
    pub fn set_status(&mut self, status: u32) {
        self.status = status;
    }
}

impl Default for FuzzDevice {
    fn default() -> Self {
        Self::new()
    }
}

impl BusDevice for FuzzDevice {
    fn read(&mut self, _base: u64, offset: u64, data: &mut [u8]) {
        let val = match offset {
            reg::INPUT_LEN => self.input_len,
            reg::CRASH_CODE => self.crash_code,
            reg::STATUS => self.status,
            _ => 0,
        };
        let bytes = val.to_le_bytes();
        let n = data.len().min(4);
        data[..n].copy_from_slice(&bytes[..n]);
    }

    fn write(&mut self, _base: u64, offset: u64, data: &[u8]) {
        if data.len() < 4 {
            return;
        }
        let val = u32::from_le_bytes(data[..4].try_into().unwrap());
        match offset {
            reg::INPUT_LEN => self.input_len = val,
            reg::CRASH_CODE => self.crash_code = val,
            // DOORBELL is handled by the fuzz loop, not here; ignore stray writes.
            _ => {}
        }
    }
}

impl MmioDevice for FuzzDevice {
    fn fdt_kind(&self) -> FdtKind {
        FdtKind::IgnitionFuzz
    }
    fn snapshot_id(&self) -> &str {
        "ignition-fuzz"
    }
    fn save(&self) -> serde_json::Value {
        serde_json::json!({
            "input_len": self.input_len,
            "crash_code": self.crash_code,
            "status": self.status,
        })
    }
    fn restore(&mut self, v: &serde_json::Value) -> Result<(), DeviceMgrError> {
        let get = |k: &str| -> u32 {
            v.get(k).and_then(|x| x.as_u64()).unwrap_or(0) as u32
        };
        self.input_len = get("input_len");
        self.crash_code = get("crash_code");
        self.status = get("status");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_u32(dev: &mut FuzzDevice, offset: u64) -> u32 {
        let mut buf = [0u8; 4];
        dev.read(0, offset, &mut buf);
        u32::from_le_bytes(buf)
    }

    #[test]
    fn input_len_host_writes_guest_reads() {
        let mut d = FuzzDevice::new();
        d.set_input_len(1234);
        assert_eq!(read_u32(&mut d, reg::INPUT_LEN), 1234);
    }

    #[test]
    fn crash_code_guest_writes_host_reads() {
        let mut d = FuzzDevice::new();
        d.write(0, reg::CRASH_CODE, &11u32.to_le_bytes());
        assert_eq!(d.crash_code(), 11);
        assert_eq!(read_u32(&mut d, reg::CRASH_CODE), 11);
    }

    #[test]
    fn status_handshake_roundtrips() {
        let mut d = FuzzDevice::new();
        assert_eq!(read_u32(&mut d, reg::STATUS), 0);
        d.set_status(1);
        assert_eq!(read_u32(&mut d, reg::STATUS), 1);
    }

    #[test]
    fn save_restore_roundtrips() {
        let mut d = FuzzDevice::new();
        d.set_input_len(7);
        d.write(0, reg::CRASH_CODE, &3u32.to_le_bytes());
        let saved = d.save();
        let mut d2 = FuzzDevice::new();
        d2.restore(&saved).unwrap();
        assert_eq!(d2.crash_code(), 3);
        let mut buf = [0u8; 4];
        d2.read(0, reg::INPUT_LEN, &mut buf);
        assert_eq!(u32::from_le_bytes(buf), 7);
    }
}
