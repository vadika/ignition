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

## The open problem

After (1)+(2) the guest no longer crashes; after (3) it still **livelocks**: 100%
CPU, the vCPU spins inside guest code and never exits to the host (an exit trace
showed zero MMIO/WFI/timer exits). Independent of the snapshot↔restore time gap
(reproduces back-to-back), so it is not vtimer drift / expired-timer backlog. The
captured PC is in kernel text with CPSR `…c5` (EL1h, IRQs masked at the snapshot
instant).

### Candidate next steps (for whoever picks this up)

- **ICH list registers / in-flight interrupt state.** `hv_gic_get/set_ich_reg`
  (LR0–15, HCR_EL2, VMCR) hold virtual interrupts mid-injection. If the snapshot
  caught the vCPU with an interrupt in a list register, not restoring it could leave
  the guest waiting on an interrupt that never re-fires. **Most likely next thing to
  try.**
- **Snapshot capture point.** The snapshot is taken by `hv_vcpus_exit` at an
  arbitrary instruction boundary (`Canceled`). HVF may need a cleaner quiesce, or
  the captured CPSR-IRQs-masked instant may be mid-critical-section. Try snapshotting
  while the guest runs a deterministic loop (`while :; do :; done`) and see if it
  resumes (rules out idle/WFI-specific issues).
- **A remaining vCPU register.** The sysreg set is generous but not exhaustive;
  diff against what HVF resets a fresh vCPU to. Consider OSLAR/OSDLR, the debug
  regs, or a trap-control reg.
- **Verify TX path on restore** by snapshotting a guest that is actively printing,
  to rule out "running fine but serial output not wired" (less likely — 100% CPU
  indicates busy, not idle).

## Tests / gate

15 test suites green (serde round-trips for every state struct; device save/restore;
snapshot dir write/read/magic). Workspace builds, 0 clippy. The live snapshot→restore
is the part that does not yet succeed.

## Commits

`f1bc34f` (vCPU+GIC state) → `17cd1d7` (the three live-debug fixes) + the lint
cleanup. Plus the Task 1–4 implementation + review-fix commits.
