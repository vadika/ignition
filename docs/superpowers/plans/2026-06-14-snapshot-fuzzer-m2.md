# Snapshot Fuzzer M2 — Coverage + Dirty-Page Reset Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add SanCov coverage feedback (8-bit edge counters in a host-readable shared region) and swap the per-iteration full-RAM-copy reset for a `hv_vm_protect` dirty-page reset, so the fuzz loop gets a coverage curve and a measurable execs/sec jump. libAFL integration is explicitly deferred to a later milestone; the existing homegrown mutation brain stays, now coverage-guided.

**Architecture:** A new RAM-backed coverage region (`FUZZ_COV_GPA`, 64 KiB, mapped host+guest exactly like the input window, naturally reset-exempt because it sits below `RAM_BASE` outside tracked guest RAM). The guest target is built with `-fsanitize-coverage=trace-pc`; an uninstrumented callback in the harness hashes the edge PC into the coverage region (mirroring the committed `docs/examples/fuzzing/harness.c`). The host zeroes the region before each input and reads it after `DONE`, accumulating a virgin map and keeping coverage-increasing inputs in the corpus. Reset reuses the already-merged `DirtyTracker` (from diff-snapshots): at the snapshot point the VMM write-protects all guest RAM; first writes fault, are logged, and re-granted WRITE; reset restores only the drained dirty pages from the base copy and re-protects. A `--reset full|dirty` flag selects the mechanism so the gate can compare throughput.

**Tech Stack:** Rust (`ignition-vmm`, `ignition-devices`, `ignition-spike`), Apple Hypervisor.framework (`ignition-hvf`), C guest harness built with clang `-fsanitize=address -fsanitize-coverage=trace-pc` in an arm64 alpine container (remote `artemis2`, per `REBUILD-GUEST-ASSETS.md`), Python gate script.

---

## Context an implementer needs (read before starting)

These facts are established in the codebase; do not re-derive them.

- **Guest RAM**: `ignition_arch::aarch64::layout::RAM_BASE = 0x4000_0000`. Guest RAM is mapped at `RAM_BASE`, size `ram_size` (fuzz default `--mem 96` → `96 << 20`).
- **Existing fuzz GPAs** (`spike/src/bin/boot.rs`): `FUZZ_CTRL_GPA = 0x0920_0000` (control region, `protocol::CONTROL_SIZE = 0x4000`, trap-MMIO via `add_fixed`, unmapped); `FUZZ_WIN_GPA = 0x0920_4000` (input window, `window_size` default `DEFAULT_WINDOW_SIZE = 0x20_0000`, RAM-backed via `map_memory`). The window ends at `0x0920_4000 + 0x20_0000 = 0x0940_4000`, far below `RAM_BASE`.
- **Page granule**: `ignition_vmm::dirty::PAGE = 16384` (16 KiB). All device GPAs must be 16 KiB-aligned (guest `/dev/mem` mmap offsets).
- **Dirty tracking already exists** (`crates/vmm/src/dirty.rs`): `DirtyTracker::new(base, size)`, `.mark(ipa)`, `.drain() -> Vec<u64>` (sorted ascending page indices, clears the set). It is `Clone` (shares an `Arc<Vec<AtomicU64>>` bitmap).
- **Write-protect mechanism** (used by diff-snapshots in `run_loop`): `ignition_hvf::vm_protect_memory(gpa, size, flags)` changes guest stage-2 permissions. Write-protect = `(HV_MEMORY_READ | HV_MEMORY_EXEC) as u64`; full grant = `(HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC) as u64`. `HvfVcpu::set_dirty_window(base, size)` makes write faults in that range surface as `VcpuExit::DirtyFault(pa)` instead of MMIO data aborts. **Host writes through the original host VA (`ram_ptr`) bypass stage-2 entirely**, so the host can always restore RAM regardless of protect state.
- **The `run_loop` DirtyFault arm** (`crates/vmm/src/vstate/vcpu_manager.rs:427`) is the exact pattern to mirror in `fuzz_loop`: `cfg.tracker.mark(pa)`, then `vm_protect_memory(pa & !(PAGE-1), PAGE, full_grant)` (re-grant WRITE on the faulting page), resume **without** advancing PC so the store re-executes.
- **PC-advance asymmetry** (already correct in `fuzz_loop`): on `SNAPSHOT_ME` the loop calls `vcpu.advance_pc()` then `controller.capture`; on `DONE`/`CRASH` it does the reset and `vcpu.clear_pending_advance()`. Do not change this.
- **Window/coverage are host-managed and reset-exempt**: the input window and the new coverage region are mapped at GPAs below `RAM_BASE`, outside the `DirtyTracker` range (`base = RAM_BASE`), so they are never write-protected, never marked dirty, never rolled back. This is the spec §6 "host-managed pages excluded from dirty-reset" property, satisfied by layout.
- **Coverage callback approach** (mirror `docs/examples/fuzzing/harness.c:34`): build only `target.c` with `-fsanitize-coverage=trace-pc`; define `__sanitizer_cov_trace_pc` in the **uninstrumented** `harness.c`. It must null-check the coverage pointer (the callback fires during global constructors before `main` maps the region) and hash `__builtin_return_address(0)` into the region: `cov[(pc >> 4) & (COV_SIZE - 1)]++`.
- **Build is remote**: the initramfs is built in an arm64 alpine container on `artemis2` via `kimage/build/build-fuzz-initramfs.sh`, then pulled to `kimage/out/fuzz-initramfs.cpio` (gitignored). The host has no Docker; C-side changes are verified by rebuilding on `artemis2` and booting, not by a local unit test. The boot binary must be re-signed after each Rust build: `scripts/sign.sh target/debug/boot`.
- **Run termination**: `boot --fuzz` loops forever; the gate (`scripts/fuzz_m1_test.py`) bounds it with a wall-clock timeout + `SIGINT`, polling the solutions dir for `crash-*.bin`. Throughput/coverage numbers must therefore be emitted as periodic stderr lines the gate can parse, not only at clean exit.

---

## File Structure

