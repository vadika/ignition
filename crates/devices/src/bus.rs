// MMIO device bus: routes guest physical accesses to registered devices.

use std::sync::{Arc, Mutex};

/// One MMIO device. Signature mirrors Firecracker's `vstate::bus::BusDevice`
/// (minus the `Arc<Barrier>` return, unused this milestone) so FC device code
/// lifts later with minimal edits.
pub trait BusDevice: Send {
    fn read(&mut self, _base: u64, _offset: u64, _data: &mut [u8]) {}
    fn write(&mut self, _base: u64, _offset: u64, _data: &[u8]) {}
}

/// Address-routed collection of MMIO devices. Ranges are assumed
/// non-overlapping. `read`/`write` take `&self` (devices carry their own
/// `Mutex`), so a fully-built `Bus` can be shared as `Arc<Bus>` across threads.
#[derive(Default)]
pub struct Bus {
    devices: Vec<(u64, u64, Arc<Mutex<dyn BusDevice>>)>, // (base, len, device)
}

impl Bus {
    pub fn new() -> Self {
        Self { devices: Vec::new() }
    }

    pub fn register(&mut self, base: u64, len: u64, dev: Arc<Mutex<dyn BusDevice>>) {
        self.devices.push((base, len, dev));
    }

    fn find(&self, addr: u64) -> Option<(u64, &Arc<Mutex<dyn BusDevice>>)> {
        self.devices
            .iter()
            .find(|(base, len, _)| addr >= *base && addr < base + len)
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
        bus.register(0x1000, 0x100, rec.clone());
        bus.write(0x1004, &[0xab]);
        assert_eq!(rec.lock().unwrap().last_write, Some((0x1000, 0x4, vec![0xab])));
    }

    #[test]
    fn read_routes_with_offset() {
        let rec = Arc::new(Mutex::new(Recorder { read_val: 0x5a, ..Default::default() }));
        let mut bus = Bus::new();
        bus.register(0x2000, 0x10, rec.clone());
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
    }
}
