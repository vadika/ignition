# Phase 1 Milestone 2b: in-kernel GIC (HvfGicV3)

Status: approved design, pre-implementation.
Date: 2026-06-12.
Project: ignition (Firecracker → macOS/HVF fork). Follows FDT generation (2a,
merged 3e55306).

## Goal

Create the in-kernel ARM GICv3 interrupt controller via Apple's `hv_gic_*` API,
compute its distributor/redistributor placement, and expose (a) the FDT
description it implies and (b) an SPI-injection call. This is what lets a guest
kernel find and use an interrupt controller. No run-loop integration yet — that
is 2d.

Kernel-boot decomposition: 2a FDT ✓ → **2b GIC (this)** → 2c kernel Image loader
+ boot protocol → 2d integration boot (replace NoIrqVcpus, wire vtimer + serial
IRQ, earlycon to our 16550) → 2e initramfs/shell.

## Approach

Lift libkrun's `HvfGicV3` (devices/legacy/hvfgicv3.rs), but call the `hv_gic_*`
functions **directly** instead of via `libloading`. All seven needed functions
are declared as direct externs in our `bindings.rs` and resolve through the
Hypervisor framework that `hvf/build.rs` already links. We target macOS 26+ only,
so the dlopen-for-backward-compat dance libkrun needs is unnecessary. This drops
the `HvfGicBindings` struct and the `libloading` symbol lookups entirely.

## Location

New module `hvf::gic` in the `hvf` crate (the GIC is a pure HVF primitive —
`hv_gic_*` calls). The `hvf` crate already depends on `arch`, so the GIC can
return `arch::aarch64::fdt::GicInfo` directly (no duplicate type, no cycle:
`arch` does not depend on `hvf`).

Declared from `crates/hvf/src/lib.rs` with `pub mod gic;`.

## Interface

```rust
// crates/hvf/src/gic.rs

/// The maintenance interrupt PPI for GICv3 (matches FC/libkrun).
pub const MAINT_IRQ: u32 = 9;

/// The in-kernel GICv3, created through Apple's hv_gic_* API.
pub struct HvfGicV3 {
    dist_base: u64,
    dist_size: u64,
    redist_base: u64,
    redist_size: u64,   // total redistributor region size (per-cpu size * vcpu_count)
    vcpu_count: u64,
}

impl HvfGicV3 {
    /// Create the in-kernel GICv3.
    ///
    /// MUST be called after `hv_vm_create` and BEFORE any vCPU is created.
    /// Distributor + redistributors are placed immediately below
    /// `mmio_mem_start` (distributor lowest), matching libkrun's layout.
    pub fn new(vcpu_count: u64, mmio_mem_start: u64) -> Result<Self, crate::Error>;

    /// The FDT interrupt-controller description implied by this GIC's placement.
    /// `maint_irq` is `MAINT_IRQ`.
    pub fn fdt_info(&self) -> arch::aarch64::fdt::GicInfo;

    /// Assert (`level=true`) or deassert a shared peripheral interrupt.
    /// `intid` is the absolute GIC INTID (an SPI is `32 + spi_index`).
    pub fn set_spi(&self, intid: u32, level: bool) -> Result<(), crate::Error>;
}
```

### `new` algorithm (direct hv_gic_* calls)

1. `let mut dist_size = 0usize; hv_gic_get_distributor_size(&mut dist_size)` → on
   non-`HV_SUCCESS`, `Err(Error::GicCreate)`.
2. `let mut redist_each = 0usize; hv_gic_get_redistributor_size(&mut redist_each)`
   → on failure `Err(Error::GicCreate)`.
3. `redist_size = redist_each as u64 * vcpu_count`.
4. `dist_base = mmio_mem_start - dist_size as u64 - redist_size`;
   `redist_base = mmio_mem_start - redist_size`.
