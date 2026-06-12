//! virtio-mmio transport (virtio 1.0 §4.2), driven synchronously.

use std::sync::Arc;

use crate::bus::BusDevice;

use super::IrqLine;
use super::blk::VirtioBlk;
use super::guest_ram::GuestRam;
use super::queue::Virtqueue;

const MAGIC: u32 = 0x7472_6976; // "virt"
const VERSION: u32 = 2;
const DEVICE_ID_BLK: u32 = 2;
const VENDOR_ID: u32 = 0x4b4e_5246; // arbitrary non-zero
const QUEUE_SIZE_MAX: u32 = 256;
/// DeviceFeatures high word (sel 1): bit 0 == VIRTIO_F_VERSION_1 (feature bit 32).
const FEATURES_HI_VERSION_1: u32 = 1;
const INT_STATUS_USED: u32 = 1;

/// A single-queue virtio-mmio block device.
pub struct VirtioMmio {
    blk: VirtioBlk,
    mem: GuestRam,
    irq: Arc<dyn IrqLine>,
    vq: Option<Virtqueue>,

    status: u32,
    device_features_sel: u32,
    queue_sel: u32,
    queue_num: u16,
    queue_ready: u32,
    desc_lo: u32,
    desc_hi: u32,
    driver_lo: u32,
    driver_hi: u32,
    device_lo: u32,
    device_hi: u32,
    interrupt_status: u32,
}

impl VirtioMmio {
    pub fn new(blk: VirtioBlk, mem: GuestRam, irq: Arc<dyn IrqLine>) -> Self {
        Self {
            blk,
            mem,
            irq,
            vq: None,
            status: 0,
            device_features_sel: 0,
            queue_sel: 0,
            queue_num: 0,
            queue_ready: 0,
            desc_lo: 0,
            desc_hi: 0,
            driver_lo: 0,
            driver_hi: 0,
            device_lo: 0,
            device_hi: 0,
            interrupt_status: 0,
        }
    }

    fn read_reg(&self, off: u64) -> u32 {
        match off {
            0x000 => MAGIC,
            0x004 => VERSION,
            0x008 => DEVICE_ID_BLK,
            0x00c => VENDOR_ID,
            0x010 => {
                if self.device_features_sel == 1 {
                    FEATURES_HI_VERSION_1
                } else {
                    0
                }
            }
            0x034 => QUEUE_SIZE_MAX,
            0x044 => self.queue_ready,
            0x060 => self.interrupt_status,
            0x070 => self.status,
            0x0fc => 0,
            0x100 => (self.blk.capacity_sectors() & 0xffff_ffff) as u32,
            0x104 => (self.blk.capacity_sectors() >> 32) as u32,
            _ => 0,
        }
    }

    fn write_reg(&mut self, off: u64, val: u32) {
        match off {
            0x014 => self.device_features_sel = val,
            0x020 => {} // DriverFeatures: accepted.
            0x024 => {} // DriverFeaturesSel: ignored (we only key DeviceFeatures off sel).
            0x030 => self.queue_sel = val,
            0x038 => self.queue_num = val as u16,
            0x044 => {
                self.queue_ready = val;
                if val == 1 && self.queue_sel == 0 {
                    // The driver writes all six addr halves before QueueReady=1
                    // (virtio 1.0 §4.2.3.2), so the shadows are stable here.
                    let desc = (u64::from(self.desc_hi) << 32) | u64::from(self.desc_lo);
                    let driver = (u64::from(self.driver_hi) << 32) | u64::from(self.driver_lo);
                    let device = (u64::from(self.device_hi) << 32) | u64::from(self.device_lo);
                    self.vq = Some(Virtqueue::new(self.queue_num, desc, driver, device));
                } else if val == 0 {
                    // Drop the queue and clear the addr shadows so a re-setup
                    // can't compose a GPA from a mix of new and stale halves.
                    self.vq = None;
                    self.desc_lo = 0;
                    self.desc_hi = 0;
                    self.driver_lo = 0;
                    self.driver_hi = 0;
                    self.device_lo = 0;
                    self.device_hi = 0;
                }
            }
            0x050 => self.notify(),
            0x064 => {
                self.interrupt_status &= !val;
                if self.interrupt_status == 0 {
                    self.irq.set_spi(false);
                }
            }
            0x070 => {
                self.status = val;
                if val == 0 {
                    self.reset();
                }
            }
            0x080 => self.desc_lo = val,
            0x084 => self.desc_hi = val,
            0x090 => self.driver_lo = val,
            0x094 => self.driver_hi = val,
            0x0a0 => self.device_lo = val,
            0x0a4 => self.device_hi = val,
            _ => {}
        }
    }

    fn reset(&mut self) {
        self.vq = None;
        self.queue_ready = 0;
        self.interrupt_status = 0;
        self.irq.set_spi(false);
    }

    fn notify(&mut self) {
        if self.queue_ready == 0 || self.queue_sel != 0 {
            return;
        }
        let mut serviced = false;
        {
            let Some(vq) = self.vq.as_mut() else { return };
            let mem = &self.mem;
            let blk = &mut self.blk;
            while let Some(chain) = vq.pop_avail(mem) {
                let len = blk.process(&chain, mem);
                vq.push_used(mem, chain.head, len);
                serviced = true;
            }
        }
        if serviced {
            // Re-asserting an already-asserted line is idempotent; the guest
            // drains the whole used ring on the one interrupt it handles, so no
            // completion is lost if a second notify lands before the ACK.
            self.interrupt_status |= INT_STATUS_USED;
            self.irq.set_spi(true);
        }
    }
}

