# DMA-aware dirty tracking (design)

**Status:** approved 2026-06-16 (architecture + components + rollout)
**Track:** disposable-sandbox showcase follow-up.
**Predecessors:** dirty-tracking + diff-snapshots, interactive reset (sub-project A),
disposable browser (sub-project B). Directly follows the reset hardening that forced
a full-copy rollback because device DMA writes escaped the dirty tracker.

## Problem

The write-protect dirty tracker (`crates/vmm/src/dirty.rs`) records only **guest
(vCPU)** writes: RAM is mapped read-only, a guest store faults (`VcpuExit::DirtyFault`),
the handler calls `tracker.mark(pa)` and re-grants WRITE
(`crates/vmm/src/vstate/vcpu_manager.rs:280-310`). Virtio **devices write guest RAM
through the host pointer** (`GuestRam::write_slice` → `ptr::copy_nonoverlapping`,
`crates/devices/src/virtio/guest_ram.rs:62-71`), bypassing stage-2 entirely, so those
pages are **never marked**.

Consequences:
- The interactive reset (`Ctrl+Alt+R`) had to use a **full-copy** rollback (revert the
  entire pristine image, ~hundreds of ms at 1 GiB) because a dirty-only rollback left
  device-written pages (used rings, RX frame data, blk read data) stale → inconsistent
  virtqueues (`virtio_net … not a head`, `bad gso`).
- **Diff snapshots** have the same latent gap: device-written pages are omitted from the
  diff. Rarely hit because queues are usually quiesced at `Ctrl-A s`, but it is a
  correctness bug.

## Goal

Mark device writes in the dirty tracker so the dirty set is complete (guest faults ∪
device writes). Then:
- Reset uses **dirty-only `rollback_pages`** (fast, ~µs–ms) whenever `--track-dirty` is
  armed; full-copy remains only as the no-tracker fallback.
- Diff snapshots become correct for an actively-DMA'ing guest.

This restores rollback *speed*; it does not change the other reset machinery.

## Why a choke-point hook is complete

Verified: **every** device→guest-RAM write funnels through `GuestRam::write_slice`
(`write_u16`/`write_u32` delegate to it). No device uses a raw host pointer or
`slice::from_raw_parts_mut`. Audited write sites, all via `GuestRam`:

| Device | Op | Site |
|---|---|---|
| net | `inject_rx` RX data | net.rs:134 (`write_slice`) |
| blk | `read_to_guest`, `set_status` | blk.rs:90,135 (`write_slice`) |
| rng | `handle_notify` | rng.rs:65 (`write_slice`) |
| gpu | `write_response` | gpu.rs:134 (`write_slice`) |
| vsock | `write_rx` | vsock/mod.rs:63 (`write_slice`) |
| input | `fill_events` | input.rs:86 (`write_slice`) |
| all | `push_used` (used ring) | queue.rs:103-106 (`write_u32`/`write_u16`) |

Because the trait `VirtioDevice` only receives `&GuestRam` to touch guest memory, any
future device that writes guest RAM **must** go through `write_slice` → it is covered
automatically. Completeness is structural, not a maintenance burden.

(`GuestRam::madvise_free` — balloon inflate — is NOT a data write; it advises the OS to
drop pages. Balloon + reset is out of scope; see below.)

## Architecture

```
device write ─► GuestRam::write_slice(gpa, data) ─► ptr::copy + (if sink) sink.mark_dirty(gpa, data.len())
vCPU write    ─► stage-2 write fault ─► VcpuExit::DirtyFault(pa) ─► tracker.mark(pa)   (unchanged)
reset/drain   ─► tracker.drain() = guest pages ∪ device pages ─► rollback_pages(pristine → live)
```

`crates/devices` does not depend on `crates/vmm`, so `GuestRam` cannot reference
`vmm::DirtyTracker`. The hook is a small trait **owned by `devices`**; `vmm`'s
`DirtyTracker` implements it (legal — `vmm` depends on `devices`, the trait is foreign
but the type is local). Marking reuses the existing atomic `DirtyTracker::mark`
(`fetch_or`, `Ordering::Relaxed`), already proven safe for concurrent device-thread /
vCPU-thread marking.

## Components

### 1. `crates/devices/src/virtio/guest_ram.rs`
- Add:
  ```rust
  /// A sink that records that guest RAM `[gpa, gpa+len)` was written, so a host-side
  /// (device/DMA) write is captured by dirty tracking just like a guest vCPU write.
  pub trait DirtySink: Send + Sync {
      fn mark_dirty(&self, gpa: u64, len: usize);
  }
  ```
- `GuestRam` gains `dirty: Option<std::sync::Arc<dyn DirtySink>>`.
- A constructor that takes the sink (e.g. `GuestRam::with_dirty(ptr, len, base, dirty)`)
  plus keeping `GuestRam::new(ptr, len, base)` = `with_dirty(.., None)` so existing
  callers/tests are unaffected.
- `write_slice` marks **after a successful copy** (skip on the bounds-check failure
  path), for the full `data.len()` range:
  ```rust
  // ... existing bounds check + ptr::copy_nonoverlapping ...
  if let Some(d) = &self.dirty { d.mark_dirty(gpa, data.len()); }
  true
  ```
  `write_u16`/`write_u32` inherit this (they call `write_slice`). Reads unchanged.
