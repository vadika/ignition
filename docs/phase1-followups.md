# Phase 1 follow-ups (carry into the kernel-boot milestone)

Captured from the UART-echo milestone's final review. None block that milestone;
all matter once a real aarch64 Linux kernel boots.

## Hazards (fix before/while bringing up a kernel)

- ✅ **DONE** (2026-06-12, commits `ecda960`/`3d6c82a`) — **Halfword MMIO write
  panicked in the hvf crate.** The `EC_DATAABORT` write path only matched access
  lengths 1/4/8 and `panic!`d on len 2, while the read path handled it. Fixed by
  sharing `encode_mmio_le`/`decode_mmio_le` across both paths (all of 1/2/4/8,
  `debug_assert` on size); removed the dead `MmioRead::addr` field. See the
  hardening plan `docs/superpowers/plans/2026-06-12-phase1-hardening.md`.

## Layering migrations (do early in the next milestone)

- ✅ **DONE** (2026-06-12, commits `62dcc30`/`4f24978`) — **`Vm` was a no-op
  wrapper.** `Vm` now owns `Vec<MappedRegion>` via `map_memory(&mut self, ...)`,
  the `hvf` field is private, and `HvfVm` is no longer re-exported; both spike
  bins migrated off `vm.hvf.map_memory`. `regions()` exposes the layout for future
  dirty-tracking.

- ✅ **DONE** (2026-06-12, commits `61e4772`/`7ec261e`) — **`Bus::register` did no
  overlap validation.** `register` now returns `Result<(), BusError>` and rejects
  overlapping ranges (half-open formula, `saturating_add`); the error names both
  colliding ranges. `find` is still a linear scan — fine at this device count.

## NEXT MILESTONE (2f): interrupt delivery → shell — RE-DIAGNOSED

⚠️ The original 2f theory (vtimer PPI not delivered) was **DISPROVEN** during
implementation. See `docs/2f-findings.md` (commit 50e7b38) for the corrected,
evidence-backed diagnosis. Summary:

1. **The timer already works.** `HV_EXIT_REASON_VTIMER_ACTIVATED` never fires —
   the in-kernel `hv_gic` delivers the vtimer natively. Do NOT chase the vtimer.
