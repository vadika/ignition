# Phase 1 follow-ups & reference notes

Phase 1 is complete: a real aarch64 Linux boots on ignition/HVF to an interactive
root login over a bidirectional 16550 console, mounts an alpine rootfs via
virtio-blk, and runs SMP (`--smp N`, secondaries via PSCI `CPU_ON`). This file
tracks the leftover items and keeps the hard-won reference facts. Every item
captured from milestone reviews is either **done**, **deferred by design**, an
**open/optional** nicety, or a **standing constraint**.

## Open / optional (no current bug; do when convenient)

- **`hv_gic_config_t` is leaked** (`crates/hvf/src/gic.rs`) — a retained OS object,
  never `os_release`d, matching `hv_vm_config_t`. Fine at process scope (one GIC for
  the process lifetime). Add a `Drop` wrapper only if GICs ever become dynamic.
- **`text_offset` alignment** (`crates/arch/src/aarch64/kernel.rs`) — a real-kernel
  validator *could* warn (not error) if `text_offset % 0x20_0000 != 0`. Modern
  kernels are 2 MiB-aligned; the copy works regardless. Optional hardening.
- **`Bus::find` is a linear scan** (`crates/devices/src/bus.rs`) — fine at the
  current device count (serial + virtio). Revisit only if the device table grows
  large.
- **earlycon stride** — the cmdline uses `earlycon=uart8250,mmio,0x9000000` (byte
  stride). If a future kernel wants 32-bit register stride, switch to
  `uart8250,mmio32,...` and widen the `Serial` access gate (currently 1-byte). Not a
  bug — a configuration contingency.

## Deferred by design

- **`GicInfo` single redistributor region — moot for HVF.** Multiple
  `#redistributor-regions` only matter for *discontiguous* redistributors. Apple's
  `hv_gic` always lays out ONE contiguous region (`per_cpu_size × vcpu_count` from a
  single `redist_base`; see `HvfGicV3::new`), so the single-region `GicInfo` +
  `create_gic_node` is correct for any vCPU count here. Revisit only if a future host
  produces split redistributor regions.
- **CPU hotplug** (`CPU_OFF`, sysfs online/offline) — out of scope. SMP models
  bring-up only; an unknown PSCI call (incl. `CPU_OFF`) returns `NOT_SUPPORTED`
  rather than acting.

## Standing constraints (not bugs)

- **`Serial`/`BusDevice` handle 1-byte accesses only** (`data.len() == 1`); other
  widths are logged and dropped. Correct for a 16550 (byte-wide registers) and the
  guest (`strb`/`ldrb`). A driver doing wider register access would silently no-op.
  Intentional, logged.
- **`NoIrqVcpus` stubs the userspace interrupt/sysreg path** (`handle_sysreg_read`
  => `Some(0)`, `handle_sysreg_write` => `true`, no userspace IRQ injection). This is
  the **correct permanent impl** for this design: the in-kernel `hv_gic` delivers all
  interrupts and per-cpu timers natively, so the userspace `Vcpus` path is
  intentionally inert — not a stopgap. Lives once in `hvf::NoIrqVcpus` (commit
  `7e1e73d`), shared by both vCPU runners.

## Resolved (chronological)

All fixed on 2026-06-12 unless noted. Commit refs in parens.

- **Halfword MMIO write panicked** (`ecda960`/`3d6c82a`) — shared
  `encode_mmio_le`/`decode_mmio_le` across read+write (1/2/4/8); dropped the dead
  `MmioRead::addr`. Plan: `docs/superpowers/plans/2026-06-12-phase1-hardening.md`.
- **`Vm` was a no-op wrapper** (`62dcc30`/`4f24978`) — owns `Vec<MappedRegion>` via
  `map_memory(&mut self)`; `hvf` field private; `HvfVm` re-export dropped.
- **`Bus::register` did no overlap validation** (`61e4772`/`7ec261e`) — returns
  `Result<(), BusError>`, rejects overlaps (half-open + `saturating_add`), names both
  colliding ranges.
- **DTB placement for large RAM** (`e19b85e`/`36010f0`) — `fdt_addr` clamps to
  `min(ram_size, DTB_EARLY_MAP_LIMIT=512 MiB)` so the DTB stays in the kernel's
  early-map window.