- Zero-length writes mark nothing (mark inside the success path, which a 0-length copy
  still reaches — `mark_dirty(gpa, 0)` must be a no-op in the impl; see component 2).

### 2. `crates/vmm/src/dirty.rs`
- `impl ignition_devices::virtio::guest_ram::DirtySink for DirtyTracker`:
  ```rust
  fn mark_dirty(&self, gpa: u64, len: usize) {
      if len == 0 { return; }
      let end = gpa.saturating_add(len as u64 - 1);
      let mut p = gpa & !((PAGE as u64) - 1);   // align down to granule
      while p <= end {
          self.mark(p);
          p += PAGE as u64;
      }
  }
  ```
  `devices` stays granule-agnostic; `vmm` owns `PAGE` (16 KiB). Multi-page writes mark
  every touched page; a single-page write marks one. (`mark` already clamps out-of-range
  gpas.)

### 3. `spike/src/bin/boot.rs`
- When `--track-dirty` is armed, wrap the `DirtyTracker` as `Arc<dyn DirtySink>` and
  construct each device's `GuestRam` with it (`GuestRam::with_dirty`). This is the
  per-device guest-RAM construction in `setup_devices` / the `DeviceContext` guest-RAM
  builder — thread the sink through both the **fresh-boot** and **`run_restore`** paths
  (both arm the tracker). When `--track-dirty` is absent, pass `None` (no marking; reset
  full-copies).
- Reset handler: revert the unconditional `rollback_full` to:
  ```rust
  match &dirty {
      Some(t) => {
          let pages = t.drain();
          ignition_vmm::reset::rollback_pages(rp.pristine.as_slice(), live, &pages, ignition_vmm::dirty::PAGE);
          let _ = ignition_hvf::vm_protect_memory(layout::RAM_BASE, ram_size, (HV_MEMORY_READ | HV_MEMORY_EXEC) as u64);
      }
      None => ignition_vmm::reset::rollback_full(rp.pristine.as_slice(), live),
  }
  ```
  (The dirty set is now complete, so dirty-only is correct.) The net carrier-bounce,
  GIC-skip, vtimer-unmask, and `present_scanout` steps are unchanged.

## Data flow & concurrency

- Device threads (net feeder, vsock reactor, vCPU threads running `handle_notify`) call
  `write_slice` → `mark_dirty` → atomic `fetch_or` on the shared bitmap. The vCPU
  fault path marks the same bitmap. `drain()` (reset/snapshot, vCPUs parked) swaps the
  bits out. No new locks; the bitmap is `Arc<Vec<AtomicU64>>`.
- The sink is the SAME `DirtyTracker` instance the reset drains and `set_dirty_config`
  installs — one tracker, two writers (faults + DMA), one drainer.

## Error handling / edge cases

- **No tracker** (`--track-dirty` absent): `GuestRam.dirty == None` → marking is a cheap
  `Option` check, no-op → reset full-copies (unchanged correctness).
- **Failed `write_slice`** (out-of-bounds): returns `false` before marking → no spurious
  marks.
- **Write spanning the granule boundary**: `mark_dirty` loops all touched pages.
- **Marking cost**: one `Option` check + (when armed) a few atomic `fetch_or`s per write;
  negligible against the copy and the I/O. Off entirely when not tracking.
- **Diff snapshots**: the same `drain()` now includes device pages → diffs are correct
  for active DMA. No code change in the diff path; behaviour just becomes correct.

## Out of scope (YAGNI)

- **Balloon `madvise_free` vs reset.** `madvise_free` frees pages (lazily zeroed by the
  OS), it is not a data write, and balloon is not in the disposable-browser path.
  Reset interaction with an actively-ballooning guest is not covered; documented, not
  fixed.
- **Replacing the net carrier-bounce / GIC-skip / vtimer-unmask.** Those resync
  interrupt/NIC state and are orthogonal; unchanged.
- **Removing `--track-dirty` as a flag.** It still gates both diff snapshots and the
  fast reset path.

## Testing

- **Unit (devices):** a fake `DirtySink` records `(gpa, len)` calls.
  - `write_slice(gpa, data)` on success calls `mark_dirty(gpa, data.len())` exactly once.
  - a failed (out-of-bounds) `write_slice` does NOT call `mark_dirty`.
  - `write_u16`/`write_u32` mark (they delegate).
  - `GuestRam::new` (no sink) never marks.
- **Unit (vmm):** `DirtyTracker::mark_dirty` page-splitting — single page, multi-page
  span, exact page-boundary crossing, `len == 0` (no-op); assert via `drain()` the exact
  page-index set. Reuse the existing `rollback_pages` revert test for the rollback side.
- **Live eyeball (the gate):**
  1. Disposable browser, `--track-dirty` (default): repeated `Ctrl+Alt+R` stays clean —
     no `not a head` / `bad gso`, net reconnects, no stall — across several resets.
  2. Reset latency drops from ~hundreds of ms (full-copy) back to ~µs–ms (dirty-only);
     confirm via the existing restore/reset timing logs or a visible snap vs pause.
  3. Diff snapshot of an actively-downloading guest restores intact (device-written
     pages now in the diff).