2. **The real blocker is virtio completion-IRQ delivery.** An MMIO trace shows
   the guest stuck in a `QueueNotify → InterruptStatus → InterruptACK` loop at
   ~10 ms (= the run loop's `MAX_PARK` WFI timeout). The guest WFIs for the
   virtio IRQ and only limps forward on the timeout, because `hv_gic_set_spi(33)`
   returns success but does NOT wake the parked guest. Fix that:
   - verify the SPI INTID against `hv_gic_get_spi_interrupt_range`/`hv_gic_get_intid`;
   - fix the edge/level/pulse timing of `set_spi` (assert during the paused exit
     vs after; deassert→assert);
   - confirm the guest enabled INTID 33 in the distributor.
3. Then: channel-based WFI parking (replace `MAX_PARK`; a no-timeout `recv()`
   will EXPOSE the IRQ bug, so fix delivery first) and serial RX for interactivity.

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

## Boot bring-up (milestone 2d) — live-debug checklist for `boot <Image>`

Run: `cargo build -p hvf-spike --bin boot && scripts/sign.sh target/debug/boot &&
target/debug/boot <Image> [initrd]`. Diagnostics on stderr, guest console on
stdout (`2>diag.txt` to separate). Expected banner: `entry=0x40000000` for a
modern defconfig kernel (text_offset=0, loaded at the 2 MiB-aligned RAM_BASE).

Symptom → cause:
- **No output at all** → DTB/cmdline mismatch or wrong load addr. Check the banner's
  entry/fdt addrs; confirm the kernel has 8250/16550 earlycon (`CONFIG_SERIAL_8250_*`)
  and the `uart@9000000` node `compatible="ns16550a"` matches its driver.
- **Hangs right after `Booting Linux on physical CPU 0x0...`** → missing timer IRQ.
  `NoIrqVcpus` doesn't inject the vtimer; earlycon prints before the timer is
  needed, but the kernel stalls once it waits on a tick. That's the 2e work
  (vtimer PPI delivery via the in-kernel GIC + real channel parking).
- **Silent stall when the kernel brings up a secondary CPU** → PSCI `CPU_ON`.
  FDT advertises `psci method="hvc"`; the hvf run loop handles known PSCI fn IDs
  (VERSION/SYSTEM_OFF/CPU_ON) but an unhandled HVC currently falls through to the
  `other =>` debug arm with no response. Single-vCPU boot avoids this.
- **Panic on a halfword MMIO write** → the hvf crate's `MmioWrite` only matches
  len 1/4/8 (see below); a kernel doing `strh` to the UART would hit it.
- **earlycon stride:** the cmdline uses `earlycon=uart8250,mmio,0x9000000` (byte
  stride / MMIO, not MMIO32). If the kernel wants 32-bit register stride, switch
  to `uart8250,mmio32,...` and widen the Serial access gate (currently 1-byte only).

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
- ✅ **DONE** (2026-06-12, commits `e19b85e`/`36010f0`) — **DTB-within-512 MiB /
  large-RAM.** `layout::fdt_addr` now clamps placement to `min(ram_size,
  DTB_EARLY_MAP_LIMIT=512 MiB)`, so for `ram_size > 512 MiB` the DTB sits just below
  the 512 MiB early-map limit instead of beyond it. (A kernel at `RAM_BASE` must fit
  in ~510 MiB to clear it — documented.)

## FDT interface (milestone 2a) — evolve as consumers land

- ⏸️ **DEFERRED — moot for HVF.** **`GicInfo` models a single redistributor
  region.** Multiple `#redistributor-regions` are only needed for *discontiguous*
  redistributors. Apple's `hv_gic` always lays out ONE contiguous redistributor
  region (`per_cpu_size × vcpu_count` from a single `redist_base`; see
  `HvfGicV3::new`), so the single-region `GicInfo` + `create_gic_node` stays correct
  for any vCPU count on this target. Revisit only if a future host produces split
  redistributor regions. No code change.
- ✅ **DONE** (2026-06-12, commit `f69feed`/`62aba00`) — **`FdtConfig.serial:
  MmioDev` was a single device.** Replaced `serial`/`virtio` fields with a typed
  `devices: Vec<FdtDevice>` (`enum FdtDevice { Serial(MmioDev), VirtioBlk(MmioDev) }`);
  `generate` dispatches per kind, so adding RTC/more virtio is a new variant + arm,
  not a new field. All three `FdtConfig` constructions migrated. The serial-console
  expectation is documented on `devices` (caller's responsibility, as in Firecracker).
- ⏸️ **DEFERRED — SMP-gated.** **mpidr `& 0x7F_FFFF` mask.** Single-vCPU MPIDR is 0,
  so the mask is a no-op today. Re-validating it against a real MPIDR scheme requires
  the SMP/vCPU milestone to first wire actual MPIDRs (vcpuid → Aff1) from
  Hypervisor.framework — nothing meaningful to validate until then. Carry into SMP.

## Constraints to remember (not bugs)

- **`Serial`/`BusDevice` only handle 1-byte accesses** (`data.len() == 1`); other
  widths are logged and dropped. Correct for a 16550 (byte-wide registers) and
  for the milestone guest (`strb`/`ldrb`), but a driver doing wider register
  access would silently no-op. Intentional, logged.

- **`NoIrqVcpus` stubs the whole interrupt/sysreg path** (no GIC): `handle_sysreg_read`
  returns `Some(0)`, `handle_sysreg_write` returns `true`, no IRQ injection. A
  booting kernel needs a real GIC-backed `Vcpus` impl (in-kernel `hv_gic` is the
  fast path; see HANDOFF GIC decision).
