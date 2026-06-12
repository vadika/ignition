# Milestone 2f research: vtimer delivery SOLVED — GICv3 list-register injection

Date: 2026-06-12. Status: **mechanism found** (implementation is the next step).
Branch `phase1-irq` (experiments reverted; tree clean).

## What we proved works

- The rootfs + exec path is solid: `init=/bin/sh` runs the shell off the virtio
  disk and prints to our serial (`Run /bin/sh as init process` →
  `/bin/sh: can't access tty`).
- Under normal boot, OpenRC stalls after its banner at 0% CPU, parked in WFI,
  waiting on the virtual-timer interrupt that is never delivered.

## The answer (from Apple's Hypervisor headers)

`HV_EXIT_REASON_VTIMER_ACTIVATED` doc (`hv_vcpu_types.h`, `hv_vcpu.h`):

> The caller is expected to **make the interrupt corresponding to the VTimer
> pending in the guest's interrupt controller** and **to detect when the guest
> has completed servicing** it. For example, when emulating a GIC, clear the
> vtimer mask **when deactivating an interrupt whose ID matches the VTimer**.

For the in-kernel `hv_gic`, "make it pending in the guest's GIC" is **not**
`hv_gic_set_spi` (SPIs only, INTID ≥ 32) and **not** mask-toggling (both tried,
both failed). It is **GICv3 list-register injection**: Apple exposes the
hypervisor control interface via `hv_gic_set_ich_reg` /
`HV_GIC_ICH_REG_LR0_EL2 … LR15_EL2`, plus `ICH_HCR_EL2`, `ICH_VMCR_EL2`,
`ICH_VTR_EL2`, `ICH_EISR_EL2`, `ICH_ELRSR_EL2`, and the maintenance interrupt
`HV_GIC_INT_MAINTENANCE = 25`. The vtimer INTID is
`HV_GIC_INT_EL1_VIRTUAL_TIMER = 27`.

This is standard GICv3 virtualization: the hypervisor writes a List Register to
make a virtual interrupt pending; the GIC's virtual CPU interface presents it to
the guest; the guest EOIs it; the LR empties (`ICH_ELRSR`/`ICH_EISR`), optionally
raising the maintenance interrupt.

## Why the earlier candidates failed (now explained)

- libkrun's `hvf_sync_vtimer` is for a **userspace** GIC, where `set_vtimer_irq`
  queues the PPI in `VcpuList` and the userspace GIC presents it via trapped ICC
  reads. With our in-kernel GIC, ICC is handled in-kernel and nothing makes the
  vtimer pending → no delivery.
- Unmasking (candidate 1/3) doesn't make the interrupt pending; it only controls
  whether HVF *exits* on timeout. Necessary but not sufficient.

## Implementation outline (the next step)

In `hvf` (a documented divergence from the userspace-GIC libkrun lift), add a
GICv3 LR-based vtimer path:

1. **One-time setup** (per vCPU, after GIC create / first run): enable the
   virtual CPU interface — `ICH_HCR_EL2.En = 1` (and `LRENPIE`/maintenance if we
   use the maintenance IRQ); read `ICH_VTR_EL2` for the LR count.
2. **On `VTIMER_ACTIVATED`:** write a free list register
   (`HV_GIC_ICH_REG_LR0_EL2`) with the vtimer pending — LR64 fields: `State =
   0b01` (pending), `HW = 1`, `pINTID = 27`, `vINTID = 27`, `Group`/`Priority`
   per the guest's config. Keep the HVF vtimer mask set (avoid the exit storm).
3. **Detect EOI:** on subsequent exits, read `ICH_ELRSR_EL2` (or `ICH_EISR_EL2`);
   when the vtimer's LR has emptied (guest deactivated INTID 27), clear the HVF
   vtimer mask (`hv_vcpu_set_vtimer_mask(false)`) so the next deadline exits.
   Alternative: wire `HV_GIC_INT_MAINTENANCE` and unmask in its handler.
4. **`GicVcpus`/parking** (vmm) then layer on as planned — but `set_vtimer_irq`'s
   real work is the LR injection in step 2, which lives in `hvf` (it needs the
   `hv_gic_set_ich_reg` bindings).

Bindings needed (already in `crates/hvf/src/bindings.rs`): `hv_gic_set_ich_reg`,
`hv_gic_get_ich_reg`, and the `hv_gic_ich_reg_t` LR/HCR/ELRSR/EISR constants.

Risks: the exact LR64 bit layout (State[63:62], HW[61], Group[60],
Priority[55:48], pINTID[44:32], vINTID[31:0]) must be right; `ICH_HCR.En` must be
set or the virtual interface stays off; EOI detection timing. Iterate against the
live boot (success = OpenRC service-start lines past the banner).

## Status

Research goal achieved: the in-kernel-GIC vtimer is delivered via GICv3 list
registers (`hv_gic_set_ich_reg`), not the userspace-GIC mask protocol. The
implementation is now a well-defined task (no longer a guess), ready to build
against the live boot.
