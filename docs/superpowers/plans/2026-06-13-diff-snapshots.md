# Diff / Incremental Snapshots Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Re-snapshot a running guest storing only the RAM pages changed since its parent, as an immutable delta chain, with dirty pages detected via HVF write-protect + Data-Abort interception.

**Architecture:** Opt-in (`--track-dirty`) tracker write-protects all guest RAM; guest writes trap as Data-Abort permission faults whose IPA is recorded in a shared bitmap, then the page is re-granted WRITE and the store re-executes. At snapshot the dirty set becomes a Diff layer (packed dirty pages + index sidecar + full vmstate) with a `parent` pointer. Restore resolves the chain, clonefiles the root, mmaps `MAP_SHARED`, and overlays each Diff's pages into the private clone.

**Tech Stack:** Rust 2024, Apple Hypervisor.framework (`hv_vm_protect`), libc mmap/clonefile, serde/serde_json.

**Reference:** Design `docs/superpowers/specs/2026-06-13-diff-snapshots-design.md`; research `docs/diff-snapshot-research.md`.

---

## File Structure

- `spike/src/bin/dirty_gate.rs` — **throwaway** feasibility spike (Task 1; deleted in Task 11).
- `crates/hvf/src/lib.rs` — add `Vm::protect_memory`; `VcpuExit::DirtyFault(u64)`; disambiguate the `EC_DATAABORT` arm (line ~888).
- `crates/vmm/src/dirty.rs` — new `DirtyTracker` (shared atomic bitmap).
- `crates/vmm/src/lib.rs` — `pub mod dirty;`.
- `crates/vmm/src/snapshot.rs` — v3 format: `SnapshotType`, `parent` on manifest, version bump + v2 reject, `write_diff_layer`/`read_diff_layer`/`apply_diff`, `resolve_chain`.
- `spike/src/bin/boot.rs` — `--track-dirty`; arm tracker + `protect_all`; handle `DirtyFault` in run loop; Full-vs-Diff snapshot handler; chain-aware `run_restore`.
- `scripts/diff_snapshot_test.py` — headless integration driver.
- `README.md`, `ROADMAP.md` — docs.

Tracking granule constant `PAGE` lives in `crates/vmm/src/dirty.rs` (default 16384, revised by Task 1 if the gate finds otherwise).

---

## Task 1: Feasibility gate spike (GO / NO-GO)

**This task gates the whole feature. Do NOT start Task 2+ until its three checks pass.** If any fails, STOP and report to the orchestrator/human.

**Files:**
- Create: `spike/src/bin/dirty_gate.rs`
- Modify: `spike/Cargo.toml` (add `[[bin]] name = "dirty_gate"`)

Build a minimal HVF VM with a hand-rolled tiny aarch64 guest (reference the deleted `spike/src/main.rs` validation-spike pattern in git history: `git show HEAD~..:spike/src/main.rs` or `git log --all --diff-filter=D -- spike/src/main.rs`, for the `DummyVcpus` + small-guest-blob approach). The guest must execute a store loop to a known RAM address.

- [ ] **Step 1: Build the spike**

Guest program (assemble a few instructions into RAM at the entry IPA): load a RAM data address into a register, then a loop: `STR Xn, [Xaddr]; ADD; B loop` (or simplest: a handful of stores to distinct pages, then `HVC #0`/WFI to exit). Host side:
1. Map RAM `RWX` via existing `Vm::map_memory`.
2. Run until the guest is about to store (or just before entry).
3. `hv_vm_protect(target_page_ipa, GRANULE, HV_MEMORY_READ | HV_MEMORY_EXEC)`.
4. Resume; expect `HV_EXIT_REASON_EXCEPTION`, EC `(syndrome>>26)&0x3f == 0x24`, DFSC `syndrome & 0x3f` in `0x0c..=0x0f`, `exception.physical_address` == target page.
5. Re-grant: `hv_vm_protect(target_page_ipa, GRANULE, READ|WRITE|EXEC)`; resume **without advancing PC**; confirm the store completes (read the RAM word back == expected) and the guest progresses.

- [ ] **Step 2: Check 1 — recoverable fault**

Run: `cargo build -p ignition-spike --bin dirty_gate && scripts/sign.sh target/debug/dirty_gate && target/debug/dirty_gate`
Expected: prints the fault syndrome/EC/DFSC/IPA, then confirms the post-regrant store wrote the expected value and the guest made forward progress. PASS = recoverable.

