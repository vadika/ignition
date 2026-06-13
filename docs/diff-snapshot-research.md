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

## Gate result

_Ran on real hardware (Apple Silicon, macOS 26.5) via the throwaway spike
`spike/src/bin/dirty_gate.rs`. Bare aarch64 guest (MMU off, EL1h), driven through the
raw HVF bindings so we control PC and read the raw exit. RAM = 512 MiB, host page = 16 KiB._

**VERDICT: GO.** The write-protect + Data-Abort interception mechanism works and is
recoverable on this hardware. All three checks passed.

1. **Recoverable write fault — PASS.** `hv_vm_protect(page, 16 KiB, READ|EXEC)`
   (dropping WRITE) makes the guest store trap with
   `HV_EXIT_REASON_EXCEPTION`, **EC = 0x24** (Data Abort), `iswrite = 1`, and
   `exception.physical_address` = the exact protected page (`0x4000_4000`), with PC
   still parked on the faulting store. The target word stayed unwritten while
   protected. After `hv_vm_protect(page, 16 KiB, READ|WRITE|EXEC)` and resume
   **without advancing PC**, the store re-executed and the word read back as the
   expected value (`0xdead_beef`); the guest made forward progress. Example exit:
   `syndrome=0x9381_0047 EC=0x24 IPA=0x4000_4000`.

   ⚠️ **Design correction (important for Task 2).** HVF does **not** report these as
   *permission* faults. The DFSC is a **translation fault**: `0x07` (level-3
   translation) on a first-touch / not-yet-populated stage-2 PTE, and `0x0f` once the
   PTE is present. The design's discriminator
   `is_permission_fault = (dfsc & 0x3c) == 0x0c` (0b0011xx) would **reject** the
   `0x07` case and so miss real dirty faults. The `EC_DATAABORT` arm must instead
   key the dirty-fault path on **"write data abort (EC 0x24, ISS iswrite bit set)
   whose `physical_address` is inside `[RAM_BASE, RAM_BASE + ram_size)`"** — not on a
   specific DFSC sub-code. The faulting IPA itself is always exactly correct, which is
   all the tracker needs.

2. **Granule — 16 KiB.** `hv_vm_protect(ipa, 4096, ...)` on a 4 KiB sub-range of a
   16 KiB host page is **rejected** (`ret = 0xfae9_4003`, `HV_BAD_ARGUMENT`). Only
   whole 16 KiB host pages are accepted. **Tracking granule = 16384 bytes**, matching
   the design's locked decision and the Apple Silicon host page. The bitmap is one bit
   per 16 KiB page.

3. **Cost — PASS.** protect → fault → regrant over **10,000 distinct pages**:
   total **48.675 ms**, **≈ 4.87 µs per first-write fault**. Well under the ~20 µs/fault
   sanity bound and far from a hang. This is the one-vmexit-per-first-write-per-page
   amortized cost the design assumed (each page faults exactly once per interval).

**Chosen GRANULE = 16384 bytes (16 KiB).**
