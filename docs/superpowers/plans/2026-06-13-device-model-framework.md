# Device-Model Framework (DeviceManager) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Introduce a `DeviceManager` and a uniform `MmioDevice` trait that own MMIO-window + SPI allocation, bus registration, FDT-node description, and snapshot enumeration; port serial/blk/net onto it so adding a device is one `add` call.

**Architecture:** A `MmioDevice` trait (in `devices`) extends `BusDevice` with FDT kind, a stable snapshot id, and `save`/`restore` over `serde_json::Value`. A `DeviceManager` (in `vmm`) bump-allocates MMIO windows and SPIs, mints GIC-backed `IrqLine`s, registers devices on the `Bus`, and produces both the FDT device list and a self-describing snapshot record list. `boot.rs` shrinks to a few `add` calls; the snapshot format becomes a device-record list with a version guard.

**Tech Stack:** Rust (edition 2024), serde / serde_json, trait upcasting (stable ≥1.86), the existing `Bus`/`VirtioMmio`/`Serial`/`HvfGicV3` code.

**Spec:** `docs/superpowers/specs/2026-06-13-device-model-framework-design.md`

**Scope note:** This is sub-project A of the "full device model" milestone. RTC (`Pl031`) and the new virtio devices (rng/balloon/vsock) are deferred to sub-projects B–E. This plan therefore implements exactly two FDT kinds — `Ns16550a` and `VirtioMmio` — and adds the `Pl031` variant when sub-project C lands.

---

## File structure

- `crates/devices/src/device.rs` *(new)* — `FdtKind`, `DeviceMgrError`, `MmioDevice` trait. One responsibility: the device-model vocabulary shared by devices and the manager.
- `crates/devices/src/lib.rs` *(modify)* — `pub mod device;` and re-exports.
- `crates/devices/src/serial.rs` *(modify)* — store `irq`; `impl MmioDevice for Serial`; rename inherent `save` → `save_state`.
- `crates/devices/src/virtio/mmio.rs` *(modify)* — add `id` to `VirtioMmio`; `impl MmioDevice`; rename inherent `save`/`restore` → `save_state`/`restore_state`.
- `crates/devices/src/virtio/mod.rs` *(modify)* — add a `NoopIrq` for irq-less construction (tests).
- `crates/arch/src/aarch64/fdt.rs` *(modify)* — collapse `FdtDevice::{VirtioBlk,VirtioNet}` into `FdtDevice::VirtioMmio`.
- `crates/vmm/src/device_manager.rs` *(new)* — `DeviceManager`, allocators, `GicIrq` adapter, `DeviceRecord`.
- `crates/vmm/src/lib.rs` *(modify)* — `pub mod device_manager;`.
- `crates/vmm/src/snapshot.rs` *(modify)* — version field + magic bump; replace `VmConfig.{serial,blk}` and `DeviceState` with `Vec<DeviceRecord>`.
- `spike/src/bin/boot.rs` *(modify)* — fresh-boot and restore wiring through `DeviceManager`.

---

## Task 1: `MmioDevice` trait, `FdtKind`, `DeviceMgrError`

**Files:**
- Create: `crates/devices/src/device.rs`
- Modify: `crates/devices/src/lib.rs`

- [ ] **Step 1: Write the failing test**

In `crates/devices/src/device.rs`:

