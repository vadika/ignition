# Interactive reset-to-checkpoint (design)

**Status:** approved 2026-06-16 (architecture + data-flow + risks + testing + reset-point semantics)
**Track:** disposable-sandbox showcase. Sub-project A of the "disposable browser" idea.
**Predecessors:** snapshot/restore + diff-snapshots + dirty-tracking, the in-loop
fuzzer `reset()` primitive, GUI M1–M5 (incl. `present_scanout`).

## Goal

Add an interactive "snap the running guest back to a checkpoint" to the live VMM,
surfaced as console hotkeys:

- **`Ctrl-A r`** — reset the running guest in place to the current reset point:
  roll guest RAM back, restore vCPU registers + GIC + device state, repaint. Near
  the fuzzer's reset cost (p50 ~36 µs for the page copy + register restore; GUI
  adds device re-restore + one present).
- **`Ctrl-A c`** — capture the current moment as the (new) in-memory reset point.

The reset point is **seeded automatically on `--restore`** (the restored snapshot
*is* the default point), so `Ctrl-A r` works immediately in restore mode without
`Ctrl-A c`. In a fresh boot (no `--restore`), `Ctrl-A c` sets the first point.

This is sub-project A; the **disposable browser** (rootfs + `disposable-browser.sh`
+ docs) is sub-project B and gets its own spec — it is the showcase, not a
dependency.

## Background — what we reuse (verified, file:line)

The snapshot path already does, in pieces, everything reset needs — reset is the
snapshot rendezvous with an **inverted leader** (restore instead of save):

- **vCPU rendezvous (2-barrier).** `crates/vmm/src/vstate/vcpu_manager.rs:137`
  `request_snapshot()` latches participants, sizes a `Barrier`, signals all vCPUs
  to exit to the rendezvous; `:500-514` each vCPU saves state at barrier 1, leader
  runs the handler, barrier 2 resumes; `:532` `run_snapshot_leader()`.
- **Per-vCPU register restore on the vCPU's own thread.** `:268`
  `vcpu.restore_state(&cp.state)` in `run_restored_one` — `crates/hvf/src/lib.rs:789`
  writes GP/sysregs/SIMD/ICC/vtimer. This is the model: each vCPU restores ITSELF
  at the barrier (no cross-thread HVF calls).
- **`VcpuCheckpoint`** `crates/vmm/src/snapshot.rs:85` = `{ mpidr, state: VcpuState }`.
- **GIC.** `crates/hvf/src/gic.rs:102` `save_state()`, `:137` `gic_restore(blob)`
  (called once at restore, `vcpu_manager.rs:250`, after redistributors exist).
- **Device state.** `crates/vmm/src/device_manager.rs:180` `save()`,
  `crates/devices/src/virtio/mmio.rs` `restore_state` / inner `restore` (gpu/input
  added in M5; idempotent table rebuilds).
- **Dirty tracking.** `crates/vmm/src/dirty.rs` bitmap + `drain()`; armed in
  `run_restore` under `--track-dirty` (`spike/src/bin/boot.rs:1616`).
- **Fuzzer reset (the analog).** `crates/vmm/src/fuzz/controller.rs:380` `reset()`
  — `:95` `restore_pages()` copies dirty pages from a base buffer; `:86`
  `restore_ram()` full copy; `:410` `vcpu.restore_state()`. Single-vCPU, base kept
  in a `Vec`. We generalize to multi-vCPU + devices, sourcing from a clonefile.
- **APFS clonefile (O(1)).** `crates/vmm/src/snapshot.rs:194` `clonefile_or_copy()`.
- **Repaint.** virtio-gpu `present_scanout` (M5) re-reads the scanout from RAM and
  presents one frame.
- **Escape FSM.** `Ctrl-A x/s/b` live in the stdin reader's escape parser
  (`spike/src/bin/boot.rs`, `Action::Snapshot` etc.); we add `r` and `c`.
- **vmnet RX gate.** the snapshot handler sets `rx_stop` to quiesce the vmnet feeder
  (`boot.rs:1771`); reused during the rollback copy.

## The reset point

