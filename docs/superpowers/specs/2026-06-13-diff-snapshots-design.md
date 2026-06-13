# Diff / Incremental Snapshots — Design

_Date: 2026-06-13. Status: design (approved), pre-implementation._
_Research basis: `docs/diff-snapshot-research.md`._

## Goal

Let a running, restored guest be re-snapshotted so the new snapshot stores **only the
guest RAM pages changed since its parent**, not a full RAM dump. Non-memory state
(vCPU registers, GIC, device records) is still captured in full each time. Snapshots
form an **immutable delta chain**; restore reassembles base + deltas without mutating
any stored artifact.

This is the on-platform equivalent of Firecracker's `snapshot_type=Diff`, but where
Firecracker reads a hardware dirty bitmap (`KVM_GET_DIRTY_LOG`), HVF exposes no such
API — so we acquire the dirty set by **write-protecting guest RAM and intercepting
Data-Abort permission faults**.

## Decisions (locked during brainstorming)

- **Scope**: live re-snapshot of a long-lived guest (needs a running dirty-tracker).
- **Mechanism**: write-protect + Data-Abort interception (research option A). `mincore`
  rejected — overapproximates badly for a long-lived guest.
- **Layering**: immutable delta chain (each diff is a new artifact with a `parent`
  pointer; nothing existing is mutated).
- **Activation**: opt-in via a `--track-dirty` flag. Guests without it run at full
  speed and cannot be diff-snapshotted.
- **Granule**: 16 KiB tracking page (matches the Apple Silicon host page; avoids
  sub-host-page `hv_vm_protect` ambiguity). Confirmed by the feasibility gate.

## Feasibility gate (MUST pass before building the feature)

A throwaway spike, gated like the fast-restore work was. Build the feature only if all
three hold:

1. **Recoverable write fault.** `hv_vm_protect(ipa, size, HV_MEMORY_READ |
   HV_MEMORY_EXEC)` on a mapped RAM range makes a subsequent guest store trap with
   `HV_EXIT_REASON_EXCEPTION`, EC = Data Abort (`0x24`), DFSC = permission fault, and
   `exception.physical_address` = the faulting IPA. After we re-grant WRITE on that
   page and resume **without advancing PC**, the store re-executes and completes
   correctly (guest sees no corruption, makes forward progress).
2. **Granule.** Determine whether `hv_vm_protect` accepts a 4 KiB sub-range of a 16 KiB
   host page. If not (or if it silently promotes to 16 KiB), the tracking granule is
   16 KiB. Record the answer; it fixes the bitmap page size.
3. **Cost.** Measure per-page first-write vmexit cost and the aggregate overhead under
   a realistic write rate (e.g. a `dd`/`stress` loop in the guest). Confirm it is a
   bounded amortized cost (one fault per page per interval), not a runaway spin.

If the gate fails, stop and reconsider (e.g. fall back to coarse `mincore` diffs, or
shelve the feature). Spike code is deleted after the gate; it is not the feature.

## Components

### 1. Dirty tracker (`crates/vmm`, new module `dirty.rs`)

- `DirtyTracker` owns a shared bitmap: `Arc<Vec<AtomicU64>>`, one bit per `PAGE` (16
  KiB) of the RAM region. `page_count = ceil(mem_size / PAGE)`.
- `mark(ipa: u64)` — given a faulting guest physical address, compute
  `(ipa - RAM_BASE) / PAGE`, set the bit (`fetch_or`, `Ordering::Relaxed`).
- `drain() -> Vec<u64>` — snapshot the set bits as sorted page indices, then clear.
- `protect_all(vm)` / `unprotect_page(vm, ipa)` — thin wrappers over a new
  `Vm::protect_memory(ipa, size, flags)` HVF call (see §2).
- The tracker is created only when `--track-dirty` is set; otherwise `None` and the
  whole path is inert (zero overhead).

### 2. HVF protect + fault path (`crates/hvf`)

- **New**: `Vm::protect_memory(&self, guest_addr: u64, size: u64, flags) ->
  Result<(), Error>` wrapping `hv_vm_protect`. Flags reuse the existing
  `HV_MEMORY_*` constants already imported for `map_memory`.
