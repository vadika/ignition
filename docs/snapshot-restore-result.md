# Snapshot / restore — WORKING (single-vCPU, clone-capable, no-net)

> **SUPERSEDED (2026-06) — read as a historical record of the FIRST snapshot milestone.**
> This documents the original **single-vCPU, no-net, eager-read** restore via `--snap-dir`/
> `--restore <dir>`. Since then: **multi-vCPU** snapshot/restore (every online core saved +
> resumed), **virtio-net** snapshot/restore (`--smp` + `--net` + sudo, link-bounce re-init),
> and **fast restore** — `clonefile` + `mmap(MAP_SHARED)` lazy/immutable-base loading, a
> `--store <dir>`/`--name` store convention (`snapshots/<name>/` + `instances/<name>-<pid>/`),
> `manifest.json`, auto-generated names, and re-snapshot. The current CLI is `--store`/`--name`,
> not `--snap-dir`/`--restore <dir>`. See the design+plan under `docs/superpowers/` for the
> current behavior.

Date: 2026-06-12. Status: **snapshot + restore fully working.** A running guest can
be snapshotted (`Ctrl-A s`), and the snapshot directory restored (`boot --restore
<dir>`) into a fresh, responsive guest that idles at ~0% CPU, keeps time, and accepts
console input. The same snapshot can be restored multiple times into independent
guests (clone).

> Update (2026-06-13): device wiring now goes through a uniform `DeviceManager`
> (`vmm::device_manager`) — MMIO-window/SPI allocation, bus registration, FDT-node
> description, and snapshot enumeration are centralized behind the `MmioDevice`
> trait. The snapshot format is **v2** (`SNAP_MAGIC = "ignition-snapshot-v2"`): a
> self-describing device-record list replaces the hand-listed `VmConfig` device
> fields, with a `check_version` guard rejecting older snapshots. Live
> snapshot/restore/clone re-verified green after the refactor.

## What works, end to end

- **Snapshot** (`Ctrl-A s`): writes a complete directory — `memory.bin` (RAM dump),
  `gic.bin` (the `hv_gic_state` distributor/redistributor blob), `disk.img` (rootfs
  copy), `vmstate.json` (vCPU + device state). The guest resumes after snapshotting.
- **Restore** (`boot --restore <dir>`): loads RAM, creates the GIC + vCPU, restores
  the GIC state, applies the saved register/timer/device state, and resumes from the
  saved PC — no kernel reload, no FDT regeneration.
- **Responsive + idle**: the restored guest parks at ~0% CPU at its idle WFI and
  responds to typed input (login prompt, shell commands).
- **Clone**: restoring one snapshot twice yields two independent guests (private
  per-clone disk copy under `std::env::temp_dir()`).

Drivers (live, not `cargo test` — they need the hypervisor entitlement + a real
kernel/rootfs): `scripts/restore_test.py` (snapshot → restore → CPU% + responsive),
`scripts/restore_clone_test.py` (login + command + two clones).

## Bugs found and fixed via live restore debugging

Each was confirmed by the guest's failure mode changing:

1. **GIC restore needs create-first.** `hv_gic_set_state` restores INTO an existing
   in-kernel GIC; it does not create one. Create the GIC (`hv_gic_create`, same
   placement as a fresh boot) before restoring its state.
2. **Pointer-authentication keys.** The restored guest faulted on `autiasp`
   ("Attempted to kill the idle task"). The kernel signs return addresses with the
   PAC keys (APIA/APIB/APDA/APDB/APGA, HI+LO); a restored vCPU needs the same keys.
   Added all 10 to the captured set.
3. **FP/SIMD state.** Added Q0–Q31 + FPCR/FPSR capture/restore (otherwise glibc's
   NEON paths corrupt on resume).
4. **The livelock — three interacting causes (see below).**

## The livelock: root cause and the three-part fix

After (1)–(3) the restored guest no longer crashed but **livelocked at 100% CPU**,
PC pinned at the idle `wfi` (`arch_cpu_idle` / `cpu_do_idle`), with **zero host
exits** — i.e. spinning entirely inside `hv_vcpu_run`. Systematic instrumentation
(a kicked PC + vtimer-state sampler) established:

- The vtimer fires once; `CNTV_CTL.ISTATUS` latches and `CNTV_CVAL` then **never
  moves** — the guest never re-arms it, so the timer IRQ is never serviced.
- WFI wakes on the pending vtimer (so it never traps to the host → no exit), but the
  IRQ is **never delivered as an exception** (PC never enters a handler). Forcing
  `PSTATE.I` clear did not help → the interrupt was not deliverable at the CPU
  interface at all.

Three things had to be true for the guest to resume correctly:

1. **GIC state must be restored AFTER the vCPU exists.** `hv_gic_set_state` restores
   the per-cpu *redistributor* state, which includes the PPI enable bits that gate
   the virtual-timer interrupt (PPI 27). Restoring it before the vCPU is created
   (the old code created the GIC and restored its state up front, then created the
   vCPU) silently dropped the redistributor state, so the timer IRQ was never
   delivered. **This was the actual livelock.** Fix: `HvfGicV3::new` creates the GIC
   up front; `gic_restore(blob)` applies the saved state on the vCPU thread, after
   `HvfVcpu::new`, before `restore_state`. (`crates/hvf/src/gic.rs`,
   `crates/vmm/src/vstate/vcpu_manager.rs::run_restored_primary`.)
2. **The WFI exit handler must be vtimer-offset-aware** (`crates/hvf/src/lib.rs`,
   `EC_WFX_TRAP`). It compared `CNTV_CVAL` against raw `mach_absolute_time()`. That
   is correct only when `vtimer_offset == 0` (fresh boot). With a nonzero restore
   offset it read the comparator as perpetually expired and the host busy-looped on
   `WaitForEventExpired`. Fixed to compare against `CNTVCT = mach - vtimer_offset`
   (read back via `hv_vcpu_get_vtimer_offset`); reduces to the original on a fresh
   boot.
3. **The vtimer offset must make CNTVCT continuous** across the snapshot
   (`restore_state`). At snapshot time `vtimer_offset == 0`, so `CNTVCT == CNTPCT ==
   mach_absolute_time() == host_counter` (captured). On restore, set `offset =
   mach_now - host_counter` so CNTVCT resumes at the captured value instead of
   jumping forward by the wall-clock gap (a forward jump expires every armed
   clock-event deadline at once → timer storm).

On Apple Silicon `CNTPCT == mach_absolute_time()` and `CNTVCT = CNTPCT - offset`;
these were confirmed empirically by the offset/cval/cntvct sampler.

## Tests / gate

15 test suites green (serde round-trips for every state struct; device save/restore;
snapshot dir write/read/magic). Workspace builds, 0 clippy. Live snapshot→restore and
clone verified by the two driver scripts above.