- `crates/devices/src/fuzz/protocol.rs` — add `DEFAULT_COV_SIZE` constant + a test asserting it is page-aligned and a power of two.
- `crates/vmm/src/fuzz/controller.rs` — add the coverage observer (virgin map, `record_coverage`, coverage-guided corpus add), the dirty-set selective restore + `ResetMode`, periodic stats emission, and the widened `FuzzController::new` signature.
- `crates/vmm/src/vstate/vcpu_manager.rs` — arm dirty tracking in `fuzz_loop` (mirror `run_loop`'s `set_dirty_window` + `DirtyFault` arm); the controller does the protect/restore/re-protect at capture/reset.
- `spike/src/bin/boot.rs` — `FUZZ_COV_GPA` constant + overlap/alignment asserts; map the coverage region; `--reset full|dirty` flag (default `dirty`); pass the coverage region, dirty tracker, reset mode into `FuzzController::new`; arm `set_dirty_config` on the manager for the fuzz path.
- `kimage/build/fuzz-harness/ignition_fuzz.h` — add `IGNITION_FUZZ_COV_GPA` / `IGNITION_FUZZ_COV_SIZE`.
- `kimage/build/fuzz-harness/harness.c` — mmap the coverage region; define the uninstrumented `__sanitizer_cov_trace_pc` callback.
- `kimage/build/build-fuzz-initramfs.sh` — add `-fsanitize-coverage=trace-pc` to the `target.c` compile (only target.c; harness.c stays uninstrumented).
- `scripts/fuzz_m2_test.py` — new M2 gate: coverage grows + corpus grows, planted crash still rediscovered, dirty-reset execs/sec > full-copy execs/sec.
- `REBUILD-GUEST-ASSETS.md` — note the coverage-instrumentation build flag.
- `docs/superpowers/specs/2026-06-14-snapshot-fuzzing-demonstrator-design.md` — check the M2 box.

---

### Task 1: Coverage-region size constant + layout asserts

**Files:**
- Modify: `crates/devices/src/fuzz/protocol.rs`
- Modify: `spike/src/bin/boot.rs:979-1034`

- [ ] **Step 1: Write the failing test** in `crates/devices/src/fuzz/protocol.rs` (add to the existing `mod tests`):

```rust
    #[test]
    fn cov_size_is_page_aligned_power_of_two() {
        // The coverage region is a host-readable 8-bit-counter map mmap'd into the
        // guest at a 16 KiB-aligned GPA; its size must be 16 KiB-aligned so the
        // guest can /dev/mem-mmap it, and a power of two so the trace-pc callback
        // can mask the hashed PC with (DEFAULT_COV_SIZE - 1).
        assert_eq!(DEFAULT_COV_SIZE % 0x4000, 0, "cov region must be 16 KiB-aligned");
        assert!(DEFAULT_COV_SIZE.is_power_of_two(), "mask trick needs a power of two");
        assert!(DEFAULT_COV_SIZE >= 0x4000, "at least one guest page");
    }
```

- [ ] **Step 2: Run it to confirm it fails**

Run: `cargo test -p ignition-devices cov_size_is_page_aligned_power_of_two`
Expected: FAIL — `cannot find value DEFAULT_COV_SIZE in this scope`.

- [ ] **Step 3: Add the constant** in `crates/devices/src/fuzz/protocol.rs`, right after `DEFAULT_WINDOW_SIZE`:

```rust
/// Default coverage-region size in bytes (64 KiB). An array of 8-bit SanCov edge
/// counters, written by the guest's `trace-pc` callback and read by the host
/// observer. A power of two so the callback can mask the hashed edge PC with
/// `DEFAULT_COV_SIZE - 1` (AFL-style hashed coverage map).
pub const DEFAULT_COV_SIZE: u64 = 0x1_0000;
```

- [ ] **Step 4: Run the test to confirm it passes**

Run: `cargo test -p ignition-devices cov_size`
Expected: PASS.

- [ ] **Step 5: Add the coverage GPA + asserts** in `spike/src/bin/boot.rs`. After the `FUZZ_WIN_GPA` const (line ~980):

```rust
// The coverage region: a host-readable RAM-backed map of 8-bit SanCov counters,
// mapped into the guest just above the input window. Like the window it sits
// below RAM_BASE, so it is outside the dirty-tracked guest-RAM range and never
// rolled back by the dirty reset (spec §6: host-managed pages are reset-exempt).
const FUZZ_COV_GPA: u64 = 0x0940_4000; // FUZZ_WIN_GPA + DEFAULT_WINDOW_SIZE (0x20_0000)
```

In `run_fuzz_mode`, extend the existing const-overlap check block (line ~1029) and the window/RAM assert so the coverage region is validated. Replace the `const { assert!(... control region overlaps the window ...) }` block with:

```rust
    // Region layout is fixed at compile time: ctrl | window | coverage, ascending,
    // non-overlapping, all below RAM_BASE.
    const {
        assert!(
            FUZZ_CTRL_GPA + protocol::CONTROL_SIZE <= FUZZ_WIN_GPA,
            "fuzz control region overlaps the window"
        );
    }
    let cov_size = protocol::DEFAULT_COV_SIZE;
    assert!(
        FUZZ_WIN_GPA + window_size <= FUZZ_COV_GPA,
        "fuzz window [{FUZZ_WIN_GPA:#x}, {:#x}) overlaps the coverage region at {FUZZ_COV_GPA:#x}",
        FUZZ_WIN_GPA + window_size
    );
    assert!(
        FUZZ_COV_GPA + cov_size <= layout::RAM_BASE,
        "fuzz coverage region [{FUZZ_COV_GPA:#x}, {:#x}) must sit below RAM_BASE {:#x}",
        FUZZ_COV_GPA + cov_size,
        layout::RAM_BASE
    );
    assert_eq!(FUZZ_COV_GPA & 0x3FFF, 0, "coverage GPA must be 16 KiB-aligned");
```

Note: `cov_size` is consumed in Task 5 (mapping). With a default 2 MiB window the window-overlap assert holds (`0x0920_4000 + 0x20_0000 = 0x0940_4000 == FUZZ_COV_GPA`). If `--window-mib` pushes the window past `FUZZ_COV_GPA`, this assert fires at runtime — acceptable, since `--window-mib` already warns it diverges from the harness-baked size.

- [ ] **Step 6: Build to confirm it compiles** (the asserts are not yet exercised until Task 5 wires `cov_size`; allow the unused binding for now if the build warns — it is consumed in Task 5).

Run: `cargo build -p ignition-spike`
Expected: compiles (a `cov_size` unused warning is fine and disappears in Task 5).

- [ ] **Step 7: Commit**

```bash
git add crates/devices/src/fuzz/protocol.rs spike/src/bin/boot.rs
git commit -m "fuzz(m2): add coverage-region size constant + GPA layout asserts"
```

---

### Task 2: Coverage observer in the controller

**Files:**
- Modify: `crates/vmm/src/fuzz/controller.rs`

This task adds the pure host-side coverage logic: an accumulated "virgin" map and a function that, given the freshly-read coverage buffer for the input just executed, returns whether new edges were hit. It does not yet wire it into the loop (Task 5) — it is unit-tested in isolation.

- [ ] **Step 1: Write the failing tests** (add to `mod tests` in `controller.rs`):

```rust
    #[test]
    fn coverage_map_reports_new_edges_then_saturates() {
        let mut cm = CoverageMap::new(8);
        // First observation: edges 1 and 4 hit -> new coverage.
        assert!(cm.record(&[0, 5, 0, 0, 2, 0, 0, 0]));
        assert_eq!(cm.covered(), 2);
        // Same edges again (different counts) -> no new coverage.
        assert!(!cm.record(&[0, 1, 0, 0, 9, 0, 0, 0]));
        assert_eq!(cm.covered(), 2);
        // A new edge (index 7) -> new coverage.
        assert!(cm.record(&[0, 0, 0, 0, 0, 0, 0, 3]));
        assert_eq!(cm.covered(), 3);
    }

    #[test]
    fn coverage_map_all_zero_is_not_new() {
        let mut cm = CoverageMap::new(4);
        assert!(!cm.record(&[0, 0, 0, 0]));
        assert_eq!(cm.covered(), 0);
    }
```

- [ ] **Step 2: Run them to confirm they fail**

Run: `cargo test -p ignition-vmm coverage_map`
Expected: FAIL — `cannot find type CoverageMap`.

- [ ] **Step 3: Implement `CoverageMap`** in `controller.rs` (place it above `FuzzController`):

```rust
/// Accumulated edge-coverage map (the host-side "virgin bits"). Each `record`
/// folds one iteration's freshly-read 8-bit counter buffer in: an index that is
/// nonzero now but was never seen before is new coverage. Counts, not just bits,
/// are read from the guest, but only first-touch is tracked — enough for the M2
/// coverage curve and the coverage-guided corpus. (libAFL's bucketed
/// `MaxMapFeedback` is the later, richer replacement.)
pub struct CoverageMap {
    seen: Vec<bool>,
    covered: usize,
}

impl CoverageMap {
    pub fn new(len: usize) -> CoverageMap {
        CoverageMap { seen: vec![false; len], covered: 0 }
    }

    /// Fold `cov` (this iteration's counters) into the accumulated map. Returns
    /// true if any previously-unseen edge was hit. `cov` may be shorter or longer
    /// than the map; only the overlapping prefix is considered.
    pub fn record(&mut self, cov: &[u8]) -> bool {
        let mut new = false;
        let n = cov.len().min(self.seen.len());
        for i in 0..n {
            if cov[i] != 0 && !self.seen[i] {
                self.seen[i] = true;
                self.covered += 1;
                new = true;
            }
        }
        new
    }

    /// Total distinct edges hit across all recorded iterations.
    pub fn covered(&self) -> usize {
        self.covered
    }
}
```

- [ ] **Step 4: Run the tests to confirm they pass**

Run: `cargo test -p ignition-vmm coverage_map`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/vmm/src/fuzz/controller.rs
git commit -m "fuzz(m2): add CoverageMap edge observer (host-side virgin bits)"
```

---

### Task 3: Dirty-set selective restore + reset mode

**Files:**
- Modify: `crates/vmm/src/fuzz/controller.rs`

This task adds the pure selective-restore primitive and a `ResetMode` enum. The live wiring (drain on the vCPU thread, protect calls) lands in Task 5; here we unit-test that restoring a page list touches exactly those pages.

- [ ] **Step 1: Write the failing tests** (add to `mod tests`):

```rust
    #[test]
    fn restore_pages_touches_only_listed_pages() {
        let pg = 16384usize;
        let base = vec![0xAAu8; 4 * pg];
        let mut live = base.clone();
        // Dirty page 1 and page 3.
        for b in &mut live[1 * pg..2 * pg] { *b = 0x55; }
        for b in &mut live[3 * pg..4 * pg] { *b = 0x11; }
        // Also scribble page 2, but DON'T list it: it must stay scribbled (proves
        // we restore only the drained set, not the whole region).
        for b in &mut live[2 * pg..3 * pg] { *b = 0x77; }
        restore_pages(&base, &mut live, &[1, 3], pg);
        assert_eq!(&live[1 * pg..2 * pg], &base[1 * pg..2 * pg], "page 1 restored");
        assert_eq!(&live[3 * pg..4 * pg], &base[3 * pg..4 * pg], "page 3 restored");
        assert!(live[2 * pg..3 * pg].iter().all(|&b| b == 0x77), "page 2 untouched");
    }

    #[test]
    fn restore_pages_clamps_partial_trailing_page() {
        let pg = 16384usize;
        let base = vec![0xAAu8; pg + 100]; // last page is partial (100 bytes)
        let mut live = base.clone();
        live[pg + 50] = 0x55;
        restore_pages(&base, &mut live, &[1], pg);
        assert_eq!(live, base, "partial trailing page restored without overrun");
    }

    #[test]
    fn reset_mode_parses() {
        assert_eq!("full".parse::<ResetMode>().unwrap(), ResetMode::Full);
        assert_eq!("dirty".parse::<ResetMode>().unwrap(), ResetMode::Dirty);
        assert!("bogus".parse::<ResetMode>().is_err());
    }
```

- [ ] **Step 2: Run them to confirm they fail**

Run: `cargo test -p ignition-vmm restore_pages`
Expected: FAIL — `cannot find function restore_pages`.

- [ ] **Step 3: Implement `restore_pages` and `ResetMode`** in `controller.rs` (place `restore_pages` next to `restore_ram`, and `ResetMode` near the top):

```rust
/// Which per-iteration RAM reset to use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResetMode {
    /// v0: memcpy the whole guest RAM from base every iteration. Correct, slow,
    /// no dirty-tracking dependency. Kept for the M2 throughput comparison.
    Full,
    /// v1: restore only guest-dirtied pages (drained from the DirtyTracker) and
    /// re-protect. This is the throughput story.
    Dirty,
}

