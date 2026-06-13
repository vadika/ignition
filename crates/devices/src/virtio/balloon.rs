//! virtio-balloon (VIRTIO_ID_BALLOON): the host raises a page target; the guest
//! inflates by posting page-frame numbers on the inflate queue, and this device
//! returns those host pages to the OS via GuestRam::madvise_free. Deflate is a
//! no-op (a freed page re-faults to zero on the guest's next touch).

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use super::guest_ram::GuestRam;
use super::mmio::VirtioDevice;
use super::queue::Virtqueue;

const VIRTIO_ID_BALLOON: u32 = 5;
/// Balloon PFNs are always in 4 KiB units (VIRTIO_BALLOON_PFN_SHIFT).
const PFN_SHIFT: u64 = 12;
const PAGE: usize = 4096;

const INFLATEQ: usize = 0;
const DEFLATEQ: usize = 1;

pub struct Balloon {
    /// Host target in 4 KiB pages (config.num_pages). Shared with the host trigger.
    num_pages: Arc<AtomicU32>,
    /// Guest-reported inflated page count (config.actual).
    actual: u32,
}

impl Balloon {
    /// Returns the device and a clone of the shared target the host trigger drives.
    pub fn new() -> (Self, Arc<AtomicU32>) {
        let num_pages = Arc::new(AtomicU32::new(0));
        (Balloon { num_pages: num_pages.clone(), actual: 0 }, num_pages)
    }

    /// 8-byte virtio_balloon_config: num_pages (0x00), actual (0x04).
    fn config_bytes(&self) -> [u8; 8] {
        let mut c = [0u8; 8];
        // Acquire pairs with the host trigger's Release store of the target.
        c[0..4].copy_from_slice(&self.num_pages.load(Ordering::Acquire).to_le_bytes());
        c[4..8].copy_from_slice(&self.actual.to_le_bytes());
        c
    }

    fn inflate(&self, vq: &mut Virtqueue, mem: &GuestRam) -> bool {
        let mut serviced = false;
        while let Some(chain) = vq.pop_avail(mem) {
            for d in &chain.descriptors {
                if !d.writable {
                    let count = (d.len / 4) as u64;
                    for i in 0..count {
                        let mut b = [0u8; 4];
                        if mem.read_slice(d.addr + i * 4, &mut b) {
                            let pfn = u32::from_le_bytes(b) as u64;
                            mem.madvise_free(pfn << PFN_SHIFT, PAGE);
                        }
                    }
                }
            }
            vq.push_used(mem, chain.head, 0);
            serviced = true;
        }
        serviced
    }

    fn deflate(&self, vq: &mut Virtqueue, mem: &GuestRam) -> bool {
        let mut serviced = false;
        while let Some(chain) = vq.pop_avail(mem) {
            vq.push_used(mem, chain.head, 0);
            serviced = true;
        }
        serviced
    }
}

