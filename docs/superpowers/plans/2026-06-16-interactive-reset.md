# Interactive Reset-to-Checkpoint Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add interactive in-place reset-to-checkpoint to the live VMM — `Ctrl-A r` rolls the running guest back to a checkpoint (RAM + vCPU registers + GIC + virtio-device state, repaint under `--gui`); `Ctrl-A c` marks the current moment as the reset point; the point is auto-seeded on `--restore`.

**Architecture:** Reset is the snapshot rendezvous with an inverted leader. We add two new rendezvous reasons (checkpoint, reset) beside the existing snapshot one in `VcpuManager`, reusing its two-barrier vCPU park. A new `crate::reset` module owns the in-memory `ResetPoint` (an O(1) clonefile `PristineRam` of guest RAM + the saved vcpu/gic/device blobs) and pure RAM-rollback helpers. `boot.rs` installs checkpoint/reset handler closures (capturing live RAM, the GIC, frozen devices, the dirty tracker, and a shared `Arc<Mutex<Option<ResetPoint>>>`), wires `Ctrl-A c`/`Ctrl-A r` into the stdin escape FSM, and seeds the initial point in `run_restore`.

**Tech Stack:** Rust, macOS HVF (`ignition-hvf`), APFS `clonefile`, virtio-mmio devices. Build the binary with `cargo build -p ignition-spike --bin boot`; re-sign with `./scripts/sign.sh target/debug/boot`. Unit tests run with `cargo test -p ignition-vmm`.

---

## Spec

Source: `docs/superpowers/specs/2026-06-16-interactive-reset-design.md`. Read it before starting. The three hotkeys are distinct: `Ctrl-A s` = disk snapshot (unchanged), `Ctrl-A c` = mark in-memory reset point, `Ctrl-A r` = roll back to it. The one real unknown is GIC mid-run re-restore — handled with a logged fallback (Task 4 / Task 5).

**Correctness requirement (read this):** reset rolls back RAM + vCPU + GIC + virtio-device state but **does not** rewind the disk. It is sound **only if the disk does not diverge between checkpoint and reset** — otherwise the rolled-back guest RAM (page cache, ext4 journal, inode cache) describes a disk that moved on → FS corruption. The disposable-browser rootfs (sub-project B) guarantees this with a **read-only rootfs + tmpfs** for all writable state (which rolls back with RAM). This plan does not add disk rollback; it documents the constraint loudly (Task 6) and the live eyeballs (Task 5.10) must use a non-diverging disk.

## Verified code shapes (ground truth, file:line)

These were read live on 2026-06-16. Use them verbatim; re-read if a line drifts.

- **`VcpuCheckpoint`** — `crates/vmm/src/snapshot.rs:84-88`:
  ```rust
  #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
  pub struct VcpuCheckpoint {
      pub mpidr: u64,
      pub state: VcpuState,
  }
  ```
- **`DeviceRecord`** — `crates/vmm/src/snapshot.rs:35-42`: `{ id: String, base: u64, size: u64, spi: u32, fdt_kind: FdtKind, state: serde_json::Value }`.
- **`clonefile_or_copy(src, dst) -> io::Result<()>`** — `crates/vmm/src/snapshot.rs:194`.
- **Dirty granule** `pub const PAGE: usize = 16384;` and `DirtyTracker::drain(&self) -> Vec<u64>` (ascending page indices) — `crates/vmm/src/dirty.rs:11,47`.
- **Re-protect free fn** `ignition_hvf::vm_protect_memory(gpa, size, flags)` with `(HV_MEMORY_READ | HV_MEMORY_EXEC) as u64` — used at `crates/vmm/src/fuzz/controller.rs:399-403`.
- **Fuzzer rollback shape** `restore_ram(base:&[u8], live:&mut[u8])` and `restore_pages(base:&[u8], live:&mut[u8], pages:&[u64], page:usize)` — `crates/vmm/src/fuzz/controller.rs:86,95`. We re-create this shape in `reset.rs` (isolated file; do **not** edit the fuzzer).
- **GIC** `HvfGicV3::save_state(&self) -> Result<Vec<u8>, Error>` (`crates/hvf/src/gic.rs:102`) and free fn `ignition_hvf::gic::gic_restore(blob:&[u8]) -> Result<(), Error>` (`crates/hvf/src/gic.rs:137`).
- **vCPU** `HvfVcpu::save_state(&self) -> Result<VcpuState, Error>` (`crates/hvf/src/lib.rs:735`), `HvfVcpu::restore_state(&self, &VcpuState) -> Result<(), Error>` (`:789`).
- **DeviceManager** `freeze(self) -> FrozenDevices` (`crates/vmm/src/device_manager.rs:163`); `FrozenDevices::save(&self) -> Vec<DeviceRecord>` (`:180`) iterates `self.records` calling `r.dev.lock().unwrap().save()`. Per-device live restore is the symmetric `r.dev.lock().unwrap().restore(&rec.state)` (the `MmioDevice::restore(&mut self, &serde_json::Value) -> Result<(), ...>` used at `add_restored` `:113-130`).
- **VcpuManager struct fields** — `crates/vmm/src/vstate/vcpu_manager.rs:59-87`: `snapshot_req: AtomicBool`, `snapshot_active: AtomicBool`, `snap_barrier: Mutex<Option<Arc<Barrier>>>`, `collected: Mutex<Vec<(u64, Result<VcpuState, Error>)>>`, `snapshot_handler: Option<SnapshotHandler>`, `dirty: Option<DirtyConfig>`, plus `vcpuids: Mutex<Vec<u64>>`, `running: Mutex<HashSet<u64>>`.
- **`request_snapshot()`** — `:137-166`; **vCPU `Canceled` arm** — `:495-515`; **`run_snapshot_leader()`** — `:532-567`; **`run_restored_one`** GIC+restore pattern — `:233-270`.
- **boot.rs `Action` enum** — `:59-71`; **`step()` match** — `:84-100` (`b's' => Action::Snapshot`); **`spawn_stdin_reader`** — `:162-225` (dispatch `Action::Snapshot => manager.request_snapshot()` at `:209-211`).
- **boot.rs `run_restore()`** `#[allow(clippy::too_many_arguments)]` — `:1490`; instance `memory.bin` clonefile + `MAP_SHARED` mmap — `:1567` and the `libc::mmap(... MAP_SHARED ...)` block; dirty arm — `:1635-1651`; net `rx_stop` — `ctx.rx_stop`; GUI tail `gpu.present_scanout()` + spawn VMM thread + `run_event_loop` — `:1936-1967`.
- **boot.rs fresh-boot `main`** RAM `MAP_ANON|MAP_PRIVATE` mmap — `:1263-1277`; dirty arm — `:868-901`; `frozen = mgr.freeze()` then `VcpuManager::new` + `set_snapshot_handler` + `set_dirty_config` — `:920-1085`; run — `:1136-1150`.
- **`DeviceContext.gpu_mmio: Option<Arc<Mutex<VirtioMmio>>>`** — `:340-362`; `VirtioMmio::present_scanout(&self)` — `crates/devices/src/virtio/mmio.rs:289`.