```rust
//! Device-model vocabulary: the trait + types shared by devices and the
//! DeviceManager. `MmioDevice` extends `BusDevice` so one trait object serves
//! both the bus (upcast) and the manager.

use crate::bus::{BusDevice, BusError};
use serde::{Deserialize, Serialize};

/// Which FDT node shape a device emits. (RTC `Pl031` arrives with sub-project C.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FdtKind {
    Ns16550a,
    VirtioMmio,
}

/// Failures from device placement / restore.
#[derive(Debug)]
pub enum DeviceMgrError {
    WindowExhausted { need: u64, remaining: u64 },
    SpiExhausted,
    BusOverlap(BusError),
    UnknownDeviceId(String),
    StateInvalid { id: String, reason: String },
}

impl std::fmt::Display for DeviceMgrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeviceMgrError::WindowExhausted { need, remaining } => {
                write!(f, "MMIO window exhausted: need {need:#x}, {remaining:#x} left")
            }
            DeviceMgrError::SpiExhausted => write!(f, "SPI range exhausted"),
            DeviceMgrError::BusOverlap(e) => write!(f, "bus overlap: {e}"),
            DeviceMgrError::UnknownDeviceId(id) => write!(f, "no builder for device id {id:?}"),
            DeviceMgrError::StateInvalid { id, reason } => {
                write!(f, "invalid saved state for {id:?}: {reason}")
            }
        }
    }
}

impl std::error::Error for DeviceMgrError {}

/// A memory-mapped device the `DeviceManager` can place, describe in the FDT, and
/// snapshot. Extends `BusDevice`: a single `Arc<Mutex<dyn MmioDevice>>` is upcast
/// to `Arc<Mutex<dyn BusDevice>>` for the bus.
pub trait MmioDevice: BusDevice {
    fn fdt_kind(&self) -> FdtKind;
    /// Stable key for the snapshot record, e.g. "serial", "virtio-blk".
    fn snapshot_id(&self) -> &str;
    /// Serialize device state for the snapshot.
    fn save(&self) -> serde_json::Value;
    /// Apply restored state; called after construction, before first run.
    fn restore(&mut self, v: &serde_json::Value) -> Result<(), DeviceMgrError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct Mock {
        id: String,
        last: serde_json::Value,
    }
    impl BusDevice for Mock {}
    impl MmioDevice for Mock {
        fn fdt_kind(&self) -> FdtKind { FdtKind::VirtioMmio }
        fn snapshot_id(&self) -> &str { &self.id }
        fn save(&self) -> serde_json::Value { self.last.clone() }
        fn restore(&mut self, v: &serde_json::Value) -> Result<(), DeviceMgrError> {
            self.last = v.clone();
            Ok(())
        }
    }

    #[test]
    fn mmio_device_upcasts_to_bus_device() {
        let dev: Arc<Mutex<dyn MmioDevice>> =
            Arc::new(Mutex::new(Mock { id: "m".into(), last: serde_json::Value::Null }));
        // Trait upcast (stable since Rust 1.86): MmioDevice -> BusDevice.
        let bus: Arc<Mutex<dyn BusDevice>> = dev.clone();
        bus.lock().unwrap().write(0, 0, &[1]); // default no-op, must not panic
        assert_eq!(dev.lock().unwrap().snapshot_id(), "m");
    }

    #[test]
    fn fdt_kind_serde_roundtrips() {
        let j = serde_json::to_value(FdtKind::Ns16550a).unwrap();
        assert_eq!(serde_json::from_value::<FdtKind>(j).unwrap(), FdtKind::Ns16550a);
    }

    #[test]
    fn save_restore_roundtrips_on_mock() {
        let mut m = Mock { id: "m".into(), last: serde_json::json!({"a": 1}) };
        let saved = m.save();
        m.restore(&serde_json::json!({"a": 2})).unwrap();
        assert_eq!(m.save(), serde_json::json!({"a": 2}));
        assert_eq!(saved, serde_json::json!({"a": 1}));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ignition-devices device::`
Expected: FAIL to compile — `crate::device` not declared in `lib.rs`.

- [ ] **Step 3: Wire the module**

In `crates/devices/src/lib.rs` add near the other `pub mod` lines:

```rust
pub mod device;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p ignition-devices device::`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/devices/src/device.rs crates/devices/src/lib.rs
git commit -m "feat(devices): MmioDevice trait, FdtKind, DeviceMgrError"
```

---

## Task 2: `impl MmioDevice for VirtioMmio`

**Files:**
- Modify: `crates/devices/src/virtio/mmio.rs`

- [ ] **Step 1: Add an `id` field + rename inherent save/restore (write the failing test first)**

Append to the `#[cfg(test)] mod tests` in `crates/devices/src/virtio/mmio.rs` (reuse the file's existing test scaffolding for a mock `VirtioDevice` + `GuestRam` + `IrqLine`; if none exists, construct with the real types as the other tests in this file do):

```rust
#[test]
fn mmio_device_trait_roundtrips_transport_state() {
    use crate::device::{FdtKind, MmioDevice};
    let mut t = test_transport("virtio-blk"); // helper: builds VirtioMmio over a mock dev
    assert_eq!(t.fdt_kind(), FdtKind::VirtioMmio);
    assert_eq!(t.snapshot_id(), "virtio-blk");
    let saved = MmioDevice::save(&t);
    MmioDevice::restore(&mut t, &saved).unwrap();
    assert_eq!(MmioDevice::save(&t), saved);
}
```

Add a `test_transport` helper in the same test module that builds a `VirtioMmio` with the file's existing mock `VirtioDevice` and a no-op `GuestRam`/`IrqLine` (mirror the construction already used by this file's other tests; use `crate::virtio::mod`'s `NoopIrq` from Task 3 if needed, or a local mock impl of `IrqLine`).

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ignition-devices virtio::mmio::tests::mmio_device_trait_roundtrips`
Expected: FAIL to compile — `VirtioMmio::new` takes 3 args (no `id`), and `MmioDevice` not implemented.

- [ ] **Step 3: Implement**

In `crates/devices/src/virtio/mmio.rs`:

1. Add `id: &'static str` to the struct and constructor:

```rust
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
            id, dev, mem, irq, queues,
            status: 0, device_features_sel: 0, queue_sel: 0, interrupt_status: 0,
        }
    }
```

2. Rename the existing inherent `pub fn save(&self) -> VirtioMmioState` to `pub fn save_state(&self) -> VirtioMmioState` and `pub fn restore(&mut self, s: &VirtioMmioState)` to `pub fn restore_state(&mut self, s: &VirtioMmioState)` (bodies unchanged).

3. Add the trait impl at the end of the file:

```rust
use crate::device::{DeviceMgrError, FdtKind, MmioDevice};

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
```

(The `restore_state` `debug_assert_eq!` on queue length is now backstopped by the checked error above.)

- [ ] **Step 4: Run tests**

Run: `cargo test -p ignition-devices virtio::mmio`
Expected: PASS. (Note: existing callers of `VirtioMmio::new`/`save`/`restore` in `boot.rs` now break — they are rewritten in Tasks 8–9. Build `boot` later; `-p ignition-devices` is green now.)

- [ ] **Step 5: Commit**

```bash
git add crates/devices/src/virtio/mmio.rs
git commit -m "feat(devices): VirtioMmio implements MmioDevice (id + Value save/restore)"
```

---

## Task 3: `impl MmioDevice for Serial` + `NoopIrq`

**Files:**
- Modify: `crates/devices/src/virtio/mod.rs` (add `NoopIrq`)
- Modify: `crates/devices/src/serial.rs`

- [ ] **Step 1: Write the failing test**

Add to `crates/devices/src/serial.rs` test module:

```rust
#[test]
fn serial_mmio_device_roundtrips() {
    use crate::device::{FdtKind, MmioDevice};
    use crate::virtio::NoopIrq;
    use std::sync::Arc;
    let irq = Arc::new(NoopIrq);
    let mut s = Serial::with_irq(SinkWriter, irq); // SinkWriter: existing test writer impl
    // dirty a register so save captures non-default state
    s.write(0, 1, &[0xab]); // IER
    let saved = MmioDevice::save(&s);
    assert_eq!(s.fdt_kind(), FdtKind::Ns16550a);
    assert_eq!(s.snapshot_id(), "serial");
    let mut s2 = Serial::with_irq(SinkWriter, Arc::new(NoopIrq));
    MmioDevice::restore(&mut s2, &saved).unwrap();
    assert_eq!(MmioDevice::save(&s2), saved);
}
```