impl std::str::FromStr for ResetMode {
    type Err = String;
    fn from_str(s: &str) -> Result<ResetMode, String> {
        match s {
            "full" => Ok(ResetMode::Full),
            "dirty" => Ok(ResetMode::Dirty),
            other => Err(format!("unknown reset mode {other:?} (want full|dirty)")),
        }
    }
}

/// Restore only the pages in `pages` (page indices into a region based at offset
/// 0) from `base` to `live`, clamping the last page to the region length. v1 of
/// the spec §6 reset: the dirty set replaces the full-RAM copy. `base`/`live`
/// must be the same length.
pub fn restore_pages(base: &[u8], live: &mut [u8], pages: &[u64], page: usize) {
    debug_assert_eq!(base.len(), live.len(), "base and live RAM must match in size");
    for &p in pages {
        let start = (p as usize) * page;
        if start >= live.len() {
            continue;
        }
        let end = (start + page).min(live.len());
        live[start..end].copy_from_slice(&base[start..end]);
    }
}
```

- [ ] **Step 4: Run the tests to confirm they pass**

Run: `cargo test -p ignition-vmm restore_pages reset_mode`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/vmm/src/fuzz/controller.rs
git commit -m "fuzz(m2): add ResetMode + restore_pages (dirty-set selective restore)"
```

