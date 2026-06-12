# Phase 1 Milestone 2d: integration boot to earlycon

Status: approved design, pre-implementation.
Date: 2026-06-12.
Project: ignition (Firecracker → macOS/HVF fork). Converges 2a FDT (3e55306),
2b GIC (aee2a2c), 2c loader (1b0ef84).

## Goal

Boot a real aarch64 Linux kernel far enough to print its early-console messages
to our 16550 on host stdout. This is the convergence milestone: load the kernel
(2c) + device tree (2a) into guest RAM, create the in-kernel GIC (2b), and run a
vCPU whose MMIO writes reach the serial device.

Success bar: **the kernel's earlycon output appears on host stdout.** Earlycon is
polled (not interrupt-driven), so this is reachable before full timer/IRQ
correctness. Timer-driven boot, initramfs, and a shell prompt are 2e.

## Approach

With the in-kernel `hv_gic`, the interrupt controller's ICC system registers and
MMIO are handled in-kernel, so the existing `NoIrqVcpus` (no manual IRQ
injection, sysreg traps acknowledged) is already correct — no new `Vcpus` impl is
needed. The work is (1) a small run-loop upgrade so idle/timer exits are handled
efficiently instead of busy-spinning, and (2) a boot harness that assembles the
kernel, DTB, GIC, memory, and serial and runs the vCPU.

Kernel delivery: the operator supplies the Image; the harness takes it as a
command-line argument. The code is built and committed now; the actual boot run
happens when the kernel is available and is then debugged live (this milestone is
inherently experimental).

## Component 1: run-loop upgrade — `crates/vmm/src/vstate/hvf_vcpu.rs`

The current `Vcpu::run` loop dispatches `MmioWrite`/`MmioRead`/`Shutdown`/
`Canceled` and falls through to a `log::debug!` no-op for everything else — which
means `WaitForEventTimeout`/`WaitForEvent` immediately re-enter and busy-spin a
core, and (for `WaitForEventTimeout`) can prevent real time from passing
efficiently to the timer deadline. Add explicit arms:

- `VcpuExit::WaitForEventTimeout(d)` → `std::thread::sleep(d.min(MAX_PARK))` then
  continue, where `const MAX_PARK: Duration = Duration::from_millis(10)`. This
  lets wall-clock advance toward the next `CNTV` deadline without pegging the CPU,
  capped so a huge `d` does not stall responsiveness.
- `VcpuExit::WaitForEvent` → `std::thread::sleep(MAX_PARK)` then continue. The
  guest is waiting for an interrupt with no timer deadline; with no injection on
  the earlycon path this would otherwise hang or hot-spin. A short bounded sleep
  is an earlycon-grade approximation; proper channel-based parking that wakes on
  `hv_gic_set_spi` is 2e.
- `VcpuExit::WaitForEventExpired` → continue (the deadline already passed).
- `VcpuExit::VtimerActivated` → continue. `HvfVcpu::run` already set the vtimer
  mask on this exit; the in-kernel GIC redelivers the timer on the next entry.

`NoIrqVcpus` is unchanged. No `cargo test` (the loop calls HVF); it is
build-checked and exercised by the boot run.

## Component 2: boot harness — `spike/src/bin/boot.rs`

A codesigned binary (built, signed with `scripts/sign.sh`, run in the main
session). Steps:

1. Parse args: `argv[1]` = kernel Image path (required), `argv[2]` = initrd path
   (optional). Read each file into a `Vec<u8>`. Missing kernel arg → print usage,
   exit non-zero.
2. `mmap` guest RAM on the host: `RAM_SIZE = 0x2000_0000` (512 MiB),
   `PROT_READ|PROT_WRITE`, `MAP_ANON|MAP_PRIVATE`. Form a `&mut [u8]` over it.
3. `let entry = arch::aarch64::kernel::load_kernel(ram, layout::RAM_BASE, &kernel)?`.
4. Optional initrd: copy it into RAM at `INITRD_OFFSET = 0x0800_0000` (128 MiB
   into RAM — clear of a kernel at base and below the FDT); record
   `(layout::RAM_BASE + INITRD_OFFSET, initrd.len())`. (If absent, `None`.)
5. `let vm = Vm::new(false)?;` then `let gic = HvfGicV3::new(1, layout::RAM_BASE)?;`
   — GIC created after the VM and before any vCPU.
