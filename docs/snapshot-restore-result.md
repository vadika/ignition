# Snapshot / restore — PARTIAL: snapshot works; restore resumes-without-crash but livelocks

Date: 2026-06-12. Status: **snapshot fully working; restore loads everything and
resumes without crashing, but the guest livelocks (100% CPU, spinning in guest code
with no host exits). Root cause not yet found.** Honest checkpoint — not a completed
milestone.

## What works

- **Snapshot** (`Ctrl-A s`): captures and writes a complete directory —
  `memory.bin` (512 MiB RAM dump), `gic.bin` (the `hv_gic_state` distributor/
  redistributor blob), `disk.img` (rootfs copy), `vmstate.json` (vCPU + device
  state). The guest **resumes and keeps running** after snapshotting. Verified end
  to end.
- **State model + I/O** (Tasks 1–3, all unit-tested, 15 suites green, 0 clippy):
  `VcpuState` (GP + 38 sysregs + FP/SIMD + vtimer + PAC keys + ICC), `VirtioMmioState`,
  `SerialSnapshot`, `VmSnapshot` write/read with magic validation + atomic temp-dir
  rename. The two-stage reviews caught + fixed: the FP/SIMD capture gap (would
  corrupt glibc on resume), the GIC state-object leak, the unsafe serial swap
  (replaced with a safe constructor), the instance-disk-pollutes-snapshot bug, the
  non-atomic write, and the handler-panic-unwinds-vCPU bug.
- **Restore** (`boot --restore <dir>`): loads RAM, restores the GIC, rebuilds the
  bus + devices from the saved state (private per-clone disk copy), creates the vCPU,
  applies the saved register state, and resumes from the saved PC — **no kernel
  reload, no FDT regeneration** (confirmed: 0 "Booting Linux" banners on restore).

## Three real bugs found + fixed via live restore debugging

Each was confirmed by the guest's failure mode changing:

1. **GIC restore needs create-first.** `HvfGicV3::from_state` originally called
   `hv_gic_set_state` alone → "Error restoring HVF GIC state". `hv_gic_set_state`
   restores INTO an existing GIC; it does not create one. Fixed to `hv_gic_create`
   (same placement config as `new`) then `hv_gic_set_state`.
2. **Pointer-authentication keys.** The restored guest panicked: "Attempted to kill
   the idle task", faulting on instruction `0xd50323bf` = `autiasp`. The kernel
   signs return addresses with the PAC keys (APIA/APIB/APDA/APDB/APGA, HI+LO); a
   restored vCPU with different keys fails authentication → corrupted pointer →
   crash. Added all 10 PAC key sysregs to the captured set. (Fixed the crash.)
3. **Per-vCPU GIC CPU-interface (ICC) state.** The ICC registers (CTLR/PMR/IGRPEN0/1/
   BPR0/1/SRE/AP0R0/AP1R0) live in the vCPU's interface, NOT in the
   distributor/redistributor `hv_gic_state` blob. Added `hv_gic_get/set_icc_reg`
   capture/restore. (Did not change the livelock — see below.)

## The open problem — narrowed by systematic debugging

After (1)+(2) the guest no longer crashes; after (3) it still **livelocks** at 100%
CPU. Also added (4) a **vtimer-offset continuity fix** (capture `mach_absolute_time`
at snapshot; on restore set `vtimer_offset += elapsed_host_counter` so the guest's
`CNTVCT` continues instead of jumping to host uptime) — correct and necessary, but
did NOT resolve the spin.

### What the systematic debugging established (tools: a gated PC sampler —
`IGNITION_SAMPLE` — that kicks the vCPU to read its PC, plus decoding instructions
straight out of `memory.bin`)

- **The spin is the kernel idle loop.** The PC sits at `0xffff800008b1182c` (~38/40
  samples). The instructions there (read from `memory.bin` at the matching offset)
  are `paciasp ; dsb sy ; wfi ; autiasp ; ret` — i.e. `arch_cpu_idle`. **WFI is
  returning immediately and the idle loop spins.**
- **Normal idle parks; restore idle spins.** A normally-booted guest left idle at
  the login prompt sits at **0.0% CPU** (WFI traps → host parks). The restored guest
  at the same idle loop is **100% CPU** (WFI does not trap). This is the core
  anomaly.
