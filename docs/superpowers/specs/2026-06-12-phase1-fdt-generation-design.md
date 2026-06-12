# Phase 1 Milestone 2a: FDT generation

Status: approved design, pre-implementation.
Date: 2026-06-12.
Project: ignition (Firecracker → macOS/HVF fork). Follows the UART-echo milestone
(MMIO bus + 16550 + threaded vCPU run loop, merged at 95502c0).

## Goal

Generate a Flattened Device Tree (DTB blob) describing a minimal aarch64 microVM,
so a Linux kernel handed this blob in X0 can discover its memory, CPUs, timer,
PSCI, interrupt controller, and serial console. This is the first sub-milestone
of kernel-boot; it has no HVF dependency and is fully unit-testable.

Boot-to-shell decomposes into: **2a FDT (this)** → 2b in-kernel GIC → 2c kernel
Image loader + boot protocol → 2d integration (kernel earlycon to our 16550) →
2e initramfs/shell. Each is its own spec/plan/build cycle.

## Approach

Use the `vm-fdt` writer crate (the same crate Firecracker uses) and lift FC's
node construction from `src/vmm/src/arch/aarch64/fdt.rs`, stripped to the
boot-minimal node set. Hand-emitting DTB bytes was rejected (error-prone,
reinvents `vm-fdt`).

## Location & dependencies

- New module: `crates/arch/src/aarch64/fdt.rs`, declared from
  `crates/arch/src/aarch64/mod.rs` (`pub mod fdt;`).
- `crates/arch/Cargo.toml`: add `vm-fdt = "0.3"` to `[dependencies]`; add a
  read-only DTB parser to `[dev-dependencies]` for tests (`fdt = "0.1"` — confirm
  the exact crate/version that parses `vm-fdt` output at implementation; fallback
  is shelling to `dtc` if present, but a pure-Rust parser is preferred).

## Interface

```rust
/// An MMIO device's placement and its SPI interrupt number.
pub struct MmioDev {
    pub addr: u64,
    pub size: u64,
    pub irq: u32, // GIC SPI number (the bare SPI index, not 32+n)
}

/// GICv3 placement, supplied by the GIC milestone (2b). Parameterized here so
/// FDT generation stays pure.
pub struct GicInfo {
    pub dist_base: u64,
    pub dist_size: u64,
    pub redist_base: u64,
    pub redist_size: u64,
    pub maint_irq: u32, // maintenance interrupt PPI number (typically 9)
}

/// Everything needed to describe the machine to the guest kernel.
pub struct FdtConfig {
    pub mem_base: u64,
    pub mem_size: u64,
    pub cpu_mpidrs: Vec<u64>,   // one entry per vCPU, in boot order
    pub cmdline: String,        // kernel command line -> /chosen bootargs
    pub serial: MmioDev,
    pub gic: GicInfo,
    pub initrd: Option<(u64, u64)>, // (guest addr, size) when an initramfs is loaded
}

/// Build the DTB. All error paths originate in `vm-fdt` (e.g. an interior NUL in
/// `cmdline` -> `Error::InvalidString`).
pub fn generate(cfg: &FdtConfig) -> Result<Vec<u8>, vm_fdt::Error>;
```

## Nodes emitted

Phandle constants: `GIC_PHANDLE = 1`, `CLOCK_PHANDLE = 2`.
Cell constants: `ADDRESS_CELLS = 2`, `SIZE_CELLS = 2`.
IRQ encoding constants (from the Linux GIC DT binding): `IRQ_TYPE_SPI = 0`,
`IRQ_TYPE_PPI = 1`, `IRQ_TYPE_EDGE_RISING = 1`, `IRQ_TYPE_LEVEL_HI = 4`.

1. **root `""`**: `compatible="linux,dummy-virt"`, `#address-cells=2`,
   `#size-cells=2`, `interrupt-parent=GIC_PHANDLE`.
