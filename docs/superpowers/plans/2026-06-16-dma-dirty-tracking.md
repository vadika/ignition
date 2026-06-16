# DMA-aware Dirty Tracking Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Mark virtio device writes to guest RAM in the dirty tracker so the dirty set is complete (guest faults ∪ device DMA), letting `Ctrl+Alt+R` reset use a fast dirty-only rollback again (and fixing the same gap in diff snapshots).

**Architecture:** A `DirtySink` trait owned by `crates/devices` is called from the single write choke point `GuestRam::write_slice`. `vmm::DirtyTracker` implements it (owning the 16 KiB page split, reusing its atomic `mark`). `boot.rs` builds each device's `GuestRam` with the tracker-as-sink when `--track-dirty` is armed, and the reset handler reverts to dirty-only `rollback_pages` (full-copy stays only as the no-tracker fallback).

**Tech Stack:** Rust. `cargo test -p ignition-devices`, `cargo test -p ignition-vmm`, `cargo build -p ignition-spike --bin boot`, `./scripts/sign.sh target/debug/boot`.

---

## Spec

Source: `docs/superpowers/specs/2026-06-16-dma-dirty-tracking-design.md`. Read it. Decision: dirty-only reset is the default when `--track-dirty` is armed; full-copy is the no-tracker fallback only.

## Verified ground truth (file:line, read 2026-06-16)

- `GuestRam` — `crates/devices/src/virtio/guest_ram.rs`: struct `{ ptr: *mut u8, len: usize, base: u64 }` (`:16-20`); `new(ptr,len,base)` (`:37`); `offset(gpa,n) -> Option<usize>` bounds check (`:41`); the **single write choke point** `write_slice(&self, gpa, data) -> bool` (`:62-72`) does `ptr::copy_nonoverlapping` on success, returns `false` on out-of-bounds; `write_u16`/`write_u32` delegate to `write_slice` (`:86-91`). `unsafe impl Send/Sync` (`:29,32`). Existing `#[cfg(test)] mod tests` (`:113`).
- All device writes funnel through `write_slice`/`write_u16`/`write_u32` (audited in the spec). No raw-pointer bypass.
- `DirtyTracker` — `crates/vmm/src/dirty.rs`: `pub const PAGE: usize = 16384;`; `#[derive(Clone)] struct DirtyTracker { base, page_count, bits: Arc<Vec<AtomicU64>> }`; `mark(&self, ipa: u64)` (clamps out-of-range, atomic `fetch_or`); `drain(&self) -> Vec<u64>` (sorted page indices, clears). Clone shares the `Arc` bitmap.
- vCPU fault marking (unchanged) — `crates/vmm/src/vstate/vcpu_manager.rs:280-310` `VcpuExit::DirtyFault(pa) => cfg.tracker.mark(pa); re-grant WRITE`.
- `DirtyConfig` + `set_dirty_config` — `crates/vmm/src/vstate/vcpu_manager.rs` (fields `base`, `size`, `tracker: DirtyTracker`). Do not change its shape.
- boot.rs wiring: `DeviceContext::guest_ram(&self) -> GuestRam { GuestRam::new(self.host, self.ram_size as usize, layout::RAM_BASE) }` (`:546-547`) — the **single** per-device construction point (every `let mem = ctx.guest_ram();` at `:598-704` flows from it). `DirtyTracker::new` is currently created **after** `setup_devices` runs: fresh-boot `:1070`, run_restore `:1869`. `set_dirty_config` at `:1269-1270` (main) and `:1683-1684` (restore). Reset handler (full-copy, to be reverted) in `install_reset_handlers` (the `rollback_full` block added earlier).
- crate deps: `vmm` depends on `devices`; `devices` does NOT depend on `vmm` (so the trait lives in `devices`).

## File structure