---

### Task 4: Guest coverage instrumentation (harness + header + build)

**Files:**
- Modify: `kimage/build/fuzz-harness/ignition_fuzz.h`
- Modify: `kimage/build/fuzz-harness/harness.c`
- Modify: `kimage/build/build-fuzz-initramfs.sh`
- Modify: `REBUILD-GUEST-ASSETS.md`

This is guest C, built remotely on `artemis2`; there is no local unit test. Verification is "the container builds it and the booted guest reports nonzero coverage" (exercised by the Task 6 gate). Keep the callback uninstrumented and null-guarded.

- [ ] **Step 1: Add coverage defines** to `kimage/build/fuzz-harness/ignition_fuzz.h`, after the `WIN` defines:

```c
#define IGNITION_FUZZ_COV_GPA    0x09404000UL  /* WIN_GPA + WIN_SIZE (0x200000) */
#define IGNITION_FUZZ_COV_SIZE   0x10000UL     /* 64 KiB, 8-bit edge counters */
```

- [ ] **Step 2: Add the coverage callback + mapping** to `kimage/build/fuzz-harness/harness.c`.

Add the coverage pointer near the other globals (after `g_win`):

```c
static volatile uint8_t *g_cov;    /* 8-bit SanCov edge counters (host reads) */
```

Add the callback (mirrors `docs/examples/fuzzing/harness.c`; lives in this uninstrumented TU so it is not itself traced). Place it after the `reg_*`/`doorbell` helpers and before `crash_handler`:

```c
/* SanCov edge callback. target.c is built with -fsanitize-coverage=trace-pc, so
 * this fires once per edge with the return address identifying the edge. We hash
 * it into the shared coverage map (8-bit counters) the host reads after DONE.
 * harness.c is NOT coverage-instrumented, or this would recurse into itself.
 * The null-guard matters: the callback can fire during libc/global init, before
 * main() maps g_cov. */
void __sanitizer_cov_trace_pc(void) {
    if (!g_cov) return;
    uintptr_t pc = (uintptr_t)__builtin_return_address(0);
    g_cov[(pc >> 4) & (IGNITION_FUZZ_COV_SIZE - 1)]++;
}
```

In `main`, after the existing `g_win` mmap and its `MAP_FAILED` check, add the coverage mmap:

```c
    g_cov = mmap(0, IGNITION_FUZZ_COV_SIZE, PROT_READ | PROT_WRITE, MAP_SHARED, fd, IGNITION_FUZZ_COV_GPA);
    if (g_cov == MAP_FAILED) return 3;
```

(Adjust the existing `if (g_ctrl == MAP_FAILED || g_win == MAP_FAILED) return 2;` to remain as-is; the new check uses return code 3.)

- [ ] **Step 3: Add the coverage build flag** in `kimage/build/build-fuzz-initramfs.sh`. The single combined compile currently builds both TUs together; coverage must instrument **only** `target.c`. Split the compile so the callback's TU stays uninstrumented. Replace the `clang -fsanitize=address -O1 -g -I/src /src/target.c /src/harness.c -o /out/root/init` line with:

```sh
  # Instrument ONLY target.c with trace-pc coverage; harness.c (which defines the
  # __sanitizer_cov_trace_pc callback) must stay uninstrumented or the callback
  # recurses. ASan is applied to both; -O1 + the volatile g_sink keep the planted
  # overflow alive.
  clang -fsanitize=address -fsanitize-coverage=trace-pc -O1 -g -I/src -c /src/target.c -o /tmp/target.o
  clang -fsanitize=address -O1 -g -I/src -c /src/harness.c -o /tmp/harness.o
  clang -fsanitize=address -O1 -g /tmp/target.o /tmp/harness.o -o /out/root/init
```

- [ ] **Step 4: Update the build comment block** in the same script — extend the recipe header to mention coverage:

Add a line under the "Compile (M1 target+harness split ...)" comment:

```sh
# M2: target.c is additionally built with -fsanitize-coverage=trace-pc so the
#   harness's __sanitizer_cov_trace_pc callback records edges into the shared
#   coverage region (FUZZ_COV_GPA). Compile target.c and harness.c as separate
#   objects (above) so the callback's TU is NOT itself instrumented.
```

- [ ] **Step 5: Document the rebuild** — in `REBUILD-GUEST-ASSETS.md`, in the "Rebuild the fuzz initramfs" section, add a sentence: the M2 build instruments `target.c` with `-fsanitize-coverage=trace-pc` and adds a third `/dev/mem` mapping for the coverage region at `0x09404000` (64 KiB); no new device nodes are needed (it reuses `/dev/mem`).

- [ ] **Step 6: Rebuild on artemis2 and pull the artifact.** This runs the remote container build (the host has no Docker). Suggest the user run it if remote access needs their credentials; otherwise run via the established artemis2 workflow in `REBUILD-GUEST-ASSETS.md`.

Run (per REBUILD-GUEST-ASSETS.md): build `build-fuzz-initramfs.sh` on artemis2, pull to `kimage/out/fuzz-initramfs.cpio`.
Expected: container build succeeds; `ldd init` still shows only `ld-musl-aarch64.so.1` + `libgcc_s.so.1`; cpio written.

