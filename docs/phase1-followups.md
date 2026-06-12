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

## Constraints to remember (not bugs)

- **`Serial`/`BusDevice` only handle 1-byte accesses** (`data.len() == 1`); other
  widths are logged and dropped. Correct for a 16550 (byte-wide registers) and
  for the milestone guest (`strb`/`ldrb`), but a driver doing wider register
  access would silently no-op. Intentional, logged.

- **`NoIrqVcpus` stubs the whole interrupt/sysreg path** (no GIC): `handle_sysreg_read`
  returns `Some(0)`, `handle_sysreg_write` returns `true`, no IRQ injection. A
  booting kernel needs a real GIC-backed `Vcpus` impl (in-kernel `hv_gic` is the
  fast path; see HANDOFF GIC decision).