- **`FdtConfig` per-device fields** (`f69feed`/`62aba00`) — replaced
  `serial`/`virtio` with a typed `devices: Vec<FdtDevice>`; `generate` dispatches per
  kind.
- **mpidr `& 0x7F_FFFF` mask / MPIDR scheme** (SMP milestone) —
  `VcpuManager::mpidr_for(index) = index` (linear Aff0 = cpu index); FDT,
  `MPIDR_EL1`, and `CPU_ON` target all key off it; mask is a no-op for index < 2^23.
  Spec: `docs/superpowers/specs/2026-06-12-smp-design.md`.
- **Unknown PSCI/HVC fn panicked the vCPU** (`6c4d676`) — returns `NOT_SUPPORTED`
  (-1 in X0) + `PsciHandled` instead of `panic!`.
- **`set_spi` reused `Error::GicCreate`** (`25246f0`) — split `Error::GicSetSpi`;
  create-path returns stay `GicCreate`.
- **`hvf::Error` / `KernelError` lacked `std::error::Error`** (`25246f0`) — both impl
  it now (compile-checked by tests).
- **`NoIrqVcpus` duplicated** (`7e1e73d`) — hoisted to `hvf::NoIrqVcpus`, one shared
  definition.

Earlier milestones (validation spike → FDT → in-kernel GIC → kernel loader →
boot-to-earlycon → virtio-blk rootfs → interrupt delivery → serial RX → SMP) are
recorded in `docs/2f-findings.md`, `docs/2e-virtio-result.md`,
`docs/serial-rx-result.md`, `docs/smp-result.md`, and the specs/plans under
`docs/superpowers/`.

## Reference facts (HVF / Apple Silicon, macOS 26) — keep

These were verified during bring-up and remain true; useful when extending the VMM.

### GIC

- **`hv_gic_set_spi` takes the ABSOLUTE GIC INTID** (SPI = `32 + spi_index`).
  The 16550 wires `SERIAL_SPI(0) + 32 = INTID 32`; virtio `VIRTIO_SPI(1) + 32 = 33`.
- **Create order:** `hv_vm_create` → `HvfGicV3::new` (before any vCPU). The GIC must
  exist before vCPU threads spawn.
- **HVF-reported sizes:** distributor `0x10000`, redistributor `0x20000` per vCPU.
  `HvfGicV3::new(1, 0x4000_0000)` placed dist=`0x3ffd0000`, redist=`0x3ffe0000` —
  valid IPAs below the MMIO window. `gic_top` is the address the GIC sits just below
  (guest RAM base).

### Boot debug checklist (`target/debug/boot [--smp N] <Image> [rootfs]`)

Diagnostics on stderr, guest console on stdout (`2>diag.txt` to separate). Expected
banner: `entry=0x40000000` for a modern defconfig kernel (text_offset=0, loaded at
the 2 MiB-aligned RAM_BASE). Re-sign after every build
(`scripts/sign.sh target/debug/boot`) — `cargo build` strips the entitlement and
`hv_vm_create` then fails `VmCreate`.

Symptom → cause (current; the historical timer/secondary-CPU/halfword stalls are
all fixed — see Resolved):

- **No output at all** → DTB/cmdline mismatch or wrong load addr. Check the banner's
  entry/fdt addrs; confirm the kernel has 8250/16550 earlycon
  (`CONFIG_SERIAL_8250_*`) and the `uart@9000000` node `compatible="ns16550a"`.
- **Boots but no shell prompt** → rootfs init/getty issue, not the VMM (see
  `docs/serial-rx-result.md`): the console is bidirectional and the serial TX/RX
  interrupts work.
- **A secondary CPU never comes online under `--smp N`** → check stderr for
  `CPU_ON for ... ignored` (MPIDR mismatch) and confirm the guest kernel has
  `CONFIG_SMP` + PSCI. The FDT advertises `psci method="hvc"` and N cpu nodes.

### Kernel loader

- `arch::aarch64::kernel::load_kernel(ram, RAM_BASE, &image)` returns the entry
  address; `arch::aarch64::layout::fdt_addr(ram_size)` gives the DTB address. Write
  the DTB into the host RAM slice at `fdt_addr - RAM_BASE`.
- `image_size > file size` (BSS): `load_kernel` copies only `image.len()` bytes; the
  delta is satisfied by pre-zeroed guest RAM. Correct — do not "fix" it to copy
  `image_size` bytes.
