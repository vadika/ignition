# Device-Model Framework (DeviceManager) — Design

Date: 2026-06-13. Status: approved design, ready for an implementation plan.

## Context

This is **sub-project A** of the "full device model" milestone. The milestone is
decomposed into:

- **A. DeviceManager framework** (this spec) — a uniform device registry that owns
  MMIO-window + SPI allocation, bus registration, FDT-node generation, and
  snapshot enumeration; existing serial/blk/net ported onto it.
- B. virtio-rng — C. RTC (PL031) — D. virtio-balloon — E. virtio-vsock.

B–E each get their own spec/plan once A lands. They depend on A and become
"implement one trait + one `add` call" once A exists.

### Why A first

Snapshot/restore must serialize **all** device state. Today the wiring is ad-hoc in
`spike/src/bin/boot.rs`: each device's MMIO window, SPI, FDT node, snapshot entry,
and GIC IRQ adapter is hand-assigned, so adding a device means editing four
independent places and hand-extending the snapshot format. A uniform framework
removes that fragility and makes B–E mechanical.

### Current state (what exists)

- `crates/devices/src/bus.rs`: `Bus` + `BusDevice` trait (address-routed MMIO).
- `crates/devices/src/virtio/mmio.rs`: `VirtioMmio` transport + `VirtioDevice`
  trait; `VirtioMmio::save()/restore()` and `VirtioMmioState`/`QueueSnapshot`.
- `crates/devices/src/virtio/mod.rs`: `IrqLine` trait.
- Devices: `serial.rs` (16550, plain MMIO), `virtio/blk.rs`, `virtio/net.rs`.
- `arch::aarch64::fdt`: `FdtDevice` enum with one variant per device type
  (`Serial`, `VirtioBlk`, `VirtioNet`), each generating a bespoke node.
- `arch::aarch64::layout`: fixed per-device consts (`SERIAL_BASE/SIZE/SPI`,
  `VIRTIO_BASE/SIZE/SPI`, `NET_BASE/SIZE/SPI`).
- `vmm::snapshot`: `VmConfig` with hand-listed `serial`/`blk` `MmioWindow` fields +
  `DeviceState`.
- `boot.rs`: builds `GicIrq` (intid = spi+32) per device, registers each on the bus
  by hand, builds the `FdtDevice` list by hand, fills `VmConfig` by hand.

Plain-MMIO devices (serial, RTC) and virtio-mmio devices (blk/net/rng/balloon/vsock)
must both fit the framework.

## Goal

A `DeviceManager` that owns device placement, routing, FDT description, and snapshot
enumeration, exposed through a single `MmioDevice` trait. After this sub-project,
serial+blk+net behave exactly as today, `boot.rs` device wiring is ~4 `add` calls,
and adding a device is "implement `MmioDevice` + one `add` call."

Non-goals: new devices (B–E), multi-vCPU snapshot, dirty-page tracking, REST API.
Net remains present on fresh boot and excluded from the snapshot path, as today.

## Architecture

### The `MmioDevice` trait (in `crates/devices/`)

```rust
pub enum FdtKind { Ns16550a, Pl031, VirtioMmio }

pub trait MmioDevice: BusDevice {
    /// Which FDT node shape this device emits.
    fn fdt_kind(&self) -> FdtKind;
    /// Stable snapshot key, e.g. "serial", "virtio-blk", "virtio-net".
    fn snapshot_id(&self) -> &str;
    /// Serialize device state for snapshot.
    fn save(&self) -> serde_json::Value;
    /// Apply restored state. Called after construction, before first run.
    fn restore(&mut self, v: &serde_json::Value) -> Result<(), DeviceMgrError>;
}
```

- Extends `BusDevice`, so a single `Arc<Mutex<dyn MmioDevice>>` serves both the bus
  (upcast to `Arc<Mutex<dyn BusDevice>>` — trait upcasting, stable since Rust 1.86)
  and the manager's record.
- `serde_json::Value` is the state boundary: each device keeps its own typed state
  struct internally (e.g. `VirtioMmioState`, `SerialSnapshot`) and (de)serializes at
  the edge. Keeps the manager device-agnostic and `vmstate.json` human-readable.

`VirtioMmio` and `Serial` implement `MmioDevice`. `VirtioMmio::save/restore` already
exist; they get wrapped to the `serde_json::Value` boundary and `fdt_kind` returns
`VirtioMmio`. `Serial` similarly (returns `Ns16550a`).

### `DeviceManager` (new `crates/vmm/src/device_manager.rs`)

`vmm` is the integration seam (already depends on `devices`, `arch`, `hvf`). The
manager owns:

- the `Bus`;
- an **MMIO window allocator**: a device region `[mmio_base, mmio_base+mmio_len)` in
  guest-physical space, handed out in caller-specified window sizes, bump-allocated;
- an **SPI allocator**: a contiguous index range `[spi_base, spi_base+spi_count)`,
  bump-allocated; INTID = `spi + 32`;
- the GIC handle, to mint `IrqLine`s — the `GicIrq` adapter (intid = spi+32) moves
  here from `boot.rs`;
- `records: Vec<DeviceRecord>`.

```rust
struct DeviceRecord {
    id: String,
    base: u64,
    size: u64,
    spi: u32,
    fdt_kind: FdtKind,
    dev: Arc<Mutex<dyn MmioDevice>>,
}
```

Construction is closure-based because the device needs its `IrqLine` (whose SPI the
manager allocates):

```rust
fn add<F>(&mut self, window_size: u64, build: F) -> Result<(), DeviceMgrError>
where F: FnOnce(Arc<dyn IrqLine>) -> Box<dyn MmioDevice>;
```