- [ ] **Step 3: Check 2 — granule**

In the spike, attempt `hv_vm_protect` on a 4096-byte sub-range of a 16384 host page; record whether it returns `HV_SUCCESS`, an error, or silently affects the whole 16K. Print the verdict.
Expected: a definitive answer. Set `PAGE` in Task 3 accordingly (16384 unless 4096 cleanly works).

- [ ] **Step 4: Check 3 — cost**

Loop the protect→fault→regrant cycle over N (e.g. 10_000) distinct pages; print total wall time and per-fault µs (use a host clock that's allowed — `std::time::Instant` is fine in a normal binary). Confirm it's a bounded ~µs-scale cost, not a hang/spin.
Expected: per-fault cost printed (sanity: < ~20 µs).

- [ ] **Step 5: Record verdict + commit**

Append a short "Gate result" section (the three answers + the chosen `PAGE`) to `docs/diff-snapshot-research.md`.

```bash
git add spike/src/bin/dirty_gate.rs spike/Cargo.toml docs/diff-snapshot-research.md
git commit -m "Diff snapshots: feasibility gate spike (write-protect dirty fault)"
```

**GATE:** all three checks pass → proceed. Else STOP and escalate.

---

## Task 2: HVF protect_memory + DirtyFault exit + DABT disambiguation

**Files:**
- Modify: `crates/hvf/src/lib.rs` (add `protect_memory` near `map_memory` ~line 389; add `VcpuExit::DirtyFault`; edit `EC_DATAABORT` arm ~line 888)

- [ ] **Step 1: Add `protect_memory`**

```rust
pub fn protect_memory(&self, guest_addr: u64, size: u64, flags: u64) -> Result<(), Error> {
    let ret = unsafe { hv_vm_protect(guest_addr, size.try_into().unwrap(), flags) };
    if ret != HV_SUCCESS { Err(Error::MemoryMap) } else { Ok(()) }
}
```
(Declare `hv_vm_protect` in the extern block alongside `hv_vm_map` if not already present: `fn hv_vm_protect(addr: hv_ipa_t, size: usize, flags: hv_memory_flags_t) -> hv_return_t;`.)

- [ ] **Step 2: Add the exit variant**

In the `VcpuExit` enum add `DirtyFault(u64)`.

- [ ] **Step 3: Add dirty-tracking RAM window state to the vCPU**

Add fields the run loop can set: `dirty_tracking: bool`, `ram_base: u64`, `ram_size: u64` (default false/0). A setter `set_dirty_window(&mut self, base: u64, size: u64)` sets them and `dirty_tracking = true`.

- [ ] **Step 4: Disambiguate the DABT arm**

At the top of `EC_DATAABORT` (before the existing MMIO decode that sets `pending_advance_pc`):
```rust
let pa = self.vcpu_exit.exception.physical_address;
let iswrite = ((syndrome >> 6) & 1) != 0; // ISS WnR bit
// GATE FINDING (Task 1): HVF reports write-protect faults as TRANSLATION faults
// (DFSC 0x07/0x0f), NOT permission faults — so do NOT key on a DFSC sub-code.
// Discriminate on: write data abort whose IPA is inside the RAM region. MMIO
// accesses have pa outside RAM; unprotected RAM accesses don't fault at all.
if self.dirty_tracking
    && iswrite
    && pa >= self.ram_base
    && pa < self.ram_base + self.ram_size
{
    // Do NOT set pending_advance_pc — the store must re-execute after re-grant.
    return Ok(VcpuExit::DirtyFault(pa));
}
// ...existing MMIO handling unchanged below...
```

- [ ] **Step 5: Build + existing tests**

Run: `cargo build -p ignition-hvf && cargo test -p ignition-hvf`
Expected: compiles; existing tests still pass (mechanism path is exercised live, not unit-tested here).

- [ ] **Step 6: Commit**

```bash
git add crates/hvf/src/lib.rs
git commit -m "HVF: protect_memory + VcpuExit::DirtyFault + DABT permission-fault disambiguation"
```

---

## Task 3: DirtyTracker module (TDD)

**Files:**
- Create: `crates/vmm/src/dirty.rs`
- Modify: `crates/vmm/src/lib.rs` (`pub mod dirty;`)
- Test: inline `#[cfg(test)]` in `dirty.rs`

Use `PAGE` = value confirmed by Task 1 (16384 unless the gate proved 4096 works).

- [ ] **Step 1: Write failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mark_and_drain_sorted_unique() {
        let t = DirtyTracker::new(RAM_BASE, 4 * PAGE as u64);
        t.mark(RAM_BASE + 2 * PAGE as u64 + 7);   // page 2
        t.mark(RAM_BASE + 7);                       // page 0
        t.mark(RAM_BASE + 2 * PAGE as u64);         // page 2 again
        assert_eq!(t.drain(), vec![0, 2]);
        assert_eq!(t.drain(), Vec::<u64>::new());   // cleared
    }
    #[test]
    fn last_partial_page_counts() {
        let t = DirtyTracker::new(RAM_BASE, 3 * PAGE as u64 + 1); // 4 pages
        assert_eq!(t.page_count(), 4);
        t.mark(RAM_BASE + 3 * PAGE as u64);
        assert_eq!(t.drain(), vec![3]);
    }
}
```
(`RAM_BASE` import: `use ignition_arch::aarch64::layout::RAM_BASE;` or pass any base in the test — keep the test base-agnostic by using the value passed to `new`.)

- [ ] **Step 2: Run — fails to compile**

Run: `cargo test -p ignition-vmm dirty`
Expected: FAIL (no `DirtyTracker`).

- [ ] **Step 3: Implement**

```rust
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub const PAGE: usize = 16384; // tracking granule (set by feasibility gate Task 1)