- **Modified**: the `EC_DATAABORT` arm at `crates/hvf/src/lib.rs:888`. Today it
  unconditionally treats the abort as MMIO (sets `pending_advance_pc = true`, decodes a
  load/store against `physical_address`). New logic, *before* the MMIO decode:

  ```
  let pa = exception.physical_address;
  let iswrite = ((syndrome >> 6) & 1) != 0;          // ISS WnR bit
  // GATE FINDING: HVF reports write-protect faults as TRANSLATION faults
  // (DFSC 0x07 first-touch / 0x0f PTE-present), NOT permission faults. So we
  // discriminate on iswrite + IPA-in-RAM, NOT on a DFSC sub-code. A genuine MMIO
  // access has pa outside the RAM range; a normal RAM read/write to an unprotected
  // page does not fault. Only a write to a write-protected RAM page lands here.
  if dirty_tracking_enabled && iswrite && pa >= RAM_BASE && pa < RAM_BASE + ram_size {
      return Ok(VcpuExit::DirtyFault(pa));           // new exit variant
      // NOTE: do NOT set pending_advance_pc — the store must re-execute.
  }
  // else: existing MMIO path, unchanged.
  ```
- **New** `VcpuExit::DirtyFault(u64)` variant. The run-loop owner handles it: `tracker.mark(pa)`;
  `vm.protect_memory(page_base(pa), PAGE, READ|WRITE|EXEC)`; loop back into `run()`
  without touching PC. The guest re-executes the faulting store, which now succeeds.
- Whether dirty tracking is "enabled" for a vCPU is a cheap flag the run loop checks;
  the hvf crate stays mechanism-only (it reports `DirtyFault`, the vmm decides).

### 3. Snapshot format v3 (`crates/vmm/src/snapshot.rs`)

- Bump `SNAP_VERSION` 2 → 3, magic `ignition-snapshot-v3`. The version guard already
  exists; v2 snapshots are read-rejected with a clear message (no migration — research
  artifact project).
- `SnapshotManifest` gains:
  - `snapshot_type: SnapshotType` (`Full` | `Diff`), serde-tagged.
  - `parent: Option<String>` — name of the immediate parent layer (None for a root/Full).
- Layer on disk under the existing store convention `<store>/snapshots/<name>/`:
  - **Full** layer: `memory.bin` (whole RAM), `gic.bin`, `disk.img`, `vmstate.json`,
    `manifest.json` — exactly as today.
  - **Diff** layer: `memory.bin` holds **only the dirty pages, packed back-to-back in
    ascending page order**; a new `dirty.idx` sidecar holds the sorted `Vec<u64>` page
    indices (bincode or fixed-width LE u64 array). `gic.bin` / `vmstate.json` /
    `manifest.json` full as usual. `disk.img`: clonefile of the **live guest's current
    disk** (the instance CoW clone it has been writing to) — disk stays whole-image
    CoW per layer; block-level disk diff is out of scope.
- New helpers: `write_diff_layer(dir, snap, dirty_pages: &[u64], ram: &[u8], ...)`
  packs only `ram[page*PAGE .. page*PAGE+PAGE]` for each index; `read_diff_layer` /
  `resolve_chain(store, leaf_name) -> Vec<Paths>` (root→leaf order via `parent`).

### 4. Snapshot flow — `Ctrl-A s` on an armed guest (`spike/src/bin/boot.rs`)

Reuses the existing multi-vCPU stop-the-world rendezvous (`vcpu_manager.rs`):

1. Rendezvous all vCPUs (existing barrier).
2. If tracker present → `let dirty = tracker.drain()`. Decide layer type:
   - guest was **restored from** chain leaf `P` and tracker armed → write a **Diff**
     layer with `parent = Some(P)`, dirty pages only.
   - fresh boot (no parent) first snapshot → **Full** layer (`parent = None`), even
     with tracking on (nothing to diff against). Subsequent snapshots → Diff vs the
     just-written layer.
3. Write the layer (full vmstate/GIC/devices always).
4. `tracker.protect_all()` again + reset bitmap → next interval starts clean.
5. Resume vCPUs.

The new layer's name is auto-generated (existing fancy-name generator) unless
`--name` given; same-name-as-parent refused unless `--force` (existing guard).