---

## File structure

- **Create `crates/vmm/src/reset.rs`** — `ResetPoint`, `PristineRam` (enum: `Mapped` RO-mmap | `Owned` Vec), `rollback_full`, `rollback_pages`. Pure + filesystem-only; no HVF. Unit-tested.
- **Modify `crates/vmm/src/lib.rs`** — add `pub mod reset;`.
- **Modify `crates/vmm/src/device_manager.rs`** — add `FrozenDevices::restore(&self, &[DeviceRecord])`.
- **Modify `crates/vmm/src/vstate/vcpu_manager.rs`** — checkpoint/reset rendezvous: new fields, `request_checkpoint`/`request_reset`, leaders, vCPU `Canceled` arms, shared `reset_point`.
- **Modify `spike/src/bin/boot.rs`** — `Action::Checkpoint`/`Action::Reset`, FSM keys `c`/`r`, dispatch, install handlers + seed `ResetPoint` in `run_restore` and `main`, repaint after reset under `--gui`.
- **Modify docs** — `docs/src/features/snapshot-restore.md`, `docs/src/features/devices.md`, `docs/src/getting-started/guest-assets.md`.

---

## Task 1: `reset` module — ResetPoint shell + pure RAM-rollback helpers

**Files:**
- Create: `crates/vmm/src/reset.rs`
- Modify: `crates/vmm/src/lib.rs` (add `pub mod reset;`)
- Test: inline `#[cfg(test)]` in `crates/vmm/src/reset.rs`

- [ ] **Step 1: Register the module**

In `crates/vmm/src/lib.rs`, add alongside the other `pub mod` lines (e.g. after `pub mod dirty;`):

```rust
pub mod reset;
```

- [ ] **Step 2: Write the failing test for the rollback helpers**

Create `crates/vmm/src/reset.rs` with only the test module first:

```rust
//! In-memory reset-to-checkpoint: an immutable RAM image plus the saved
//! vcpu/GIC/device state, and the pure helpers that roll live RAM back to it.

#[cfg(test)]
mod tests {
    use super::*;

    const PG: usize = 16384;

    #[test]
    fn rollback_full_reverts_everything() {
        let pristine = vec![0xAAu8; PG * 4];
        let mut live = vec![0xFFu8; PG * 4];
        rollback_full(&pristine, &mut live);
        assert_eq!(live, pristine);
    }

    #[test]
    fn rollback_pages_reverts_only_listed_pages() {
        let pristine = vec![0xAAu8; PG * 4];
        let mut live = vec![0xFFu8; PG * 4];
        // Revert pages 1 and 3 only.
        rollback_pages(&pristine, &mut live, &[1, 3], PG);
        assert!(live[0..PG].iter().all(|&b| b == 0xFF), "page 0 untouched");
        assert!(live[PG..2 * PG].iter().all(|&b| b == 0xAA), "page 1 reverted");
        assert!(live[2 * PG..3 * PG].iter().all(|&b| b == 0xFF), "page 2 untouched");
        assert!(live[3 * PG..4 * PG].iter().all(|&b| b == 0xAA), "page 3 reverted");
    }

    #[test]
    fn rollback_pages_skips_out_of_range_index() {
        let pristine = vec![0xAAu8; PG * 2];
        let mut live = vec![0xFFu8; PG * 2];
        // Page 99 is past the end; must not panic and must leave RAM unchanged.
        rollback_pages(&pristine, &mut live, &[99], PG);
        assert!(live.iter().all(|&b| b == 0xFF));
    }
}
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test -p ignition-vmm reset::tests`
Expected: FAIL — `cannot find function rollback_full` / `rollback_pages`.

- [ ] **Step 4: Implement the rollback helpers**

Prepend to `crates/vmm/src/reset.rs` (above the test module):

```rust
/// Copy the entire pristine image over live RAM. Used when no dirty tracker is
/// armed, so every page may have changed.
pub fn rollback_full(pristine: &[u8], live: &mut [u8]) {
    debug_assert_eq!(pristine.len(), live.len(), "pristine and live RAM must match in size");
    live.copy_from_slice(pristine);
}

/// Copy only the listed pages from the pristine image back over live RAM.
/// `page` is the tracking granule (`crate::dirty::PAGE`). Indices past the end
/// of RAM are skipped (defensive — `drain()` never emits them).
pub fn rollback_pages(pristine: &[u8], live: &mut [u8], pages: &[u64], page: usize) {
    debug_assert_eq!(pristine.len(), live.len(), "pristine and live RAM must match in size");
    for &p in pages {
        let start = (p as usize) * page;
        if start >= live.len() {
            continue;
        }
        let end = (start + page).min(live.len());
        live[start..end].copy_from_slice(&pristine[start..end]);
    }
}
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p ignition-vmm reset::tests`
Expected: PASS (3 tests).

- [ ] **Step 6: Commit**

```bash
git add crates/vmm/src/lib.rs crates/vmm/src/reset.rs
git commit -m "reset: pure RAM-rollback helpers (rollback_full/rollback_pages)"
```

---

## Task 2: `PristineRam` — clonefile + RO mmap, with an owned fallback

**Files:**
- Modify: `crates/vmm/src/reset.rs`
- Test: inline `#[cfg(test)]` in `crates/vmm/src/reset.rs`

