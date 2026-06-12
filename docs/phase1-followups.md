# Phase 1 follow-ups (carry into the kernel-boot milestone)

Captured from the UART-echo milestone's final review. None block that milestone;
all matter once a real aarch64 Linux kernel boots.

## Hazards (fix before/while bringing up a kernel)

- **Halfword MMIO write panics in the hvf crate.** `crates/hvf/src/lib.rs` (the
  `EC_DATAABORT` write path, ~line 639) only matches access lengths 1/4/8 and
  `panic!`s on len 2 — while the `MmioRead` readback (~line 560) *does* handle
  len 2. A real kernel/virtio guest can issue halfword (`strh`) MMIO writes, so
  this is a latent panic during bring-up. It is lifted-verbatim libkrun code, so
  decide: patch our fork, or confirm guests never do halfword MMIO. Track it.

## Layering migrations (do early in the next milestone)

- **`Vm` is a no-op wrapper.** `crates/vmm/src/vstate/hvf_vm.rs` owns only
  `pub hvf: HvfVm`; the harness reaches through `vm.hvf.map_memory(...)`. Kernel
  boot needs `Vm` to own guest-memory regions (for FDT placement + future
  dirty-tracking). Give `Vm` real memory-management methods and make `hvf`
  private; migrate the spike's `vm.hvf.*` reach-through first.

- **`Bus::register` does no overlap validation; `find` is a linear scan.** Fine
  at 1–2 devices. When GIC + virtio land, have `register` return a `Result` with
  an overlap check before the device table grows.

## GIC (milestone 2b) — confirmed facts for 2d integration

- **`hv_gic_set_spi` takes the ABSOLUTE GIC INTID** (SPI = `32 + spi_index`),
  confirmed by the gic-smoke run: `set_spi(32, true/false)` succeeded. So the
  serial's FDT `irq` (bare SPI index, e.g. 33) must be passed to `set_spi` as
  `32 + irq` when wiring the 16550 IRQ in 2d.
- **Create order works:** `hv_vm_create` → `HvfGicV3::new` (no vCPU yet) is
  accepted. 2d must create the GIC before spawning vCPU threads.
- **HVF-reported sizes (macOS 26, Apple Silicon):** distributor `0x10000`,
  redistributor `0x20000` per vCPU. `HvfGicV3::new(1, 0x4000_0000)` placed
  dist=`0x3ffd0000`, redist=`0x3ffe0000` — valid IPAs below the MMIO window.
- **`hv_gic_config_t` is leaked** (retained OS object, never `os_release`d) —
  matches `hv_vm_config_t`. Fine at process scope; add a Drop wrapper if GICs
  ever become dynamic.
- **`set_spi` reuses `Error::GicCreate`** on failure (single-variant choice).
  When `set_spi` moves onto the hot IRQ-injection path in 2d, split out
  `Error::GicSetSpi` — the "creating GIC" Display string misleads for a runtime
  injection failure.
- **`HvfGicV3::new(vcpu_count, gic_top)`**: `gic_top` = the address the GIC sits
  just below (in the smoke, guest RAM base `0x4000_0000`). When the 2c layout
  module lands, pass the real value (likely RAM base) — not the serial MMIO
  address.

## Kernel loader (milestone 2c) — for the 2d boot integration

- **Wiring:** `arch::aarch64::kernel::load_kernel(ram, RAM_BASE, &image)` returns
  the entry address; `arch::aarch64::layout::fdt_addr(ram_size)` gives the DTB
  address. Feed both to `HvfVcpu::set_initial_state(entry, fdt_addr)` (already
  built) and write the DTB bytes into the host RAM slice at `fdt_addr - RAM_BASE`.
  `load_kernel` takes `&mut [u8]` so pass the HVF mmap slice directly.
- **`KernelError` should impl `std::error::Error`** once it propagates through the
  VMM error chain in 2d (trivial: `impl std::error::Error for KernelError {}`).
  Same applies to `hvf::Error`.
- **`text_offset` alignment:** a real-kernel validator could warn (not error) if
  `text_offset % 0x20_0000 != 0` — modern kernels are 2 MiB-aligned. The copy
  works regardless.
- **`image_size` > file size (BSS):** `load_kernel` copies only `image.len()`
  bytes; the delta is satisfied by pre-zeroed guest RAM. Correct — don't "fix" it
  to copy `image_size` bytes.
- **DTB-within-512 MiB / large-RAM:** `layout::fdt_addr` has a `TODO(larger-ram)`
  — top-of-RAM placement only stays within the kernel's early-map 512 MiB window
  while `ram_size <= 512 MiB`. Add a guard when bigger RAM lands.

## FDT interface (milestone 2a) — evolve as consumers land

- **`GicInfo` models a single redistributor region** (`redist_base`/`redist_size`
  scalars). Correct for the default single-region GICv3. Large vCPU counts need
  multiple redist regions → `#redistributor-regions` + a region slice in both
  `GicInfo` and `create_gic_node`. The GIC milestone (2b) produces these values;
  re-check then.
- **`FdtConfig.serial: MmioDev` is a single device.** When virtio-mmio / RTC land,
  switch to `Vec<MmioDev>` (or a typed device list) instead of per-device fields
  to avoid an `FdtConfig` field explosion. `MmioDev` is already named generically
  for reuse. Not a lock-in now.
- **mpidr `& 0x7F_FFFF` mask assumes Aff2 bit 23 == 0.** When the vCPU milestone
  wires real MPIDRs from Hypervisor.framework (HANDOFF: write vcpuid to Aff1),
  re-validate the mask against the actual MPIDR scheme.

## Constraints to remember (not bugs)

- **`Serial`/`BusDevice` only handle 1-byte accesses** (`data.len() == 1`); other
  widths are logged and dropped. Correct for a 16550 (byte-wide registers) and
  for the milestone guest (`strb`/`ldrb`), but a driver doing wider register
  access would silently no-op. Intentional, logged.

- **`NoIrqVcpus` stubs the whole interrupt/sysreg path** (no GIC): `handle_sysreg_read`
  returns `Some(0)`, `handle_sysreg_write` returns `true`, no IRQ injection. A
  booting kernel needs a real GIC-backed `Vcpus` impl (in-kernel `hv_gic` is the
  fast path; see HANDOFF GIC decision).
