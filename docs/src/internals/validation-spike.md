# Validation spike

This chapter records the early end-to-end validation: from the first proof that
libkrun's HVF code compiles and runs on the current macOS SDK, through the first
real Linux kernel boot, to the first interactive login prompt. The spike binary
(`hvf-spike`, later `ignition-spike`) has since been removed; its hvf-crate
coverage is subsumed by the `boot` binary and the crate tests, and the lifted code
now lives in the `crates/` workspace. The results below are kept as the milestones
that de-risked the port.

## The spike: lifted code compiles and runs

Date: 2026-06-12. Machine: Apple Silicon, macOS 26.5.1 (build 25F80), arm64.
Toolchain: rustc/cargo 1.96.0 (Homebrew). SDK: MacOSX 26.5 (Xcode).

The concrete first task from the [design decisions](design-decisions.md): confirm
libkrun's `hvf` crate, lifted into a standalone consumer, compiles and runs against
the current macOS SDK before committing to fork structure.

The spike lifted, **verbatim**:
- `bindings.rs` (4712 L) — libkrun's generated Hypervisor.framework bindings
- `lib.rs` (731 L) → `src/hvf/mod.rs` — only edits: dropped `#[macro_use] extern
  crate log` for `use log::{...}`, and repointed the one external dep
  `arch::aarch64::sysreg::{SYSREG_MASK, sys_reg_name}` to a local `crate::arch`.
- `sysreg.rs` (146 L) → `src/arch.rs` — copied unchanged.