`PristineRam` is the immutable RAM image a reset rolls back to. In restore mode it is an O(1) `clonefile` of the instance `memory.bin` mapped read-only (CoW on disk, no host-RAM doubling). In fresh boot there is no backing file (`MAP_ANON`), so it falls back to an owned `Vec<u8>` copy. Both expose `as_slice()`, so the rollback helpers stay byte-slice-pure.

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` in `crates/vmm/src/reset.rs`:

```rust
    #[test]
    fn pristine_owned_round_trips_bytes() {
        let src = vec![0x5Au8; PG * 2];
        let p = PristineRam::from_copy(&src);
        assert_eq!(p.as_slice(), &src[..]);
    }

    #[test]
    fn pristine_mapped_round_trips_bytes() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("ignition-pristine-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("memory.bin");
        let dst = dir.join("pristine.bin");
        let bytes = vec![0xC3u8; PG * 3];
        std::fs::File::create(&src).unwrap().write_all(&bytes).unwrap();

        let p = PristineRam::from_clone(&src, &dst, bytes.len()).unwrap();
        assert_eq!(p.as_slice(), &bytes[..]);

        drop(p);
        std::fs::remove_dir_all(&dir).ok();
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p ignition-vmm reset::tests`
Expected: FAIL — `cannot find type PristineRam`.

- [ ] **Step 3: Implement `PristineRam`**

Add to `crates/vmm/src/reset.rs` (above the test module, below the rollback helpers). Note `crate::snapshot::clonefile_or_copy` is reused for the CoW clone.

```rust
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::Path;

/// The immutable RAM image a `Ctrl-A r` rolls back to.
///
/// `Mapped` is a read-only mmap of an APFS clonefile (O(1), CoW on disk — the
/// disposable-browser fan-out case). `Owned` is a plain heap copy, used for a
/// fresh boot whose guest RAM has no backing file (`MAP_ANON`).
pub enum PristineRam {
    Mapped { ptr: *mut libc::c_void, len: usize },
    Owned(Vec<u8>),
}

// SAFETY: `Mapped` holds a read-only, immutable mmap. The pointer is never
// written through and the mapping outlives every reader (dropped only when the
// ResetPoint is replaced). Sharing the slice across threads is sound.
unsafe impl Send for PristineRam {}
unsafe impl Sync for PristineRam {}

impl PristineRam {
    /// Clone `src` to `dst` (CoW where supported) and map `dst` read-only.
    /// The caller is responsible for quiescing/`msync`ing `src` first.
    pub fn from_clone(src: &Path, dst: &Path, len: usize) -> io::Result<PristineRam> {
        crate::snapshot::clonefile_or_copy(src, dst)?;
        let f = std::fs::OpenOptions::new().read(true).open(dst)?;
        // SAFETY: mapping `len` bytes of a file we just created at `len`.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                f.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::other("mmap of pristine.bin failed"));
        }
        Ok(PristineRam::Mapped { ptr, len })
    }

    /// Take an owned copy of the current live RAM (fresh-boot fallback).
    pub fn from_copy(live: &[u8]) -> PristineRam {
        PristineRam::Owned(live.to_vec())
    }

    pub fn as_slice(&self) -> &[u8] {
        match self {
            // SAFETY: `ptr`/`len` came from a successful PROT_READ mmap and the
            // mapping is immutable for the lifetime of `self`.
            PristineRam::Mapped { ptr, len } => unsafe {
                std::slice::from_raw_parts(*ptr as *const u8, *len)
            },
            PristineRam::Owned(v) => v.as_slice(),
        }
    }
}