impl VirtioDevice for Balloon {
    fn device_id(&self) -> u32 {
        VIRTIO_ID_BALLOON
    }
    fn device_features(&self, _sel: u32) -> u32 {
        0
    }
    fn config_read(&self, offset: u64, data: &mut [u8]) {
        let cfg = self.config_bytes();
        for (i, b) in data.iter_mut().enumerate() {
            let o = offset as usize + i;
            *b = if o < cfg.len() { cfg[o] } else { 0 };
        }
    }
    fn config_write(&mut self, offset: u64, data: &[u8]) {
        if offset == 0x04 && data.len() == 4 {
            self.actual = u32::from_le_bytes(data.try_into().unwrap());
        }
    }
    fn queue_count(&self) -> usize {
        2
    }
    fn handle_notify(&mut self, queue_idx: usize, vq: &mut Virtqueue, mem: &GuestRam) -> bool {
        match queue_idx {
            INFLATEQ => self.inflate(vq, mem),
            DEFLATEQ => self.deflate(vq, mem),
            _ => false,
        }
    }
    fn save(&self) -> serde_json::Value {
        serde_json::json!({
            "num_pages": self.num_pages.load(Ordering::Acquire),
            "actual": self.actual,
        })
    }
    fn restore(&mut self, v: &serde_json::Value) -> Result<(), String> {
        let num_pages = v.get("num_pages").and_then(|x| x.as_u64())
            .ok_or("balloon: missing num_pages")? as u32;
        let actual = v.get("actual").and_then(|x| x.as_u64())
            .ok_or("balloon: missing actual")? as u32;
        // Release pairs with the device's Acquire load in config_bytes().
        self.num_pages.store(num_pages, Ordering::Release);
        self.actual = actual;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: u64 = 0x4000_0000;
    const DESC: u64 = BASE + 0x1000;
    const AVAIL: u64 = BASE + 0x2000;
    const USED: u64 = BASE + 0x3000;
    const DATA: u64 = BASE + 0x500;

    fn mem(backing: &mut Vec<u8>) -> GuestRam {
        GuestRam::new(backing.as_mut_ptr(), backing.len(), BASE)
    }
    fn write_desc(m: &GuestRam, i: u64, addr: u64, len: u32, flags: u16, next: u16) {
        let d = DESC + i * 16;
        m.write_slice(d, &addr.to_le_bytes());
        m.write_slice(d + 8, &len.to_le_bytes());
        m.write_slice(d + 12, &flags.to_le_bytes());
        m.write_slice(d + 14, &next.to_le_bytes());
    }
    fn offer_head0(m: &GuestRam) {
        m.write_u16(AVAIL + 2, 1);
        m.write_u16(AVAIL + 4, 0);
    }

    #[test]
    fn inflate_services_queue() {
        let mut backing = vec![0u8; 0x4000];
        let m = mem(&mut backing);
        m.write_slice(DATA, &0x4_0000u32.to_le_bytes());
        m.write_slice(DATA + 4, &0x4_0001u32.to_le_bytes());
        write_desc(&m, 0, DATA, 8, 0, 0);
        offer_head0(&m);
        let mut vq = Virtqueue::new(8, DESC, AVAIL, USED);
        let (mut b, _t) = Balloon::new();
        assert!(b.handle_notify(0, &mut vq, &m));
        assert_eq!(m.read_u32(USED + 4), Some(0));
        assert_eq!(m.read_u32(USED + 8), Some(0));
    }

    #[test]
    fn deflate_services_queue() {
        let mut backing = vec![0u8; 0x4000];
        let m = mem(&mut backing);
        write_desc(&m, 0, DATA, 8, 0, 0);
        offer_head0(&m);
        let mut vq = Virtqueue::new(8, DESC, AVAIL, USED);
        let (mut b, _t) = Balloon::new();
        assert!(b.handle_notify(1, &mut vq, &m));
        assert_eq!(m.read_u32(USED + 4), Some(0));
    }

    #[test]
    fn config_read_reports_target() {
        let (b, t) = Balloon::new();
        t.store(64 * 256, Ordering::Relaxed);
        let mut d = [0u8; 4];
        b.config_read(0x00, &mut d);
        assert_eq!(u32::from_le_bytes(d), 64 * 256);
    }

    #[test]
    fn config_write_stores_actual() {
        let (mut b, _t) = Balloon::new();
        b.config_write(0x04, &1234u32.to_le_bytes());
        let mut d = [0u8; 4];
        b.config_read(0x04, &mut d);
        assert_eq!(u32::from_le_bytes(d), 1234);
    }

    #[test]
    fn identity() {
        let (b, _t) = Balloon::new();
        assert_eq!(b.device_id(), 5);
        assert_eq!(b.queue_count(), 2);
        assert_eq!(b.device_features(0), 0);
    }

    #[test]
    fn save_restore_roundtrips_target_and_actual() {
        let (mut b, t) = Balloon::new();
        t.store(64 * 256, Ordering::Relaxed); // host target = 64 MiB in pages
        b.config_write(0x04, &(40 * 256u32).to_le_bytes()); // guest reported actual
        let saved = b.save();

        let (mut b2, t2) = Balloon::new();
        b2.restore(&saved).expect("restore ok");
        assert_eq!(t2.load(Ordering::Relaxed), 64 * 256, "shared target restored");
        let mut d = [0u8; 8];
        b2.config_read(0x00, &mut d);
        assert_eq!(u32::from_le_bytes(d[0..4].try_into().unwrap()), 64 * 256);
        assert_eq!(u32::from_le_bytes(d[4..8].try_into().unwrap()), 40 * 256);
    }
}