5. `config = hv_gic_config_create()`;
   `hv_gic_config_set_distributor_base(config, dist_base)`;
   `hv_gic_config_set_redistributor_base(config, redist_base)` — each checked.
6. `hv_gic_create(config)` — checked.
7. Store fields (`dist_size`/`redist_size` as `u64`).

`fdt_info` returns `GicInfo { dist_base, dist_size, redist_base, redist_size,
maint_irq: MAINT_IRQ }`. `set_spi` calls `hv_gic_set_spi(intid, level)`.

### Error type change

Add one variant to `crate::Error` (in `crates/hvf/src/lib.rs`): `GicCreate`, with
a `Display` arm like `"Error creating in-kernel HVF GIC"`. Used for every
non-success `hv_gic_*` return in `new` and `set_spi` (a single variant is enough;
the smoke test distinguishes failures by which call returns).

## Verification

A codesigned smoke binary `spike/src/bin/gic-smoke.rs` (same pattern as
`hvf-spike` / `uart-echo`; built, signed with `scripts/sign.sh`, run in the main
session because it needs the hypervisor entitlement). It:

1. `Vm::new(false)` — create the VM (links + entitlement).
2. `HvfGicV3::new(1, MMIO_MEM_START)` with a local `const MMIO_MEM_START: u64 =
   0x4000_0000` — proves the create sequence works on macOS 26. (Throwaway
   constant; the real layout module lands in 2c.)
3. Assert the layout: `dist_size > 0`, `redist_size > 0`,
   `redist_base == MMIO_MEM_START - redist_size`,
   `dist_base == MMIO_MEM_START - dist_size - redist_size`,
   `dist_base < redist_base`, `redist_base < MMIO_MEM_START`.
4. `set_spi(32, true)` then `set_spi(32, false)` → both `Ok` (32 = first SPI).
5. Build an `FdtConfig` using `gic.fdt_info()` plus a sample serial `MmioDev`,
   call `arch::aarch64::fdt::generate(&cfg)` → `Ok` with a non-empty blob —
   proves GIC placement and FDT generation compose.
6. Print `== GIC-SMOKE PASSED ==`.

Because this needs HVF + the entitlement, it is a signed binary, not a
`cargo test`. The build (`cargo build`) and `cargo build --workspace` must be
clean; the binary is the acceptance gate.

## Out of scope (deferred to 2d integration)

- Replacing `NoIrqVcpus` with a GIC-backed `Vcpus` impl.
- vtimer PPI injection on `VtimerActivated` exits.
- Routing the serial 16550's IRQ through `set_spi` during a real run.
- The definitive memory/MMIO layout (RAM base, MMIO window, GIC placement as
  named constants) — a throwaway `MMIO_MEM_START` is used here; the layout
  module is authored with the kernel loader in 2c.
- SMP (`vcpu_count > 1`) bringup and per-vCPU redistributor wiring beyond sizing.

## Risks / open points

- **`hv_gic_create` ordering**: Apple requires it after `hv_vm_create` and before
  any `hv_vcpu_create`. The smoke test creates no vCPU, so it is safe; 2d must
  create the GIC before vCPU threads spawn. Documented on `new`.
- **`hv_gic_set_spi` intid semantics**: whether `intid` is the absolute INTID
  (`32 + n`) or a bare SPI index. The smoke test asserts via the return code with
  `intid = 32`; if it errors, try the bare index and record the finding.
- **`mmio_mem_start` validity**: `0x4000_0000` must be a legal IPA with enough
  room below it for dist+redist. If `hv_gic_create` rejects the computed bases,
  raise `mmio_mem_start` and note it for the 2c layout.

## References to lift from

- `libkrun/src/devices/src/legacy/hvfgicv3.rs` — the create sequence + layout math
- `crates/hvf/src/bindings.rs` — direct `hv_gic_*` extern declarations (lines ~4455–4580)
- `crates/arch/src/aarch64/fdt.rs` — `GicInfo` consumed by `fdt_info()`
