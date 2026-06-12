# Milestone 2f findings: vtimer delivery is a research wall (in-kernel GIC)

Date: 2026-06-12. Status: **blocked**, needs focused research (not more
guess-and-check). Branch `phase1-irq` (experiments reverted; tree clean).

## What we proved works

- **The rootfs + exec path is solid.** Booting `init=/bin/sh` (cmdline tweak)
  runs the shell off the virtio disk and it prints to our serial:
  `Run /bin/sh as init process` → `/bin/sh: can't access tty`. So the kernel
  execs userspace from `/dev/vda` and userspace output reaches stdout.
- The stall under normal boot is OpenRC waiting after its banner — at 0% CPU,
  parked in WFI (established in 2e), i.e. waiting on an interrupt that never
  arrives.

## The blocker

Userspace timed waits (OpenRC `sleep`/service timeouts, the scheduler tick) need
the **EL1 virtual-timer interrupt**, which is never delivered to the guest under
the in-kernel `hv_gic`.

`hvf::run` masks the vtimer on the HVF `VTIMER_ACTIVATED` exit; `hvf_sync_vtimer`
(verbatim libkrun, **userspace-GIC** logic) only unmasks once the timer condition
clears — which assumes a userspace GIC injected the PPI for the guest to ACK.
Under our in-kernel GIC nothing injects it, so it never clears, and it stays
masked.

## Candidates tried (all FAILED — boot still stalls at `OpenRC 0.52.1`)

1. **Unmask unconditionally** in `hvf_sync_vtimer` (drop the `if !irq_state`
   gate). No effect.
2. **Unmask before re-entry** at the top of `hvf::run` (so the guest runs with
   the vtimer live, not masked). No effect.

Neither makes the in-kernel GIC raise PPI 27 (`HV_GIC_INT_EL1_VIRTUAL_TIMER`).
Conclusion: unmasking the HVF vtimer mask alone does not cause the in-kernel GIC
to deliver the vtimer PPI. The CNTV→GIC PPI wiring needs something we haven't
found by inspection.

## What to investigate next (real research, not guessing)

- **Apple `hv_gic` semantics.** Read `Hypervisor/hv_gic*.h` in the SDK and Apple
  docs for how the architected virtual timer connects to the in-kernel GIC's PPI
  27. There may be a required `hv_gic` call, a redistributor config, or a
  different exit/mask protocol than the userspace-GIC model libkrun uses.
- **`hv_vcpu_set_pending_interrupt` interplay.** Whether asserting the IRQ line
  makes the GIC present the vtimer INTID at `ICC_IAR1` (candidate 2 in the spec,
  not yet isolated from the masking changes).
- **Alternative: userspace GICv3** (libkrun's `gicv3.rs` — snapshot-friendly,
  has a PROVEN vtimer path via `VcpuList`). This is the HANDOFF's documented
  perf-vs-snapshot trade. Switching the GIC backend is a larger change but has a
  known-working timer + is needed for snapshot anyway. Could be the right pivot.

## Reaching a prompt without the timer (possible interim)

A bare interactive shell prints a prompt without sleeps. `init=/bin/sh` runs but
busybox is non-interactive on a non-tty (no PS1, no prompt). Forcing interactive
(`sh -i`) or wiring a minimal serial RX + tty so the shell sees a terminal could
surface a prompt independent of the timer — but that overlaps the deferred
serial-RX work.

## Decision needed

2f's vtimer goal is a genuine research problem. Options: (a) deep-research Apple
`hv_gic`'s vtimer wiring; (b) pivot the GIC to userspace gicv3 (known-good timer +
snapshot-ready); (c) chase an interim prompt via `sh -i` + minimal serial RX;
(d) pause 2f. The reverted experiments and this analysis are the milestone's
output so far.