#[derive(Clone)]
pub struct DirtyTracker {
    base: u64,
    page_count: u64,
    bits: Arc<Vec<AtomicU64>>,
}

impl DirtyTracker {
    pub fn new(base: u64, size: u64) -> Self {
        let page_count = size.div_ceil(PAGE as u64);
        let words = (page_count as usize).div_ceil(64);
        let bits = Arc::new((0..words).map(|_| AtomicU64::new(0)).collect());
        Self { base, page_count, bits }
    }
    pub fn page_count(&self) -> u64 { self.page_count }
    pub fn mark(&self, ipa: u64) {
        if ipa < self.base { return; }
        let p = (ipa - self.base) / PAGE as u64;
        if p >= self.page_count { return; }
        self.bits[(p / 64) as usize].fetch_or(1u64 << (p % 64), Ordering::Relaxed);
    }
    pub fn drain(&self) -> Vec<u64> {
        let mut out = Vec::new();
        for (wi, w) in self.bits.iter().enumerate() {
            let v = w.swap(0, Ordering::Relaxed);
            if v == 0 { continue; }
            for b in 0..64 {
                if (v >> b) & 1 == 1 { out.push(wi as u64 * 64 + b); }
            }
        }
        out // already ascending
    }
}
```

- [ ] **Step 4: Run — passes**

Run: `cargo test -p ignition-vmm dirty`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/vmm/src/dirty.rs crates/vmm/src/lib.rs
git commit -m "vmm: DirtyTracker shared atomic bitmap (16K granule)"
```

---

## Task 4: Snapshot format v3 — SnapshotType + parent + v2 reject (TDD)

**Files:**
- Modify: `crates/vmm/src/snapshot.rs` (`SNAP_MAGIC`/`SNAP_VERSION`, `SnapshotManifest`, version guard in `read_*`)
- Test: inline `#[cfg(test)]`

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn manifest_roundtrip_with_type_and_parent() {
    let m = SnapshotManifest { name: "child".into(), created: 1, mem_size: 1<<20,
        vcpu_count: 1, snapshot_type: SnapshotType::Diff, parent: Some("root".into()) };
    let j = serde_json::to_vec(&m).unwrap();
    let back: SnapshotManifest = serde_json::from_slice(&j).unwrap();
    assert_eq!(back, m);
    assert_eq!(SnapshotManifest::new_full("root".into(), 1<<20, 1).parent, None);
}
#[test]
fn v2_snapshot_rejected() {
    let snap = VmSnapshot { magic: "ignition-snapshot-v2".into(), version: 2,
        ..VmSnapshot::minimal_for_test() };
    assert!(check_version(&snap).is_err());
}
```
(Add a small `VmSnapshot::minimal_for_test()` helper under `#[cfg(test)]` if none exists.)

- [ ] **Step 2: Run — fails**