### 5. Restore flow (`spike/src/bin/boot.rs run_restore`)

1. `resolve_chain(store, restore_name)` → ordered `[root .. leaf]`. Validate every
   layer's `mem_size` agrees and v3.
2. `clonefile_or_copy(root.memory, inst/memory.bin)` (existing fast-restore path).
3. `mmap(MAP_SHARED)` the instance file as guest RAM (existing).
4. For each **Diff** layer in order: read its `dirty.idx` + packed `memory.bin`, and
   for each dirty page write its 16 KiB into the mmap'd region at
   `RAM_BASE-relative page*PAGE` (direct memcpy into the mapping; the MAP_SHARED
   mapping persists it to the private clone). Base + all layers stay untouched.
5. Restore vmstate/GIC/devices from the **leaf** layer (existing `run_restored`).
6. If `--track-dirty`, arm tracking and `protect_all()` after the chain is applied, so
   this restored guest can itself be re-snapshotted (chain grows by one).

### 6. CLI (`spike/src/bin/boot.rs main`)

- New `--track-dirty` flag (bool). Arms the tracker on boot and on restore.
- No new subcommand for diff vs full: an armed, restored guest's `Ctrl-A s` produces a
  Diff automatically (it has a parent); a fresh guest's first `Ctrl-A s` is Full.
- `--store` / `--name` / `--restore` / `--force` / `--mem` unchanged.

## Data flow

```
boot/restore --track-dirty
   └─ map RAM ─ protect_all(READ|EXEC) ─ arm tracker
        guest store ─► DABT permission fault (EC 0x24, DFSC perm, pa∈RAM)
                         └─ VcpuExit::DirtyFault(pa)
                              └─ tracker.mark(pa); unprotect page (RWX); resume (no PC advance)
   Ctrl-A s:
        rendezvous ─ dirty = tracker.drain()
                   ─ write Diff layer {parent, packed dirty pages, dirty.idx, full vmstate}
                   ─ protect_all + reset bitmap ─ resume

restore <leaf>:
        resolve_chain root..leaf
        clonefile root.memory → instance; mmap MAP_SHARED
        for layer in diffs: memcpy each dirty page into mapping
        restore vmstate/GIC/devices from leaf
```

## Error handling

- Diff snapshot requested but tracker not armed (`--track-dirty` absent) → refuse with
  a clear message ("dirty tracking not enabled; restart with --track-dirty for diffs").
- Restore: any layer missing, wrong version, `mem_size` mismatch, or a `parent` that
  doesn't resolve → fail before mapping memory, naming the broken layer.
- `dirty.idx` page index ≥ `page_count` → reject the layer (corrupt).
- `hv_vm_protect` error on re-grant → fatal for that vCPU (can't safely resume); log
  the IPA and return the HVF error.
- A Diff layer whose `parent` chain doesn't terminate at a Full root → reject.

## Testing

- **Unit (`crates/vmm`)**: `DirtyTracker` mark/drain/bit math (page boundaries,
  last partial page); `resolve_chain` ordering and cycle/missing-parent rejection;
  diff layer pack/unpack round-trip (`write_diff_layer` then apply → identical bytes).
- **Format**: v2 snapshot rejected by v3 reader; manifest serde round-trip with
  `snapshot_type`/`parent`.
- **Integration (headless driver, like `restore_test.py`)**:
  boot `--track-dirty` → snapshot (Full root) → mutate a known guest region → re-snapshot
  (Diff) → assert Diff `memory.bin` physical size ≪ full RAM and ≈ touched region;
  restore the leaf → assert the mutated region reads back correctly and untouched
  regions match the base; assert base + all layer artifacts are byte-identical before
  and after restore (immutability). Confirm restored-from-diff guest idles ~0% CPU.
- **Gate spike** (separate, pre-feature): the three feasibility checks above.

## Out of scope

- Chain flatten / compaction command (collapse a chain to one Full). Planned follow-up;
  restore-latency mitigation for deep chains. Noted in ROADMAP.
- Block-level disk diff (disk stays whole-image clonefile CoW per layer).
- REST API surface for snapshot type (the broader REST work is its own roadmap item).
- `mincore` coarse fallback (rejected for the live-guest use case).
- x86 / non-aarch64.
```