- **Modify `crates/devices/src/virtio/guest_ram.rs`** — add `DirtySink` trait, `GuestRam.dirty` field, `with_dirty` constructor, mark in `write_slice`. Unit-tested here.
- **Modify `crates/vmm/src/dirty.rs`** — `impl DirtySink for DirtyTracker` (page split). Unit-tested here.
- **Modify `spike/src/bin/boot.rs`** — create the tracker before device construction, thread the sink into `DeviceContext`/`guest_ram()` (fresh-boot + restore), revert the reset handler to dirty-only-when-armed.
- **Modify `docs/src/features/snapshot-restore.md`** — note DMA-aware tracking + fast reset + diff-snapshot correctness.

---

## Task 1: `DirtySink` trait + `GuestRam` marks device writes

**Files:**
- Modify: `crates/devices/src/virtio/guest_ram.rs`
- Test: inline `#[cfg(test)]` in the same file

- [ ] **Step 1: Write the failing test**

Add to the existing `#[cfg(test)] mod tests` in `guest_ram.rs`:

```rust
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct RecordingSink(Mutex<Vec<(u64, usize)>>);
    impl DirtySink for RecordingSink {
        fn mark_dirty(&self, gpa: u64, len: usize) {
            self.0.lock().unwrap().push((gpa, len));
        }
    }

    #[test]
    fn write_slice_marks_dirty_on_success() {
        let mut backing = vec![0u8; 0x1000];
        let sink = Arc::new(RecordingSink::default());
        let m = GuestRam::with_dirty(backing.as_mut_ptr(), backing.len(), 0x4000_0000, Some(sink.clone()));
        assert!(m.write_slice(0x4000_0020, &[1, 2, 3, 4]));
        assert!(m.write_u32(0x4000_0040, 0xdead_beef)); // delegates to write_slice
        let calls = sink.0.lock().unwrap().clone();
        assert_eq!(calls, vec![(0x4000_0020, 4), (0x4000_0040, 4)]);
    }

    #[test]
    fn failed_write_does_not_mark() {
        let mut backing = vec![0u8; 0x100];
        let sink = Arc::new(RecordingSink::default());
        let m = GuestRam::with_dirty(backing.as_mut_ptr(), backing.len(), 0x4000_0000, Some(sink.clone()));
        assert!(!m.write_u32(0x4000_00fe, 0)); // crosses the end -> rejected
        assert!(sink.0.lock().unwrap().is_empty());
    }

    #[test]
    fn no_sink_does_not_panic() {
        let mut backing = vec![0u8; 0x100];
        let m = GuestRam::new(backing.as_mut_ptr(), backing.len(), 0x4000_0000); // no sink
        assert!(m.write_u32(0x4000_0010, 7));
        assert_eq!(m.read_u32(0x4000_0010), Some(7));
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p ignition-devices guest_ram`
Expected: FAIL — `cannot find trait DirtySink` / `no function with_dirty`.

- [ ] **Step 3: Add the trait + field + constructor + mark**

In `guest_ram.rs`, add the import and trait near the top (after the existing `use libc;`):

```rust
use std::sync::Arc;

/// Records that guest RAM `[gpa, gpa+len)` was written by the host side (a virtio
/// device / DMA), so such writes are captured by dirty tracking exactly like a
/// guest vCPU write fault. Implemented by the VMM's dirty tracker.
pub trait DirtySink: Send + Sync {
    fn mark_dirty(&self, gpa: u64, len: usize);
}
```

Add the field to the struct:

```rust
pub struct GuestRam {
    ptr: *mut u8,
    len: usize,
    base: u64,
    dirty: Option<Arc<dyn DirtySink>>,
}
```

Update `new` and add `with_dirty`:

```rust
    /// `ptr`/`len` describe the host mapping; `base` is the guest physical
    /// address it is mapped at.
    pub fn new(ptr: *mut u8, len: usize, base: u64) -> Self {
        Self::with_dirty(ptr, len, base, None)
    }

    /// Like `new`, but every successful write is reported to `dirty` so device
    /// (DMA) writes are captured by dirty tracking. `None` disables marking.
    pub fn with_dirty(ptr: *mut u8, len: usize, base: u64, dirty: Option<Arc<dyn DirtySink>>) -> Self {
        Self { ptr, len, base, dirty }
    }
```

Mark in `write_slice` after the successful copy:

```rust
    pub fn write_slice(&self, gpa: u64, data: &[u8]) -> bool {
        match self.offset(gpa, data.len()) {
            Some(off) => {
                // SAFETY: bounds checked by `offset`; disjoint-by-protocol (see
                // module doc) — no other thread touches this buffer region concurrently.
                unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), self.ptr.add(off), data.len()) };
                if let Some(d) = &self.dirty {
                    d.mark_dirty(gpa, data.len());
                }
                true
            }
            None => false,
        }
    }
```

(The `unsafe impl Send/Sync for GuestRam` stay valid — `Arc<dyn DirtySink: Send + Sync>` is itself `Send + Sync`.)

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p ignition-devices guest_ram`
Expected: PASS (the 3 new tests + the existing `round_trip_within_bounds` / `out_of_bounds_rejected` / `madvise_free_bounds`).

- [ ] **Step 5: Clippy + commit**

Run: `cargo clippy -p ignition-devices 2>&1 | tail -5` — no NEW warnings.

```bash
git add crates/devices/src/virtio/guest_ram.rs
git commit -m "devices: GuestRam DirtySink hook — mark device writes for dirty tracking"
```
Commit message plain — NO trailer.

---

## Task 2: `DirtyTracker` implements `DirtySink`

**Files:**
- Modify: `crates/vmm/src/dirty.rs`
- Test: inline `#[cfg(test)]` in the same file

- [ ] **Step 1: Write the failing test**

Add to (or create) the `#[cfg(test)] mod tests` in `crates/vmm/src/dirty.rs`:

```rust
    use super::*;
    use ignition_devices::virtio::guest_ram::DirtySink;

    #[test]
    fn mark_dirty_splits_pages() {
        let t = DirtyTracker::new(0x4000_0000, (PAGE as u64) * 8);
        // A write wholly inside page 0.
        t.mark_dirty(0x4000_0000 + 16, 32);
        // A write spanning the page-2/page-3 boundary.
        let boundary = 0x4000_0000 + (PAGE as u64) * 3 - 8;
        t.mark_dirty(boundary, 32);
        // Zero-length marks nothing.
        t.mark_dirty(0x4000_0000 + (PAGE as u64) * 5, 0);
        let mut pages = t.drain();
        pages.sort_unstable();
        assert_eq!(pages, vec![0, 2, 3]);
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p ignition-vmm dirty::`
Expected: FAIL — `no method named mark_dirty` (the trait isn't implemented yet).

- [ ] **Step 3: Implement `DirtySink` for `DirtyTracker`**

Add to `crates/vmm/src/dirty.rs` (after the `impl DirtyTracker`):

```rust
impl ignition_devices::virtio::guest_ram::DirtySink for DirtyTracker {
    /// Mark every PAGE granule touched by a host-side write of `len` bytes at
    /// `gpa`. `devices` stays granule-agnostic; the 16 KiB `PAGE` split lives here.
    fn mark_dirty(&self, gpa: u64, len: usize) {
        if len == 0 {
            return;
        }
        let end = gpa.saturating_add(len as u64 - 1);
        let mut p = gpa & !((PAGE as u64) - 1); // align down to the granule
        while p <= end {
            self.mark(p);
            p = match p.checked_add(PAGE as u64) {
                Some(n) => n,
                None => break,
            };
        }
    }
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p ignition-vmm dirty::`
Expected: PASS (`mark_dirty_splits_pages` plus any existing dirty tests).

- [ ] **Step 5: Clippy + commit**

Run: `cargo clippy -p ignition-vmm 2>&1 | tail -5` — no NEW warnings (pre-existing `fuzz/controller.rs` warnings are not yours).

```bash
git add crates/vmm/src/dirty.rs
git commit -m "vmm: DirtyTracker implements DirtySink (page-split device writes)"
```
Commit message plain.

---

## Task 3: Wire the sink into devices + revert reset to dirty-only

**Files:**
- Modify: `spike/src/bin/boot.rs`

No unit tests (HVF/integration). Gate: builds + clippy clean + signs; live eyeball is Task 5.

- [ ] **Step 1: Read the relevant boot.rs regions**

Read `spike/src/bin/boot.rs` around: the `DeviceContext` struct definition + `guest_ram()` (`:340-362`, `:546-547`); the fresh-boot `DirtyTracker::new` + `DeviceContext { ... }` build + `setup_devices(...)` call ordering (around `:1000-1080`); the run_restore equivalents (`:1862-1880` + its `DeviceContext`/`setup_devices`); the `install_reset_handlers` reset closure (the `rollback_full` block). Note the import `use ignition_devices::virtio::guest_ram::GuestRam;` at `:29` — extend it to also import `DirtySink`.

- [ ] **Step 2: Add a `dirty` field to `DeviceContext` and use it in `guest_ram()`**

In the `DeviceContext` struct, add:

```rust
    /// When set (under --track-dirty), every device GuestRam reports its writes
    /// here so device DMA is captured by dirty tracking. None disables marking.
    dirty: Option<std::sync::Arc<dyn ignition_devices::virtio::guest_ram::DirtySink>>,
```

Change `guest_ram()`:

```rust
    fn guest_ram(&self) -> GuestRam {
        GuestRam::with_dirty(self.host, self.ram_size as usize, layout::RAM_BASE, self.dirty.clone())
    }
```

- [ ] **Step 3: Create the tracker BEFORE device construction (fresh-boot path)**

The tracker is currently created at `:1070`, AFTER `setup_devices`. Device `GuestRam`s are built during `setup_devices`, so the sink must exist before then. Move the `dirty_tracker` creation to **before** the `DeviceContext { ... }` initializer, and set the `dirty` field from it.

Concretely: locate the fresh-boot `let dirty_tracker: Option<DirtyTracker> = if track_dirty { ... DirtyTracker::new(layout::RAM_BASE, ram_size) ... } else { None };` block (`:1070-1078`) and move it up to just before the `DeviceContext { ... }` build. Then in the `DeviceContext { ... }` initializer add:

```rust
        dirty: dirty_tracker
            .as_ref()
            .map(|t| std::sync::Arc::new(t.clone()) as std::sync::Arc<dyn ignition_devices::virtio::guest_ram::DirtySink>),
```

`DirtyTracker` is `Clone` and shares its `Arc` bitmap, so the sink clone and the `set_dirty_config(... tracker: tracker.clone() ...)` (`:1269-1270`, unchanged) and the reset-handler `dirty` (ResetWiring, unchanged) all mark/drain the SAME bitmap. Leave `set_dirty_config` exactly where it is.

- [ ] **Step 4: Same reorder in `run_restore`**

Apply the identical change in `run_restore`: move its `DirtyTracker::new(layout::RAM_BASE, mem_size)` block (`:1862-1870`) above that path's `DeviceContext { ... }` build, and add the same `dirty: dirty_tracker.as_ref().map(...)` field to the restore `DeviceContext` initializer. Its `set_dirty_config` (`:1683-1684` region) stays put. (Both `DeviceContext` builders must get the `dirty` field — the struct now has it, so a missing initializer is a compile error that will catch any path you forgot.)

- [ ] **Step 5: Revert the reset handler to dirty-only when armed**

In `install_reset_handlers`, the reset closure currently does an unconditional `rollback_full` + drain/re-protect. Replace that block with dirty-only-when-armed (the dirty set is now complete):

```rust
            // Dirty-only rollback when a tracker is armed: device DMA writes are now
            // marked too (GuestRam DirtySink), so the drained set is complete. Full
            // copy only when there is no tracker.
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
```

Leave the net carrier-bounce, GIC-skip comment, `present_scanout`, and `rx_stop` handling exactly as they are. (`rollback_pages` is already `pub` in `crate::reset`; the import path `ignition_vmm::reset::rollback_pages` matches the earlier full-copy edit.)

- [ ] **Step 6: Build, clippy, sign**

Run:
```bash
cargo build -p ignition-spike --bin boot 2>&1 | tail -8
cargo clippy -p ignition-spike --bin boot 2>&1 | tail -6
./scripts/sign.sh target/debug/boot
```
Expected: builds (a missing `dirty:` initializer on any `DeviceContext` is a compile error — fix all sites), 0 NEW clippy warnings (pre-existing `run_fuzz_mode` too_many_arguments not yours), signs. The fuzz path builds its own `DeviceContext` too — give it `dirty: None` (the fuzz reset uses its own controller, not this sink) unless it already has a tracker to thread; `None` preserves current fuzz behavior.

- [ ] **Step 7: Commit**

```bash
git add spike/src/bin/boot.rs
git commit -m "boot: thread DirtyTracker into device GuestRam; reset back to dirty-only when armed"
```
Commit message plain.

---

## Task 4: Documentation

**Files:**
- Modify: `docs/src/features/snapshot-restore.md`

- [ ] **Step 1: Update the dirty-tracking / reset notes**

In `docs/src/features/snapshot-restore.md`, find where the interactive reset and dirty tracking are described (the "Interactive reset-to-checkpoint" section and any diff-snapshot mention). Add a short paragraph: the dirty tracker now records **device (DMA) writes** as well as guest vCPU writes — a `DirtySink` hook at `GuestRam`'s write path marks the same bitmap the write-protect faults use. Consequences: `Ctrl+Alt+R` reset uses a fast **dirty-only** rollback again (full copy only without `--track-dirty`), and **diff snapshots** are now correct for a guest doing active DMA (previously device-written pages could be omitted). Keep the existing note that the net carrier-bounce / GIC-skip are separate reset concerns.

- [ ] **Step 2: Commit**

```bash
git add docs/src/features/snapshot-restore.md
git commit -m "docs: dirty tracking now captures device DMA writes (fast reset + correct diffs)"
```
Commit message plain.

---

## Task 5: Live eyeball (HUMAN STEP — hand back)

Not automatable (needs the hypervisor + GUI). The agent STOPS after Task 4 and hands these to the human.

- [ ] **Step 1: Fast + correct reset.** `sudo scripts/disposable-browser.sh`, browse to an HTTPS site, `Ctrl+Alt+R` several times. Expect: clean snap-back (no `not a head` / `bad gso`), net reconnects, AND the rollback is fast again (the snap is near-instant vs the earlier ~hundreds-of-ms pause). Confirm repeated resets stay clean.
- [ ] **Step 2 (optional): diff-snapshot correctness.** With `--track-dirty`, start a download/active traffic, `Ctrl-A s` (diff snapshot), then restore that snapshot headless or GUI; confirm it comes up intact (device-written pages were captured).
- [ ] **Step 3: Report** so docs can note the measured reset latency, then finish the branch.

---

## Self-review notes (for the executor)

- **Spec coverage:** DirtySink trait + GuestRam mark (Task 1), DirtyTracker page-split impl (Task 2), boot.rs sink wiring + dirty-only reset revert (Task 3), docs incl. diff-snapshot benefit (Task 4), live gate (Task 5). All spec sections mapped.
- **Type/name consistency:** `DirtySink::mark_dirty(&self, gpa: u64, len: usize)` identical in the trait (Task 1), the impl (Task 2), and the recording fake (Task 1 test). `GuestRam::with_dirty(ptr, len, base, Option<Arc<dyn DirtySink>>)` used in Task 1 and Task 3 (`guest_ram()`). `DirtyTracker` is `Clone`/shares bitmap — relied on in Task 3 (sink clone vs `set_dirty_config` clone vs reset `dirty`). `ignition_vmm::reset::rollback_pages` / `rollback_full` / `ignition_vmm::dirty::PAGE` match the earlier reset code.
- **Ordering hazard (Task 3):** the tracker MUST be created before `setup_devices` so the sink is baked into each device's `GuestRam`. Steps 3–4 move the creation up in BOTH `main` and `run_restore`; the new mandatory `DeviceContext.dirty` field makes any forgotten site a compile error.
- **No-regression:** without `--track-dirty`, `dirty` is `None` → no marking (cheap `Option` check) → reset full-copies → identical to today. Fuzz `DeviceContext` gets `dirty: None`.