Run: `cargo test -p ignition-vmm snapshot`
Expected: FAIL.

- [ ] **Step 3: Implement**

```rust
pub const SNAP_MAGIC: &str = "ignition-snapshot-v3";
pub const SNAP_VERSION: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SnapshotType { Full, Diff }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotManifest {
    pub name: String,
    pub created: u64,
    pub mem_size: u64,
    pub vcpu_count: u64,
    pub snapshot_type: SnapshotType,
    pub parent: Option<String>,
}
impl SnapshotManifest {
    pub fn new_full(name: String, mem_size: u64, vcpu_count: u64) -> Self { /* created from SystemTime; type Full; parent None */ }
    pub fn new_diff(name: String, parent: String, mem_size: u64, vcpu_count: u64) -> Self { /* type Diff; parent Some */ }
}

pub fn check_version(s: &VmSnapshot) -> io::Result<()> {
    if s.magic != SNAP_MAGIC || s.version != SNAP_VERSION {
        return Err(io::Error::other(format!(
            "unsupported snapshot {} v{} (need {} v{})", s.magic, s.version, SNAP_MAGIC, SNAP_VERSION)));
    }
    Ok(())
}
```
Wire `check_version` into `read_snapshot`. Replace the old `SnapshotManifest::new` call sites with `new_full` (boot/full path).

- [ ] **Step 4: Run — passes; full crate**

Run: `cargo test -p ignition-vmm`
Expected: PASS (fix any call sites the rename touched).

- [ ] **Step 5: Commit**

```bash
git add crates/vmm/src/snapshot.rs
git commit -m "snapshot: v3 format with SnapshotType + parent, reject v2"
```

---

## Task 5: write_diff_layer / read_diff_layer / apply_diff (TDD)

**Files:**
- Modify: `crates/vmm/src/snapshot.rs`
- Test: inline

A Diff layer's `memory.bin` = dirty pages packed in ascending index order; `dirty.idx` = the sorted page indices as LE u64. `apply_diff` overlays packed pages onto a target byte slice.

- [ ] **Step 1: Write failing test (round-trip)**

```rust
#[test]
fn diff_pack_apply_roundtrip() {
    let page = dirty::PAGE;
    let mut ram = vec![0u8; 4 * page];
    for i in 0..ram.len() { ram[i] = (i % 251) as u8; }
    let dirty = vec![1u64, 3];
    let dir = tempfile::tempdir().unwrap();
    write_diff_pages(dir.path(), &dirty, &ram).unwrap();      // writes memory.bin + dirty.idx
    let (idx, packed) = read_diff_pages(dir.path()).unwrap();
    assert_eq!(idx, dirty);
    let mut target = vec![0u8; 4 * page];
    apply_diff(&mut target, &idx, &packed).unwrap();
    for &p in &dirty {
        let o = p as usize * page;
        assert_eq!(&target[o..o+page], &ram[o..o+page]);
    }
    assert!(target[0..page].iter().all(|&b| b == 0)); // page 0 untouched
}
```

- [ ] **Step 2: Run — fails**

Run: `cargo test -p ignition-vmm diff_pack`
Expected: FAIL.

- [ ] **Step 3: Implement**

```rust
pub fn write_diff_pages(dir: &Path, dirty: &[u64], ram: &[u8]) -> io::Result<()> {
    let page = dirty::PAGE;
    let mut mem = fs::File::create(dir.join("memory.bin"))?;
    for &p in dirty {
        let o = p as usize * page;
        mem.write_all(&ram[o..o + page])?;
    }
    let mut idx = fs::File::create(dir.join("dirty.idx"))?;
    for &p in dirty { idx.write_all(&p.to_le_bytes())?; }
    Ok(())
}
pub fn read_diff_pages(dir: &Path) -> io::Result<(Vec<u64>, Vec<u8>)> {
    let raw = fs::read(dir.join("dirty.idx"))?;
    let idx: Vec<u64> = raw.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect();
    let packed = fs::read(dir.join("memory.bin"))?;
    Ok((idx, packed))
}
pub fn apply_diff(target: &mut [u8], idx: &[u64], packed: &[u8]) -> io::Result<()> {
    let page = dirty::PAGE;
    if packed.len() != idx.len() * page { return Err(io::Error::other("diff packed size mismatch")); }
    for (i, &p) in idx.iter().enumerate() {
        let o = p as usize * page;
        if o + page > target.len() { return Err(io::Error::other("diff page out of range")); }
        target[o..o + page].copy_from_slice(&packed[i * page..(i + 1) * page]);
    }
    Ok(())
}
```