- [ ] **Step 7: Commit**

```bash
git add kimage/build/fuzz-harness/ignition_fuzz.h kimage/build/fuzz-harness/harness.c kimage/build/build-fuzz-initramfs.sh REBUILD-GUEST-ASSETS.md
git commit -m "fuzz(m2): instrument guest target with trace-pc coverage into shared region"
```

---

### Task 5: Wire coverage + dirty reset into the controller and loop

**Files:**
- Modify: `crates/vmm/src/fuzz/controller.rs`
- Modify: `crates/vmm/src/vstate/vcpu_manager.rs`
- Modify: `spike/src/bin/boot.rs`

This is the integration task: extend `FuzzController` to own the coverage region + dirty tracker + reset mode, fold coverage into the corpus, do the dirty reset, emit periodic stats; arm dirty tracking in `fuzz_loop`; map the coverage region and pass everything through in `run_fuzz_mode`.

- [ ] **Step 1: Extend `FuzzController`** in `controller.rs`.

Add imports at the top (the controller now drives the protect calls on the vCPU thread):

```rust
use std::time::Instant;

use crate::dirty::{DirtyTracker, PAGE};
```

Add fields to the `FuzzController` struct (after `window_len`):

```rust
    cov_ptr: *mut u8,
    cov_len: usize,
    coverage: CoverageMap,
    corpus: Vec<Vec<u8>>,
    last_input: Vec<u8>,
    reset_mode: ResetMode,
    dirty: Option<DirtyTracker>,
    ram_base_gpa: u64,
    started: Option<Instant>,
    last_dirty_pages: u64,
```

Widen `FuzzController::new` to accept the coverage region, reset mode, dirty tracker, and RAM base GPA. New signature and body (replace the existing `new`):

```rust
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        ram: (*mut u8, usize),
        window: (*mut u8, usize),
        cov: (*mut u8, usize),
        ram_base_gpa: u64,
        reset_mode: ResetMode,
        dirty: Option<DirtyTracker>,
        seeds: Vec<Vec<u8>>,
        replay: Option<Vec<u8>>,
        seed_rng: u64,
        solutions_dir: PathBuf,
    ) -> FuzzController {
        let corpus = if seeds.is_empty() { vec![vec![0u8; 1]] } else { seeds.clone() };
        FuzzController {
            base_ram: Vec::new(),
            base_state: None,
            ram_ptr: ram.0,
            ram_len: ram.1,
            window_ptr: window.0,
            window_len: window.1,
            cov_ptr: cov.0,
            cov_len: cov.1,
            coverage: CoverageMap::new(cov.1),
            corpus,
            last_input: Vec::new(),
            reset_mode,
            dirty,
            ram_base_gpa,
            rng: Rng::new(seed_rng),
            seeds: if seeds.is_empty() { vec![vec![0u8; 1]] } else { seeds },
            seed_idx: 0,
            replay,
            solutions_dir,
            crash_count: 0,
            iterations: 0,
            captured: false,
            started: None,
            last_dirty_pages: 0,
        }
    }
```

(Keep the existing `seeds`/`seed_idx` fields; the corpus is the live, growing set and the seeds remain for reference. `prepare_next_input` below switches to the corpus.)

Add a `cov` accessor next to `window`:

```rust
    fn cov(&mut self) -> &mut [u8] {
        // SAFETY: see struct doc; single-threaded, mapping outlives the run.
        unsafe { std::slice::from_raw_parts_mut(self.cov_ptr, self.cov_len) }
    }
```

- [ ] **Step 2: Rewrite `prepare_next_input`** to mutate from the growing corpus, remember the input, and zero coverage before the guest runs it:

```rust
    /// Pick a corpus entry, mutate it into the shared window, zero the coverage
    /// map (so the next run's counters are fresh), remember the bytes for a
    /// possible corpus add, and return the input length.
    fn prepare_next_input(&mut self) -> u32 {
        // Zero coverage before the guest accumulates this iteration's edges.
        for b in self.cov().iter_mut() {
            *b = 0;
        }
        if let Some(fixed) = self.replay.clone() {
            self.last_input = fixed.clone();
            return replay_into(&fixed, self.window());
        }
        let pick = self.rng.below(self.corpus.len());
        let seed = self.corpus[pick].clone();
        let max = self.window_len;
        let input = mutate(&seed, &mut self.rng, max);
        let n = input.len().min(self.window_len);
        self.window()[..n].copy_from_slice(&input[..n]);
        self.last_input = input[..n].to_vec();
        n as u32
    }
```

- [ ] **Step 3: Add coverage folding + corpus growth + stats** — a helper called on every `DONE`/`CRASH` before reset:

```rust
    /// Read this iteration's coverage, fold it into the accumulated map, and keep
    /// the input in the corpus if it reached a new edge (coverage-guided growth;
    /// the homegrown analogue of libAFL's MaxMapFeedback). Emits a periodic stats
    /// line (parsed by the M2 gate). Must run before `reset` rolls back RAM — but
    /// the coverage region is reset-exempt, so the ordering is for clarity only.
    fn observe_and_stats(&mut self) {
        let cov_snapshot = self.cov().to_vec();
        let new_cov = self.coverage.record(&cov_snapshot);
        if new_cov && !self.replay.is_some() && self.corpus.len() < 4096 {
            self.corpus.push(self.last_input.clone());
        }
        // Periodic stats: every 2000 iterations and on the first.
        if self.iterations == 1 || self.iterations % 2000 == 0 {
            let elapsed = self.started.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
            let eps = if elapsed > 0.0 { self.iterations as f64 / elapsed } else { 0.0 };
            log::info!(
                "fuzz: iters={} execs/sec={:.0} cov={} corpus={} dirty_pages={} reset={:?}",
                self.iterations, eps, self.coverage.covered(), self.corpus.len(),
                self.last_dirty_pages, self.reset_mode
            );
            // Also to stderr so the gate can parse without RUST_LOG configured.
            eprintln!(
                "fuzz: iters={} execs/sec={:.0} cov={} corpus={} dirty_pages={} reset={:?}",
                self.iterations, eps, self.coverage.covered(), self.corpus.len(),
                self.last_dirty_pages, self.reset_mode
            );
        }
    }
```

- [ ] **Step 4: Update `capture`, `on_done`, `on_crash`, `reset`** to drive the new paths.

