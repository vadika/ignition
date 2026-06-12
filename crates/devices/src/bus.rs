// MMIO device bus: routes guest physical accesses to registered devices.

use std::sync::{Arc, Mutex};

/// One MMIO device. Signature mirrors Firecracker's `vstate::bus::BusDevice`
/// (minus the `Arc<Barrier>` return, unused this milestone) so FC device code
/// lifts later with minimal edits.
pub trait BusDevice: Send {
    fn read(&mut self, _base: u64, _offset: u64, _data: &mut [u8]) {}
    fn write(&mut self, _base: u64, _offset: u64, _data: &[u8]) {}
}

/// Why a `Bus::register` was rejected.
#[derive(Debug, PartialEq, Eq)]
pub enum BusError {
    /// The requested range overlaps an already-registered device.
    Overlap {
        base: u64,
        len: u64,
        existing_base: u64,
        existing_len: u64,
    },
}

impl std::fmt::Display for BusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BusError::Overlap { base, len, existing_base, existing_len } => write!(
                f,
                "MMIO range [{base:#x}, {:#x}) overlaps registered [{existing_base:#x}, {:#x})",
                base.saturating_add(*len),
                existing_base.saturating_add(*existing_len),
            ),
        }
    }
}

impl std::error::Error for BusError {}

/// A registered device and the guest-physical range it occupies.
type BusEntry = (u64, u64, Arc<Mutex<dyn BusDevice>>); // (base, len, device)

/// Address-routed collection of MMIO devices. Ranges are non-overlapping
/// (enforced by `register`). `read`/`write` take `&self` (devices carry their
/// own `Mutex`), so a fully-built `Bus` can be shared as `Arc<Bus>` across
/// threads.
#[derive(Default)]
pub struct Bus {
    devices: Vec<BusEntry>,
}

impl Bus {
    pub fn new() -> Self {
        Self { devices: Vec::new() }
    }

    pub fn register(
        &mut self,
        base: u64,
        len: u64,
        dev: Arc<Mutex<dyn BusDevice>>,
    ) -> Result<(), BusError> {
        // Two half-open ranges [a, a+alen) and [b, b+blen) overlap iff
        // a < b+blen and b < a+alen.
        if let Some((existing_base, existing_len, _)) = self.devices.iter().find(|(b, blen, _)| {
            base < b.saturating_add(*blen) && *b < base.saturating_add(len)
        }) {
            return Err(BusError::Overlap {
                base,
                len,
                existing_base: *existing_base,
                existing_len: *existing_len,
            });
        }
        self.devices.push((base, len, dev));
        Ok(())
    }

    fn find(&self, addr: u64) -> Option<(u64, &Arc<Mutex<dyn BusDevice>>)> {
        // Linear scan is fine at this device count; revisit if the table grows.
        self.devices
            .iter()
            .find(|(base, len, _)| addr.checked_sub(*base).is_some_and(|off| off < *len))
            .map(|(base, _, dev)| (*base, dev))
    }

    pub fn read(&self, addr: u64, data: &mut [u8]) {
        match self.find(addr) {
            Some((base, dev)) => dev.lock().unwrap().read(base, addr - base, data),
            None => log::warn!("MMIO read miss at {addr:#x}"),
        }
    }

    pub fn write(&self, addr: u64, data: &[u8]) {
        match self.find(addr) {
            Some((base, dev)) => dev.lock().unwrap().write(base, addr - base, data),
            None => log::warn!("MMIO write miss at {addr:#x}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Recorder {
        last_write: Option<(u64, u64, Vec<u8>)>,
        read_val: u8,
    }
    impl BusDevice for Recorder {
        fn write(&mut self, base: u64, offset: u64, data: &[u8]) {
            self.last_write = Some((base, offset, data.to_vec()));
        }
        fn read(&mut self, _base: u64, _offset: u64, data: &mut [u8]) {
            data[0] = self.read_val;
        }
    }

    #[test]
    fn write_routes_with_base_and_offset() {
        let rec = Arc::new(Mutex::new(Recorder::default()));
        let mut bus = Bus::new();
        bus.register(0x1000, 0x100, rec.clone()).unwrap();
        bus.write(0x1004, &[0xab]);
        assert_eq!(rec.lock().unwrap().last_write, Some((0x1000, 0x4, vec![0xab])));
    }

    #[test]
    fn read_routes_with_offset() {
        let rec = Arc::new(Mutex::new(Recorder { read_val: 0x5a, ..Default::default() }));
        let mut bus = Bus::new();
        bus.register(0x2000, 0x10, rec.clone()).unwrap();
        let mut buf = [0u8; 1];
        bus.read(0x2008, &mut buf);
        assert_eq!(buf[0], 0x5a);
    }

    #[test]
    fn out_of_range_access_is_ignored() {
        let bus = Bus::new();
        bus.write(0xdead, &[1]); // must not panic
        let mut b = [0u8; 1];
        bus.read(0xbeef, &mut b); // must not panic
        assert_eq!(b[0], 0, "read miss must not write the buffer");
    }

    #[test]
    fn overlapping_register_is_rejected() {
        let a = Arc::new(Mutex::new(Recorder::default()));
        let b = Arc::new(Mutex::new(Recorder::default()));
        let mut bus = Bus::new();
        bus.register(0x1000, 0x100, a).unwrap();
        // [0x1080, 0x10C0) overlaps [0x1000, 0x1100).
        let err = bus.register(0x1080, 0x40, b).unwrap_err();
        assert_eq!(
            err,
            BusError::Overlap { base: 0x1080, len: 0x40, existing_base: 0x1000, existing_len: 0x100 }
        );
    }

    #[test]
    fn adjacent_register_is_allowed() {
        let a = Arc::new(Mutex::new(Recorder::default()));
        let b = Arc::new(Mutex::new(Recorder::default()));
        let mut bus = Bus::new();
        bus.register(0x1000, 0x100, a).unwrap();
        // [0x1100, 0x1200) is adjacent, not overlapping.
        assert!(bus.register(0x1100, 0x100, b).is_ok());
    }
}
