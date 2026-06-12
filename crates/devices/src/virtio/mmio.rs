//! virtio-mmio transport (virtio 1.0 §4.2), driven synchronously.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::bus::BusDevice;

use super::IrqLine;
use super::guest_ram::GuestRam;
use super::queue::Virtqueue;

const MAGIC: u32 = 0x7472_6976; // "virt"
const VERSION: u32 = 2;
const VENDOR_ID: u32 = 0x4b4e_5246; // arbitrary non-zero
const QUEUE_SIZE_MAX: u32 = 256;
/// DeviceFeatures high word (sel 1): bit 0 == VIRTIO_F_VERSION_1 (feature bit 32).
const FEATURES_HI_VERSION_1: u32 = 1;
const INT_STATUS_USED: u32 = 1;

/// A virtio device plugged into the mmio transport. The transport owns the
/// virtqueues and the interrupt line; the device supplies identity, features,
/// config space, and per-queue servicing.
pub trait VirtioDevice: Send {
    fn device_id(&self) -> u32;
    /// The device-feature word for `sel` (0 = bits 0..31, 1 = bits 32..63). The
    /// transport adds VIRTIO_F_VERSION_1 (bit 32) itself, so return only this
    /// device's own bits.
    fn device_features(&self, sel: u32) -> u32;
    /// Fill `data` from device config space at `offset` (relative to 0x100),
    /// little-endian. The guest reads config at arbitrary widths (Linux reads the
    /// 6-byte virtio-net MAC byte-by-byte), so this must serve any `data.len()`.
    fn config_read(&self, offset: u64, data: &mut [u8]);
    fn queue_count(&self) -> usize;
    /// Service a QueueNotify on `queue_idx`. Returns true if any buffer was used.
    fn handle_notify(&mut self, queue_idx: usize, vq: &mut Virtqueue, mem: &GuestRam) -> bool;
    /// Inject a received frame into the RX queue. Default: not an RX device.
    fn inject_rx(&mut self, _vq: &mut Virtqueue, _mem: &GuestRam, _frame: &[u8]) -> bool {
        false
    }
}

/// Per-queue driver-programmed state.
#[derive(Default)]
struct QueueState {
    num: u16,
    ready: u32,
    desc_lo: u32,
    desc_hi: u32,
    driver_lo: u32,
    driver_hi: u32,
    device_lo: u32,
    device_hi: u32,
    vq: Option<Virtqueue>,
}

/// Serializable snapshot of a single virtqueue's driver-programmed state and ring indices.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueSnapshot {
    pub num: u16,
    pub ready: u32,
    pub desc_lo: u32,
    pub desc_hi: u32,
    pub driver_lo: u32,
    pub driver_hi: u32,
    pub device_lo: u32,
    pub device_hi: u32,
    pub last_avail: u16,
    pub used: u16,
}

/// Serializable snapshot of the full virtio-mmio transport state (registers + queues).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtioMmioState {
    pub status: u32,
    pub queue_sel: u32,
    pub device_features_sel: u32,
    pub interrupt_status: u32,
    pub queues: Vec<QueueSnapshot>,
}

/// A virtio-mmio transport hosting one `VirtioDevice`.
pub struct VirtioMmio {
    dev: Box<dyn VirtioDevice>,
    mem: GuestRam,
    irq: Arc<dyn IrqLine>,
    queues: Vec<QueueState>,
    status: u32,
    device_features_sel: u32,
    queue_sel: u32,
    interrupt_status: u32,
}

impl VirtioMmio {
    pub fn new(dev: Box<dyn VirtioDevice>, mem: GuestRam, irq: Arc<dyn IrqLine>) -> Self {
        let queues = (0..dev.queue_count()).map(|_| QueueState::default()).collect();
        Self {
            dev,
            mem,
            irq,
            queues,
            status: 0,
            device_features_sel: 0,
            queue_sel: 0,
            interrupt_status: 0,
        }
    }

    fn read_reg(&self, off: u64) -> u32 {
        let sel = self.queue_sel as usize;
        match off {
            0x000 => MAGIC,
            0x004 => VERSION,
            0x008 => self.dev.device_id(),
            0x00c => VENDOR_ID,
            0x010 => {
                if self.device_features_sel == 1 {
                    FEATURES_HI_VERSION_1 | self.dev.device_features(1)
                } else {
                    self.dev.device_features(0)
                }
            }
            0x034 => QUEUE_SIZE_MAX,
            0x044 => self.queues.get(sel).map_or(0, |q| q.ready),
            0x060 => self.interrupt_status,
            0x070 => self.status,
            0x0fc => 0,
            // Config space (>= 0x100) is byte-addressable and served in
            // BusDevice::read, not here (it handles non-32-bit widths).
            _ => 0,
        }
    }

    fn write_reg(&mut self, off: u64, val: u32) {
        let sel = self.queue_sel as usize;
        match off {
            0x014 => self.device_features_sel = val & 1,
            0x020 | 0x024 => {}
            0x030 => self.queue_sel = val,
            0x038 => {
                if let Some(q) = self.queues.get_mut(sel) {
                    q.num = val as u16;
                }
            }
            0x044 => self.set_queue_ready(sel, val),
            0x050 => self.notify(val), // QueueNotify carries the queue index in `val`
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
            0x080 => self.set_addr(sel, |q| &mut q.desc_lo, val),
            0x084 => self.set_addr(sel, |q| &mut q.desc_hi, val),
            0x090 => self.set_addr(sel, |q| &mut q.driver_lo, val),
            0x094 => self.set_addr(sel, |q| &mut q.driver_hi, val),
            0x0a0 => self.set_addr(sel, |q| &mut q.device_lo, val),
            0x0a4 => self.set_addr(sel, |q| &mut q.device_hi, val),
            _ => {}
        }
    }