`capture` — start the clock, and in `Dirty` mode write-protect all guest RAM so subsequent guest writes fault and are tracked:

```rust
    pub fn capture(&mut self, vcpu: &HvfVcpu) -> Result<u32, ignition_hvf::Error> {
        let live = self.live_ram().to_vec();
        self.base_ram = live;
        self.base_state = Some(vcpu.save_state()?);
        self.captured = true;
        self.started = Some(Instant::now());
        if self.reset_mode == ResetMode::Dirty {
            // Drop WRITE on all guest RAM; first write per page faults (DirtyFault),
            // gets logged + re-granted by the fuzz loop. The window/cov regions are
            // mapped below RAM_BASE, outside this range, so they stay writable.
            ignition_hvf::vm_protect_memory(
                self.ram_base_gpa,
                self.ram_len as u64,
                (ignition_hvf::bindings::HV_MEMORY_READ | ignition_hvf::bindings::HV_MEMORY_EXEC) as u64,
            )?;
        }
        Ok(self.prepare_next_input())
    }
```

`on_done` — observe coverage, then prepare + reset:

```rust
    pub fn on_done(&mut self, vcpu: &mut HvfVcpu) -> Result<u32, ignition_hvf::Error> {
        self.iterations += 1;
        self.observe_and_stats();
        let len = self.prepare_next_input();
        self.reset(vcpu)?;
        Ok(len)
    }
```

`on_crash` — record the solution, then same as done (coverage still folded):

```rust
    pub fn on_crash(&mut self, vcpu: &mut HvfVcpu, crash_code: u32, input_len: u32) -> Result<u32, ignition_hvf::Error> {
        let n = (input_len as usize).min(self.window_len);
        let input = self.window()[..n].to_vec();
        if let Err(e) = write_solution(&self.solutions_dir, self.crash_count, &input, crash_code) {
            log::warn!("failed to write fuzz solution: {e}");
        }
        self.crash_count += 1;
        log::info!("fuzz: CRASH captured (code={crash_code}, len={n}), solutions={}", self.crash_count);
        self.iterations += 1;
        self.observe_and_stats();
        let len = self.prepare_next_input();
        self.reset(vcpu)?;
        Ok(len)
    }
```

`reset` — branch on mode; in `Dirty` mode drain the tracker, restore only those pages, re-protect all RAM:

```rust
    fn reset(&mut self, vcpu: &mut HvfVcpu) -> Result<(), ignition_hvf::Error> {
        let base = std::mem::take(&mut self.base_ram);
        match self.reset_mode {
            ResetMode::Full => {
                restore_ram(&base, self.live_ram());
            }
            ResetMode::Dirty => {
                let pages = self.dirty.as_ref().expect("dirty mode requires a tracker").drain();
                self.last_dirty_pages = pages.len() as u64;
                restore_pages(&base, self.live_ram(), &pages, PAGE);
                // Re-protect ALL RAM (drop WRITE) so the next iteration starts clean
                // and re-arms the write-protect faults. drain() already cleared the
                // bitmap.
                ignition_hvf::vm_protect_memory(
                    self.ram_base_gpa,
                    self.ram_len as u64,
                    (ignition_hvf::bindings::HV_MEMORY_READ | ignition_hvf::bindings::HV_MEMORY_EXEC) as u64,
                )?;
            }
        }
        self.base_ram = base;
        let state = self.base_state.as_ref().expect("reset before capture");
        vcpu.restore_state(state)?;
        vcpu.clear_pending_advance();
        Ok(())
    }
```

- [ ] **Step 5: Arm dirty tracking in `fuzz_loop`** (`crates/vmm/src/vstate/vcpu_manager.rs`). At the top of `fuzz_loop`, before the `loop {`, mirror `run_loop`'s arming:

```rust
        let vcpus: Arc<dyn Vcpus> = Arc::new(NoIrqVcpus);
        // Arm dirty-page tracking (ResetMode::Dirty). The window is set before the
        // guest runs; RAM is only write-protected later, at the snapshot point
        // (FuzzController::capture), so boot-time writes don't fault. With no
        // dirty config (full-copy reset) this is a no-op and DirtyFault never fires.
        let dirty = self.dirty.clone();
        if let Some(cfg) = &dirty {
            vcpu.set_dirty_window(cfg.base, cfg.size);
        }
```

Add a `DirtyFault` arm to the `fuzz_loop` match (mirror `run_loop:427`), before the `VcpuExit::Shutdown` arm:

```rust
                VcpuExit::DirtyFault(pa) => {
                    if let Some(cfg) = &dirty {
                        cfg.tracker.mark(pa);
                        let page_base = pa & !((PAGE as u64) - 1);
                        ignition_hvf::vm_protect_memory(
                            page_base,
                            PAGE as u64,
                            (ignition_hvf::bindings::HV_MEMORY_READ
                                | ignition_hvf::bindings::HV_MEMORY_WRITE
                                | ignition_hvf::bindings::HV_MEMORY_EXEC) as u64,
                        )
                        .expect("dirty-tracking re-grant of guest page failed");
                    } else {
                        log::warn!("fuzz DirtyFault at {pa:#x} but dirty tracking is not armed");
                    }
                }
```

(`DirtyTracker` and `PAGE` are already imported at the top of `vcpu_manager.rs`.)

- [ ] **Step 6: Map the coverage region + wire the controller** in `spike/src/bin/boot.rs` `run_fuzz_mode`.

After the window `map_memory` block (line ~1151), add the coverage region mmap + map (same shape as the window):

```rust
    // The shared COVERAGE region: host anon mmap mapped into the guest at
    // FUZZ_COV_GPA. The guest's trace-pc callback writes 8-bit edge counters here;
    // the host zeroes it before each input and reads it after DONE. Like the
    // window it lives below RAM_BASE, so the dirty reset never rolls it back.
    let cov_host = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            cov_size as usize,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_ANON | libc::MAP_PRIVATE,
            -1,
            0,
        )
    };
    if cov_host == libc::MAP_FAILED {
        return Err(io::Error::other("mmap of fuzz coverage region failed"));
    }
    vm.map_memory(cov_host as u64, FUZZ_COV_GPA, cov_size)
        .map_err(|e| io::Error::other(format!("hv_vm_map coverage: {e}")))?;
```