impl BusDevice for VirtioMmio {
    fn read(&mut self, _base: u64, offset: u64, data: &mut [u8]) {
        if data.len() == 4 {
            data.copy_from_slice(&self.read_reg(offset).to_le_bytes());
        } else {
            log::warn!("virtio-mmio: non-32-bit read at {offset:#x} len {}", data.len());
        }
    }

    fn write(&mut self, _base: u64, offset: u64, data: &[u8]) {
        if data.len() == 4 {
            self.write_reg(offset, u32::from_le_bytes(data.try_into().unwrap()));
        } else {
            log::warn!("virtio-mmio: non-32-bit write at {offset:#x} len {}", data.len());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::virtio::guest_ram::GuestRam;

    const BASE: u64 = 0x4000_0000;

    #[derive(Default)]
    struct FakeIrq(Mutex<Vec<bool>>);
    impl IrqLine for FakeIrq {
        fn set_spi(&self, level: bool) {
            self.0.lock().unwrap().push(level);
        }
    }

    fn disk() -> std::fs::File {
        let mut f = tempfile::tempfile().unwrap();
        f.write_all(&[0xBBu8; 1024]).unwrap(); // 2 sectors
        f
    }

    fn dev(backing: &mut Vec<u8>, irq: Arc<dyn IrqLine>) -> VirtioMmio {
        let mem = GuestRam::new(backing.as_mut_ptr(), backing.len(), BASE);
        VirtioMmio::new(VirtioBlk::new(disk()).unwrap(), mem, irq)
    }

    fn rd(d: &mut VirtioMmio, off: u64) -> u32 {
        let mut b = [0u8; 4];
        d.read(BASE, off, &mut b);
        u32::from_le_bytes(b)
    }
    fn wr(d: &mut VirtioMmio, off: u64, v: u32) {
        d.write(BASE, off, &v.to_le_bytes());
    }

    #[test]
    fn identity_registers() {
        let mut backing = vec![0u8; 0x1000];
        let mut d = dev(&mut backing, Arc::new(FakeIrq::default()));
        assert_eq!(rd(&mut d, 0x000), 0x7472_6976);
        assert_eq!(rd(&mut d, 0x004), 2);
        assert_eq!(rd(&mut d, 0x008), 2);
        assert_eq!(rd(&mut d, 0x034), 256);
        assert_eq!(rd(&mut d, 0x100), 2); // capacity sectors low
        // DeviceFeatures high word advertises VERSION_1.
        wr(&mut d, 0x014, 1); // DeviceFeaturesSel = 1
        assert_eq!(rd(&mut d, 0x010), 1);
    }

    #[test]
    fn notify_services_a_request_and_pulses_irq() {
        // Lay out a one-entry queue in guest RAM and a single blk IN request.
        let mut backing = vec![0u8; 0x6000];
        let irq = Arc::new(FakeIrq::default());
        let mut d = dev(&mut backing, irq.clone());

        // Guest physical addresses (offsets from BASE).
        let desc = BASE + 0x1000;
        let avail = BASE + 0x2000;
        let used = BASE + 0x3000;
        let hdr = BASE + 0x0100;
        let data = BASE + 0x0200;
        let status = BASE + 0x0800;

        // Build the request header (type IN=0, sector 1) directly in RAM.
        {
            let m = GuestRam::new(backing.as_mut_ptr(), backing.len(), BASE);
            m.write_u32(hdr, 0); // type IN
            m.write_u32(hdr + 4, 0);
            m.write_u32(hdr + 8, 1); // sector 1 (low)
            m.write_u32(hdr + 12, 0);
            let wd = |i: u64, a: u64, l: u32, fl: u16, nx: u16| {
                let dd = desc + i * 16;
                m.write_slice(dd, &a.to_le_bytes());
                m.write_slice(dd + 8, &l.to_le_bytes());
                m.write_slice(dd + 12, &fl.to_le_bytes());
                m.write_slice(dd + 14, &nx.to_le_bytes());
            };
            wd(0, hdr, 16, 1, 1); // NEXT -> 1
            wd(1, data, 512, 1 | 2, 2); // NEXT|WRITE -> 2
            wd(2, status, 1, 2, 0); // WRITE, end
            m.write_u16(avail + 2, 1); // avail.idx = 1
            m.write_u16(avail + 4, 0); // ring[0] = 0
        }

        // Program the queue registers and notify.
        wr(&mut d, 0x080, desc as u32);
        wr(&mut d, 0x084, (desc >> 32) as u32);
        wr(&mut d, 0x090, avail as u32);
        wr(&mut d, 0x094, (avail >> 32) as u32);
        wr(&mut d, 0x0a0, used as u32);
        wr(&mut d, 0x0a4, (used >> 32) as u32);
        wr(&mut d, 0x038, 8); // QueueNum
        wr(&mut d, 0x044, 1); // QueueReady
        wr(&mut d, 0x050, 0); // QueueNotify

        // The data buffer now holds sector 1 (0xBB), used ring advanced, IRQ pulsed.
        let m = GuestRam::new(backing.as_mut_ptr(), backing.len(), BASE);
        let mut out = [0u8; 512];
        m.read_slice(data, &mut out);
        assert!(out.iter().all(|&b| b == 0xBB));
        assert_eq!(m.read_u16(used + 2), Some(1)); // used.idx
        assert_eq!(rd(&mut d, 0x060), 1); // InterruptStatus = used
        assert_eq!(*irq.0.lock().unwrap().last().unwrap(), true);

        // ACK clears the interrupt and deasserts.
        wr(&mut d, 0x064, 1);
        assert_eq!(rd(&mut d, 0x060), 0);
        assert_eq!(*irq.0.lock().unwrap().last().unwrap(), false);
    }
}