Link: `cargo:rustc-link-lib=framework=Hypervisor` (same as libkrun's vmm/build.rs).
Entitlement: ad-hoc codesign with `com.apple.security.hypervisor`.

The guest was 5 hand-assembled aarch64 instructions: store byte to unmapped MMIO
`0x09000000` (→ EC_DATAABORT), then spin on WFI (→ EC_WFX_TRAP).

Results, all passing:

1. **Compiles**: 0 errors, only dead-code warnings (unused enum variants/fields
   the spike doesn't exercise). Lifted code is clean against rustc 1.96 / edition
   2024 (let-chains, `unsafe extern`, etc. all fine).
2. **Links + entitlement**: `hv_vm_create` succeeds → framework linkage and the
   hypervisor entitlement both work with ad-hoc codesign.
3. **Runs**: VM + thread-affine vCPU created, 1 MiB guest RAM mapped, boot regs
   set (PC, X0), `hv_vcpu_run` drove the guest. Observed exits, in order:
   - `MmioWrite(0x09000000, [0x48, 0, 0, 0])`  — 'H', correct addr/data
   - `WaitForEvent`                            — WFI decoded correctly
4. **Bindings ABI matches macOS 26.5 SDK** (C probe vs checked-in asserts):
   `hv_vcpu_exit_t` size 32 / align 8, `reason`@0, `exception`@8;
   `hv_vcpu_exit_exception_t` syndrome@0 / virtual_address@8 / physical_address@16;
   `HV_EXIT_REASON` CANCELED=0 / EXCEPTION=1 / VTIMER=2. **Exact match.**

Implications for the fork:

- libkrun's checked-in `bindings.rs` is **reusable verbatim** on macOS 26.5 — no
  bindgen regeneration needed.
- The ESR_EL2 syndrome decode in `lib.rs::run()` works as-is end to end.
- Green light to commit to fork structure and proceed to Phase 1.

## First real kernel boot

Date: 2026-06-12. Host: macOS 26.5.1, Apple Silicon.
Guest: Linux 6.1.0 aarch64 (Firecracker `microvm-kernel-ci-aarch64-6.1.config`),
built via `kimage/build/build-kernel.sh`. Booted with:

```console
cargo build -p ignition-spike --bin boot
scripts/sign.sh target/debug/boot
target/debug/boot kimage/out/Image          # 2>diag  1>guest-console
```

The success criterion was earlycon output. The kernel went much further: it booted
to the init/rootfs handoff (214 lines of console), then panicked only because no
root filesystem was provided (expected: no `root=`, no virtio-blk yet).

Harness diagnostics:
```console
kernel : 16923136 bytes, entry=0x40000000
dtb    : 1326 bytes @ 0x5fe00000
gic    : dist=[0x3ffd0000, 0x10000] redist=[0x3ffe0000, 0x20000]
cmdline: console=ttyS0 earlycon=uart8250,mmio,0x9000000 reboot=k panic=1
```

Key proofs that every prior milestone composed correctly:
- `Machine model: linux,dummy-virt` — the FDT root node.
- `earlycon: uart8250 at MMIO 0x0000000009000000` + 200+ console lines — the 16550
  serial over the MMIO bus and `default_cmdline`.
- `NUMA: Faking a node at [mem 0x40000000-0x5fffffff]` — the RAM layout.
- `psci: PSCIv0.2 detected in firmware` — the FDT psci node + HVC conduit; PSCI
  `SYSTEM_OFF` at the end was handled by the run loop → clean exit.
- `GICv3: 988 SPIs implemented`, `CPU0: found redistributor 0 region 0:0x3ffe0000`
  — the in-kernel `hv_gic`, at exactly the redistributor address `HvfGicV3`
  computed.
- `arch_timer: cp15 timer(s) running at 24.00MHz (virt)`, clocksource +
  `sched_clock` registered, BogoMIPS calibrated — the virtual timer worked; the
  run loop's bounded WFI/`WaitForEventTimeout` parking + vtimer masking was
  sufficient.

Final lines:
```console
[    0.046760] VFS: Cannot open root device "(null)" or unknown-block(0,0): error -6
[    0.046965] Kernel panic - not syncing: VFS: Unable to mount root fs on unknown-block(0,0)
[    0.048841] Rebooting in 1 seconds..
== guest requested shutdown (PSCI SYSTEM_OFF) -> [vcpu exited cleanly]
```

## Findings: interrupt delivery to a login prompt

A real aarch64 Linux boots on ignition/HVF to an Alpine `(none) login:` prompt on
host stdout. The root cause that had been blocking it was the **serial TX-empty
interrupt**, a VMM-side fix, not the vtimer and not virtio, both of which were
already correct. Three theories preceded the right one; the evidence trail is kept
below so the dead ends aren't re-walked.

The fix: the kernel's interrupt-driven 8250 tty blocks after the 16-byte TX FIFO
fills, waiting for the THRE (TX-holding-register-empty) interrupt. Our 16550
(`vm_superio::Serial`) was wired with a no-op `Trigger`, so that interrupt was
never raised: OpenRC's first service write filled the FIFO and hung, which looked
like a dead boot. printk's console path *polls* THRE, so the kernel banner and
dmesg printed fine, masking the gap until userspace used the tty layer.

Wiring the serial's `Trigger` to pulse the GIC's serial SPI (INTID 32, the same
`hv_gic_set_spi` edge-pulse mechanism virtio already used) unblocked it. OpenRC
then ran every sysinit service to `[ ok ]`, printed `/etc/issue`, and getty
emitted the login prompt.

- `crates/devices/src/serial.rs`: `SerialIrq` enum `{Noop, Gic(Arc<dyn IrqLine>)}`
  impl `vm_superio::Trigger`; the `Gic` variant asserts then deasserts the SPI
  (edge-rising; the GIC latches the edge). `Serial::with_irq(out, irq)` selects it;
  `Serial::new(out)` keeps the `Noop` line for the output-only smoke harnesses.
- `spike/src/bin/boot.rs`: `GicIrq { gic, intid }` now carries the absolute INTID;
  the serial is wired with `intid = SERIAL_SPI + 32` (= 32), virtio with
  `VIRTIO_SPI + 32` (= 33).

Reproduce: `target/debug/boot kimage/out/Image kimage/out/rootfs.ext4` reaches
`(none) login:` (~236 console lines) in ~30 s. Re-sign after any rebuild;
`cargo build --workspace` relinks `boot` and strips the hypervisor entitlement
(`hv_vm_create` then fails with `VmCreate`); `scripts/sign.sh target/debug/boot`.

Evidence trail (theories disproven before the right one):

1. **vtimer delivery — WRONG.** `HV_EXIT_REASON_VTIMER_ACTIVATED` never fires; the
   in-kernel `hv_gic` delivers the EL1 vtimer natively. The list-register injection
   experiment was moot and was reverted.
2. **virtio completion-IRQ — WRONG.** Logging every block request: 711 requests in
   ~31 s, all `status = 0`, across distinct sectors — the guest acks every
   completion. virtio + `hv_gic_set_spi` delivery were already correct.
3. **rootfs init / controlling-tty — WRONG.** The boot *looked* gated on
   OpenRC/getty config because output stopped mid-banner. `init=/sbin/getty` then
   printed exactly ~16 chars (`Welcome to Alpin`) before stopping — exactly the TX
   FIFO size — which finally fingered the serial TX interrupt as the real,
   VMM-side cause.

The ignition VMM boots a real aarch64 Linux to a userspace login prompt with a
working virtio-blk rootfs, native virtual timer, and full interrupt delivery
(virtio completion + serial TX). The shell-prompt bar is met; serial RX for
interactive input followed on the next milestone.

## Phase-1 follow-ups (historical)

Phase 1 is complete: a real aarch64 Linux boots on ignition/HVF to an interactive
root login over a bidirectional 16550 console, mounts an alpine rootfs via
virtio-blk, and runs SMP (`--smp N`, secondaries via PSCI `CPU_ON`). The items
below are the still-relevant leftovers and the hard-won reference facts.

### Open / optional (no current bug; do when convenient)

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

### Deferred by design

- **`GicInfo` single redistributor region — moot for HVF.** Multiple
  `#redistributor-regions` only matter for *discontiguous* redistributors. Apple's
  `hv_gic` always lays out ONE contiguous region (`per_cpu_size × vcpu_count` from a
  single `redist_base`; see `HvfGicV3::new`), so the single-region `GicInfo` +
  `create_gic_node` is correct for any vCPU count here. Revisit only if a future host
  produces split redistributor regions.
- **CPU hotplug** (`CPU_OFF`, sysfs online/offline) — out of scope. SMP models
  bring-up only; an unknown PSCI call (incl. `CPU_OFF`) returns `NOT_SUPPORTED`
  rather than acting.

### Standing constraints (not bugs)

- **`Serial`/`BusDevice` handle 1-byte accesses only** (`data.len() == 1`); other
  widths are logged and dropped. Correct for a 16550 (byte-wide registers) and the
  guest (`strb`/`ldrb`). A driver doing wider register access would silently no-op.
  Intentional, logged.
- **`NoIrqVcpus` stubs the userspace interrupt/sysreg path** (`handle_sysreg_read`
  => `Some(0)`, `handle_sysreg_write` => `true`, no userspace IRQ injection). This is
  the **correct permanent impl** for this design: the in-kernel `hv_gic` delivers all
  interrupts and per-cpu timers natively, so the userspace `Vcpus` path is
  intentionally inert, not a stopgap. Lives once in `hvf::NoIrqVcpus`, shared by both
  vCPU runners.

### Reference facts (HVF / Apple Silicon, macOS 26)

These were verified during bring-up and remain true; useful when extending the VMM.

GIC:

- **`hv_gic_set_spi` takes the ABSOLUTE GIC INTID** (SPI = `32 + spi_index`).
  The 16550 wires `SERIAL_SPI(0) + 32 = INTID 32`; virtio `VIRTIO_SPI(1) + 32 = 33`.
- **Create order:** `hv_vm_create` → `HvfGicV3::new` (before any vCPU). The GIC must
  exist before vCPU threads spawn.
- **HVF-reported sizes:** distributor `0x10000`, redistributor `0x20000` per vCPU.
  `HvfGicV3::new(1, 0x4000_0000)` placed dist=`0x3ffd0000`, redist=`0x3ffe0000` —
  valid IPAs below the MMIO window. `gic_top` is the address the GIC sits just below
  (guest RAM base).

Boot debug checklist (`target/debug/boot [--smp N] <Image> [rootfs]`):

Diagnostics on stderr, guest console on stdout (`2>diag.txt` to separate). Expected
banner: `entry=0x40000000` for a modern defconfig kernel (text_offset=0, loaded at
the 2 MiB-aligned RAM_BASE). Re-sign after every build
(`scripts/sign.sh target/debug/boot`); `cargo build` strips the entitlement and
`hv_vm_create` then fails `VmCreate`.

Symptom → cause:

- **No output at all** → DTB/cmdline mismatch or wrong load addr. Check the banner's
  entry/fdt addrs; confirm the kernel has 8250/16550 earlycon
  (`CONFIG_SERIAL_8250_*`) and the `uart@9000000` node `compatible="ns16550a"`.
- **Boots but no shell prompt** → rootfs init/getty issue, not the VMM: the console
  is bidirectional and the serial TX/RX interrupts work.
- **A secondary CPU never comes online under `--smp N`** → check stderr for
  `CPU_ON for ... ignored` (MPIDR mismatch) and confirm the guest kernel has
  `CONFIG_SMP` + PSCI. The FDT advertises `psci method="hvc"` and N cpu nodes.

Kernel loader:

- `arch::aarch64::kernel::load_kernel(ram, RAM_BASE, &image)` returns the entry
  address; `arch::aarch64::layout::fdt_addr(ram_size)` gives the DTB address. Write
  the DTB into the host RAM slice at `fdt_addr - RAM_BASE`.
- `image_size > file size` (BSS): `load_kernel` copies only `image.len()` bytes; the
  delta is satisfied by pre-zeroed guest RAM. Correct — do not "fix" it to copy
  `image_size` bytes.
