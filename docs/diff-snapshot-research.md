# Diff / Incremental Snapshots — Research Findings

_Date: 2026-06-13. Research artifact preceding design. Status: research, no code yet._

Goal: write **diff snapshots** (only guest pages changed since the base) instead of
dumping all RAM every time. Question is *how to know which pages changed* on
Apple Hypervisor.framework (HVF), which — unlike Linux/KVM — has no dirty-log API.

## 1. HVF dirty-tracking API surface

**No native dirty-bitmap API.** Searched all `Hypervisor.framework/Headers/` (macOS
26.5 SDK) for `dirty`/`log`/`track` — nothing analogous to `KVM_GET_DIRTY_LOG`. No
stage-2 hardware dirty-bit (ARMv8 FEAT_TTHM/DBM) exposure either. A kernel-only
`hv_data_abort_notification_t` exists in `usr/include/arm64/hv/hv_kern_types.h` but
is **not** in the public framework.

What we *do* have:

- `hv_return_t hv_vm_protect(hv_ipa_t ipa, size_t size, hv_memory_flags_t flags)` —
  re-protect an already-mapped guest range. Drop `HV_MEMORY_WRITE` → guest writes
  trap. (`hv_vm.h`.) Page-aligned IPA, size multiple of page size.
- `hv_memory_flags_t`: `HV_MEMORY_READ=1`, `HV_MEMORY_WRITE=2`, `HV_MEMORY_EXEC=4`.
- On a guest write to a write-protected page, vCPU exits with
  `HV_EXIT_REASON_EXCEPTION`; `hv_vcpu_exit_t.exception` carries:
  - `syndrome` (ESR_EL2): EC in bits[31:26]; Data Abort = `0x24` (lower EL).
    DFSC in ISS bits[5:0] distinguishes **permission fault** (0b0011xx) from a
    translation fault.
  - `physical_address` — the **faulting guest IPA**. Exactly the page address we need.
- Guest stage-2 granule is **4 KiB** (host page is 16 KiB on Apple Silicon — protect
  granularity must respect the larger of the two; see open question Q-PAGE).

**Verdict:** write-protect + Data-Abort interception is the *only precise* dirty
mechanism on HVF. Cost model: **one vmexit per first write to each clean page**
(~0.5–2 µs each). KVM gets this free from hardware stage-2 dirty bits; we pay per page.

## 2. ignition substrate (where this hooks in)

- **RAM**: one contiguous region. Boot = `mmap(MAP_ANON|MAP_PRIVATE)`
  (`spike/src/bin/boot.rs:537`); restore = `mmap(MAP_SHARED)` on a clonefile'd
  `memory.bin` (`boot.rs:764`). Mapped via `hv_vm_map(host, RAM_BASE=0x4000_0000,
  size, RWX)` (`crates/hvf/src/lib.rs:396`). RAM is a monolithic blob today — no
  page-granular structure.
- **Exit handler**: `EC_DATAABORT` arm at `crates/hvf/src/lib.rs:888`. **Critical
  subtlety:** today every data abort is assumed to be MMIO — it sets
  `pending_advance_pc = true` and decodes a load/store against an unmapped address.
  A write-protect *permission* fault on a mapped RAM page is a *different* abort: we
  must mark the page dirty, re-grant WRITE via `hv_vm_protect`, and **re-execute the
  store (do NOT advance PC)** — opposite of the MMIO path. Disambiguate on DFSC +
  whether `physical_address` lands inside the RAM region.
- **Full RAM dump** today: `fs::File::create(&p.memory)?.write_all(ram)?`
  (`crates/vmm/src/snapshot.rs:176`).
- **Non-memory state** (always full, never diffed): vCPU regs `VcpuState`
  (`hvf/src/lib.rs:87`), GIC blob via `hv_gic_state_*` (`hvf/src/gic.rs:102`),
  device records (`vmm/src/device_manager.rs:173`). Snapshot format = `VmSnapshot`
  v2, magic `ignition-snapshot-v2` (`snapshot.rs:20`).

## 3. How Firecracker does it (the reference)

- Enables `KVM_MEM_LOG_DIRTY_PAGES` per memory slot; reads the dirty bitmap with
  `KVM_GET_DIRTY_LOG` (`refs/firecracker/.../vstate/vm.rs:545`). **Fallback when
  tracking is off: `mincore()`** (overapproximate — resident, not necessarily written).
- **Diff file = sparse single file.** `dump_dirty` (`.../memory.rs:142`) walks the
  bitmap and `seek`s over clean pages (leaving holes), `write_all`s only dirty runs.
  File logical size = full RAM; physical blocks = only dirty pages.
- **Restore = merge-in-place, single layer.** Diff is written *into* the existing
  base memory file (reused if size matches), so the mem file is always "base + latest
  diff merged". Not a chain of deltas at rest — the merge happens at write time.
- **vmstate always fully serialized** (`MicrovmState`, snapshot v10) — only guest RAM
  is diffed. API: `PUT /snapshot/create {snapshot_type: "Diff"|"Full", ...}`.

## 4. The gap we must bridge, and the option space

FC's mechanism is welded to KVM hardware dirty bits. We have none. The dirty-set
acquisition is the whole research problem; everything downstream (sparse write,
merge-on-restore, full vmstate) ports cleanly.

Candidate mechanisms for the dirty set:

| Option | Precision | Runtime cost | Notes |
|---|---|---|---|
| **A. Write-protect + DABT fault** | exact | vmexit per first write/page | FC-equivalent; needs the EC_DATAABORT disambiguation above |
| **B. `mincore()` on MAP_SHARED instance file** | overapprox (resident ⊇ written) | ~0 runtime; one syscall at snapshot | FC's own fallback; includes read-faulted pages → bigger diff |
| **C. APFS extent / SEEK_HOLE diff of the clone** | — | — | **not viable**: clonefile'd files report fully populated; shared-vs-private extents aren't visible via SEEK_HOLE |
| **D. Hybrid** | tunable | tunable | e.g. mincore to bound, write-protect only on a sampled window |

The **restore side already has a unique asset**: `clonefile` + `MAP_SHARED`. A
restored guest's instance `memory.bin` *is* a CoW delta of the base at the
filesystem level — but APFS won't tell us which extents diverged (option C), so we
can't harvest it cheaply. That asset helps *restore*, not *dirty detection*.

## Open questions for design

- **Q-MECH**: A (precise, vmexit cost) vs B (mincore, cheap+imprecise) vs D (hybrid)?
  Drives everything.
- **Q-PAGE**: protect/track granule — 4 KiB guest vs 16 KiB host page. `hv_vm_protect`
  on a 4 KiB sub-range of a 16 KiB host page may be rejected or promoted; needs a
  spike. Likely settle on 16 KiB tracking granule to match host pages.
- **Q-LAYER**: merge-in-place (FC model, one base file mutates) vs an explicit
  delta-chain (`base` + `diff-001` + `diff-002`, layered at restore). Chain preserves
  history + immutability (matches our existing immutable-base convention) but
  complicates restore.
- **Q-SCOPE**: do we need a *running-guest* dirty tracker (re-snapshot a live restored
  VM with only its changes), or only a *boot→snapshot* diff against a golden base?
- **Q-GATE**: feasibility spike required before committing — confirm (1) `hv_vm_protect`
  WRITE-removal traps as a recoverable DABT we can resume from, (2) granule behaviour,
  (3) measured per-page vmexit cost on a realistic write rate.
