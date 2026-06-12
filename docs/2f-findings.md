# Milestone 2f research: the blocker is virtio IRQ delivery, NOT the timer

Date: 2026-06-12. Status: **re-diagnosed** — the timer premise was wrong; the
real blocker is the virtio completion interrupt not waking the guest. Branch
`phase1-irq` (experiments reverted; tree clean).

## Corrected diagnosis (supersedes the earlier "vtimer wall" write-up)

1. **The virtual timer already works.** Instrumenting the vCPU run loop showed
   `HV_EXIT_REASON_VTIMER_ACTIVATED` **never fires** (0 occurrences across a full
   boot). The in-kernel `hv_gic` delivers the EL1 virtual timer to the guest
   itself; HVF does not surface it. So the scheduler tick / userspace `sleep`
   are fine — the timer was a red herring.

2. **The stall is a virtio poll loop.** Tracing every MMIO access, the guest's
   steady-state right before it goes idle is a tight repeat of:
   `WR 0x0a00_0050` (virtio QueueNotify) → `RD 0x0a00_0060` (InterruptStatus) →
   `WR 0x0a00_0064` (InterruptACK), at roughly one iteration per ~10 ms — i.e.
   matching the run loop's `MAX_PARK` WFI timeout (`hvf_vcpu.rs`).

3. **Conclusion:** the guest submits a virtio request, WFIs for the completion
   interrupt, and only makes progress when our bounded WFI sleep times out and
   re-enters (so it re-reads `InterruptStatus`, which we set synchronously during
   the notify). The **virtio completion IRQ raised via `hv_gic_set_spi(33, …)` is
   not actually waking the guest.** `hv_gic_set_spi` returns `HV_SUCCESS` (per
   gic-smoke) but success ≠ delivery. Boot therefore crawls at ~100 I/O/s and
   never finishes OpenRC's many reads.

## What to investigate next (the real 2f)

The milestone re-scopes to **make `hv_gic_set_spi` actually deliver the virtio
SPI to the guest**, or find the correct delivery call:

- **INTID correctness.** Confirm the SPI INTID. The FDT advertises virtio
  `interrupts = [SPI, 1, EDGE]` → INTID `32 + 1 = 33`, and we call
  `hv_gic_set_spi(33, …)`. Verify against `hv_gic_get_spi_interrupt_range()` and
  `hv_gic_get_intid()` that 33 is the value the in-kernel GIC expects (it may
  index differently).
- **Edge vs level / pulse timing.** We assert on notify (`set_spi(true)`) and
  deassert on ACK (`set_spi(false)`). The FDT says EDGE_RISING. Check whether the
  guest configured it level, and whether asserting *during the paused MMIO exit*
  (before the guest re-enters) latches the edge. Try a deassert→assert pulse, or
  asserting after the exit.
- **Does the guest enable the SPI?** Confirm the guest enabled INTID 33 in the
  distributor (it should, since it set up the virtio IRQ) — a redistributor/
  distributor reg read via `hv_gic_get_distributor_reg` could verify.
- **Cross-check with the serial.** The serial uses INTID 32 (SPI 0) but only for
  output (polled) — it has not exercised IRQ delivery, so virtio is the first
  real test of `hv_gic_set_spi` delivery. If virtio IRQ is fixed, the same path
  serves serial RX later.

## What works (unchanged)

- virtio-blk mounts the rootfs and runs init (2e); the kernel boots to userspace.
- The bounded-WFI timeout is currently masking the bug (the guest limps via
  timeout polling). Channel parking would EXPOSE it (a no-timeout `recv()` would
  hang outright until the IRQ fires) — so the IRQ-delivery fix must land before
  or with the parking change.

## Status

Research goal re-scoped: the in-kernel-GIC **vtimer is not the problem** (it works
natively); the problem is **virtio SPI interrupt delivery via `hv_gic_set_spi`**.
The earlier list-register/vtimer experiments are reverted (moot — that exit never
fires). Next: prove and fix `hv_gic_set_spi` delivery against the live boot.
