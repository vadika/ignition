# Snapshot & restore

See [The clone primitive](../concepts/clone-primitive.md) for the mechanism.

ignition snapshots a running guest and restores it lazily from an immutable base.

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

## GUI snapshot, restore & fan-out

A `--gui` guest (the cage + foot desktop over virtio-gpu/virtio-input) snapshots
and restores like any other: `Ctrl-A s` writes a snapshot of the live desktop,
and `boot --gui --restore <name>` reopens a window with the desktop resuming
where it left off. The virtio-gpu resource table and scanout binding plus the
virtio-input config cursor are serialized; pixels are not — on restore the
device re-reads the scanout from the restored guest-RAM backing and presents one
frame, so the window paints the resumed screen before the guest runs again.

Because each restore clones the immutable base into its own copy-on-write
instance dir (keyed by pid), one warm base fans out into N independent desktops,
each with its own window:

```console
# take one warm-base snapshot of a logged-in desktop (Ctrl-A s), then:
scripts/fanout-gui.sh 3 warm-base
# -> 3 boot --gui --restore processes, 3 windows, 3 isolated guests
```

Networking fans out too. Pass `--net` (needs `sudo` for vmnet shared mode) when
you snapshot and when you fan out, and each clone gets its **own MAC and DHCP
lease** — verified with 3 simultaneous clones, each on a distinct IP:

```console
sudo scripts/fanout-gui.sh 3 warm-base --net
```

This works because the GUI rootfs runs the same `netwatch` carrier-poller as the
base rootfs: every restore starts a fresh vmnet interface (new MAC) and the VMM
bounces the virtio-net link down→up, the poller catches that edge, rebinds
`virtio_net` so the guest re-reads the fresh MAC, then re-runs DHCP. Without the
poller a restored guest would keep the snapshot's MAC and every clone would DHCP
to the same address.

The base snapshot is never mutated; closing a clone's window tears down only
that guest.

## Interactive reset-to-checkpoint

Two console hotkeys let you capture a running guest's state as an in-memory
"reset point" and roll the live guest back to it without tearing the VM down:

- **`Ctrl-A c`** — mark the current moment as the reset point. The VMM captures
  guest RAM (via an O(1) APFS `clonefile` copy), vCPU registers, GIC state, and
  virtio-device state, then prints `[reset point marked]` and lets the guest
  continue.
- **`Ctrl-A r`** — roll the running guest back IN PLACE to that reset point:
  guest RAM is restored (only the pages that changed when `--track-dirty` is
  armed, or a full copy without it — both produce a correct result), vCPU
  registers, GIC state, and virtio-device state are all applied, and under
  `--gui` the rolled-back screen is repainted. The guest then resumes from the
  reset-point moment. Prints `[reset to checkpoint]`. If no reset point exists
  yet, prints `reset: no checkpoint - press Ctrl-A c first`.

**Auto-seed on `--restore`.** When a guest is started with `boot --restore
<dir>`, the restored snapshot is automatically installed as the initial reset
point before the guest runs. `Ctrl-A r` therefore works immediately after a
restore — no `Ctrl-A c` needed. A fresh cold boot has no reset point until you
press `Ctrl-A c`.

**Distinct from `Ctrl-A s`.** `Ctrl-A s` writes a named snapshot directory to
disk (a full, persistent snapshot usable for future restores and fan-out clones).
`Ctrl-A c`/`Ctrl-A r` operate entirely in memory and on the live guest; no
directory is written.

**GIC mid-run re-restore.** Applying GIC state to a running guest (`hv_gic_set_state`
while the VM is live) is best-effort: all vCPUs are parked before the call and
the state is applied atomically from their perspective. If HVF rejects the call
mid-run the reset logs `[reset] gic_restore rejected mid-run ...` and continues;
any in-flight interrupts re-settle within a tick or two. This is the designed
fallback — the guest remains functional.

> **Disk non-divergence is required for correctness.** Reset rolls back guest
> RAM, vCPU registers, GIC state, and virtio-device state, but the disk is
> NOT rewound. If the guest has written to a read-write rootfs between the
> checkpoint and the reset, the rolled-back guest RAM (page cache, ext4
> journal, inode cache) will describe a disk that has moved on, causing
> filesystem corruption.
>
> The intended usage mounts the rootfs **read-only** and places all writable
> state (`/tmp`, browser profile, downloads, etc.) on a **tmpfs** overlay that
> lives in guest RAM. That RAM rolls back cleanly with `Ctrl-A r`, and the
> immutable rootfs never diverges. **A read-write rootfs that is written between
> `Ctrl-A c` and `Ctrl-A r` will corrupt the filesystem.**

## Related

- [The clone primitive](../concepts/clone-primitive.md) — the mechanism behind this feature.
- [Diff / incremental snapshots](diff-snapshots.md) — only-changed-pages snapshots on top.
- [Boot & restore latency](../benchmarks/boot-and-restore.md) — how fast restore is.