(If the test module lacks a zero-size writer, add `#[derive(Clone, Default)] struct SinkWriter;` with a `Write` impl that discards bytes.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ignition-devices serial::tests::serial_mmio_device_roundtrips`
Expected: FAIL — `NoopIrq` missing, `MmioDevice` not implemented, `Serial` has no `irq` field.

- [ ] **Step 3: Implement**

In `crates/devices/src/virtio/mod.rs`, after the `IrqLine` trait:

```rust
/// An `IrqLine` that drops all assertions — for irq-less construction (tests,
/// and the manager before a real GIC line is attached).
pub struct NoopIrq;
impl IrqLine for NoopIrq {
    fn set_spi(&self, _level: bool) {}
}
```

In `crates/devices/src/serial.rs`:

1. Add an `irq` field and store it in constructors:

```rust
pub struct Serial<W: Write + Send> {
    inner: vm_superio::Serial<SerialIrq, NoEvents, W>,
    irq: Arc<dyn IrqLine>,
}

impl<W: Write + Send> Serial<W> {
    pub fn new(out: W) -> Self {
        let irq: Arc<dyn IrqLine> = Arc::new(crate::virtio::NoopIrq);
        Self { inner: vm_superio::Serial::new(SerialIrq::Noop, NoEvents, out), irq }
    }
    pub fn with_irq(out: W, irq: Arc<dyn IrqLine>) -> Self {
        let inner = vm_superio::Serial::new(SerialIrq::Gic(irq.clone()), NoEvents, out);
        Self { inner, irq }
    }
```

(Adjust the exact `vm_superio::Serial::new` trigger arg to match the current code; the only change is storing `irq` and cloning it into the trigger. If `new` currently uses a different `SerialIrq` variant, keep it but still set `self.irq` to a `NoopIrq`.)

2. Rename inherent `pub fn save(&self) -> SerialSnapshot` to `pub fn save_state(&self) -> SerialSnapshot` (body unchanged). Keep `from_snapshot` as-is.

3. Add the trait impl (requires `W: Default` so `restore` can rebuild in place):

```rust
use crate::device::{DeviceMgrError, FdtKind, MmioDevice};

impl<W: Write + Send + Default> MmioDevice for Serial<W> {
    fn fdt_kind(&self) -> FdtKind { FdtKind::Ns16550a }
    fn snapshot_id(&self) -> &str { "serial" }
    fn save(&self) -> serde_json::Value {
        serde_json::to_value(self.save_state()).expect("SerialSnapshot serializes")
    }
    fn restore(&mut self, v: &serde_json::Value) -> Result<(), DeviceMgrError> {
        let snap: SerialSnapshot = serde_json::from_value(v.clone())
            .map_err(|e| DeviceMgrError::StateInvalid { id: "serial".into(), reason: e.to_string() })?;
        // vm_superio applies serial state only at construction, so rebuild in place
        // from the stored irq + a fresh writer (W: Default).
        *self = Serial::from_snapshot(W::default(), self.irq.clone(), &snap);
        Ok(())
    }
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p ignition-devices serial`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/devices/src/serial.rs crates/devices/src/virtio/mod.rs
git commit -m "feat(devices): Serial implements MmioDevice; add NoopIrq"
```

---

## Task 4: Collapse `arch::fdt::FdtDevice` to kind-driven nodes

**Files:**
- Modify: `crates/arch/src/aarch64/fdt.rs`

- [ ] **Step 1: Update the failing test**

Change the existing `virtio_node_present_only_when_set` test (and any using `FdtDevice::VirtioBlk`/`VirtioNet`) to the new variant:

```rust
cfg.devices.push(FdtDevice::VirtioMmio(MmioDev { addr: 0x0a00_0000, size: 0x200, irq: 1 }));
```

Add a test asserting two virtio devices both render as `virtio,mmio`:

```rust
#[test]
fn two_virtio_devices_both_render() {
    let mut cfg = sample();
    cfg.devices.push(FdtDevice::VirtioMmio(MmioDev { addr: 0x0a00_0000, size: 0x200, irq: 1 }));
    cfg.devices.push(FdtDevice::VirtioMmio(MmioDev { addr: 0x0a00_0200, size: 0x200, irq: 2 }));
    let blob = generate(&cfg).unwrap();
    let fdt = Fdt::new(&blob).unwrap();
    assert!(fdt.find_node("/virtio_mmio@a000000").is_some());
    assert!(fdt.find_node("/virtio_mmio@a000200").is_some());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ignition-arch fdt`
Expected: FAIL to compile — `FdtDevice::VirtioMmio` doesn't exist (only `VirtioBlk`/`VirtioNet`).

- [ ] **Step 3: Implement**

In `crates/arch/src/aarch64/fdt.rs`:

```rust
pub enum FdtDevice {
    /// 16550-compatible serial -> `ns16550a` node.
    Serial(MmioDev),
    /// Any virtio-mmio device -> `virtio,mmio` node (kernel reads the device id
    /// from the mmio registers, so blk/net/rng/... share one node shape).
    VirtioMmio(MmioDev),
}
```

Update the match in `generate`:

```rust
FdtDevice::Serial(m) => create_serial_node(&mut fdt, m)?,
FdtDevice::VirtioMmio(m) => create_virtio_node(&mut fdt, m)?,
```

(`create_serial_node` / `create_virtio_node` are unchanged.)

- [ ] **Step 4: Run tests**

Run: `cargo test -p ignition-arch fdt`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/arch/src/aarch64/fdt.rs
git commit -m "refactor(arch): collapse FdtDevice virtio variants into VirtioMmio"
```

---
## Task 5: `DeviceManager` — allocators, `add`, `fdt_devices`

**Files:**
- Create: `crates/vmm/src/device_manager.rs`
- Modify: `crates/vmm/src/lib.rs`

**API decided up front (used unchanged through Task 9):**
- `add<D, F>(&mut self, window_size, build) -> Result<Arc<Mutex<D>>, DeviceMgrError>` where `D: MmioDevice + 'static`, `F: FnOnce(Arc<dyn IrqLine>) -> D`. Allocates window+SPI, builds the device, registers it on the bus (upcast), records it, and **returns the typed `Arc`** so the caller (e.g. the stdin reader) keeps a handle.
- `add_restored<D, F>(&mut self, rec, build) -> Result<Arc<Mutex<D>>, DeviceMgrError>` — same, but places at `rec.base`/`rec.spi` and applies `rec.state`.
- `fdt_devices(&self) -> Vec<FdtDevice>` — before freezing.
- `freeze(self) -> FrozenDevices` — consumes the manager; `FrozenDevices` is `Send + Sync` and offers `bus() -> Arc<Bus>` and `save() -> Vec<DeviceRecord>`.

- [ ] **Step 1: Write the failing test**

Create `crates/vmm/src/device_manager.rs`:

```rust
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
```

> Why no full `DeviceManager` unit test: `irq_for` holds an `Arc<HvfGicV3>`, and a real GIC needs the hypervisor entitlement (not available under `cargo test`). The allocator and the serde contract are the unit-testable surface; end-to-end placement is covered by the live drivers in Task 10.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ignition-vmm device_manager::`
Expected: FAIL to compile — module not declared in `lib.rs`.

- [ ] **Step 3: Wire the module**

In `crates/vmm/src/lib.rs`:

```rust
pub mod device_manager;
```

- [ ] **Step 4: Run tests + clippy**

Run: `cargo test -p ignition-vmm device_manager:: && cargo clippy -p ignition-vmm`
Expected: PASS (2 tests), 0 warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/vmm/src/device_manager.rs crates/vmm/src/lib.rs
git commit -m "feat(vmm): DeviceManager (allocators, add, fdt_devices, freeze/save)"
```

---

## Task 6: `DeviceManager::add_restored`

**Files:**
- Modify: `crates/vmm/src/device_manager.rs`

- [ ] **Step 1: Write the failing test**

Add to the `tests` module:

```rust
#[test]
fn restored_record_state_serde_roundtrips() {
    // add_restored applies rec.state via MmioDevice::restore; here we assert the
    // record carrying arbitrary state survives serde (the snapshot transport).
    let r = DeviceRecord {
        id: "serial".into(), base: 0x900_0000, size: 0x1000, spi: 0,
        fdt_kind: FdtKind::Ns16550a, state: serde_json::json!({"scratch": 9}),
    };
    let back: DeviceRecord = serde_json::from_value(serde_json::to_value(&r).unwrap()).unwrap();
    assert_eq!(back.state, serde_json::json!({"scratch": 9}));
}
```

- [ ] **Step 2: Run test to verify it fails (or guard-passes)**

Run: `cargo test -p ignition-vmm device_manager::tests::restored_record_state`
Expected: PASS as a serde guard; the new behavior is `add_restored` (added next).

- [ ] **Step 3: Implement `add_restored`**

Add to `impl DeviceManager`:

```rust
/// Restore one device at its SAVED base/SPI (exact replay, not freshly
/// allocated). `build` constructs the device fresh; `rec.state` is then applied
/// via `MmioDevice::restore`. Returns the typed handle.
pub fn add_restored<D, F>(&mut self, rec: &DeviceRecord, build: F) -> Result<Arc<Mutex<D>>, DeviceMgrError>
where
    D: MmioDevice + 'static,
    F: FnOnce(Arc<dyn IrqLine>) -> D,
{
    let mut dev = build(self.irq_for(rec.spi));
    dev.restore(&rec.state)?;
    let typed = Arc::new(Mutex::new(dev));
    let dyn_dev: Arc<Mutex<dyn MmioDevice>> = typed.clone();
    // keep the bump allocators ahead of restored resources so a later add() won't collide
    self.mmio_next = self.mmio_next.max(rec.base + rec.size);
    self.spi_next = self.spi_next.max(rec.spi + 1);
    self.place(rec.base, rec.size, rec.spi, dyn_dev)?;
    Ok(typed)
}
```

- [ ] **Step 4: Run tests + clippy**

Run: `cargo test -p ignition-vmm device_manager:: && cargo clippy -p ignition-vmm`
Expected: PASS, 0 warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/vmm/src/device_manager.rs
git commit -m "feat(vmm): DeviceManager::add_restored (exact-replay placement)"
```

---

## Task 7: Snapshot format — device-record list + version guard

**Files:**
- Modify: `crates/vmm/src/snapshot.rs`

- [ ] **Step 1: Write the failing test**

Replace the snapshot round-trip test(s) in `crates/vmm/src/snapshot.rs` with the new shape:

```rust
#[test]
fn snapshot_roundtrips_with_device_records() {
    use crate::device_manager::DeviceRecord;
    use devices::device::FdtKind;
    let snap = VmSnapshot::new(
        VmConfig { mem_size: 0x2000_0000, vcpu_count: 1 },
        VcpuState::default(),
        vec![DeviceRecord {
            id: "serial".into(), base: 0x900_0000, size: 0x1000, spi: 0,
            fdt_kind: FdtKind::Ns16550a, state: serde_json::json!({"scratch": 7}),
        }],
    );
    let json = serde_json::to_string(&snap).unwrap();
    let back: VmSnapshot = serde_json::from_str(&json).unwrap();
    assert_eq!(back.version, SNAP_VERSION);
    assert_eq!(back.magic, SNAP_MAGIC);
    assert_eq!(back.devices.len(), 1);
    assert_eq!(back.devices[0].id, "serial");
}

#[test]
fn check_version_rejects_old() {
    let bad = serde_json::json!({
        "magic": SNAP_MAGIC, "version": 0,
        "config": {"mem_size": 1, "vcpu_count": 1},
        "vcpu": VcpuState::default(), "devices": []
    });
    let parsed: VmSnapshot = serde_json::from_value(bad).unwrap();
    assert!(check_version(&parsed).is_err());
}
```

(If `VcpuState` is not `Default`, build a minimal instance the way the existing snapshot tests do.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ignition-vmm snapshot`
Expected: FAIL to compile — `VmConfig` still has `serial`/`blk`; `DeviceState` referenced; no `version`/`SNAP_VERSION`/`check_version`/`devices`.

- [ ] **Step 3: Implement**

In `crates/vmm/src/snapshot.rs`:

```rust
pub const SNAP_MAGIC: &str = "ignition-snapshot-v2";
pub const SNAP_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmConfig {
    pub mem_size: u64,
    pub vcpu_count: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VmSnapshot {
    pub magic: String,
    pub version: u32,
    pub config: VmConfig,
    pub vcpu: VcpuState,
    pub devices: Vec<crate::device_manager::DeviceRecord>,
}

impl VmSnapshot {
    pub fn new(config: VmConfig, vcpu: VcpuState, devices: Vec<crate::device_manager::DeviceRecord>) -> Self {
        Self { magic: SNAP_MAGIC.to_string(), version: SNAP_VERSION, config, vcpu, devices }
    }
}

/// Reject snapshots this binary can't restore.
pub fn check_version(s: &VmSnapshot) -> io::Result<()> {
    if s.magic != SNAP_MAGIC || s.version != SNAP_VERSION {
        return Err(io::Error::other(format!(
            "incompatible snapshot: magic={:?} version={} (want {:?} v{})",
            s.magic, s.version, SNAP_MAGIC, SNAP_VERSION
        )));
    }
    Ok(())
}
```

Delete the `MmioWindow` and `DeviceState` structs. In `read_snapshot`, replace the bare `if snap.magic != SNAP_MAGIC { ... }` check with `check_version(&snap)?`.

- [ ] **Step 4: Run tests**

Run: `cargo test -p ignition-vmm snapshot`
Expected: PASS. (`boot.rs` still references old fields — fixed in Task 8.)

- [ ] **Step 5: Commit**

```bash
git add crates/vmm/src/snapshot.rs
git commit -m "feat(vmm): snapshot v2 — device-record list + version guard"
```

---

## Task 8: `boot.rs` fresh boot + snapshot through `DeviceManager`

**Files:**
- Modify: `crates/arch/src/aarch64/layout.rs`
- Modify: `spike/src/bin/boot.rs`

This task does fresh-boot wiring AND the snapshot handler together, because `boot.rs` won't compile until both the old `bus.register`/`FdtDevice`/`VmConfig` uses are replaced.

- [ ] **Step 1: Device-region constants**

In `crates/arch/src/aarch64/layout.rs`, remove `SERIAL_BASE/SIZE/SPI`, `VIRTIO_BASE/SIZE/SPI`, `NET_BASE/SIZE/SPI`; add:

```rust
/// Device MMIO region; serial + virtio windows are bump-allocated here.
pub const MMIO_BASE: u64 = 0x0900_0000;
pub const MMIO_LEN: u64 = 0x0020_0000; // 2 MiB of device space
/// Per-device window size; 16550 and virtio-mmio both fit in 0x1000.
pub const MMIO_WINDOW: u64 = 0x1000;
/// SPI allocation range (FDT interrupt index; GIC INTID = index + 32).
pub const SPI_BASE: u32 = 0;
pub const SPI_COUNT: u32 = 32;
```

(Serial is added first, so it lands at `MMIO_BASE = 0x0900_0000` — unchanged from today. Keep `RAM_BASE`, `FDT_MAX_SIZE`, `fdt_addr`, etc.)

- [ ] **Step 2: `FlushWriter: Default`**

In `spike/src/bin/boot.rs`, ensure the stdout writer is `Default`:

```rust
#[derive(Default)]
struct FlushWriter;
```

(Keep its existing `Write` impl.)

- [ ] **Step 3: Fresh-boot wiring**

Delete the local `GicIrq` struct (now in `DeviceManager`), the per-device `bus.register` calls, and the hand-built `fdt_devices` vector. Replace with:

```rust
use vmm::device_manager::DeviceManager;
use arch::aarch64::layout;

// gic: Arc<HvfGicV3> already created.
let mut mgr = DeviceManager::new(
    gic.clone(), layout::MMIO_BASE, layout::MMIO_LEN, layout::SPI_BASE, layout::SPI_COUNT);

// Serial (always). Returns the typed Arc for the stdin reader.
let serial = mgr
    .add(layout::MMIO_WINDOW, |irq| Serial::with_irq(FlushWriter, irq))
    .expect("add serial");

if let Some(ref path) = disk_path {
    let disk_file = fs::OpenOptions::new().read(true).write(true).open(path).expect("open disk");
    let blk = VirtioBlk::new(disk_file).expect("VirtioBlk::new");
    let guest_ram = GuestRam::new(host as *mut u8, RAM_SIZE as usize, layout::RAM_BASE);
    mgr.add(layout::MMIO_WINDOW, move |irq| VirtioMmio::new("virtio-blk", Box::new(blk), guest_ram, irq))
        .expect("add blk");
}
if net {
    let net_dev = /* existing VirtioNet::new construction */;
    let guest_ram_net = GuestRam::new(host as *mut u8, RAM_SIZE as usize, layout::RAM_BASE);
    mgr.add(layout::MMIO_WINDOW, move |irq| VirtioMmio::new("virtio-net", Box::new(net_dev), guest_ram_net, irq))
        .expect("add net");
}

let cfg = FdtConfig {
    mem_base: layout::RAM_BASE,
    mem_size: RAM_SIZE,
    cpu_mpidrs: (0..smp).map(mpidr_for).collect(),
    cmdline: /* existing cmdline */,
    devices: mgr.fdt_devices(),
    gic: gic.fdt_info(),
    initrd: None,
};
let dtb = fdt::generate(&cfg).expect("fdt generate failed");
// ... existing: copy dtb into RAM at fdt_addr ...

// Freeze: bus for the run loop, shareable handle for the snapshot handler.
let frozen = Arc::new(mgr.freeze());
let bus = frozen.bus();
```

Update the stdin reader to take the typed `serial` Arc (it already expects `Arc<Mutex<Serial<FlushWriter>>>`) — pass `serial.clone()`.

- [ ] **Step 4: Snapshot handler**

The handler captures `frozen` (for device state) and the GIC. Replace the body that built `VmConfig`/`DeviceState` by hand:

```rust
let snap_devices = frozen.clone();
let gic_for_handler = gic.clone();
let mut manager = VcpuManager::new(smp, bus.clone());
manager.set_snapshot_handler(Box::new(move |vcpu: &HvfVcpu| {
    let vcpu_state = vcpu.save_state().expect("save vcpu");
    let devices = snap_devices.save();
    let gic_blob = gic_for_handler.save_state().expect("save gic");
    let config = VmConfig { mem_size: RAM_SIZE, vcpu_count: 1 };
    let snap = VmSnapshot::new(config, vcpu_state, devices);
    // ... existing artifact writing: memory.bin, gic.bin, disk.img, vmstate.json(snap) ...
}));
```

Adjust imports: `use vmm::snapshot::{self, VmConfig, VmSnapshot};` (drop `MmioWindow`, `DeviceState`, `MmioDev` from the snapshot import if present).

- [ ] **Step 5: Build, sign, smoke test**

Run:
```bash
cargo build -p hvf-spike --bin boot && scripts/sign.sh target/debug/boot
target/debug/boot kimage/out/Image kimage/out/rootfs.ext4   # reaches login:, Ctrl-A x to quit
```
Expected: builds clean; boots to `login:` exactly as before.

- [ ] **Step 6: Commit**

```bash
git add spike/src/bin/boot.rs crates/arch/src/aarch64/layout.rs
git commit -m "refactor(boot): fresh boot + snapshot via DeviceManager"
```

---

## Task 9: `boot.rs` restore through `DeviceManager::add_restored`

**Files:**
- Modify: `spike/src/bin/boot.rs`

- [ ] **Step 1: Rewrite `run_restore` device construction**

Replace the hand-built bus/device construction with the manager + an id-keyed match. `read_snapshot` already calls `check_version` (Task 7), so the version is validated on load.

```rust
let (snap, gic_blob, paths) = snapshot::read_snapshot(dir)?;
assert_eq!(snap.config.vcpu_count, 1, "restore only supports single-vCPU snapshots");
assert_eq!(snap.config.mem_size, RAM_SIZE, "snapshot mem_size != this binary's RAM_SIZE");

// ... existing: mmap RAM, load memory.bin into it, create VM ...
let gic = Arc::new(HvfGicV3::new(snap.config.vcpu_count, layout::RAM_BASE)
    .map_err(|e| io::Error::other(format!("GIC create: {e}")))?);
vm.map_memory(host_addr, layout::RAM_BASE, RAM_SIZE)
    .map_err(|e| io::Error::other(format!("hv_vm_map: {e}")))?;

let mut mgr = DeviceManager::new(
    gic.clone(), layout::MMIO_BASE, layout::MMIO_LEN, layout::SPI_BASE, layout::SPI_COUNT);

let mut serial_handle = None;
for rec in &snap.devices {
    match rec.id.as_str() {
        "serial" => {
            let s = mgr.add_restored(rec, |irq| Serial::with_irq(FlushWriter, irq))
                .map_err(io::Error::other)?;
            serial_handle = Some(s);
        }
        "virtio-blk" => {
            // private per-clone instance disk (existing temp-copy logic)
            let instance_disk = std::env::temp_dir().join(format!("ignition-instance-{}.img", process::id()));
            fs::copy(&paths.disk, &instance_disk)?;
            let disk_file = fs::OpenOptions::new().read(true).write(true).open(&instance_disk)?;
            let blk = VirtioBlk::new(disk_file).map_err(io::Error::other)?;
            let guest_ram = GuestRam::new(host as *mut u8, RAM_SIZE as usize, layout::RAM_BASE);
            mgr.add_restored(rec, move |irq| VirtioMmio::new("virtio-blk", Box::new(blk), guest_ram, irq))
                .map_err(io::Error::other)?;
        }
        other => return Err(io::Error::other(format!("unknown device id in snapshot: {other}"))),
    }
}
let serial = serial_handle.expect("snapshot had no serial device");
let frozen = mgr.freeze();
let bus = frozen.bus();

// console + stdin reader using `serial` (existing TermiosGuard / spawn_stdin_reader)
let manager = VcpuManager::new(1, bus);
spawn_stdin_reader(serial.clone(), termios.saved(), manager.clone());
// gic state is restored on the vCPU thread inside run_restored (gic_blob passed through)
manager.run_restored(snap.vcpu, Some(gic_blob))
    .map_err(|e| io::Error::other(format!("run_restored: {e}")))?;
```

(`map_err(io::Error::other)` adapts `DeviceMgrError` — it is `std::error::Error` from Task 1.)

- [ ] **Step 2: Build + sign + lint**

Run:
```bash
cargo build --workspace && cargo clippy --workspace && scripts/sign.sh target/debug/boot
```
Expected: workspace builds clean, 0 clippy warnings.

- [ ] **Step 3: Commit**

```bash
git add spike/src/bin/boot.rs
git commit -m "refactor(boot): restore via DeviceManager::add_restored"
```

---

## Task 10: Full regression — unit suites + live snapshot/restore/clone

**Files:** none (verification + docs).

- [ ] **Step 1: Unit + lint gate**

Run:
```bash
cargo test --workspace
cargo clippy --workspace
```
Expected: all suites green (the existing 15 plus the new device/manager/snapshot tests), 0 clippy warnings.

- [ ] **Step 2: Live snapshot → restore**

Run: `rm -rf snapshot snapshot2 && python3 scripts/restore_test.py`
Expected: `RESULT: snapshot=True restore_cpu≈0% responsive=True`. (Old v1 snapshots are rejected by `check_version`; the driver makes a fresh one.)

- [ ] **Step 3: Live clone**

Run: `python3 scripts/restore_clone_test.py`
Expected: both clones `marker=True`, `cpu≈0%`.

- [ ] **Step 4: Timing sanity (optional)**

Run: `python3 scripts/boot_vs_restore_timing.py 4`
Expected: fresh-boot and restore times in the same ballpark as before the refactor.

- [ ] **Step 5: Docs + commit**

Add a one-line note to `docs/snapshot-restore-result.md` that device wiring now goes through `DeviceManager` and the snapshot format is v2.

```bash
git add docs/snapshot-restore-result.md
git commit -m "docs: DeviceManager device model + snapshot v2"
```

---

## Notes for the implementer

- **Trait upcasting** (`Arc<Mutex<dyn MmioDevice>>` → `Arc<Mutex<dyn BusDevice>>`) is stable since Rust 1.86 (repo is on 1.96); it works because `MmioDevice: BusDevice`. If coercion fails, check that supertrait bound.
- **`Send + Sync`:** `FrozenDevices` holds `Arc<Bus>` and `Vec<Record>` of `Arc<Mutex<dyn MmioDevice>>`. `dyn MmioDevice: Send` (via `BusDevice: Send`), so `Mutex<dyn MmioDevice>: Send + Sync` and the whole thing is shareable across the vCPU thread. No extra bound needed; do **not** add `Sync` to `MmioDevice` unless the compiler demands it.
- **`serde_json::Value` boundary:** devices keep typed state structs (`VirtioMmioState`, `SerialSnapshot`); only trait methods cross to `Value`. `vmstate.json` stays human-readable.
- **Old snapshots break** (`v1` → `v2`); `check_version` refuses them. `rm -rf snapshot snapshot2` before live testing.
- **Net** stays present on fresh boot and excluded from the snapshot path; the restore match errors on an unexpected `virtio-net` record rather than silently handling it.
- **`save_state`/`gic.save_state()`** in the snapshot handler are the existing inherent methods (`HvfVcpu::save_state`, `HvfGicV3::save_state`); only the device inherent `save`/`restore` were renamed to `save_state`/`restore_state` (Tasks 2–3).