- [ ] **Step 4: Run — passes**

Run: `cargo test -p ignition-vmm diff_pack`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/vmm/src/snapshot.rs
git commit -m "snapshot: diff layer pack/unpack/apply (dirty.idx sidecar)"
```

---

## Task 6: resolve_chain (TDD)

**Files:**
- Modify: `crates/vmm/src/snapshot.rs`
- Test: inline (build fake store dirs with manifests)

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn resolve_chain_root_to_leaf() {
    let store = tempfile::tempdir().unwrap();
    write_manifest_at(store.path(), &SnapshotManifest::new_full("root".into(), 4096, 1));
    write_manifest_at(store.path(), &SnapshotManifest::new_diff("mid".into(), "root".into(), 4096, 1));
    write_manifest_at(store.path(), &SnapshotManifest::new_diff("leaf".into(), "mid".into(), 4096, 1));
    let chain = resolve_chain(store.path(), "leaf").unwrap();
    let names: Vec<_> = chain.iter().map(|m| m.name.clone()).collect();
    assert_eq!(names, vec!["root", "mid", "leaf"]);
}
#[test]
fn resolve_chain_missing_parent_errors() {
    let store = tempfile::tempdir().unwrap();
    write_manifest_at(store.path(), &SnapshotManifest::new_diff("orphan".into(), "ghost".into(), 4096, 1));
    assert!(resolve_chain(store.path(), "orphan").is_err());
}
#[test]
fn resolve_chain_cycle_errors() {
    // root claims parent=leaf, leaf claims parent=root → cycle
    let store = tempfile::tempdir().unwrap();
    let mut a = SnapshotManifest::new_diff("a".into(), "b".into(), 4096, 1);
    let mut b = SnapshotManifest::new_diff("b".into(), "a".into(), 4096, 1);
    write_manifest_at(store.path(), &a); write_manifest_at(store.path(), &b);
    assert!(resolve_chain(store.path(), "a").is_err());
}
```
(`write_manifest_at` test helper: create `<store>/snapshots/<name>/manifest.json`.)

- [ ] **Step 2: Run — fails**

Run: `cargo test -p ignition-vmm resolve_chain`
Expected: FAIL.

- [ ] **Step 3: Implement**

```rust
pub fn resolve_chain(store: &Path, leaf: &str) -> io::Result<Vec<SnapshotManifest>> {
    let mut chain = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut cur = leaf.to_string();
    loop {
        if !seen.insert(cur.clone()) {
            return Err(io::Error::other(format!("snapshot chain cycle at {cur}")));
        }
        let m = read_manifest(&base_dir(store, &cur))
            .map_err(|e| io::Error::other(format!("missing layer {cur}: {e}")))?;
        let parent = m.parent.clone();
        chain.push(m);
        match parent {
            Some(p) => cur = p,
            None => break, // reached Full root
        }
    }
    chain.reverse(); // root..leaf
    Ok(chain)
}
```

- [ ] **Step 4: Run — passes**