2. **`cpus`**: `#address-cells=2`, `#size-cells=0`; one child `cpu@N` per entry in
   `cpu_mpidrs`: `device_type="cpu"`, `compatible="arm,arm-v8"`,
   `enable-method="psci"`, `reg=(mpidr & 0x7F_FFFF)`. **No cache subnodes** (the
   775-loc sysfs parse from FC's `cache_info.rs` is dropped — no macOS equivalent).
3. **`memory@ram`**: `device_type="memory"`, `reg=[mem_base, mem_size]` (u64 array).
   (FC carves out a reserved `SYSTEM_MEM_SIZE` prefix; we map RAM straight, so no
   carve-out.)
4. **`chosen`**: `bootargs=cmdline`. When `initrd=Some((addr,size))`, also
   `linux,initrd-start=addr` and `linux,initrd-end=addr+size` (u64). No `rng-seed`
   (would pull `aws-lc-rs`), no `linux,pci-probe-only` (no PCI).
5. **`intc`** (GICv3): `compatible="arm,gic-v3"`, `interrupt-controller` (empty
   prop), `#interrupt-cells=3`, `reg=[dist_base,dist_size,redist_base,redist_size]`
   (u64 array), `phandle=GIC_PHANDLE`, `#address-cells=2`, `#size-cells=2`,
   `ranges` (empty prop), `interrupts=[IRQ_TYPE_PPI, gic.maint_irq,
   IRQ_TYPE_LEVEL_HI]`. **No ITS/`msic` subnode** (no MSI).
6. **`apb-pclk`**: `compatible="fixed-clock"`, `#clock-cells=0`,
   `clock-frequency=24_000_000`, `clock-output-names="clk24mhz"`,
   `phandle=CLOCK_PHANDLE`. (Referenced by the serial node.)
7. **`timer`**: `compatible="arm,armv8-timer"`, `always-on` (empty prop),
   `interrupts` = the four standard PPIs `[13,14,11,10]` each encoded as
   `[IRQ_TYPE_PPI, n, IRQ_TYPE_LEVEL_HI]`.
8. **`psci`**: `compatible="arm,psci-0.2"`, `method="hvc"`.
9. **`uart@{serial.addr:x}`**: `compatible="ns16550a"`,
   `reg=[serial.addr, serial.size]`, `clocks=CLOCK_PHANDLE`,
   `clock-names="apb_pclk"`, `interrupts=[IRQ_TYPE_SPI, serial.irq,
   IRQ_TYPE_EDGE_RISING]`.

Node emission order follows FC: root opens, then cpus, memory, chosen, intc,
apb-pclk, timer, psci, uart, then root closes; `finish()` returns the blob.

**Explicitly dropped** (no backing device this milestone): virtio-mmio, vmgenid,
vmclock, PCI, RTC (pl031). They get added when their devices land.

## Testing

Pure `cargo test` (no HVF, no entitlement). Build a representative config:
2 CPUs (mpidrs `0x0`, `0x1`), `mem_base=0x4000_0000`/`mem_size=0x2000_0000`,
`cmdline="console=ttyS0 earlycon=uart8250,mmio,0x9000000"`, serial
`{0x900_0000, 0x1000, irq=33}`, gic `{0x800_0000, 0x1_0000, 0x80a_0000,
0xc0000, maint_irq=9}`. Generate, parse the blob with the `fdt` crate, assert:

1. DTB magic is valid / `Fdt::new` succeeds (structural integrity).
2. root `compatible == "linux,dummy-virt"`, `#address-cells == 2`.
3. `/memory@ram` `reg == [mem_base, mem_size]`.
4. `/chosen` `bootargs == cmdline`.
5. cpu node count == `cpu_mpidrs.len()`, and `cpu@0`/`cpu@1` `reg` match the mpidrs.
6. `/intc` `compatible == "arm,gic-v3"`, `#interrupt-cells == 3`, `reg` ==
   the four gic values.
7. `/psci` `method == "hvc"`.
8. serial node `compatible == "ns16550a"`, `reg == [serial.addr, serial.size]`,
   `interrupts == [0, 33, 1]`.
9. initrd: with `initrd=Some((0x4800_0000, 0x10_0000))`, `/chosen` has
   `linux,initrd-start`/`-end`; with `None`, neither property is present.

If the chosen parser cannot read a property type cleanly (e.g. needs raw cell
access for `reg`), assert on the raw bytes via the parser's property-bytes API.
Confirm the parser-reads-vm-fdt-output assumption in the first test; if it fails,
switch to `dtc -I dtb -O dts` round-trip parsing in the test harness and note it.

## Out of scope

GIC creation (`hv_gic_*` — 2b), kernel Image loading + arm64 boot protocol (2c),
writing the DTB into guest memory and pointing X0 at it (2c/2d), SMP bringup,
any real interrupt delivery. `GicInfo` values are inputs here; the GIC milestone
produces them.

## References to lift from

- `firecracker/src/vmm/src/arch/aarch64/fdt.rs` — node construction (this design
  is a stripped lift of it)
- `firecracker/src/vmm/Cargo.toml` — `vm-fdt = "0.3.0"`
