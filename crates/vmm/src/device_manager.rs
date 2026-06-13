//! Device registry: owns the MMIO bus, bump-allocates MMIO windows + SPIs, mints
//! GIC-backed IrqLines, and enumerates devices for FDT + snapshot.

use std::sync::{Arc, Mutex};

use arch::aarch64::fdt::{FdtDevice, MmioDev};
use devices::bus::{Bus, BusDevice};
use devices::device::{DeviceMgrError, FdtKind, MmioDevice};
use devices::virtio::IrqLine;
use hvf::gic::HvfGicV3;

/// Mints `IrqLine`s backed by the in-kernel GIC. INTID = SPI index + 32.
struct GicIrq {
    gic: Arc<HvfGicV3>,
    intid: u32,
}
impl IrqLine for GicIrq {
    fn set_spi(&self, level: bool) {
        let _ = self.gic.set_spi(self.intid, level);
    }
}

/// One placed device + the resources it occupies.
struct Record {
    id: String,
    base: u64,
    size: u64,
    spi: u32,
    fdt_kind: FdtKind,
    dev: Arc<Mutex<dyn MmioDevice>>,
}

/// Serializable projection of a `Record` (no live device handle).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DeviceRecord {
    pub id: String,
    pub base: u64,
    pub size: u64,
    pub spi: u32,
    pub fdt_kind: FdtKind,
    pub state: serde_json::Value,
}

pub struct DeviceManager {
    gic: Arc<HvfGicV3>,
    bus: Bus,
    mmio_next: u64,
    mmio_end: u64,
    spi_next: u32,
    spi_end: u32,
    records: Vec<Record>,
}

impl DeviceManager {
    pub fn new(gic: Arc<HvfGicV3>, mmio_base: u64, mmio_len: u64, spi_base: u32, spi_count: u32) -> Self {
        Self {
            gic,
            bus: Bus::new(),
            mmio_next: mmio_base,
            mmio_end: mmio_base + mmio_len,
            spi_next: spi_base,
            spi_end: spi_base + spi_count,
            records: Vec::new(),
        }
    }

    fn alloc(&mut self, size: u64) -> Result<(u64, u32), DeviceMgrError> {
        if self.mmio_next + size > self.mmio_end {
            return Err(DeviceMgrError::WindowExhausted { need: size, remaining: self.mmio_end - self.mmio_next });
        }
        if self.spi_next >= self.spi_end {
            return Err(DeviceMgrError::SpiExhausted);
        }
        let base = self.mmio_next;
        let spi = self.spi_next;
        self.mmio_next += size;
        self.spi_next += 1;
        Ok((base, spi))
    }

    fn irq_for(&self, spi: u32) -> Arc<dyn IrqLine> {
        Arc::new(GicIrq { gic: self.gic.clone(), intid: spi + 32 })
    }

    fn place(&mut self, base: u64, size: u64, spi: u32, dev: Arc<Mutex<dyn MmioDevice>>) -> Result<(), DeviceMgrError> {
        let (id, fdt_kind) = {
            let d = dev.lock().unwrap();
            (d.snapshot_id().to_string(), d.fdt_kind())
        };
        let bus_dev: Arc<Mutex<dyn BusDevice>> = dev.clone(); // trait upcast (Rust >= 1.86)
        self.bus.register(base, size, bus_dev).map_err(DeviceMgrError::BusOverlap)?;
        self.records.push(Record { id, base, size, spi, fdt_kind, dev });
        Ok(())
    }

    /// Allocate a window+SPI, build the device with its IrqLine, place it, and
    /// return the typed handle (for the stdin reader etc.).
    pub fn add<D, F>(&mut self, window_size: u64, build: F) -> Result<Arc<Mutex<D>>, DeviceMgrError>
    where
        D: MmioDevice + 'static,
        F: FnOnce(Arc<dyn IrqLine>) -> D,
    {
        let (base, spi) = self.alloc(window_size)?;
        let typed = Arc::new(Mutex::new(build(self.irq_for(spi))));
        let dyn_dev: Arc<Mutex<dyn MmioDevice>> = typed.clone();
        self.place(base, window_size, spi, dyn_dev)?;
        Ok(typed)
    }

    /// FDT device descriptors for `fdt::generate` (call before `freeze`).
    pub fn fdt_devices(&self) -> Vec<FdtDevice> {
        self.records
            .iter()
            .map(|r| {
                let m = MmioDev { addr: r.base, size: r.size, irq: r.spi };
                match r.fdt_kind {
                    FdtKind::Ns16550a => FdtDevice::Serial(m),
                    FdtKind::VirtioMmio => FdtDevice::VirtioMmio(m),
                }
            })
            .collect()
    }

    /// Freeze for the run loop. After this, no more devices can be added.
    pub fn freeze(self) -> FrozenDevices {
        FrozenDevices { bus: Arc::new(self.bus), records: self.records }
    }
}

/// A frozen device set: shareable (`Send + Sync`) across the vCPU thread for the
/// run loop (bus) and the snapshot handler (`save`).
pub struct FrozenDevices {
    bus: Arc<Bus>,
    records: Vec<Record>,
}

impl FrozenDevices {
    pub fn bus(&self) -> Arc<Bus> {
        self.bus.clone()
    }
    /// Snapshot every device: self-describing records the restore path replays.
    pub fn save(&self) -> Vec<DeviceRecord> {
        self.records
            .iter()
            .map(|r| DeviceRecord {
                id: r.id.clone(),
                base: r.base,
                size: r.size,
                spi: r.spi,
                fdt_kind: r.fdt_kind,
                state: r.dev.lock().unwrap().save(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_is_sequential_and_bounds_checked() {
        // Allocator logic mirrored without a GIC (HVF calls need the entitlement).
        // region [0x1000, 0x1600): 3 windows of 0x200; SPIs 5..8.
        let end = 0x1000 + 0x600u64;
        let spi_end = 8u32;
        let mut next = 0x1000u64;
        let mut spi = 5u32;
        let mut alloc = |size: u64| -> Result<(u64, u32), &'static str> {
            if next + size > end { return Err("window"); }
            if spi >= spi_end { return Err("spi"); }
            let b = next; let s = spi; next += size; spi += 1; Ok((b, s))
        };
        assert_eq!(alloc(0x200), Ok((0x1000, 5)));
        assert_eq!(alloc(0x200), Ok((0x1200, 6)));
        assert_eq!(alloc(0x200), Ok((0x1400, 7)));
        assert_eq!(alloc(0x200), Err("window"));
    }

    #[test]
    fn device_record_serde_roundtrips() {
        let r = DeviceRecord {
            id: "virtio-blk".into(), base: 0xa00_0000, size: 0x200, spi: 1,
            fdt_kind: FdtKind::VirtioMmio, state: serde_json::json!({"status": 7}),
        };
        let back: DeviceRecord = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back, r);
    }
}