Run: `cargo test -p ignition-vmm resolve_chain`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/vmm/src/snapshot.rs
git commit -m "snapshot: resolve_chain (root..leaf, cycle + missing-parent guards)"
```

---

## Task 7: boot.rs — --track-dirty flag, arm tracker, handle DirtyFault

**Files:**
- Modify: `spike/src/bin/boot.rs`

- [ ] **Step 1: Add the flag**

Parse `--track-dirty` (bool, default false) in `main`. Thread it into the boot and restore paths.

- [ ] **Step 2: Arm on boot**

After `vm.map_memory(host_addr, RAM_BASE, ram_size)` on the boot path: if `track_dirty`, create `let tracker = DirtyTracker::new(RAM_BASE, ram_size);`, call `vm.protect_memory(RAM_BASE, ram_size, HV_MEMORY_READ | HV_MEMORY_EXEC)`, and for each vCPU `vcpu.set_dirty_window(RAM_BASE, ram_size)`. Keep `tracker` available to the snapshot handler (clone the `Arc`-backed tracker; it is `Clone`).

- [ ] **Step 3: Handle `DirtyFault` in the run loop**

Where vCPU exits are matched, add:
```rust
VcpuExit::DirtyFault(pa) => {
    tracker.mark(pa);
    let page_base = pa & !((dirty::PAGE as u64) - 1);
    vm.protect_memory(page_base, dirty::PAGE as u64,
        HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC)
        .expect("re-grant write on dirty page");
    // loop continues; PC was not advanced, store re-executes
}
```
(If only one vCPU has the `vm` handle, ensure `protect_memory` is reachable from the run loop; `Vm` is `Clone`/`Arc`-shared in this codebase — follow the existing pattern.)

- [ ] **Step 4: Build + sign + smoke**

Run: `cargo build -p ignition-spike --bin boot && scripts/sign.sh target/debug/boot`
Then boot with tracking and confirm the guest still reaches a shell (manual or via the existing boot smoke). Expected: boots normally with `--track-dirty` (just slower on first writes).

- [ ] **Step 5: Commit**

```bash
git add spike/src/bin/boot.rs
git commit -m "boot: --track-dirty arms write-protect tracker, handles DirtyFault"
```

---

## Task 8: boot.rs — Full-vs-Diff snapshot handler

**Files:**
- Modify: `spike/src/bin/boot.rs` (the `Ctrl-A s` snapshot handler / `write_named_snapshot`)

- [ ] **Step 1: Decide layer type**

In the snapshot handler, after the vCPU rendezvous: if `tracker` present AND a `restored_from`/parent leaf is known → write a **Diff** layer (`SnapshotManifest::new_diff(name, parent, mem_size, vcpu_count)`, `write_diff_pages(base_dir, &tracker.drain(), ram)`). Otherwise (fresh boot, first snapshot) → write a **Full** layer (existing `write_snapshot` + `new_full`). After writing: if tracking, `vm.protect_memory(RAM_BASE, ram_size, READ|EXEC)` again to start the next interval clean (the drain already cleared the bitmap).

- [ ] **Step 2: Refuse diff without tracking**

If a Diff would be implied (guest restored from a chain) but `tracker` is `None` → `eprintln!` "dirty tracking not enabled; restart with --track-dirty for diffs" and write nothing (or fall back to Full with a warning — pick refuse, per spec).

- [ ] **Step 3: Reuse vmstate/GIC/devices full-write**

Diff layers still write `gic.bin` + `vmstate.json` + `manifest.json` + `disk.img` (clonefile of the live instance disk) exactly like full layers — only `memory.bin` differs (packed) plus the `dirty.idx` sidecar.

- [ ] **Step 4: Build + sign**

Run: `cargo build -p ignition-spike --bin boot && scripts/sign.sh target/debug/boot`
Expected: compiles.

- [ ] **Step 5: Commit**

```bash
git add spike/src/bin/boot.rs
git commit -m "boot: snapshot handler writes Full root or Diff layer by parent"
```

---

## Task 9: boot.rs — chain-aware run_restore

**Files:**
- Modify: `spike/src/bin/boot.rs` (`run_restore`)

- [ ] **Step 1: Resolve + validate chain**

`let chain = snapshot::resolve_chain(store, restore_name)?;` Validate all layers share `mem_size` and are v3. The **root** (`chain[0]`) is Full; the rest are Diff. The **leaf** (`chain.last()`) provides vmstate/GIC/devices.

- [ ] **Step 2: Clonefile root + mmap**

`clonefile_or_copy(&base_dir(store, &chain[0].name).join("memory.bin"), &inst_mem)?;` then the existing `mmap(MAP_SHARED)` of `inst_mem` → `ram: &mut [u8]`.

- [ ] **Step 3: Overlay each Diff layer**

```rust
for m in &chain[1..] {
    let d = base_dir(store, &m.name);
    let (idx, packed) = snapshot::read_diff_pages(&d)?;
    snapshot::apply_diff(ram, &idx, &packed)?; // writes into the mmap'd private clone
}
```

- [ ] **Step 4: Restore leaf vmstate, optionally re-arm**

Read the leaf's `vmstate.json`/`gic.bin` (existing `read_snapshot` against the leaf dir). After `run_restored`, if `--track-dirty`: create a fresh `DirtyTracker`, `protect_memory(... READ|EXEC)`, `set_dirty_window` on each vCPU, and set the parent for the next snapshot to `restore_name` (the leaf). So re-snapshotting this guest appends a new Diff with `parent = leaf`.

- [ ] **Step 5: Build + sign**

Run: `cargo build -p ignition-spike --bin boot && scripts/sign.sh target/debug/boot`
Expected: compiles.

- [ ] **Step 6: Commit**

```bash
git add spike/src/bin/boot.rs
git commit -m "boot: chain-aware restore — clonefile root, overlay diff layers, re-arm"
```

---

## Task 10: Integration driver + run

**Files:**
- Create: `scripts/diff_snapshot_test.py`

- [ ] **Step 1: Write the driver**

Model it on `scripts/restore_test.py`. Flow:
1. Boot `--store STORE --name root --track-dirty <kernel> <rootfs>`; after boot completes, send `Ctrl-A s` (writes Full `root`); record `root/memory.bin` physical size (`st_blocks*512`).
2. Restore `--store STORE --restore root --track-dirty`; in-guest, write a bounded known region (e.g. `dd if=/dev/zero of=/tmp/x bs=1M count=8`); `Ctrl-A s` (writes Diff `<auto>` with `parent=root`).
3. Assert the Diff layer's `memory.bin` physical size ≪ full RAM and roughly ≈ the touched region (allow slack for kernel background writes).
4. Restore the **leaf**; assert it reaches a shell and a sentinel written before the diff reads back correctly.
5. Assert immutability: md5 of `root/memory.bin`, `root/disk.img`, and every layer's artifacts identical before and after the leaf restore.
6. Confirm restored-from-diff guest idles ~0% CPU (`ps -o %cpu`).
Print: `diff_smaller=<bool> diff_mb=<n> full_mb=<n> leaf_responsive=<bool> immutable=<bool> restore_cpu=<n>%`.

- [ ] **Step 2: Run it**

Run: `python3 scripts/diff_snapshot_test.py`
Expected: `diff_smaller=True ... leaf_responsive=True immutable=True` and low CPU.

- [ ] **Step 3: Commit**

```bash
git add scripts/diff_snapshot_test.py
git commit -m "test: headless diff-snapshot driver (size, correctness, immutability)"
```

---

## Task 11: Cleanup + docs

**Files:**
- Delete: `spike/src/bin/dirty_gate.rs`; remove its `[[bin]]` from `spike/Cargo.toml`
- Modify: `README.md`, `ROADMAP.md`

- [ ] **Step 1: Remove the gate spike**

```bash
git rm spike/src/bin/dirty_gate.rs
```
Remove the `[[bin]] name = "dirty_gate"` block from `spike/Cargo.toml`.

- [ ] **Step 2: README**

Add a "Diff snapshots" subsection under Snapshot & restore: `--track-dirty` arms tracking; a restored armed guest's `Ctrl-A s` writes a Diff layer (`parent` = the leaf it restored from); restore reassembles the chain transparently. Note the per-page first-write cost and that vmstate is always full.

- [ ] **Step 3: ROADMAP**

Flip "Diff / incremental snapshots" from `[ ]` to `[x]` under Shipped (move it from Next), link `docs/superpowers/specs/2026-06-13-diff-snapshots-design.md`. Update the parity table row "Diff snapshots" to ✅ (`hv_vm_protect` write-fault tracking). Leave chain flatten/compaction as a `[ ]` follow-up under Planned.

- [ ] **Step 4: Build + full test**

Run: `cargo build && cargo test`
Expected: workspace builds, all tests pass.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "Diff snapshots: remove gate spike, update README + ROADMAP"
```

---

## Self-review notes

- Spec coverage: tracker (§1→T3), HVF protect+fault (§2→T2), v3 format (§3→T4-6), snapshot flow (§4→T7-8), restore flow (§5→T9), CLI (§6→T7), feasibility gate (→T1), testing (→T3-6,T10). Covered.
- Type consistency: `SnapshotType`, `SnapshotManifest{snapshot_type,parent}`, `new_full`/`new_diff`, `DirtyTracker::{new,mark,drain,page_count}`, `write_diff_pages`/`read_diff_pages`/`apply_diff`, `resolve_chain`, `VcpuExit::DirtyFault`, `Vm::protect_memory`, `PAGE` — names used identically across tasks.
- `PAGE` granule is provisional 16384, finalized by Task 1; Task 3 must use the gate's value.
- **Gate discipline:** Task 1 is GO/NO-GO; Tasks 2+ presuppose it passed.