- **It is NOT the vtimer.** Force-masking the vtimer (`hv_vcpu_set_vtimer_mask(true)`)
  AND the offset-continuity fix both leave it spinning.
- **It is NOT interrupt delivery at the CPU interface.** Forcing `ICC_IGRPEN0/1 = 0`
  on restore does not stop the spin (IGRPEN gates *delivery as an exception*, not the
  WFI *wake*, which is at the redistributor/distributor pending level).
- **It is NOT the restored GIC blob.** Restoring with a *fresh* GIC
  (`hv_gic_create` only, skipping `hv_gic_set_state`) still spins. So no
  restored-pending-interrupt is the cause.
- **Independent of the snapshot↔restore time gap** (reproduces back-to-back).

### ROOT CAUSE FOUND + PROVEN: the virtual-timer comparator

Skipping the generic-timer sysregs on restore (`IGNITION_SKIP_TIMER` probe — not
restoring `CNTV_CTL/CVAL`, `CNTP_CTL/CVAL`, `CNTKCTL`) makes the guest **park at
0.0% CPU** instead of spinning. So the cause is the timer: the captured
`CNTV_CTL.ENABLE=1` plus the **absolute** `CNTV_CVAL` deadline, which in the
restored vCPU's counter domain is already-expired → `ISTATUS` stays set → the
virtual-timer PPI is perpetually pending at the redistributor → the idle WFI never
traps → 100% CPU spin. This explains why masking the vtimer, zeroing `IGRPEN`, and
using a fresh GIC all failed: none of them clear the redistributor pending bit that
the enabled+expired comparator keeps re-setting.

### The remaining blocker (well-defined): re-anchor the comparator

Skip-timer parks but is **non-functional** (timer disabled → no scheduler tick →
unresponsive). The guest needs the timer enabled with a comparator that is NOT
expired in the restored counter domain. Every host-side way to achieve that was
tried and is blocked by missing HVF primitives:

- **Adjust `vtimer_offset` by the mach-counter delta** — `mach_absolute_time` is a
  different counter domain than the guest's `CNTVCT`; both signs still spin.
- **Anchor the offset to the captured `CNTV_CVAL`** — same domain problem.
- **Binary-search the offset using `CNTV_CTL.ISTATUS`** — `ISTATUS` is not
  recomputed while the vCPU is stopped, so the host reads a stale value and the
  search never converges.
- **Write the relative `CNTV_TVAL`** (which would set `CVAL = CNTVCT + TVAL`,
  domain-independent) — HVF's sysreg enum exposes `CNTP_TVAL_EL0` but **not**
  `CNTV_TVAL_EL0`.
- HVF exposes no gettable `CNTVCT_EL0`/`CNTPCT_EL0` to compute a correct offset.

**Next-session options:** (a) run the restored vCPU for a single bounded step so the
timer hardware recomputes `ISTATUS`, then binary-search the offset across real
runs (slower but `ISTATUS` becomes live); (b) find the exact relationship between
`hv_vcpu_set_vtimer_offset` and `mach_absolute_time` (Apple HVF docs / a calibration
that reads back something live) and set the offset so `CNTVCT` continues from the
snapshot; (c) capture the guest's `CNTVCT` at snapshot via a guest-cooperative read
and re-anchor `CNTV_CVAL` to `CNTVCT_fresh + tick`. Note there may ALSO be a second
issue (serial-RX delivery on restore) to verify once the timer is fixed — skip-timer
parked but did not respond to typed input.

The `IGNITION_SAMPLE` PC sampler + `HvfVcpu::read_pc` + the `host_counter` capture
are left in (gated/harmless) for this work.

## Tests / gate

15 test suites green (serde round-trips for every state struct; device save/restore;
snapshot dir write/read/magic). Workspace builds, 0 clippy. The live snapshot→restore
is the part that does not yet succeed.

## Commits

`f1bc34f` (vCPU+GIC state) → `17cd1d7` (the three live-debug fixes) + the lint
cleanup. Plus the Task 1–4 implementation + review-fix commits.