6. Build the FDT:
   ```
   let cfg = FdtConfig {
       mem_base: layout::RAM_BASE,
       mem_size: RAM_SIZE,
       cpu_mpidrs: vec![0],
       cmdline: layout::default_cmdline(),
       serial: MmioDev { addr: layout::SERIAL_BASE, size: layout::SERIAL_SIZE, irq: layout::SERIAL_SPI },
       gic: gic.fdt_info(),
       initrd, // Option<(u64, u64)>
   };
   let dtb = arch::aarch64::fdt::generate(&cfg)?;
   let fdt_addr = layout::fdt_addr(RAM_SIZE);
   ```
   Write `dtb` into the RAM slice at offset `(fdt_addr - layout::RAM_BASE)`
   (bounds-checked; `dtb.len() <= FDT_MAX_SIZE`).
7. `vm.map_memory(host_addr, layout::RAM_BASE, RAM_SIZE)?`.
8. Wire the device bus: a `Serial` writing to `io::stdout()` registered on a `Bus`
   at `[SERIAL_BASE, SERIAL_BASE + SERIAL_SIZE)`; wrap in `Arc<Bus>`.
9. `Vcpu::new(0 /*mpidr*/, entry, fdt_addr, bus).start()`, then `join()`. The
   vCPU thread sets `PC=entry`, `X0=fdt_addr` (via `set_initial_state`) and runs;
   the kernel's earlycon `STR`s to the 16550 THR are dispatched to stdout.
10. Print a startup banner (kernel/initrd sizes, entry, fdt_addr, GIC layout)
    before running so a silent failure is diagnosable.

`spike/Cargo.toml`: add a `[[bin]] name = "boot"` target. Dependencies already
present (`hvf`, `vmm`, `devices`, `arch`, `libc`, `log`, `env_logger`).

## Data flow

`kernel file → Vec → load_kernel → RAM@entry` · `layout+gic → FdtConfig →
generate → DTB → RAM@fdt_addr` · `RAM → hv_vm_map` · `vCPU run → MMIO write →
Bus → Serial → stdout`.

## Testing / acceptance

- **Now (no kernel):** `cargo build --workspace` clean; `cargo build -p hvf-spike
  --bin boot` clean. The run-loop change keeps the existing `hvf-spike`,
  `uart-echo`, `gic-smoke` bins building and the 21 arch unit tests passing.
- **When the kernel is supplied:** `scripts/sign.sh target/debug/boot` then
  `target/debug/boot <kernel> [initrd]`; success = visible kernel earlycon lines
  on stdout. This is run in the main session and debugged iteratively.

There is no synthetic-kernel unit test: a meaningful boot needs a real Image, and
the byte-placement logic it relies on (`load_kernel`, `fdt::generate`) is already
unit-tested in 2c/2a.

## Risks (expected live-debug iterations)

- **DTB/cmdline mismatch:** the `earlycon=uart8250,mmio,<SERIAL_BASE>` clause must
  match the kernel's 16550 earlycon driver; an FDT node the kernel rejects yields
  silence. Mitigation: the FDT is the unit-tested 2a output; adjust `cmdline`
  if the kernel wants a different earlycon form.
- **Load address / `text_offset`:** if the kernel advertises a non-zero
  `text_offset` or needs 2 MiB alignment we don't honor, the entry is wrong.
  `load_kernel` honors the header's `text_offset`; the banner prints `entry` for
  checking.
- **No-timeout WFI before earlycon:** if the kernel hits a bare `WaitForEvent`
  (no timer) before printing, the bounded-sleep approximation spins without
  progress. Then we add real parking (2e pulled forward).
- **MMIO width:** a halfword (`strh`) MMIO write panics in the hvf crate
  (recorded in `docs/phase1-followups.md`); a kernel doing halfword UART access
  would hit it. Watch for it during bring-up.

## Out of scope (→ 2e)

initramfs→shell, channel-based WFI parking that wakes on injected IRQs, serial RX
interrupt injection via `set_spi`, SMP (`vcpu_count > 1`), full virtual-timer
correctness, the `Vm`-owns-RAM refactor (the harness owns the mmap directly here,
matching the existing smoke bins).

## References

- `libkrun/src/vmm/src/macos/vstate.rs` — run loop + WFE parking (the channel
  version we approximate now and adopt in 2e)
- `crates/hvf/src/lib.rs` `run` / `set_initial_state` — exit model + boot regs
- `crates/arch/src/aarch64/{kernel,layout,fdt}.rs` — load/placement/DTB (2c/2a)
- `crates/hvf/src/gic.rs` — `HvfGicV3` (2b)
