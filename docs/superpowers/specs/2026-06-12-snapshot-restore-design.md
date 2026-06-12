# Snapshot / restore (single-vCPU, clone-capable) — design

Date: 2026-06-12. Milestone: snapshot a running guest to disk and restore it in a
fresh process — repeatedly — so one snapshot clones into N independent microVMs
(Firecracker's headline use case).

## Goal

A running guest (logged-in shell, a marker written to `/tmp`) is snapshotted with
**Ctrl-A s** to a directory. `boot --restore <dir>` in a fresh process resumes the
guest exactly where it left off (RAM, registers, GIC, timer, device state intact).
Running `--restore` N times yields N independent VMs from the one immutable
snapshot.

## Scope (v1)

- **Single vCPU** (`--smp 1`). SMP snapshot (per-vCPU capture across N paused
  threads) is a fast follow-up.
- **No `--net`** — live vmnet state can't serialize; snapshot is refused if `--net`
  is active.
- **Full RAM dump** (no dirty-page diffing).

## Feasibility (HVF APIs, all present)

- vCPU: `hv_vcpu_get/set_reg` (GP), `hv_vcpu_get/set_sys_reg` (141 sysregs),
  `hv_vcpu_get/set_vtimer_mask`, `hv_vcpu_get/set_vtimer_offset`. Thread-affine —
  state is read/written only on the vCPU's own thread.
- GIC: `hv_gic_state_create` → `hv_gic_state_get_size` → `hv_gic_state_get_data`
  yields an **opaque state blob**; `hv_gic_set_state` restores it. No per-register
  enumeration needed.

## Approach: snapshot on the vCPU thread

vCPU registers can only be read on the owning thread (HVF thread-affinity), so the
snapshot runs **on the vCPU thread at a `Canceled` exit** (rejected alternatives:
reading vCPU state from a controller thread — impossible; a guest-triggered magic
MMIO — needs guest cooperation).

## Snapshot artifacts (a directory)

- `memory.bin` — the guest RAM dump (`RAM_SIZE`, 512 MiB).
- `gic.bin` — the opaque `hv_gic` state blob.
- `disk.img` — a copy of the rootfs **at snapshot time** (each clone gets independent
  disk state; the source blk file is flushed before copying).
- `vmstate.json` — serde JSON: a `version`/magic, `VmConfig`, `VcpuState`,
  `DeviceState`.

## Components

### `crates/hvf` — vCPU + GIC state access

- `HvfVcpu::save_state() -> VcpuState` (on the vCPU thread): all GP regs (X0–X30,
  FP, LR, SP, PC, PSTATE/CPSR) + a curated sysreg list + vtimer mask/offset.
  `HvfVcpu::restore_state(&VcpuState)` sets them on a fresh vCPU.
  - **Curated sysreg list** (EL1 guest-resume set, pinned in the plan): `SCTLR_EL1,
    TTBR0_EL1, TTBR1_EL1, TCR_EL1, MAIR_EL1, AMAIR_EL1, VBAR_EL1, SP_EL0, SP_EL1,
    ELR_EL1, SPSR_EL1, ESR_EL1, FAR_EL1, CONTEXTIDR_EL1, TPIDR_EL0, TPIDR_EL1,
    TPIDRRO_EL0, CPACR_EL1, CSSELR_EL1, AFSR0_EL1, AFSR1_EL1, PAR_EL1, MDSCR_EL1`,
    the EL2 regs set at boot (`HCR_EL2, CNTHCTL_EL2, CPTR_EL2, VBAR_EL2`), and the
    timer regs (`CNTV_CTL_EL0, CNTV_CVAL_EL0, CNTVOFF_EL2, CNTKCTL_EL1,
    CNTP_CTL_EL0, CNTP_CVAL_EL0`). `MPIDR_EL1` is set at vCPU create, not restored
    here. A `set` that returns an HVF error aborts restore (loud, not silent).
- `HvfGicV3::save_state() -> Vec<u8>` (`state_create`/`get_size`/`get_data`); a free
  `gic_restore(&[u8])` (`hv_gic_set_state`) applied before the vCPU is created.

### `crates/vmm/src/snapshot.rs` (new) — the state model + orchestration

```rust
const SNAP_MAGIC: &str = "ignition-snapshot-v1";

struct VmSnapshot { magic: String, config: VmConfig, vcpu: VcpuState, devices: DeviceState }
struct VmConfig { mem_size: u64, vcpu_count: u64, serial: MmioWindow, blk: MmioWindow }
struct VcpuState { gp: Vec<u64>, sysregs: Vec<(u32, u64)>, vtimer_mask: bool, vtimer_offset: u64 }
struct DeviceState { blk: VirtioMmioState, serial: SerialState }
```

(All `#[derive(Serialize, Deserialize)]`. `memory.bin`/`gic.bin`/`disk.img` are raw
files alongside, not embedded.)

- `write_snapshot(dir, …)` — serialize `vmstate.json`, dump the RAM slice to
  `memory.bin`, write `gic.bin`, flush + copy the disk to `disk.img`.
- `read_snapshot(dir) -> (VmSnapshot, gic_blob, paths)` — validate magic/version.

### Device save/restore

- `VirtioMmio::save() -> VirtioMmioState` / `restore(state)`: the register shadows
  (status, queue_sel, per-queue num/ready/addr-halves), `interrupt_status`, and each
  `Virtqueue`'s `last_avail_idx`/`used_idx`. `Virtqueue` gains `indices()`/
  `set_indices()` (the ring GPAs are re-derived from the restored addr shadows). The
  blk backing file is referenced by path — restore opens the private `disk.img`.
- `Serial::save()/restore()`: IER/IIR/LSR/MCR + the TX/RX FIFO bytes (via
  `vm_superio`'s state accessor `Serial::state()` / `from_state()`; wrap as needed).

### Orchestration

- **Snapshot:** the console reader thread's escape FSM gains **Ctrl-A s** →
  `snapshot_req.store(true)` + `vcpu_request_exit`. The vCPU `run_loop`, on
  `Canceled` with `snapshot_req` set, calls `do_snapshot()` (vcpu.save_state +
  gic.save_state + `write_snapshot`), clears the flag, and **continues the loop**
  (re-enters `run`) so the guest keeps running.
- **Restore (`boot --restore <dir>`):** `read_snapshot` → `Vm::new` + mmap
  `RAM_SIZE` + load `memory.bin` → `HvfGicV3::new` + `gic_restore(gic.bin)` → build
  the bus from `DeviceState` (+ a private copy of `disk.img`) → `HvfVcpu::new(mpidr)`
  + `restore_state` → spawn the vCPU thread → resume at the saved PC. **No kernel
  Image load, no FDT generation** — the running kernel + DTB are already in
  `memory.bin`.
- **Clone:** run `boot --restore <dir>` N times; each copies `disk.img` to a private
  path and mmaps its own RAM from `memory.bin`. The snapshot dir is immutable.

## Concurrency

Single-vCPU: the snapshot runs entirely on the one vCPU thread at a `Canceled`
exit — out of `hv_vcpu_run`, regs readable, RAM stable (no in-flight guest writes).
The reader thread only flags + kicks. The blk device is flushed (`sync_all`) before
the disk copy. No multi-thread pause coordination in v1.

## Error handling

- `--net` active → snapshot refused with a clear message.
- Snapshot write failure → log + keep the guest running (best-effort; never crashes
  the guest).
- Restore: validate `magic`/`version` + presence of all four artifacts +
  `mem_size` match; any mismatch → clear error + exit. A failed sysreg `set` aborts
  restore (a missing reg faults the guest on resume — fail loud).

## Testing

- **Unit (no HVF, pure):** serde round-trip of `VmSnapshot`/`VcpuState`/
  `DeviceState` (serialize → deserialize → equal); `Virtqueue` index save/restore;
  `VirtioMmio::save → restore` into a fresh device (queue indices + reg shadows
  match); `Serial::save → restore`.
- **Integration (the bar — entitlement + a TTY, NO sudo, drivable via piped input):**
  1. Boot (no net), log in, `echo CLONE_ME > /tmp/marker`, **Ctrl-A s** → snapshot
     dir written.
  2. `boot --restore ./snapshot` (fresh process) → shell resumes; `cat /tmp/marker`
     = `CLONE_ME` (RAM survived); the prompt responds (vCPU/GIC/timer restored).
  3. **Clone:** `boot --restore ./snapshot` twice → two independent VMs, each with
     the marker, each independently usable.

## Out of scope (v1)

- SMP snapshot (single-vCPU only) — fast follow-up.
- `--net` snapshot (live vmnet) — refused.
- Dirty-page diffing / incremental snapshots — full RAM dump.
- A versioned binary format with backward-compat (Firecracker's `versionize`) — a
  single `version` string + `serde_json` is enough for a research fork; a version
  mismatch is rejected, not migrated.