`add`: allocate window (`base`) + SPI; mint `IrqLine`; `dev = build(irq)`; read
`dev.snapshot_id()`/`fdt_kind()`; wrap in `Arc<Mutex<_>>`; register on the bus at
`(base, window_size)`; push the record.

Device *construction* stays caller-side (backends differ: open a disk, build a net
backend); the manager owns placement, routing, FDT, and snapshot.

### Accessors

- `bus(&self) -> Arc<Bus>` — for the run loop (build the bus, then freeze into `Arc`).
- `fdt_devices(&self) -> Vec<FdtDeviceDesc>` where `FdtDeviceDesc { kind, base, size,
  spi }` — fed to `fdt::generate`.
- `save(&self) -> Vec<DeviceRecord-without-dev>` (the serializable projection:
  `{id, base, size, spi, fdt_kind, state}`).

## Data flow

### Fresh boot
1. `DeviceManager::new(gic, guest_ram, mmio_base, mmio_len, spi_base, spi_count)`.
2. Harness calls `add(window_size, build_closure)` per device (serial; blk/net if
   flagged). Each allocates the next window+SPI and wires the device.
3. `manager.fdt_devices()` → `fdt::generate`. Virtio devices all emit the
   `virtio,mmio` node; serial → `ns16550a`; RTC → `arm,pl031`.
4. Freeze the bus (`manager.bus()`); the run loop dispatches MMIO through it
   unchanged.

### Snapshot (`Ctrl-A s`)
`manager.save()` → `Vec<{id, base, size, spi, fdt_kind, state}>`. This replaces the
hand-listed `VmConfig.serial`/`.blk` `MmioWindow` fields + `DeviceState` in
`snapshot.rs`. Non-device config (`mem_size`, `vcpu_count`) is unchanged. The device
section of `vmstate.json` is now this self-describing list.

### Restore
Device construction is backend-specific, so the manager cannot reconstruct devices
generically. The harness keeps an `id → build-closure` map (the same closures as
fresh boot).
1. Read the device-record list from the snapshot.
2. For each record: `manager.add_restored(record_meta, build)` — places the device
   at the record's **saved** base/SPI (exact replay, not freshly allocated),
   registers on the bus, then calls `dev.restore(&record.state)`.
3. FDT is not regenerated on restore (already in `memory.bin`), as today.

Determinism: base/SPI are saved per-record and replayed, so restore reproduces the
exact layout regardless of allocation order or device-set flags.

## Error handling

`DeviceMgrError`:
- `WindowExhausted` — MMIO region full.
- `SpiExhausted` — SPI range full.
- `BusOverlap(BusError)` — defensive; the allocator shouldn't produce overlaps.
- `UnknownDeviceId(String)` — a restore record has no matching build closure.
- `StateInvalid { id, reason }` — a device's `restore` rejects its `Value`.

`add`/`fdt`/bus failures are fatal in the boot harness (`expect`/exit), same as
today's hand-wiring failures. Restore failures abort the restore with a clear
message; never half-build.

The snapshot device-format change is a hard break: add a `version` field (or bump
`SNAP_MAGIC`); restore refuses an incompatible snapshot with a clear error rather
than mis-parsing an old one.

## Testing

Unit (no hypervisor entitlement — runs under `cargo test`):
- **Allocator**: sequential windows are non-overlapping and contiguous; SPIs
  increment from `spi_base`; window/SPI exhaustion returns the right error.
- **DeviceManager** with a mock `MmioDevice` (records its id; trivial `save`/`restore`
  echoing a `Value`): `add` registers on the bus at the allocated base; `save()`
  yields one record per device with the correct base/SPI/id; `add_restored` places at
  the saved base/SPI and feeds the saved state to `restore`; the round-trip
  `save → add_restored → save` is byte-identical.
- **IrqLine minting**: a mock GIC asserts the manager mints one `IrqLine` per device
  with INTID `spi+32`.
- **FDT**: `fdt_devices()` returns the right `(kind, base, size, spi)` tuples;
  `fdt::generate` for each kind still produces a valid blob (extend existing fdt
  tests). All virtio devices share the `virtio,mmio` node generator.
- **Snapshot version guard**: restore rejects a snapshot with a wrong/absent version.

Live (driver scripts, not `cargo test` — the real regression gate):
- `scripts/restore_test.py` and `scripts/restore_clone_test.py` still pass:
  serial+blk+net snapshot → restore is responsive, idles at ~0% CPU, and clones.

## Migration (this sub-project ships working software)

1. Define `FdtKind` + `MmioDevice` (+ `DeviceMgrError`) in `devices`.
2. Implement `MmioDevice` for `Serial` and `VirtioMmio`.
3. New `vmm::device_manager` with the allocator, `add`/`add_restored`/`save`/
   `fdt_devices`/`bus`, and the `GicIrq` adapter moved from `boot.rs`.
4. Collapse `arch::fdt::FdtDevice` variants into kind-driven generation
   (`Ns16550a`/`Pl031`/`VirtioMmio`), driven by `FdtDeviceDesc`.
5. Rewrite `boot.rs` fresh-boot wiring to build the manager and `add` serial/blk/net;
   delete the per-device layout consts the allocator replaces (keep `RAM_BASE`, FDT
   addr, the device-region base/len, SPI base).
6. Rewrite `boot.rs` restore wiring + `vmm::snapshot` to the device-record list with a
   version guard; replace `VmConfig.serial`/`.blk`/`DeviceState`.
7. Re-run the live drivers as the regression gate.

End state: serial+blk+net behave exactly as now; `boot.rs` device wiring is ~4 `add`
calls; B–E are drop-in (one `MmioDevice` impl + one `add`/restore closure each).