Build the dirty tracker (when `reset_mode == Dirty`) and arm the manager. Replace the controller construction + `VcpuManager::new`/`run_fuzz` block (lines ~1183-1204) with:

```rust
    // Dirty tracker for ResetMode::Dirty: covers all guest RAM, base = RAM_BASE.
    let dirty_tracker: Option<DirtyTracker> = if reset_mode == ResetMode::Dirty {
        Some(DirtyTracker::new(layout::RAM_BASE, ram_size))
    } else {
        None
    };

    let controller = FuzzController::new(
        (host as *mut u8, ram_size as usize),
        (win_host as *mut u8, window_size as usize),
        (cov_host as *mut u8, cov_size as usize),
        layout::RAM_BASE,
        reset_mode,
        dirty_tracker.clone(),
        seeds,
        replay,
        0xF1FA_5EED,
        solutions_dir.to_path_buf(),
    );

    let mut manager = VcpuManager::new(1, bus);
    if let Some(tracker) = &dirty_tracker {
        manager.set_dirty_config(DirtyConfig {
            base: layout::RAM_BASE,
            size: ram_size,
            tracker: tracker.clone(),
        });
    }
    manager
        .run_fuzz(
            entry,
            fdt_addr,
            FUZZ_CTRL_GPA + protocol::reg::DOORBELL,
            FUZZ_CTRL_GPA,
            fuzz_dev,
            controller,
        )
        .map_err(|e| io::Error::other(format!("run_fuzz: {e}")))?;
    Ok(())
```

Add the needed imports to `boot.rs` if not present: `ResetMode` from `ignition_vmm::fuzz::controller`, and `DirtyTracker` from `ignition_vmm` (check the existing `use` for `DirtyTracker` — the diff-snapshot path already imports it; `DirtyConfig` is imported at line 41). Add `use ignition_vmm::fuzz::controller::ResetMode;` next to the existing `FuzzController` import.

- [ ] **Step 7: Add the `--reset` flag + plumb `reset_mode`** through `run_fuzz_mode`.

In arg parsing (near the other fuzz flags, line ~538), add:

```rust
            "--reset" => {
                let v = args.next().expect("--reset needs full|dirty");
                reset_mode = v.parse().expect("--reset must be full|dirty");
            }
```

Declare the default near the other fuzz locals (line ~510): `let mut reset_mode = ignition_vmm::fuzz::controller::ResetMode::Dirty;`. Add `reset_mode` to the `run_fuzz_mode` signature and the call site (line ~622). Update both usage strings (lines ~605, ~646) to include `[--reset full|dirty]`.

- [ ] **Step 8: Build, sign, and run the full test suite**

Run:
```bash
cargo build -p ignition-spike && scripts/sign.sh target/debug/boot
cargo test --workspace
```
Expected: builds; all tests pass (the new controller unit tests from Tasks 2-3 included).

- [ ] **Step 9: Commit**

```bash
git add crates/vmm/src/fuzz/controller.rs crates/vmm/src/vstate/vcpu_manager.rs spike/src/bin/boot.rs
git commit -m "fuzz(m2): wire coverage observer + dirty-page reset into the fuzz loop"
```

---

### Task 6: M2 gate — coverage growth, crash rediscovery, throughput jump

**Files:**
- Create: `scripts/fuzz_m2_test.py`
- Modify: `docs/superpowers/specs/2026-06-14-snapshot-fuzzing-demonstrator-design.md`

This gate proves the three M2 properties: (a) coverage grows and the corpus expands beyond the seed; (b) the planted crash is still rediscovered and deterministically replayable (M1 carried forward, now via the dirty reset); (c) dirty-reset execs/sec exceeds full-copy execs/sec on equal wall-clock.

- [ ] **Step 1: Write the gate script** `scripts/fuzz_m2_test.py`:

```python
#!/usr/bin/env python3
"""M2 gate: coverage feedback + dirty-page reset.

(a) Coverage grows: the periodic "cov=" stat increases above its first reading
    and the corpus expands past the single seed.
(b) The planted overflow is still rediscovered (via the dirty reset) and the
    saved input replays deterministically.
(c) Throughput: dirty-reset execs/sec > full-copy execs/sec on equal wall-clock.

Parses the controller's periodic stderr line:
    fuzz: iters=.. execs/sec=.. cov=.. corpus=.. dirty_pages=.. reset=..
"""
import glob, os, re, signal, subprocess, sys, tempfile, time

BOOT = os.environ.get("BOOT_BIN", "target/debug/boot")
KERNEL = os.environ.get("FUZZ_KERNEL", "kimage/out/Image")
INITRAMFS = os.environ.get("FUZZ_INITRAMFS", "kimage/out/fuzz-initramfs.cpio")
SEED = bytes([ord('F'), ord('U'), ord('Z'), 1, ord('C'), 16, 0] + list(range(1, 21)))
STAT = re.compile(r"fuzz: iters=(\d+) execs/sec=([\d.]+) cov=(\d+) corpus=(\d+)")

def run(extra, sol, timeout, stop_on_crash):
    cmd = [BOOT, "--fuzz", "--mem", "96", "--initramfs", INITRAMFS,
           "--solutions", sol] + extra + [KERNEL]
    p = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    deadline = time.time() + timeout
    found = None
    while time.time() < deadline:
        if stop_on_crash and glob.glob(os.path.join(sol, "crash-*.bin")):
            found = sorted(glob.glob(os.path.join(sol, "crash-*.bin")))[0]
            break
        if p.poll() is not None:
            break
        time.sleep(0.5)
    try:
        p.send_signal(signal.SIGINT); p.wait(timeout=5)
    except Exception:
        p.kill()
    out = p.stdout.read().decode(errors="replace") if p.stdout else ""
    stats = STAT.findall(out)
    return found, out, stats

def eps_of(stats):
    # last parsed execs/sec
    return float(stats[-1][1]) if stats else 0.0

def main():
    for x in (BOOT, KERNEL, INITRAMFS):
        if not os.path.exists(x):
            print(f"missing artifact: {x}", file=sys.stderr); sys.exit(2)
    d = tempfile.mkdtemp(prefix="fuzz-m2-")
    seed = os.path.join(d, "seed.bin"); open(seed, "wb").write(SEED)

    # (a)+(c-dirty): coverage growth + crash rediscovery on the dirty reset.
    sol_d = os.path.join(d, "dirty")
    found, out, stats = run(["--reset", "dirty", "--seed", seed], sol_d, 90, True)
    if not stats:
        print(out); print("FAIL: no fuzz stats line parsed", file=sys.stderr); sys.exit(1)
    cov_first = int(stats[0][3-1]); cov_last = int(stats[-1][3-1])  # cov= is group 3
    corpus_last = int(stats[-1][4-1])
    if cov_last <= cov_first:
        print(out); print(f"FAIL: coverage did not grow ({cov_first}->{cov_last})", file=sys.stderr); sys.exit(1)
    if corpus_last <= 1:
        print(out); print(f"FAIL: corpus did not grow past seed ({corpus_last})", file=sys.stderr); sys.exit(1)
    print(f"PASS(a): coverage grew {cov_first}->{cov_last}, corpus={corpus_last}")
    if not found:
        print(out); print("FAIL: planted overflow not rediscovered (dirty reset)", file=sys.stderr); sys.exit(1)
    print("PASS(b1): rediscovered planted overflow ->", found)

    # (b2): replay determinism (dirty reset).
    sol_r = os.path.join(d, "replay")
    found2, out2, _ = run(["--reset", "dirty", "--replay", found], sol_r, 30, True)
    if not found2:
        print(out2); print("FAIL: replayed crash did not reproduce", file=sys.stderr); sys.exit(1)
    print("PASS(b2): replayed crash reproduced ->", found2)

    # (c): throughput jump. Run each mode for a fixed wall-clock (no crash stop)
    # and compare steady-state execs/sec.
    sol_f = os.path.join(d, "full")
    _, outf, sf = run(["--reset", "full", "--seed", seed], sol_f, 25, False)
    sol_d2 = os.path.join(d, "dirty2")
    _, outd, sd = run(["--reset", "dirty", "--seed", seed], sol_d2, 25, False)
    eps_full, eps_dirty = eps_of(sf), eps_of(sd)
    print(f"throughput: full={eps_full:.0f} execs/sec, dirty={eps_dirty:.0f} execs/sec")
    if not (eps_dirty > eps_full > 0):
        print(outf); print(outd)
        print(f"FAIL: dirty reset not faster ({eps_dirty:.0f} <= {eps_full:.0f})", file=sys.stderr); sys.exit(1)
    print(f"PASS(c): dirty reset faster ({eps_dirty:.0f} > {eps_full:.0f} execs/sec)")
    print("PASS: M2 gate")

if __name__ == "__main__":
    main()
```