```
struct ResetPoint {
    pristine: <read-only mmap of a clonefile of the instance memory.bin>,
    vcpus:    Vec<VcpuCheckpoint>,   // per-mpidr registers/ICC/vtimer
    gic_blob: Vec<u8>,
    devices:  Vec<DeviceRecord>,     // each device's saved state (id + state JSON)
}
```

Held in the VMM as `Option<ResetPoint>` (None until seeded). One at a time
(`Ctrl-A c` replaces it). The pristine mmap is read-only and immutable; the live
guest RAM is the separate MAP_SHARED instance mapping.

**Seeding on restore.** In `run_restore`, after RAM is reassembled (clone + diff
overlay) and before vCPUs run: `clonefile` the instance `memory.bin` →
`pristine.bin` (O(1), APFS CoW), mmap it read-only, and build the initial
`ResetPoint` from the already-loaded `snap.vcpus`, `gic_blob`, `snap.devices`.
Cost: one clonefile, near-free.

**Fresh boot.** No reset point until `Ctrl-A c`.

## Capture — `Ctrl-A c`

`request_checkpoint()` (mirrors `request_snapshot`): park all vCPUs at the
rendezvous; the leader, with vCPUs parked (consistent RAM):

1. `clonefile` the live instance `memory.bin` → a fresh `pristine.bin`, mmap RO.
2. capture `gic.save_state()` → `gic_blob`; `frozen.save()` → device records.
3. collect each vCPU's `save_state()` at the barrier (as the snapshot path does).
4. if a dirty tracker is armed, `drain()` it and re-protect RAM, so the next
   `Ctrl-A r` rolls back only what changes AFTER this checkpoint.

Store as the new `ResetPoint`. Resume. (No disk snapshot is written — that is
still `Ctrl-A s`.)

## Rollback — `Ctrl-A r`

If no reset point: print `reset: no checkpoint — press Ctrl-A c first` and no-op.
Else `request_reset()`: park all vCPUs; leader (vCPUs parked):

1. `rx_stop = true` (quiesce vmnet RX feeder, if any).
2. RAM rollback from `pristine`:
   - dirty tracker armed → `drain()` page indices, copy only those pages
     `pristine[p] → instance[p]`; then `vm_protect_memory(RAM, R|X)` to re-arm.
   - no tracker → full copy `pristine → instance`.
3. `gic_restore(gic_blob)`.
4. for each saved device record: `dev.lock().restore_state(record)` (virtio
   transport + inner gpu/input/etc. state back to the checkpoint).
5. `rx_stop = false`.

Each vCPU, at the barrier on its own thread, calls `restore_state(checkpoint for
its mpidr)`. Barrier 2 → resume. Then the GPU handle's `present_scanout()`
repaints the rolled-back scanout into the window.

Net: the guest resumes at the checkpoint's PC with checkpoint RAM/registers/
devices — the browser snaps back to its clean homepage.

## Wiring (boot.rs)

- Escape FSM: add `Action::Reset` (`r`) and `Action::Checkpoint` (`c`).
- `VcpuManager`: add `request_checkpoint()` / `request_reset()` +
  `run_checkpoint_leader()` / `run_reset_leader()` parallel to the snapshot pair,
  plus a barrier handler arm where each vCPU does `save_state()` (checkpoint) or
  `restore_state()` (reset). A shared `Arc<Mutex<Option<ResetPoint>>>` holds the
  point; the leader reads/writes it.
- `run_restore`: build + install the initial `ResetPoint` (clonefile pristine +
  loaded blobs) before the run loop; pass the GPU handle so the post-reset
  `present_scanout` can fire (GUI mode).
- Fresh-boot path (`main`): no initial point; `Ctrl-A c` creates one. (Fresh boot
  has no `gic_blob`/device records loaded, so `Ctrl-A c` captures them live — the
  same `save_state()`/`frozen.save()` calls the snapshot handler already makes.)
- Available in both headless and `--gui`; under `--gui` add the `present_scanout`
  repaint (no-op headless).

## Risks & error handling

- **GIC mid-run re-restore.** `hv_gic_set_state` is proven only at create-time.
  Mitigation: all vCPUs parked before the call; it overwrites GIC state wholesale
  with the same blob shape the restore path uses. **Unknown until eyeballed.**
  Fallback: if HVF rejects it mid-run, skip GIC re-restore and let interrupts
  re-settle as the guest runs (degraded but functional) — gate behind a check, log
  the skip.
