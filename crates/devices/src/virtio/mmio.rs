//! virtio-mmio transport (virtio 1.0 §4.2), driven synchronously.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::bus::BusDevice;
use crate::device::{DeviceMgrError, FdtKind, MmioDevice};

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
const INT_STATUS_CONFIG: u32 = 2;

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
    /// Apply a guest write to device config space at `offset` (relative to 0x100).
    /// Default: ignore (most devices have read-only config).
    fn config_write(&mut self, _offset: u64, _data: &[u8]) {}
    fn queue_count(&self) -> usize;
    /// Service a QueueNotify on `queue_idx`. Returns true if any buffer was used.
    fn handle_notify(&mut self, queue_idx: usize, vq: &mut Virtqueue, mem: &GuestRam) -> bool;
    /// Inject a received frame into the RX queue. Default: not an RX device.
    fn inject_rx(&mut self, _vq: &mut Virtqueue, _mem: &GuestRam, _frame: &[u8]) -> bool {
        false
    }

    /// Reactor entry for async-RX devices (vsock): read host-side data and fill the
    /// RX virtqueue. Default: no-op.
    fn fill_rx(&mut self, _rx_vq: &mut Virtqueue, _mem: &GuestRam) -> bool {
        false
    }

    /// Host fds an async-RX device wants the reactor to poll. Default: none.
    fn vsock_poll_set(&self) -> Vec<std::os::unix::io::RawFd> {
        Vec::new()
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
// Serde mirror of QueueState's driver-programmed fields + ring indices; intentionally decoupled from the runtime struct.
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
    id: &'static str,
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
    pub fn new(id: &'static str, dev: Box<dyn VirtioDevice>, mem: GuestRam, irq: Arc<dyn IrqLine>) -> Self {
        let queues = (0..dev.queue_count()).map(|_| QueueState::default()).collect();
        Self {
            id,
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

    /// Reactor hook: drive the device's async RX (queue 0) and raise the used IRQ if
    /// anything was delivered. Builds/saves the RX queue exactly as inject_rx does.
    pub fn poll_vsock_rx(&mut self) -> bool {
        let Some(q) = self.queues.get_mut(0) else {
            return false;
        };
        if q.ready == 0 {
            return false;
        }
        let Some(vq) = q.vq.as_mut() else {
            return false;
        };
        let delivered = self.dev.fill_rx(vq, &self.mem);
        if delivered {
            self.interrupt_status |= INT_STATUS_USED;
            self.irq.set_spi(true);
        }
        delivered
    }

    /// vsock reactor support: the host fds the device wants polled (empty for others).
    pub fn vsock_poll_set(&self) -> Vec<std::os::unix::io::RawFd> {
        self.dev.vsock_poll_set()
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
    /// Raise a config-change interrupt: the guest will re-read config space. Used
    /// by the host to push a new balloon target (or any future config change).
    pub fn signal_config_change(&mut self) {
        self.interrupt_status |= INT_STATUS_CONFIG;
        self.irq.set_spi(true);
    }

    /// Capture the transport register + queue state for snapshot.
    pub fn save_state(&self) -> VirtioMmioState {
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
    pub fn restore_state(&mut self, s: &VirtioMmioState) {
        self.status = s.status;
        self.queue_sel = s.queue_sel;
        self.device_features_sel = s.device_features_sel;
        self.interrupt_status = s.interrupt_status;
        debug_assert_eq!(self.queues.len(), s.queues.len(), "restore queue-count mismatch");
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
            } else {
                q.vq = None;
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
        if offset >= 0x100 {
            self.dev.config_write(offset - 0x100, data);
        } else if data.len() == 4 {
            self.write_reg(offset, u32::from_le_bytes(data.try_into().unwrap()));
        } else {
            log::warn!("virtio-mmio: non-32-bit write at {offset:#x} len {}", data.len());
        }
    }
}

impl MmioDevice for VirtioMmio {
    fn fdt_kind(&self) -> FdtKind { FdtKind::VirtioMmio }
    fn snapshot_id(&self) -> &str { self.id }
    fn save(&self) -> serde_json::Value {
        serde_json::to_value(self.save_state()).expect("VirtioMmioState serializes")
    }
    fn restore(&mut self, v: &serde_json::Value) -> Result<(), DeviceMgrError> {
        let s: VirtioMmioState = serde_json::from_value(v.clone())
            .map_err(|e| DeviceMgrError::StateInvalid { id: self.id.into(), reason: e.to_string() })?;
        if s.queues.len() != self.queues.len() {
            return Err(DeviceMgrError::StateInvalid {
                id: self.id.into(),
                reason: format!("queue count {} != {}", s.queues.len(), self.queues.len()),
            });
        }
        self.restore_state(&s);
        Ok(())
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
        VirtioMmio::new("virtio-blk", Box::new(VirtioBlk::new(disk()).unwrap()), mem, irq)
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

    fn test_transport(id: &'static str) -> VirtioMmio {
        // Leak the backing so its lifetime exceeds the VirtioMmio (test-only).
        let backing = Box::leak(vec![0u8; 0x1000].into_boxed_slice());
        let mem = GuestRam::new(backing.as_mut_ptr(), backing.len(), BASE);
        VirtioMmio::new(id, Box::new(VirtioBlk::new(disk()).unwrap()), mem, Arc::new(FakeIrq::default()))
    }

    #[test]
    fn mmio_device_trait_roundtrips_transport_state() {
        use crate::device::{FdtKind, MmioDevice};
        let mut t = test_transport("virtio-blk");
        assert_eq!(t.fdt_kind(), FdtKind::VirtioMmio);
        assert_eq!(t.snapshot_id(), "virtio-blk");
        let saved = MmioDevice::save(&t);
        MmioDevice::restore(&mut t, &saved).unwrap();
        assert_eq!(MmioDevice::save(&t), saved);
    }

    #[test]
    fn config_write_routes_to_device() {
        use std::sync::{Arc, Mutex};
        use crate::virtio::NoopIrq;

        #[derive(Clone, Default)]
        struct RecDev { writes: Arc<Mutex<Vec<(u64, Vec<u8>)>>> }
        impl VirtioDevice for RecDev {
            fn device_id(&self) -> u32 { 99 }
            fn device_features(&self, _: u32) -> u32 { 0 }
            fn config_read(&self, _: u64, _: &mut [u8]) {}
            fn queue_count(&self) -> usize { 1 }
            fn handle_notify(&mut self, _: usize, _: &mut Virtqueue, _: &GuestRam) -> bool { false }
            fn config_write(&mut self, offset: u64, data: &[u8]) {
                self.writes.lock().unwrap().push((offset, data.to_vec()));
            }
        }

        let backing = Box::leak(vec![0u8; 0x1000].into_boxed_slice());
        let mem = GuestRam::new(backing.as_mut_ptr(), backing.len(), 0x4000_0000);
        let dev = RecDev::default();
        let writes = dev.writes.clone();
        let mut t = VirtioMmio::new("rec", Box::new(dev), mem, Arc::new(NoopIrq));

        t.write(0, 0x104, &[0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(writes.lock().unwrap().as_slice(), &[(0x04, vec![0xde, 0xad, 0xbe, 0xef])]);
    }

    #[test]
    fn signal_config_change_sets_bit_and_asserts_irq() {
        use std::sync::{Arc, Mutex};

        #[derive(Default)]
        struct RecIrq { level: Mutex<Option<bool>> }
        impl crate::virtio::IrqLine for RecIrq {
            fn set_spi(&self, level: bool) { *self.level.lock().unwrap() = Some(level); }
        }

        #[derive(Default)]
        struct Z;
        impl VirtioDevice for Z {
            fn device_id(&self) -> u32 { 0 }
            fn device_features(&self, _: u32) -> u32 { 0 }
            fn config_read(&self, _: u64, _: &mut [u8]) {}
            fn queue_count(&self) -> usize { 0 }
            fn handle_notify(&mut self, _: usize, _: &mut Virtqueue, _: &GuestRam) -> bool { false }
        }

        let backing = Box::leak(vec![0u8; 0x1000].into_boxed_slice());
        let mem = GuestRam::new(backing.as_mut_ptr(), backing.len(), 0x4000_0000);
        let irq = Arc::new(RecIrq::default());
        let mut t = VirtioMmio::new("z", Box::new(Z), mem, irq.clone());

        t.signal_config_change();
        let mut b = [0u8; 4];
        t.read(0, 0x060, &mut b);
        assert_eq!(u32::from_le_bytes(b) & 0b10, 0b10, "config-change bit set");
        assert_eq!(*irq.level.lock().unwrap(), Some(true), "irq asserted");
    }

    #[test]
    fn poll_vsock_rx_drives_fill_rx_and_irq() {
        use std::sync::{Arc, Mutex};
        #[derive(Default)]
        struct RecIrq { level: Mutex<Option<bool>> }
        impl crate::virtio::IrqLine for RecIrq {
            fn set_spi(&self, level: bool) { *self.level.lock().unwrap() = Some(level); }
        }
        struct FillDev;
        impl VirtioDevice for FillDev {
            fn device_id(&self) -> u32 { 19 }
            fn device_features(&self, _: u32) -> u32 { 0 }
            fn config_read(&self, _: u64, _: &mut [u8]) {}
            fn queue_count(&self) -> usize { 3 }
            fn handle_notify(&mut self, _: usize, _: &mut Virtqueue, _: &GuestRam) -> bool { false }
            fn fill_rx(&mut self, _rx: &mut Virtqueue, _mem: &GuestRam) -> bool { true }
        }
        let backing = Box::leak(vec![0u8; 0x6000].into_boxed_slice());
        let mem = GuestRam::new(backing.as_mut_ptr(), backing.len(), 0x4000_0000);
        let irq = Arc::new(RecIrq::default());
        let mut t = VirtioMmio::new("vsock", Box::new(FillDev), mem, irq.clone());
        // Configure queue 0 ready the same way the notify test does.
        let base: u64 = 0x4000_0000;
        let desc = base + 0x1000;
        let avail = base + 0x2000;
        let used_ring = base + 0x3000;
        wr(&mut t, 0x030, 0); // QueueSel = 0
        wr(&mut t, 0x080, desc as u32);
        wr(&mut t, 0x084, (desc >> 32) as u32);
        wr(&mut t, 0x090, avail as u32);
        wr(&mut t, 0x094, (avail >> 32) as u32);
        wr(&mut t, 0x0a0, used_ring as u32);
        wr(&mut t, 0x0a4, (used_ring >> 32) as u32);
        wr(&mut t, 0x038, 8); // QueueNum
        wr(&mut t, 0x044, 1); // QueueReady
        assert!(t.poll_vsock_rx());
        assert_eq!(*irq.level.lock().unwrap(), Some(true));
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
        let st = d.save_state();
        let mut backing2 = vec![0u8; 0x6000];
        let mut d2 = dev(&mut backing2, Arc::new(FakeIrq::default()));
        d2.restore_state(&st);
        assert_eq!(d2.save_state(), st); // round-trips
    }
}