    fn set_addr(&mut self, sel: usize, field: impl Fn(&mut QueueState) -> &mut u32, val: u32) {
        if let Some(q) = self.queues.get_mut(sel) {
            *field(q) = val;
        }
    }

    fn set_queue_ready(&mut self, sel: usize, val: u32) {
        let Some(q) = self.queues.get_mut(sel) else {
            return;
        };
        if val != 0 {
            q.ready = 1;
            let desc = (u64::from(q.desc_hi) << 32) | u64::from(q.desc_lo);
            let driver = (u64::from(q.driver_hi) << 32) | u64::from(q.driver_lo);
            let device = (u64::from(q.device_hi) << 32) | u64::from(q.device_lo);
            q.vq = Some(Virtqueue::new(q.num, desc, driver, device));
        } else {
            *q = QueueState::default();
        }
    }

    fn notify(&mut self, queue_idx: u32) {
        let idx = queue_idx as usize;
        let Some(q) = self.queues.get_mut(idx) else {
            return;
        };
        if q.ready == 0 {
            return;
        }
        let Some(vq) = q.vq.as_mut() else {
            return;
        };
        let serviced = self.dev.handle_notify(idx, vq, &self.mem);
        if serviced {
            self.raise();
        }
    }

    /// Inject a received frame into RX queue 0 (called from the host RX thread,
    /// which must hold the transport's `Mutex`). Returns false if there was no
    /// free RX buffer (frame dropped).
    pub fn inject_rx(&mut self, frame: &[u8]) -> bool {
        let Some(q) = self.queues.get_mut(0) else {
            return false;
        };
        if q.ready == 0 {
            return false;
        }
        let Some(vq) = q.vq.as_mut() else {
            return false;
        };
        let used = self.dev.inject_rx(vq, &self.mem, frame);
        if used {
            self.raise();
        }
        used
    }

    fn raise(&mut self) {
        self.interrupt_status |= INT_STATUS_USED;
        self.irq.set_spi(true);
    }

    fn reset(&mut self) {
        for q in &mut self.queues {
            *q = QueueState::default();
        }
        self.interrupt_status = 0;
        self.irq.set_spi(false);
    }
}

impl VirtioMmio {
    /// Capture the transport register + queue state for snapshot.
    pub fn save(&self) -> VirtioMmioState {
        let queues = self
            .queues
            .iter()
            .map(|q| {
                let (la, u) = q.vq.as_ref().map_or((0, 0), |vq| vq.indices());
                QueueSnapshot {
                    num: q.num,
                    ready: q.ready,
                    desc_lo: q.desc_lo,
                    desc_hi: q.desc_hi,
                    driver_lo: q.driver_lo,
                    driver_hi: q.driver_hi,
                    device_lo: q.device_lo,
                    device_hi: q.device_hi,
                    last_avail: la,
                    used: u,
                }
            })
            .collect();
        VirtioMmioState {
            status: self.status,
            queue_sel: self.queue_sel,
            device_features_sel: self.device_features_sel,
            interrupt_status: self.interrupt_status,
            queues,
        }
    }

    /// Restore transport register + queue state from a snapshot.
    pub fn restore(&mut self, s: &VirtioMmioState) {
        self.status = s.status;
        self.queue_sel = s.queue_sel;
        self.device_features_sel = s.device_features_sel;
        self.interrupt_status = s.interrupt_status;
        for (q, snap) in self.queues.iter_mut().zip(&s.queues) {
            q.num = snap.num;
            q.ready = snap.ready;
            q.desc_lo = snap.desc_lo;
            q.desc_hi = snap.desc_hi;
            q.driver_lo = snap.driver_lo;
            q.driver_hi = snap.driver_hi;
            q.device_lo = snap.device_lo;
            q.device_hi = snap.device_hi;
            if snap.ready != 0 {
                let desc = (u64::from(snap.desc_hi) << 32) | u64::from(snap.desc_lo);
                let driver = (u64::from(snap.driver_hi) << 32) | u64::from(snap.driver_lo);
                let device = (u64::from(snap.device_hi) << 32) | u64::from(snap.device_lo);
                let mut vq = Virtqueue::new(snap.num, desc, driver, device);
                vq.set_indices(snap.last_avail, snap.used);
                q.vq = Some(vq);
            }
        }
    }
}

impl BusDevice for VirtioMmio {
    fn read(&mut self, _base: u64, offset: u64, data: &mut [u8]) {
        // Config space is byte-addressable (any width); registers are 32-bit.
        if offset >= 0x100 {
            self.dev.config_read(offset - 0x100, data);
        } else if data.len() == 4 {
            data.copy_from_slice(&self.read_reg(offset).to_le_bytes());
        } else {
            log::warn!("virtio-mmio: non-32-bit register read at {offset:#x} len {}", data.len());
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
    use crate::virtio::blk::VirtioBlk;
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
        VirtioMmio::new(Box::new(VirtioBlk::new(disk()).unwrap()), mem, irq)
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

    #[test]
    fn virtio_mmio_state_round_trips() {
        let mut backing = vec![0u8; 0x6000];
        let irq = Arc::new(FakeIrq::default());
        let mut d = dev(&mut backing, irq);
        // Set queue 0: num=8, desc_lo=0x1000, ready=1
        wr(&mut d, 0x080, 0x1000); // QueueDescLow
        wr(&mut d, 0x038, 8); // QueueNum
        wr(&mut d, 0x044, 1); // QueueReady
        let st = d.save();
        let mut backing2 = vec![0u8; 0x6000];
        let mut d2 = dev(&mut backing2, Arc::new(FakeIrq::default()));
        d2.restore(&st);
        assert_eq!(d2.save(), st); // round-trips
    }
}