- **Requires nothing mandatory.** `--track-dirty` only selects the fast dirty-only
  path; without it `Ctrl-A r` does a correct full pristine→instance copy.
- **Device re-restore quiescence.** Done with all vCPUs parked + vmnet RX gated, so
  no in-flight MMIO/DMA. gpu/input restore rebuilds tables (idempotent); blk has no
  in-flight request while parked.
- **pristine.bin cost.** One O(1) clonefile per checkpoint; disk grows only as the
  guest dirties pages (CoW). Old pristine is dropped when `Ctrl-A c` replaces the
  point. Cleaned with the instance dir on exit.
- **Snapshot vs checkpoint confusion.** `Ctrl-A s` = disk snapshot (named lineage,
  unchanged). `Ctrl-A c` = in-memory reset point. Distinct keys, distinct messages.

## Correctness requirement — the disk must not diverge

Snapshot/restore stays consistent only because it restores **both** RAM and disk
to the same instant. Reset rolls back RAM + vCPU + GIC + virtio-device *state*
but **does not rewind the disk**. If the guest writes to the disk between
checkpoint and reset, the rolled-back guest RAM (page cache, ext4 journal, inode
cache) then describes a disk that has moved on — metadata mismatch, journal
replay errors, filesystem corruption.

Therefore reset is sound **only if the disk does not diverge between checkpoint
and reset.** The disposable-browser rootfs (sub-project B) guarantees this by
mounting the rootfs **read-only** and putting all writable state (browser
profile, `/tmp`, `/var`, downloads) on **tmpfs in guest RAM**, which rolls back
with RAM. With an immutable disk, RAM rollback alone is fully consistent and
fast — no disk machinery in the reset path.

This is a documented constraint of the reset feature, surfaced loudly in the
docs and enforced by the sub-project-B rootfs. Rolling the disk back too (for a
general writable-disk guest) is deliberately out of scope (see below).

## Out of scope (YAGNI)

- Multiple named in-memory checkpoints / a checkpoint stack (one point, replaced).
- Persisting the reset point to disk (that is `Ctrl-A s`).
- Cross-host / migration.
- Disk (virtio-blk) rollback to the checkpoint. Reset assumes an immutable /
  non-diverging disk (see the correctness requirement above); a guest that writes
  to its rootfs between checkpoint and reset is unsupported. General disk rollback
  (clonefile disk at checkpoint, copy-back on reset with virtio-blk quiesced) is a
  later concern, not built here. Browser state is ephemeral by design — it lives
  in RAM/tmpfs and rolls back with the machine.

## Testing

- **Unit (Rust):**
  - dirty-page rollback copy: synthetic `pristine` + `instance` buffers + a dirty
    index list → only those pages revert, untouched pages unchanged; full-copy path
    reverts everything.
  - `request_reset`/`request_checkpoint` participant + barrier sizing mirrors the
    existing snapshot rendezvous test.
  - `ResetPoint` seeding in restore mode: the initial point holds the loaded
    vcpu/gic/device blobs (assert non-empty after restore wiring).
- **Live eyeball (real gate):**
  1. GUI rootfs, `--track-dirty`: type junk in foot → `Ctrl-A r` → screen snaps
     back to the pre-typing state, still interactive.
  2. `--smp 2`: same, exercising multi-vCPU register restore.
  3. `--net`: link/IP survive or re-settle after reset.
  - **Disk consistency:** the live GUI rootfs used for these eyeballs must not
    write to its rootfs across a reset (read-only mount, or no post-checkpoint
    disk writes), per the correctness requirement. A reset that rolls back RAM
    against a diverged disk is expected to corrupt the guest FS — that is the
    constraint, not a bug to chase.
  4. Fresh boot (no `--restore`): `Ctrl-A r` → "press Ctrl-A c first"; `Ctrl-A c`
     then `Ctrl-A r` → rolls back to the marked moment.
  5. GIC sanity: after reset, timers/interrupts still fire (guest not wedged).
