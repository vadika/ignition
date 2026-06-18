//! The `ignition-fuzz` control-register device. Holds the trap-MMIO scalars the
//! host and guest exchange each iteration: INPUT_LEN (host->guest), CRASH_CODE
//! (guest->host). The DOORBELL register carries no state here — a store to it
//! traps and is handled by the fuzz loop directly.

use crate::bus::BusDevice;
use crate::fuzz::protocol::reg;

pub struct FuzzDevice {
    input_len: u32,
    crash_code: u32,
}

impl FuzzDevice {
    pub fn new() -> FuzzDevice {
        FuzzDevice { input_len: 0, crash_code: 0 }
    }
    /// Host: set the input length the guest will read this iteration.
    pub fn set_input_len(&mut self, len: u32) {
        self.input_len = len;
    }
    /// Host: read the crash reason class the guest wrote on a CRASH doorbell.
    pub fn crash_code(&self) -> u32 {
        self.crash_code
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
}
