# Multi-vCPU Snapshot+Restore (stop-the-world) — Design

Date: 2026-06-13

## Goal

Lift the single-vCPU snapshot restriction. Snapshot and restore an `--smp N`
microVM — including `--smp N --net` — recreating every online core at its saved
PC so the restored guest sees all N cores, idles at ~0% CPU, and stays
responsive. Networking re-establishes via the existing link-bounce path with no
extra code.

## Background — why snapshot is single-vCPU today

The snapshot handler is gated to `smp == 1` in three places:

- `spike/src/bin/boot.rs` — `if smp == 1 { install handler } else { eprintln! }`.
- `crates/vmm/src/vstate/vcpu_manager.rs` — `set_snapshot_handler` asserts
  `mpidrs.len() == 1`; `request_snapshot` interrupts only `vcpuids.first()`.
- restore path — `assert_eq!(snap.config.vcpu_count, 1)`.

Two real obstacles:

1. **Per-vCPU register capture under HVF thread-affinity.** A vCPU's registers
   can only be read/written on its *own* thread (`hv_vcpu_get_reg` is
   thread-bound). With one vCPU the `Canceled`-exit handler runs on that single
   thread and saves it. With N vCPUs, *every* vCPU thread must participate — one
   thread cannot save another's regs.
2. **Schema.** `VmSnapshot` stores exactly one `VcpuState` (`snap.vcpu`); there is
   no per-vCPU array.

What is **already** SMP-safe: the GIC blob (`hv_gic_state_*`) is global and
contains the distributor plus *all* per-cpu redistributor state; per-vcpu GIC
CPU-interface (ICC) registers are saved inside each `VcpuState`.

## Architecture

Live-RAM-grab snapshot model is unchanged (no guest suspend). The new piece is a
**stop-the-world rendezvous**: all running vCPU threads quiesce at a `Canceled`
exit, each saves its own register state into a shared collection, and one leader
thread reads the global state (RAM + GIC + device records) and writes the
snapshot while the others wait. Restore is the mirror: create every online core,
restore the GIC once all redistributors exist, then each thread restores its own
registers and resumes at its saved PC.

Approach chosen: **A — Barrier rendezvous, primary-agnostic leader.** Two
`std::sync::Barrier`s bracket the global capture. (Rejected: B — channel+condvar
collection, same semantics with more moving parts; C — suspend-assisted via PSCI
`SYSTEM_SUSPEND` + kernel PM, a cleaner but much larger model, kept as future
work.)

### Components

1. **Schema (`crates/vmm/src/snapshot.rs`).** Replace `pub vcpu: VcpuState` with
   `pub vcpus: Vec<VcpuCheckpoint>`, where:

   ```rust
   pub struct VcpuCheckpoint {
       pub mpidr: u64,
       pub state: hvf::VcpuState,
   }
   ```

   The vector is sorted by `mpidr` before write (deterministic output).
   `config.vcpu_count` already exists. `magic`/`version` stay
   `ignition-snapshot-v2` / `2` — the single-vcpu reader path is replaced, not
   kept alongside (no mixed-version support needed; snapshots are local and
   short-lived).

2. **Snapshot coordination (`vcpu_manager.rs`).** New manager fields:
   - `snap_barrier: Mutex<Option<Arc<Barrier>>>` — built per snapshot, sized to
     the latched online count.
   - `collected: Mutex<Vec<(u64, Result<hvf::VcpuState, hvf::Error>)>>` — each
     thread pushes `(mpidr, save_state())`.
   - `snapshot_active: bool` (under the `running` mutex) — freezes CPU_ON during
     the snapshot window.

   The handler type changes:

   ```rust
   type SnapshotHandler = Box<dyn Fn(&[VcpuCheckpoint]) + Send + Sync>;
   ```

   The manager collects per-vCPU state internally and hands the leader the
   assembled slice; the closure does the global capture + file write.

   `run_loop` gains an `mpidr: u64` parameter so each thread tags its checkpoint.

3. **Restore (`vcpu_manager.rs`).** `run_restored(vcpu_state, gic_blob)` becomes
   `run_restored(checkpoints: Vec<VcpuCheckpoint>, gic_blob: Option<Vec<u8>>)`. It
   spawns one thread per checkpoint and pre-seeds `running` with all restored
   mpidrs (a later stray CPU_ON is then correctly rejected `AlreadyRunning`).

4. **Boot wiring (`spike/src/bin/boot.rs`).** Drop the `smp == 1` gate; install
   the handler for all `smp`. The closure builds
   `VmSnapshot { vcpus: checkpoints.to_vec(), .. }`. The net RX-quiesce
   (`stop_rx` + drain) is device-global and fires once in the leader before the
   RAM read — unchanged. Restore reads `snap.vcpus` and calls `run_restored`.