impl Drop for PristineRam {
    fn drop(&mut self) {
        if let PristineRam::Mapped { ptr, len } = self {
            // SAFETY: unmapping exactly the region we mapped.
            unsafe { libc::munmap(*ptr, *len) };
        }
    }
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p ignition-vmm reset::tests`
Expected: PASS (5 tests total).

- [ ] **Step 5: Commit**

```bash
git add crates/vmm/src/reset.rs
git commit -m "reset: PristineRam (clonefile RO-mmap, owned fallback)"
```

---

## Task 3: `FrozenDevices::restore` — revert live device state from records

**Files:**
- Modify: `crates/vmm/src/device_manager.rs`
- Test: inline `#[cfg(test)]` in `crates/vmm/src/device_manager.rs`

This mirrors the existing `FrozenDevices::save()` (`:180`): for each saved record, find the matching live device by `base` and push its state back via the `MmioDevice::restore` already used at `add_restored` (`:113-130`). A device whose restore fails is logged, not fatal (reset is best-effort, vCPUs are parked).

- [ ] **Step 1: Read the existing `save()` + its test**

Read `crates/vmm/src/device_manager.rs`. Confirm the `FrozenDevices` struct field `records: Vec<...>` and the exact element type (each element exposes `.id`, `.base`, and `.dev: Arc<Mutex<dyn MmioDevice>>`). Locate the `#[cfg(test)] mod tests` and the existing fake `MmioDevice` used to test `save()` — reuse it in Step 2. If the fake has no mutable state to round-trip, extend it with a single `u64` field that `save()` serializes and `restore()` reads back.

- [ ] **Step 2: Write the failing test**

Add to the device_manager `#[cfg(test)] mod tests`, reusing the existing fake device (named `TestDev` here — adjust to the actual local name). The test builds a manager with one device, freezes it, mutates the live device, then `restore`s the captured record and asserts the mutation was reverted:

```rust
    #[test]
    fn frozen_restore_reverts_live_device_state() {
        // Build a manager with one fake device, capture its records, then mutate
        // the live device and confirm restore() puts the saved state back.
        let mut mgr = test_manager_with_one_device(); // existing helper or inline build
        let frozen = mgr.freeze();
        let saved = frozen.save();

        // Mutate the live device away from the saved state.
        frozen.records()[0].dev.lock().unwrap().restore(&serde_json::json!({ "v": 999 })).unwrap();

        // Roll it back.
        frozen.restore(&saved);

        let now = frozen.save();
        assert_eq!(now, saved, "device state must match the captured records after restore");
    }
```

If `FrozenDevices` has no `records()` accessor for the test, assert via `frozen.save()` equality instead of reaching into `records` directly (preferred — keeps the field private).

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test -p ignition-vmm device_manager`
Expected: FAIL — `no method named restore found for ... FrozenDevices`.

- [ ] **Step 4: Implement `FrozenDevices::restore`**

Add to the `impl FrozenDevices` block in `crates/vmm/src/device_manager.rs`, beside `save()`:

```rust
    /// Push each saved record's state back into the matching live device,
    /// matched by MMIO base. Best-effort: a device that rejects its state is
    /// logged and skipped (used by the in-place reset path, vCPUs parked).
    pub fn restore(&self, records: &[DeviceRecord]) {
        for rec in records {
            let Some(r) = self.records.iter().find(|r| r.base == rec.base) else {
                log::warn!("reset: no live device at base {:#x} for record {}", rec.base, rec.id);
                continue;
            };
            if let Err(e) = r.dev.lock().unwrap().restore(&rec.state) {
                log::warn!("reset: device {} restore failed: {e}", rec.id);
            }
        }
    }
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p ignition-vmm device_manager`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/vmm/src/device_manager.rs
git commit -m "device_manager: FrozenDevices::restore reverts live device state from records"
```

---

## Task 4: Checkpoint + reset rendezvous in `VcpuManager`

**Files:**
- Modify: `crates/vmm/src/vstate/vcpu_manager.rs`
- Test: inline `#[cfg(test)]` in `crates/vmm/src/vstate/vcpu_manager.rs`

Add two rendezvous reasons beside snapshot. **Checkpoint** is nearly identical to snapshot: vCPUs save their own state at barrier-1 → leader builds a `ResetPoint` (via a handler closure) instead of writing to disk. **Reset** is inverted: leader rolls back RAM/GIC/devices at barrier-1→2 (via a handler closure), then each vCPU restores *its own* registers from the shared `ResetPoint` after barrier-2 — exactly the `run_restored_one` model.

The single in-flight guard `snapshot_active` is reused for all three reasons (only one rendezvous at a time) — rename it `rendezvous_active` for clarity.

- [ ] **Step 1: Read the current rendezvous code**

Read `crates/vmm/src/vstate/vcpu_manager.rs` fully. Confirm the struct fields (`:59-87`), `request_snapshot` (`:137-166`), the vCPU `Canceled` arm (`:495-515`), `run_snapshot_leader` (`:532-567`), and the existing `#[cfg(test)] mod tests` (note how it constructs a `VcpuManager` and how/whether it seeds `vcpuids`).

- [ ] **Step 2: Add imports, handler types, and struct fields**

At the top of the file, alongside the existing `use` lines, add:

```rust
use crate::reset::ResetPoint;
```

Near the existing `SnapshotHandler` type alias, add:

```rust
/// Builds and stores a `ResetPoint` from the vCPU checkpoints collected at the
/// barrier (clonefile pristine + capture gic/devices). Runs on the leader vCPU
/// thread with all vCPUs parked.
pub type CheckpointHandler = Box<dyn Fn(Vec<VcpuCheckpoint>) + Send + Sync>;

/// Rolls live RAM/GIC/device state back to the current `ResetPoint`. Runs on the
/// leader vCPU thread with all vCPUs parked. Per-vCPU register restore happens
/// afterward on each vCPU's own thread.
pub type ResetHandler = Box<dyn Fn() + Send + Sync>;
```

Rename the `snapshot_active` field to `rendezvous_active` (update every reference — `request_snapshot` and `run_snapshot_leader`). Then add to the `VcpuManager` struct:

```rust
    checkpoint_req: AtomicBool,
    reset_req: AtomicBool,
    checkpoint_handler: Option<CheckpointHandler>,
    reset_handler: Option<ResetHandler>,
    /// Shared with the handler closures and read by each vCPU at a reset barrier
    /// to find its own checkpoint. `None` until seeded (on `--restore`) or set by
    /// `Ctrl-A c`.
    reset_point: Arc<Mutex<Option<ResetPoint>>>,
```

In the `VcpuManager::new` constructor, initialize them:

```rust
            checkpoint_req: AtomicBool::new(false),
            reset_req: AtomicBool::new(false),
            checkpoint_handler: None,
            reset_handler: None,
            reset_point: Arc::new(Mutex::new(None)),
```

- [ ] **Step 3: Add setters and the shared-point accessor**

Beside `set_snapshot_handler`, add:

```rust
    pub fn set_checkpoint_handler(&mut self, h: CheckpointHandler) {
        self.checkpoint_handler = Some(h);
    }

    pub fn set_reset_handler(&mut self, h: ResetHandler) {
        self.reset_handler = Some(h);
    }

    /// The shared reset point. Cloned by `boot.rs` so the handler closures and
    /// the seeding code can read/write the same `Option<ResetPoint>`.
    pub fn reset_point(&self) -> Arc<Mutex<Option<ResetPoint>>> {
        self.reset_point.clone()
    }

    /// True once a reset point exists (seeded on restore or set by `Ctrl-A c`).
    pub fn has_reset_point(&self) -> bool {
        self.reset_point.lock().unwrap().is_some()
    }
```

- [ ] **Step 4: Write the failing tests (request no-ops + reason flags)**

Add to the `#[cfg(test)] mod tests`. These cover the parts testable without HVF: a `request_*` with no handler installed is a no-op (does not set its flag); `request_reset` with no reset point is a no-op. Mirror however the existing snapshot test builds a manager.

```rust
    #[test]
    fn request_checkpoint_without_handler_is_noop() {
        let m = test_manager(); // mirror the existing snapshot test's constructor
        m.request_checkpoint();
        assert!(!m.checkpoint_req.load(Ordering::Relaxed));
    }

    #[test]
    fn request_reset_without_handler_is_noop() {
        let m = test_manager();
        m.request_reset();
        assert!(!m.reset_req.load(Ordering::Relaxed));
    }
```

- [ ] **Step 5: Run the tests to verify they fail**

Run: `cargo test -p ignition-vmm vcpu_manager`
Expected: FAIL — `no method named request_checkpoint` / `request_reset`.

- [ ] **Step 6: Implement `request_checkpoint` and `request_reset`**

Model both on `request_snapshot` (`:137-166`). `request_checkpoint` is identical except it gates on `checkpoint_handler`, sets `checkpoint_req`, and (like snapshot) clears `collected`. `request_reset` gates on `reset_handler` **and** a present reset point, sets `reset_req`, and does **not** touch `collected` (vCPUs restore themselves, nothing to collect).

```rust
    pub fn request_checkpoint(self: &Arc<Self>) {
        if self.checkpoint_handler.is_none() {
            return;
        }
        {
            let _running = self.running.lock().unwrap();
            if self.rendezvous_active.swap(true, Ordering::Relaxed) {
                return;
            }
        }
        let ids: Vec<u64> = self.vcpuids.lock().unwrap().clone();
        if ids.is_empty() {
            self.rendezvous_active.store(false, Ordering::Relaxed);
            return;
        }
        *self.snap_barrier.lock().unwrap() = Some(Arc::new(Barrier::new(ids.len())));
        self.collected.lock().unwrap().clear();
        self.checkpoint_req.store(true, Ordering::Release);
        for id in ids {
            let _ = ignition_hvf::vcpu_request_exit(id);
        }
    }

    pub fn request_reset(self: &Arc<Self>) {
        if self.reset_handler.is_none() || self.reset_point.lock().unwrap().is_none() {
            return;
        }
        {
            let _running = self.running.lock().unwrap();
            if self.rendezvous_active.swap(true, Ordering::Relaxed) {
                return;
            }
        }
        let ids: Vec<u64> = self.vcpuids.lock().unwrap().clone();
        if ids.is_empty() {
            self.rendezvous_active.store(false, Ordering::Relaxed);
            return;
        }
        *self.snap_barrier.lock().unwrap() = Some(Arc::new(Barrier::new(ids.len())));
        self.reset_req.store(true, Ordering::Release);
        for id in ids {
            let _ = ignition_hvf::vcpu_request_exit(id);
        }
    }
```

> Note on signatures: these take `self: &Arc<Self>` (the vCPU arms call leaders on `self` as an `Arc`, like `run_snapshot_leader`). If the existing `request_snapshot` takes `&self`, keep it `&self` and call the leaders via `self` — match whatever the file already does so call sites in `boot.rs` (`manager.request_*()` on an `Arc<VcpuManager>`) compile unchanged.

- [ ] **Step 7: Implement the two leaders**

Add beside `run_snapshot_leader`. The checkpoint leader drains `collected` into sorted `VcpuCheckpoint`s (reuse the exact draining/sorting/error logic from `run_snapshot_leader` `:532-567`) and passes them to the handler, which stores the `ResetPoint`. The reset leader just invokes its handler (RAM/GIC/device rollback lives in the closure).

```rust
    fn run_checkpoint_leader(self: &Arc<Self>) {
        let mut items = std::mem::take(&mut *self.collected.lock().unwrap());
        items.sort_by_key(|(mpidr, _)| *mpidr);
        let mut checkpoints = Vec::with_capacity(items.len());
        let mut failed = None;
        for (mpidr, res) in items {
            match res {
                Ok(state) => checkpoints.push(VcpuCheckpoint { mpidr, state }),
                Err(e) => {
                    failed = Some((mpidr, e));
                    break;
                }
            }
        }
        match failed {
            Some((mpidr, e)) => log::error!("checkpoint aborted: vcpu {mpidr:#x} save_state failed: {e}"),
            None => {
                if let Some(h) = &self.checkpoint_handler {
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| h(checkpoints)));
                    if r.is_err() {
                        log::error!("checkpoint handler panicked; guest resumed");
                    }
                }
            }
        }
        self.checkpoint_req.store(false, Ordering::Release);
        self.rendezvous_active.store(false, Ordering::Relaxed);
    }

    fn run_reset_leader(self: &Arc<Self>) {
        if let Some(h) = &self.reset_handler {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| h()));
            if r.is_err() {
                log::error!("reset handler panicked; guest resumed (state may be inconsistent)");
            }
        }
        // Leave reset_req set until after the per-vCPU register restore so the
        // Canceled arm knows to restore; clear it on the leader's own restore
        // path below is not possible, so clear here — peers read reset_point, not
        // reset_req, for the register step.
        self.reset_req.store(false, Ordering::Release);
        self.rendezvous_active.store(false, Ordering::Relaxed);
    }
```

- [ ] **Step 8: Extend the vCPU `Canceled` arm**

In the vCPU run loop `Canceled` arm (`:495-515`), keep the existing `snapshot_req` branch and add checkpoint + reset branches. Capture `mpidr` (already in scope per the snapshot arm). The reset branch must read `reset_req` **before** the leader clears it; latch it into a local at the top of the arm so the post-barrier register restore still fires:

```rust
VcpuExit::Canceled => {
    if self.snapshot_req.load(Ordering::Acquire) {
        // ... existing snapshot branch, unchanged ...
        continue;
    }
    if self.checkpoint_req.load(Ordering::Acquire) {
        // Same as snapshot: save our own registers, meet at the barrier.
        let st = vcpu.save_state();
        self.collected.lock().unwrap().push((mpidr, st));
        let bar = self.snap_barrier.lock().unwrap().clone()
            .expect("snap_barrier set when checkpoint_req is set");
        if bar.wait().is_leader() {
            self.run_checkpoint_leader();
        }
        bar.wait();
        continue;
    }
    if self.reset_req.load(Ordering::Acquire) {
        let bar = self.snap_barrier.lock().unwrap().clone()
            .expect("snap_barrier set when reset_req is set");
        // Barrier 1: all parked before the leader touches RAM/GIC/devices.
        if bar.wait().is_leader() {
            self.run_reset_leader();
        }
        // Barrier 2: rollback complete; now each vCPU restores its own registers.
        bar.wait();
        if let Some(rp) = self.reset_point.lock().unwrap().as_ref() {
            if let Some(cp) = rp.vcpus.iter().find(|c| c.mpidr == mpidr) {
                if let Err(e) = vcpu.restore_state(&cp.state) {
                    log::error!("reset: vcpu {mpidr:#x} restore_state failed: {e}");
                }
            }
        }
        continue;
    }
    return Ok(());
}
```

- [ ] **Step 9: Run the tests + full vmm suite**

Run: `cargo test -p ignition-vmm`
Expected: PASS — all prior suites green plus the two new `request_*` no-op tests. Fix any reference left over from the `snapshot_active` → `rendezvous_active` rename (compiler will point them out).

- [ ] **Step 10: Clippy + commit**

Run: `cargo clippy -p ignition-vmm --all-targets`
Expected: 0 warnings.

```bash
git add crates/vmm/src/vstate/vcpu_manager.rs
git commit -m "vcpu_manager: checkpoint + reset rendezvous (inverted snapshot leader)"
```

---

## Task 5: Wire `Ctrl-A c` / `Ctrl-A r` and seed the reset point in `boot.rs`

**Files:**
- Modify: `spike/src/bin/boot.rs`

No unit tests (needs HVF + a live guest). The real gate is the live eyeball at the end. There IS a unit test for `step()` at `boot.rs:1983-2023` — extend it so the FSM changes are covered.

- [ ] **Step 1: Extend the `Action` enum and `step()` FSM**

In `boot.rs:59-71` add two variants:

```rust
enum Action {
    Forward([u8; 2], usize),
    Pending,
    Quit,
    Snapshot,
    Checkpoint,
    Reset,
    Balloon,
}
```

In `step()` (`:84-100`), beside `b's' => Action::Snapshot,` add:

```rust
                b'c' => Action::Checkpoint,
                b'r' => Action::Reset,
```

- [ ] **Step 2: Extend the `step()` unit test, run it (fail then pass)**

In the existing `#[cfg(test)] mod tests` for `step` (`:1983-2023`), add cases asserting `Ctrl-A c` → `Action::Checkpoint` and `Ctrl-A r` → `Action::Reset` (match the existing test's style, e.g. comparing via a small helper or `matches!`):

```rust
    #[test]
    fn ctrl_a_c_is_checkpoint() {
        let mut s = EscState::Normal;
        assert!(matches!(step(&mut s, CTRL_A), Action::Pending));
        assert!(matches!(step(&mut s, b'c'), Action::Checkpoint));
    }

    #[test]
    fn ctrl_a_r_is_reset() {
        let mut s = EscState::Normal;
        assert!(matches!(step(&mut s, CTRL_A), Action::Pending));
        assert!(matches!(step(&mut s, b'r'), Action::Reset));
    }
```

Run: `cargo test -p ignition-spike --bin boot step` → first FAIL (variants missing if Step 1 skipped), then PASS after Step 1.

- [ ] **Step 3: Dispatch the new actions in `spawn_stdin_reader`**

In the `match step(...)` dispatch (`:194-222`), beside `Action::Snapshot`, add:

```rust
                Action::Checkpoint => {
                    eprintln!("\n[reset point marked]");
                    manager.request_checkpoint();
                }
                Action::Reset => {
                    if manager.has_reset_point() {
                        eprintln!("\n[reset to checkpoint]");
                        manager.request_reset();
                    } else {
                        eprintln!("\nreset: no checkpoint - press Ctrl-A c first");
                    }
                }
```

- [ ] **Step 4: Factor a reusable handler-installer helper**

Both `main` (fresh boot) and `run_restore` need to install the checkpoint + reset handlers with the same captured resources. Add a free function in `boot.rs` near the snapshot-handler setup. It captures: the live RAM pointer as a `usize` (the existing `host_usize` trick to stay `Send`), `ram_size`, the instance `memory.bin` path (or `None` for fresh boot → owned pristine), the instance dir (for fresh `pristine.bin` names), the GIC, the frozen devices, the dirty tracker, `rx_stop`, and the shared `reset_point` Arc.

```rust
/// Resources the checkpoint/reset handlers capture. `mem_file` is `Some` in
/// restore mode (instance memory.bin → clonefile pristine) and `None` for a
/// fresh boot (MAP_ANON → owned copy).
struct ResetWiring {
    host_usize: usize,
    ram_size: u64,
    mem_file: Option<PathBuf>,
    inst_dir: PathBuf,
    gic: Arc<HvfGicV3>,
    frozen: Arc<FrozenDevices>,
    dirty: Option<DirtyTracker>,
    rx_stop: Option<Arc<AtomicBool>>,
}

fn install_reset_handlers(manager: &mut VcpuManager, w: ResetWiring) {
    let point = manager.reset_point();

    // --- checkpoint: capture current RAM + gic + devices into a new ResetPoint ---
    {
        let point = point.clone();
        let ResetWiring { host_usize, ram_size, ref mem_file, ref inst_dir, ref gic, ref frozen, ref dirty, ref rx_stop } = w;
        let mem_file = mem_file.clone();
        let inst_dir = inst_dir.clone();
        let gic = gic.clone();
        let frozen = frozen.clone();
        let dirty = dirty.clone();
        let rx_stop = rx_stop.clone();
        manager.set_checkpoint_handler(Box::new(move |checkpoints| {
            // vCPUs parked. Quiesce the vmnet RX feeder during the RAM clone.
            if let Some(stop) = &rx_stop { stop.store(true, Ordering::Release); }
            let live: &[u8] = unsafe {
                std::slice::from_raw_parts(host_usize as *const u8, ram_size as usize)
            };
            let pristine = match &mem_file {
                Some(src) => {
                    // MAP_SHARED -> flush so the clonefile sees current RAM.
                    unsafe { libc::msync(host_usize as *mut libc::c_void, ram_size as usize, libc::MS_SYNC); }
                    let dst = inst_dir.join(format!("pristine-{}.bin", std::process::id()));
                    let _ = std::fs::remove_file(&dst);
                    match ignition_vmm::reset::PristineRam::from_clone(src, &dst, ram_size as usize) {
                        Ok(p) => p,
                        Err(e) => {
                            log::error!("checkpoint: clonefile pristine failed ({e}); falling back to copy");
                            ignition_vmm::reset::PristineRam::from_copy(live)
                        }
                    }
                }
                None => ignition_vmm::reset::PristineRam::from_copy(live),
            };
            let gic_blob = match gic.save_state() {
                Ok(b) => b,
                Err(e) => { log::error!("checkpoint: gic save_state failed: {e}"); if let Some(stop) = &rx_stop { stop.store(false, Ordering::Release); } return; }
            };
            let devices = frozen.save();
            // Discard dirty pages accumulated up to now and re-arm, so the next
            // reset rolls back only changes AFTER this checkpoint.
            if let Some(t) = &dirty {
                let _ = t.drain();
                let _ = ignition_hvf::vm_protect_memory(
                    layout::RAM_BASE, ram_size,
                    (HV_MEMORY_READ | HV_MEMORY_EXEC) as u64,
                );
            }
            *point.lock().unwrap() = Some(ignition_vmm::reset::ResetPoint {
                pristine, vcpus: checkpoints, gic_blob, devices,
            });
            if let Some(stop) = &rx_stop { stop.store(false, Ordering::Release); }
        }));
    }

    // --- reset: roll live RAM/GIC/devices back to the current ResetPoint ---
    {
        let point = point.clone();
        let ResetWiring { host_usize, ram_size, ref gic, ref frozen, ref dirty, ref rx_stop, .. } = w;
        let gic = gic.clone();
        let frozen = frozen.clone();
        let dirty = dirty.clone();
        let rx_stop = rx_stop.clone();
        manager.set_reset_handler(Box::new(move || {
            let guard = point.lock().unwrap();
            let Some(rp) = guard.as_ref() else { return; };
            if let Some(stop) = &rx_stop { stop.store(true, Ordering::Release); }
            let live: &mut [u8] = unsafe {
                std::slice::from_raw_parts_mut(host_usize as *mut u8, ram_size as usize)
            };
            match &dirty {
                Some(t) => {
                    let pages = t.drain();
                    ignition_vmm::reset::rollback_pages(rp.pristine.as_slice(), live, &pages, ignition_vmm::dirty::PAGE);
                    let _ = ignition_hvf::vm_protect_memory(
                        layout::RAM_BASE, ram_size,
                        (HV_MEMORY_READ | HV_MEMORY_EXEC) as u64,
                    );
                }
                None => ignition_vmm::reset::rollback_full(rp.pristine.as_slice(), live),
            }
            // GIC mid-run re-restore: proven only at create-time. Best-effort; on
            // rejection, log and let interrupts re-settle (spec fallback).
            if let Err(e) = ignition_hvf::gic::gic_restore(&rp.gic_blob) {
                log::warn!("reset: gic_restore rejected mid-run ({e}); continuing without GIC re-restore");
            }
            frozen.restore(&rp.devices);
            if let Some(stop) = &rx_stop { stop.store(false, Ordering::Release); }
        }));
    }
}
```

> Adjust the exact `use`/path prefixes (`ignition_vmm::reset::...`, `layout::RAM_BASE`, `HV_MEMORY_READ`) to match how `boot.rs` already refers to these (the snapshot handler and the restore dirty-arm show the in-scope names). `FrozenDevices`, `HvfGicV3`, `DirtyTracker`, `PathBuf`, `AtomicBool`, `Arc` are already imported in `boot.rs`.

- [ ] **Step 5: Install handlers in the fresh-boot path (`main`)**

After `frozen = Arc::new(mgr.freeze())` and the `VcpuManager::new` + `set_snapshot_handler` + `set_dirty_config` block (`:920-1085`), and before `spawn_stdin_reader`, call:

```rust
    install_reset_handlers(&mut manager, ResetWiring {
        host_usize: host as usize,
        ram_size,
        mem_file: None,            // MAP_ANON: owned-copy pristine on Ctrl-A c
        inst_dir: std::env::temp_dir(),
        gic: gic.clone(),
        frozen: frozen.clone(),
        dirty: dirty_tracker.clone(),
        rx_stop: rx_stop.clone(),  // whatever the fresh-boot net path named it; None if no --net
    });
```

Match the local variable names already present (`host`, `ram_size`, `gic`, `frozen`, `dirty_tracker`, and the fresh-boot `rx_stop`/`ctx.rx_stop`). The fresh-boot path has no reset point until the user presses `Ctrl-A c`.

- [ ] **Step 6: Install handlers AND seed the initial point in `run_restore`**

In `run_restore` after `setup_devices` + `frozen`/`manager` setup and the dirty-arm (around `:1714`+, before the run tail), seed the point from the already-loaded blobs, then install the handlers. The seed clones the instance `memory.bin` into a `pristine.bin` and builds the first `ResetPoint`:

```rust
    // Seed the reset point: the restored snapshot IS the default Ctrl-A r target.
    {
        let pristine_dst = inst_dir.join("pristine.bin");
        let _ = fs::remove_file(&pristine_dst);
        let pristine = ignition_vmm::reset::PristineRam::from_clone(&inst_mem, &pristine_dst, mem_size as usize)
            .map_err(|e| io::Error::other(format!("seed pristine clonefile: {e}")))?;
        *manager.reset_point().lock().unwrap() = Some(ignition_vmm::reset::ResetPoint {
            pristine,
            vcpus: snap.vcpus.clone(),
            gic_blob: gic_blob.clone(),
            devices: snap.devices.clone(),
        });
    }
    install_reset_handlers(&mut manager, ResetWiring {
        host_usize: host as usize,
        ram_size: mem_size,
        mem_file: Some(inst_mem.clone()),
        inst_dir: inst_dir.clone(),
        gic: gic.clone(),
        frozen: frozen.clone(),
        dirty: dirty_tracker.clone(),
        rx_stop: rx_stop_snap.clone(),
    });
```

`inst_mem`, `mem_size`, `host`, `snap.vcpus`, `gic_blob`, `snap.devices`, `inst_dir`, `gic`, `dirty_tracker`, `rx_stop_snap` are all in scope in `run_restore` (confirm names against the file). Note `snap.vcpus` / `gic_blob` are consumed later by `run_restored` — use `.clone()` here (VcpuCheckpoint derives Clone; gic_blob is a `Vec<u8>`). If `manager` is not `mut`, make it `let mut manager`.

- [ ] **Step 7: Repaint after a reset under `--gui`**

The reset hotkey runs on the vCPU thread; the window repaint must come from the GPU device. The simplest correct hook: after `request_reset`, the rolled-back scanout needs one `present_scanout`. Since the reset handler runs on the leader vCPU thread (no winit access) but `present_scanout` only pushes a `Frame` into the display channel (non-blocking, thread-safe — same call the GUI restore tail makes at `:1944`), call it at the **end of the reset handler closure**, after `frozen.restore(...)`, when a GPU handle is present.

Extend `ResetWiring` with `gpu: Option<Arc<Mutex<VirtioMmio>>>` and, in the reset closure (Step 4), after `frozen.restore(&rp.devices);`:

```rust
            if let Some(gpu) = &gpu { gpu.lock().unwrap().present_scanout(); }
```

Pass `gpu: ctx.gpu_mmio.clone()` from `run_restore` and `gpu: None` (or the fresh-boot gpu handle if `--gui`) from `main`. Capture `gpu` in the reset closure like the other fields.

- [ ] **Step 8: Build, sign, clippy**

Run:
```bash
cargo build -p ignition-spike --bin boot
cargo clippy -p ignition-spike --bin boot --all-targets
./scripts/sign.sh target/debug/boot
```
Expected: builds clean, 0 clippy warnings, signs. If clippy flags `too_many_arguments` on `install_reset_handlers` or a long closure, prefer the existing repo convention `#[allow(clippy::too_many_arguments)]`.

- [ ] **Step 9: Commit**

```bash
git add spike/src/bin/boot.rs
git commit -m "boot: wire Ctrl-A c / Ctrl-A r, seed reset point on restore"
```

- [ ] **Step 10: Live eyeball gate (the real test)**

These need the hypervisor entitlement + the GUI rootfs; run them by hand and report what the screen does. First take a warm-base GUI snapshot if you don't have one (`boot --gui --track-dirty ... Image rootfs-gui.ext4`, log in, `Ctrl-A s`, name it `warm-base`).

**Disk-divergence caveat for the eyeball:** the current GUI rootfs mounts read-write, so a few seconds of shell/`/var` writes between `Ctrl-A c` and `Ctrl-A r` can leave transient ext4 inconsistency after a reset (this is exactly the documented constraint — sub-project B's read-only + tmpfs rootfs removes it). For a clean eyeball, either avoid disk writes between checkpoint and reset, or boot the rootfs read-only (`ro` on the kernel cmdline). Note any FS warnings observed.

1. **Restore + reset:** `target/debug/boot --gui --track-dirty --restore warm-base` → in the window, type junk in foot → `Ctrl-A r` → screen snaps back to the restored desktop, still interactive.
2. **Mark + reset:** type a known marker → `Ctrl-A c` ("[reset point marked]") → type more junk → `Ctrl-A r` → rolls back to the marker, not the original restore point.
3. **No-point message:** fresh boot `target/debug/boot --gui Image rootfs-gui.ext4` → `Ctrl-A r` → prints `reset: no checkpoint - press Ctrl-A c first`; then `Ctrl-A c`, type junk, `Ctrl-A r` → rolls back.
4. **SMP:** `--smp 2 --track-dirty --restore warm-base` → reset still lands correctly (multi-vCPU register restore).
5. **Net:** add `--net` under `sudo` → after reset, link/IP survive or re-settle (the netwatch poller re-DHCPs on the carrier bounce).
6. **GIC sanity:** after a reset, the guest clock keeps ticking and the shell stays responsive (timers/interrupts not wedged). If `gic_restore` logged a mid-run rejection, note whether the guest still runs (fallback path).

Report results. If GIC re-restore wedges the guest (test 6 fails), the fallback is already in place (logged warn, no hard fail) — capture the log line for the docs note.

---

## Task 6: Documentation

**Files:**
- Modify: `docs/src/features/snapshot-restore.md`
- Modify: `docs/src/features/devices.md`
- Modify: `docs/src/getting-started/guest-assets.md`

- [ ] **Step 1: Document the hotkeys in snapshot-restore.md**

After the "GUI snapshot, restore & fan-out" section, add a "Interactive reset-to-checkpoint" subsection: explain `Ctrl-A r` (in-place rollback of RAM + vCPU + GIC + virtio-device state, repaint under `--gui`), `Ctrl-A c` (mark the in-memory reset point), that the point is auto-seeded on `--restore`, and that `--track-dirty` makes the rollback copy only changed pages (correct full-copy without it). Reflect the live-test outcome for the GIC mid-run note (works / falls back). **Include a bold correctness note:** reset does not rewind the disk, so it is consistent only when the disk does not diverge between checkpoint and reset — use a read-only rootfs with tmpfs for writable state (as the disposable-browser rootfs does); a diverged disk will corrupt the guest filesystem.

- [ ] **Step 2: Cross-reference in devices.md**

In the GUI section of `docs/src/features/devices.md`, add a sentence that the GUI guest also supports in-place reset-to-checkpoint (`Ctrl-A r` / `Ctrl-A c`), linking to snapshot-restore.md.

- [ ] **Step 3: Note the hotkeys in guest-assets.md**

In the "Rebuild the GUI rootfs" section where `Ctrl-A s` is mentioned, add that `Ctrl-A c` marks an in-memory reset point and `Ctrl-A r` rolls back to it in place (distinct from the `Ctrl-A s` disk snapshot).

- [ ] **Step 4: Commit**

```bash
git add docs/
git commit -m "docs: interactive reset-to-checkpoint (Ctrl-A r / Ctrl-A c)"
```

---

## Self-review notes (for the executor)

- **Spec coverage:** ResetPoint (Task 1+2), seeding on restore (Task 5.6), `Ctrl-A c` capture (Task 4 leader + Task 5.4 checkpoint closure), `Ctrl-A r` rollback (Task 4 leader + Task 5.4 reset closure), dirty-fast vs full-copy (Task 1 helpers + Task 5.4 branch), GIC fallback (Task 5.4 logged warn), disk non-divergence requirement (docs Task 6.1, bold note), present_scanout repaint (Task 5.7), distinct keys (Task 5.1-5.3). All covered.
- **Spec gap resolved:** fresh-boot `Ctrl-A c` (no `MAP_SHARED` file) → `PristineRam::Owned` copy. The spec assumed restore mode; this plan makes fresh boot correct too.
- **Type consistency:** `ResetPoint { pristine: PristineRam, vcpus: Vec<VcpuCheckpoint>, gic_blob: Vec<u8>, devices: Vec<DeviceRecord> }` used identically in reset.rs, vcpu_manager.rs, and both boot.rs install/seed sites. `rollback_pages(pristine, live, pages, page)` signature identical at definition (Task 1) and call (Task 5.4). `crate::dirty::PAGE` is the granule everywhere.
- **Known soft spots (flag to reviewer):** (a) Task 3's device test depends on the existing device_manager fake — adapt names. (b) `request_*` `self` receiver (`&self` vs `&Arc<Self>`) must match the existing `request_snapshot` so `boot.rs` call sites compile. (c) GIC mid-run re-restore is unproven until Task 5.10 test 6 — the fallback is non-fatal by design.