Note on the regex group indexing: `STAT` captures `(iters, execs/sec, cov, corpus)` as groups 1-4; the script reads `stats[i][2]` for cov and `stats[i][3]` for corpus (0-based tuple from `findall`). Fix the indices to plain `stats[0][2]`, `stats[-1][2]`, `stats[-1][3]` when implementing — do not ship the `3-1`/`4-1` placeholders.

- [ ] **Step 2: Make it executable and run the gate**

Run:
```bash
chmod +x scripts/fuzz_m2_test.py
python3 scripts/fuzz_m2_test.py
```
Expected:
```
PASS(a): coverage grew <a>-><b>, corpus=<n>
PASS(b1): rediscovered planted overflow -> ...
PASS(b2): replayed crash reproduced -> ...
throughput: full=<x> execs/sec, dirty=<y> execs/sec
PASS(c): dirty reset faster (<y> > <x> execs/sec)
PASS: M2 gate
```
(Requires the M2 initramfs from Task 4 and a signed boot binary from Task 5. If coverage is flat, the trace-pc build flag did not take — re-check Task 4 step 3.)

- [ ] **Step 3: Check the M2 box** in `docs/superpowers/specs/2026-06-14-snapshot-fuzzing-demonstrator-design.md` line ~207: change `- [ ] **M2` to `- [x] **M2`.

- [ ] **Step 4: Commit**

```bash
git add scripts/fuzz_m2_test.py docs/superpowers/specs/2026-06-14-snapshot-fuzzing-demonstrator-design.md
git commit -m "fuzz(m2): coverage + dirty-reset gate; mark M2 complete"
```

---

## Self-Review

**Spec coverage (§8 target build, §6 reset, §9 host brain, §12 risks, M2 milestone):**
- §8 "SanCov counter section in the shared window" + "harness reads exactly INPUT_LEN bytes" → Task 4 (trace-pc into the coverage region; harness already reads `INPUT_LEN`). Implementation note: this plan uses `trace-pc` (callback-into-shared-region, matching the committed `docs/examples/fuzzing` twin) rather than `inline-8bit-counters`, because inline counters live in the binary's `.bss` (inside dirty-tracked guest RAM, rolled back each reset) and cannot be relocated to a `/dev/mem` mapping at runtime. The callback approach puts the counters in the host-readable, reset-exempt region directly — same observable behavior, simpler, deterministic. Flagged here so the spec reviewer does not read it as a deviation-without-cause.
- §6 reset v1 (dirty pages via `hv_vm_protect`, host-managed window exempt) → Tasks 3 + 5. The window/coverage exemption is by-layout (below `RAM_BASE`), not by an explicit exempt-list.
- §9 host brain: keeps the homegrown mutator, adds a `MaxMapFeedback`-analogue (coverage-guided corpus growth). libAFL is explicitly deferred (scope decision).
- §12 risks: window-exemption correctness is structurally guaranteed (separate sub-`RAM_BASE` mapping) + asserted at boot (Task 1 layout asserts); the coverage-flat failure mode is caught by gate (a); snapshot-point drift unchanged from M0/M1.
- M2 milestone gate ("coverage curve stabilizes; execs/sec jumps") → Task 6 (a) coverage grows + (c) execs/sec dirty > full.

**Placeholder scan:** one intentional placeholder is called out explicitly — the `3-1`/`4-1` regex indices in Task 6 step 1, with the fix stated in the following note. No other TBDs.

**Type consistency:** `ResetMode`, `CoverageMap`, `restore_pages`, `FuzzController::new` signature, `DirtyTracker`, `DirtyConfig`, `vm_protect_memory`, `set_dirty_window`, `set_dirty_config` are used consistently across Tasks 1-6 and match the existing codebase symbols verified during planning.

**Granularity:** each task is test-first where the logic is pure Rust (Tasks 1-3, 5); the guest-C and integration tasks (4, 6) are verified by the in-VMM gate, mirroring how M1 handled the un-unit-testable guest build.