## Data flow

### Snapshot (N vCPUs)

1. `Ctrl-A s` → `request_snapshot` (console thread): lock `running`; latch
   `N = running.len()`; set `snapshot_active = true`; build
   `Arc<Barrier::new(N)>` into `snap_barrier`; clear `collected`; set
   `snapshot_req`; `request_exit` **every** registered vcpuid.
2. Each vCPU thread, on `Canceled` with `snapshot_req` set: `vcpu.save_state()`,
   push `(mpidr, result)` into `collected`, then `bar.wait()` (barrier₁). The
   barrier is a full happens-before edge → every push is visible afterward.
3. The `is_leader()` thread: if any saved result is `Err`, log the failing mpidr
   and **abort** (no file written); else sort `collected` by mpidr, build
   `&[VcpuCheckpoint]`, call the handler (reads RAM + GIC + device records, writes
   `memory.bin`/`gic.bin`/`disk.img`/`vmstate.json`). Non-leader threads block on
   `bar.wait()` (barrier₂).
4. Leader hits `bar.wait()` (barrier₂) → all release; `snapshot_active = false`;
   every thread `continue`s and resumes the guest.

### Restore (N vCPUs)

1. Read `snap.vcpus`. `run_restored(checkpoints, gic_blob)` spawns one thread per
   checkpoint; pre-seed `running` with all mpidrs.
2. Each thread: `HvfVcpu::new(mpidr)`, register vcpuid, `bar.wait()` (barrier₁ =
   "all redistributors exist").
3. Leader calls `gic_restore(blob)` once, then `bar.wait()` (barrier₂).
4. Each thread `restore_state(own checkpoint)` (regs + per-cpu ICC, after the GIC
   is restored) → `run_loop(mpidr)`. Guest resumes mid-execution on every core.

## CPU_ON freeze

`spawn` checks `snapshot_active` under the `running` mutex — the same lock
`request_snapshot` latches `N` under — so no `claim` can race with the latch. A
PSCI CPU_ON landing in the ~1 ms snapshot window is dropped (the guest already
received `X0 = 0` success from the hvf PSCI handler, so a dropped bring-up would
hang that core). This is documented: **take snapshots after boot completes**
(the boot-timer already marks this) — post-boot all cores are online and no
CPU_ON is in flight. Mid-boot snapshot is explicitly not guaranteed.

## Error handling

- **Per-vCPU save failure:** each thread stores a `Result`; the leader aborts the
  write if any is `Err` (torn-snapshot prevention), logs the failing mpidr, guest
  resumes. Best-effort/abortable, same contract as today.
- **Shutdown vs snapshot race:** if `shutdown` fires during the window, abort the
  snapshot (threads return via the top-of-loop shutdown check rather than
  deadlocking on the barrier).
- **Restore bring-up failure:** any `HvfVcpu::new` or `gic_restore` error
  propagates as the first error via `join_all`; partial bring-up aborts the
  restore.
- **vcpu_count:** the hard `assert_eq!(.., 1)` restore guard is removed; restore
  recreates exactly whatever `snap.vcpus` holds.

## Testing

- **Unit (`snapshot.rs`):** round-trip a `VmSnapshot` carrying
  `vcpus: Vec<VcpuCheckpoint>` of 4 distinct mpidrs; assert the deserialized
  order is sorted by mpidr and `config.vcpu_count` survives.
- **Unit (`vcpu_manager.rs`):** keep `mpidr_for` / `claim` tests; add a test that
  with `snapshot_active = true`, `claim`/`spawn` rejects a CPU_ON (pure state, no
  HVF).
- **Live (the success bar):** `boot --smp 4 Image rootfs.ext4`, `Ctrl-A s`,
  `--restore` → guest `nproc` == 4, idles ~0% CPU, responsive. Repeat with
  `--smp 4 --net` → all cores up + internet.
- **Headless driver:** extend `scripts/restore_test.py` (or add
  `restore_smp_test.py`) to boot `--smp 4`, snapshot, restore, assert responsive
  and `nproc` == 4.

## Scope / YAGNI

- Multi-vCPU snapshot for all `smp`, including `--net` (still requires `sudo` for
  vmnet). Restore recreates the online core set at saved PCs.
- No CPU_OFF / hotplug (PSCI bring-up stays one-way).
- No mid-boot snapshot guarantee — a concurrent CPU_ON during the snapshot window
  is dropped; snapshot after boot.
- Active network connections still reset on restore (link bounce — accepted).
- Suspend-assisted snapshot (PSCI `SYSTEM_SUSPEND` + kernel `PM_SLEEP`, using the
  kernel's own freeze/restore hooks) explicitly out — a cleaner but much larger
  alternative model, documented as future work.
