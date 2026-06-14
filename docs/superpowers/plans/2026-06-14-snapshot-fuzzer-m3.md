# Snapshot-Fuzzer M3 (Benchmark) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add benchmark instrumentation to the in-VMM snapshot fuzzer (reset-latency p50/p99 decomposed into page-copy vs register-restore, dirty-set-size distribution, execs/sec, coverage curve, time-to-rediscover) and run it against a real **libpng-current** target, emitting `docs/fuzzing-demonstrator-result.md` + a benchmark section in `docs/benchmarks.md`.

**Architecture:** Metrics live in a new `crates/vmm/src/fuzz/metrics.rs` accumulator owned by `FuzzController`. The controller's `reset()` brackets the page-copy and register-restore regions with `Instant` timers; `observe_and_stats` samples the coverage curve; `on_crash` records time-to-first-crash. On clean shutdown the loop writes a machine-parseable metrics file. A second guest build adds a real libpng decoder target (SanCov coverage **only**, no ASan — this is the spec §12 recommendation: a coverage-only build for the throughput benchmark, the existing ASan build kept for the deterministic bug-finding number). A Python harness (`scripts/fuzz_m3_bench.py`) runs both builds, parses the metrics file, and assembles the numbers.

**Tech Stack:** Rust (`ignition-vmm`), C harness (clang + SanCov on arm64-musl in alpine via Docker on `artemis2`), libpng 1.6.43 + zlib 1.3.1 built from source, Python 3 benchmark/gate, Hypervisor.framework execution on the host Mac.

**Methodology note (carry into the result doc):** The libpng benchmark build is **SanCov-only** (`-fsanitize-coverage=trace-pc`, no `-fsanitize=address`). Spec §12 flags ASan shadow as a dirty-set inflator; measuring throughput / reset-latency / dirty-set on a coverage-only build isolates the snapshot machinery from ASan shadow churn. The planted-CVE correctness number (time-to-rediscover) stays on the existing ASan synthetic target. The Linux/KVM cross-check (Nyx/AFL++) is **deferred** out of M3 and recorded as a follow-up.

---

## File Structure

- `crates/vmm/src/fuzz/metrics.rs` (**new**) — the `Metrics` accumulator + `percentile` helper + report serialization. Pure, unit-tested.
- `crates/vmm/src/fuzz/mod.rs` — register `pub mod metrics;`.
- `crates/vmm/src/fuzz/controller.rs` — own a `Metrics`, instrument `reset()`/`observe_and_stats`/`on_crash`, add `metrics_out` field + `write_metrics()`; widen `new()`.
- `crates/vmm/src/vstate/vcpu_manager.rs` — call `controller.write_metrics()` at the loop's clean-exit points.
- `spike/src/bin/boot.rs` — add `--metrics <path>` flag, thread it through `run_fuzz_mode` + `FuzzController::new`.
- `kimage/build/fuzz-harness/target_png.c` (**new**) — libpng decode target (`target_parse`).
- `kimage/build/build-fuzz-initramfs.sh` — add a `libpng` target mode that builds zlib + libpng with SanCov and links `fuzz-initramfs-libpng.cpio`.
- `scripts/fuzz_m3_bench.py` (**new**) — M3 benchmark/gate.
- `docs/fuzzing-demonstrator-result.md` (**new**) — narrative result doc.
- `docs/benchmarks.md` — append the M3 fuzzer benchmark section.
- `REBUILD-GUEST-ASSETS.md` — document the libpng initramfs build.
- `docs/superpowers/specs/2026-06-14-snapshot-fuzzing-demonstrator-design.md` — mark M3 `[x]`.

---

### Task 1: Metrics accumulator (pure, TDD)

**Files:**
- Create: `crates/vmm/src/fuzz/metrics.rs`
- Modify: `crates/vmm/src/fuzz/mod.rs`

- [ ] **Step 1: Write the failing tests**

Create `crates/vmm/src/fuzz/metrics.rs`:

```rust
//! M3 benchmark accumulator: per-iteration reset-latency (split into page-copy
//! vs register-restore), dirty-set-size samples, the coverage-over-time curve,
//! and time-to-first-crash. Sample vectors are capped so a multi-hour run cannot
//! grow unbounded; once capped, `capped` is set and later samples are dropped
//! (steady state is reached quickly, so the retained prefix is representative —
//! noted in the result doc).

/// Hard cap on retained per-iteration samples (≈8 MiB per u32 vector).
pub const SAMPLE_CAP: usize = 2_000_000;

#[derive(Default)]
pub struct Metrics {
    reset_total_us: Vec<u32>,
    restore_us: Vec<u32>,
    regs_us: Vec<u32>,
    dirty_pages: Vec<u32>,
    cov_curve: Vec<(f64, u64)>, // (elapsed_secs, distinct_edges)
    first_crash_secs: Option<f64>,
    capped: bool,
}

/// Nearest-rank percentile of an already-sorted slice. `p` in [0.0, 1.0].
/// Empty slice -> 0.
pub fn percentile(sorted: &[u32], p: f64) -> u32 {
    if sorted.is_empty() {
        return 0;
    }
    let p = p.clamp(0.0, 1.0);
    // nearest-rank: rank = ceil(p * N), 1-based; index = rank-1, clamped.
    let rank = (p * sorted.len() as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(sorted.len() - 1);
    sorted[idx]
}

impl Metrics {
    pub fn new() -> Metrics {
        Metrics::default()
    }

    fn push_capped(v: &mut Vec<u32>, x: u32, capped: &mut bool) {
        if v.len() >= SAMPLE_CAP {
            *capped = true;
            return;
        }
        v.push(x);
    }

    pub fn record_reset(&mut self, total_us: u32, restore_us: u32, regs_us: u32) {
        let mut capped = self.capped;
        Metrics::push_capped(&mut self.reset_total_us, total_us, &mut capped);
        Metrics::push_capped(&mut self.restore_us, restore_us, &mut capped);
        Metrics::push_capped(&mut self.regs_us, regs_us, &mut capped);
        self.capped = capped;
    }

    pub fn record_dirty(&mut self, pages: u32) {
        let mut capped = self.capped;
        Metrics::push_capped(&mut self.dirty_pages, pages, &mut capped);
        self.capped = capped;
    }

    pub fn sample_coverage(&mut self, elapsed_secs: f64, edges: u64) {
        self.cov_curve.push((elapsed_secs, edges));
    }

    /// Record time-to-first-crash; only the first call sticks.
    pub fn record_first_crash(&mut self, elapsed_secs: f64) {
        if self.first_crash_secs.is_none() {
            self.first_crash_secs = Some(elapsed_secs);
        }
    }

    pub fn capped(&self) -> bool {
        self.capped
    }
    pub fn first_crash_secs(&self) -> Option<f64> {
        self.first_crash_secs
    }

    /// Render the machine-parseable report block consumed by
    /// `scripts/fuzz_m3_bench.py`. `iterations`/`elapsed_secs` come from the
    /// controller (the loop counter and the run clock).
    pub fn report(&self, iterations: u64, elapsed_secs: f64) -> String {
        let mut rt = self.reset_total_us.clone();
        let mut rs = self.restore_us.clone();
        let mut rg = self.regs_us.clone();
        let mut dp = self.dirty_pages.clone();
        rt.sort_unstable();
        rs.sort_unstable();
        rg.sort_unstable();
        dp.sort_unstable();
        let eps = if elapsed_secs > 0.0 { iterations as f64 / elapsed_secs } else { 0.0 };
        let cov_final = self.cov_curve.last().map(|&(_, e)| e).unwrap_or(0);
        let crash = match self.first_crash_secs {
            Some(s) => format!("{s:.3}"),
            None => "none".to_string(),
        };
        let mut out = String::new();
        out.push_str(&format!("metric iters={iterations} elapsed_s={elapsed_secs:.3} execs_per_sec={eps:.0}\n"));
        out.push_str(&format!(
            "metric reset_us_p50={} reset_us_p99={} reset_us_max={}\n",
            percentile(&rt, 0.50), percentile(&rt, 0.99), rt.last().copied().unwrap_or(0)
        ));
        out.push_str(&format!(
            "metric restore_us_p50={} restore_us_p99={}\n",
            percentile(&rs, 0.50), percentile(&rs, 0.99)
        ));
        out.push_str(&format!(
            "metric regs_us_p50={} regs_us_p99={}\n",
            percentile(&rg, 0.50), percentile(&rg, 0.99)
        ));
        out.push_str(&format!(
            "metric dirty_pages_p50={} dirty_pages_p99={} dirty_pages_max={}\n",
            percentile(&dp, 0.50), percentile(&dp, 0.99), dp.last().copied().unwrap_or(0)
        ));
        out.push_str(&format!("metric coverage_final={cov_final} time_to_crash_s={crash} capped={}\n", self.capped));
        for &(t, e) in &self.cov_curve {
            out.push_str(&format!("covsample t={t:.3} edges={e}\n"));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_nearest_rank() {
        let v: Vec<u32> = (1..=100).collect(); // 1..100 sorted
        assert_eq!(percentile(&v, 0.50), 50);
        assert_eq!(percentile(&v, 0.99), 99);
        assert_eq!(percentile(&v, 1.0), 100);
        assert_eq!(percentile(&v, 0.0), 1);
    }

    #[test]
    fn percentile_empty_is_zero() {
        assert_eq!(percentile(&[], 0.5), 0);
    }

    #[test]
    fn first_crash_only_first_sticks() {
        let mut m = Metrics::new();
        m.record_first_crash(1.5);
        m.record_first_crash(9.9);
        assert_eq!(m.first_crash_secs(), Some(1.5));
    }

    #[test]
    fn report_has_all_metric_keys() {
        let mut m = Metrics::new();
        m.record_reset(100, 70, 30);
        m.record_reset(200, 150, 50);
        m.record_dirty(4);
        m.record_dirty(8);
        m.sample_coverage(0.0, 2);
        m.sample_coverage(2.0, 10);
        let r = m.report(5000, 5.0);
        for key in [
            "execs_per_sec=", "reset_us_p50=", "restore_us_p50=", "regs_us_p50=",
            "dirty_pages_p50=", "coverage_final=", "time_to_crash_s=none",
        ] {
            assert!(r.contains(key), "missing {key} in:\n{r}");
        }
        assert!(r.contains("covsample t=2.000 edges=10"));
    }

    #[test]
    fn cap_sets_flag_and_stops_growth() {
        let mut m = Metrics::new();
        for _ in 0..(SAMPLE_CAP + 10) {
            m.record_dirty(1);
        }
        assert!(m.capped());
    }
}
```

- [ ] **Step 2: Register the module — edit `crates/vmm/src/fuzz/mod.rs`**

```rust
//! Host-side fuzzer brain and per-iteration reset (M0).

pub mod controller;
pub mod metrics;
```

- [ ] **Step 3: Run the tests to verify they pass**

Run: `cargo test -p ignition-vmm fuzz::metrics`
Expected: PASS (5 tests: percentile_nearest_rank, percentile_empty_is_zero, first_crash_only_first_sticks, report_has_all_metric_keys, cap_sets_flag_and_stops_growth).

Note: `SAMPLE_CAP` is large; `cap_sets_flag_and_stops_growth` pushes ~2M u32 (~8 MiB) — fast, but if it is noticeably slow on the CI box, that is acceptable for a once-per-build test.

- [ ] **Step 4: Commit**

```bash
git add crates/vmm/src/fuzz/metrics.rs crates/vmm/src/fuzz/mod.rs
git commit -m "fuzz(m3): metrics accumulator (reset-latency split, dirty-set, coverage curve)"
```

---

### Task 2: Wire metrics into the controller

**Files:**
- Modify: `crates/vmm/src/fuzz/controller.rs`

- [ ] **Step 1: Import Metrics and add fields**

At the top of `controller.rs`, after the existing `use crate::dirty::{DirtyTracker, PAGE};`:

```rust
use crate::fuzz::metrics::Metrics;
```

Add two fields to `struct FuzzController` (place them next to `last_dirty_pages`):

```rust
    metrics: Metrics,
    metrics_out: Option<PathBuf>,
```

- [ ] **Step 2: Widen `new()` to accept `metrics_out`**

Change the `new` signature to add a final parameter `metrics_out: Option<PathBuf>` (after `solutions_dir`), and initialize the two new fields. The full updated signature and the changed tail of the initializer:

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
        metrics_out: Option<PathBuf>,
    ) -> FuzzController {
        let corpus = if seeds.is_empty() { vec![vec![0u8; 1]] } else { seeds };
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
            replay,
            solutions_dir,
            crash_count: 0,
            iterations: 0,
            captured: false,
            started: None,
            last_dirty_pages: 0,
            metrics: Metrics::new(),
            metrics_out,
        }
    }
```

- [ ] **Step 3: Sample the coverage curve in `observe_and_stats`**

In `observe_and_stats`, inside the existing `if self.iterations == 1 || self.iterations % 2000 == 0 {` block, after computing `elapsed`/`eps` and before the `log::info!`, add the coverage-curve sample:

```rust
            self.metrics.sample_coverage(elapsed, self.coverage.covered() as u64);
```

- [ ] **Step 4: Record time-to-first-crash in `on_crash`**

In `on_crash`, immediately after `self.crash_count += 1;`, record the first-crash time relative to the run clock:

```rust
        if let Some(t) = self.started {
            self.metrics.record_first_crash(t.elapsed().as_secs_f64());
        }
```

(`record_first_crash` keeps only the first value, so calling it on every crash is correct.)

- [ ] **Step 5: Instrument `reset()` with the latency split**

Replace the body of `fn reset` with the timed version (timers bracket the RAM restore region and the register restore region separately; dirty-set size is recorded in the Dirty arm):

```rust
    fn reset(&mut self, vcpu: &mut HvfVcpu) -> Result<(), ignition_hvf::Error> {
        let base = std::mem::take(&mut self.base_ram);
        let t_restore = Instant::now();
        match self.reset_mode {
            ResetMode::Full => {
                restore_ram(&base, self.live_ram());
            }
            ResetMode::Dirty => {
                // Ordering note: the guest vCPU is paused for the entire duration of
                // reset() (single vCPU thread; reset runs between vcpu.run() calls), so
                // no guest instruction executes between the re-protect below and the
                // register restore — the ordering is immaterial, there is no fault window.
                let pages = self.dirty.as_ref().expect("dirty mode requires a tracker").drain();
                self.last_dirty_pages = pages.len() as u64;
                self.metrics.record_dirty(pages.len() as u32);
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
        let restore_us = t_restore.elapsed().as_micros().min(u32::MAX as u128) as u32;
        self.base_ram = base;
        let state = self.base_state.as_ref().expect("reset before capture");
        let t_regs = Instant::now();
        vcpu.restore_state(state)?;
        vcpu.clear_pending_advance();
        let regs_us = t_regs.elapsed().as_micros().min(u32::MAX as u128) as u32;
        self.metrics.record_reset(restore_us.saturating_add(regs_us), restore_us, regs_us);
        Ok(())
    }
```

- [ ] **Step 6: Add `write_metrics()`**

Add a public method to `impl FuzzController` (place it after `reset`):

```rust
    /// Write the accumulated benchmark metrics to `metrics_out` (if set) as the
    /// machine-parseable block parsed by `scripts/fuzz_m3_bench.py`, and echo a
    /// one-line summary to stderr. Called once on clean shutdown. Safe to call
    /// before any iteration ran (emits zeroes).
    pub fn write_metrics(&self) {
        let elapsed = self.started.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
        let report = self.metrics.report(self.iterations, elapsed);
        if let Some(path) = &self.metrics_out {
            if let Err(e) = std::fs::write(path, &report) {
                log::warn!("failed to write fuzz metrics to {}: {e}", path.display());
            }
        }
        // First line is the per-run summary; handy even without --metrics.
        if let Some(first) = report.lines().next() {
            eprintln!("fuzz-metrics {first}");
        }
    }
```

- [ ] **Step 7: Verify it compiles and existing tests still pass**

Run: `cargo test -p ignition-vmm fuzz::`
Expected: PASS (Task 1 metrics tests + all existing controller tests). The `new()` callers in `boot.rs` will not compile yet — that is fixed in Task 3; build only the lib here:

Run: `cargo build -p ignition-vmm`
Expected: builds clean.

- [ ] **Step 8: Commit**

```bash
git add crates/vmm/src/fuzz/controller.rs
git commit -m "fuzz(m3): instrument reset latency split + dirty-set + coverage curve + time-to-crash"
```

---

### Task 3: `--metrics` flag and shutdown emit

**Files:**
- Modify: `spike/src/bin/boot.rs`
- Modify: `crates/vmm/src/vstate/vcpu_manager.rs`

- [ ] **Step 1: Emit metrics at the fuzz loop's clean-exit points**

In `crates/vmm/src/vstate/vcpu_manager.rs`, `fn fuzz_loop`, call `controller.write_metrics()` at each clean exit:

The loop-top shutdown check becomes:

```rust
            if self.shutdown.load(Ordering::Acquire) {
                controller.write_metrics();
                return Ok(());
            }
```

The `VcpuExit::Shutdown` arm becomes:

```rust
                VcpuExit::Shutdown => {
                    controller.write_metrics();
                    self.request_shutdown();
                    return Ok(());
                }
```

The `VcpuExit::Canceled` arm becomes:

```rust
                VcpuExit::Canceled => {
                    controller.write_metrics();
                    return Ok(());
                }
```

- [ ] **Step 2: Add the `--metrics` CLI flag in `boot.rs`**

In the fuzz-flag declarations (near line 517, by `reset_mode`), add:

```rust
    let mut metrics_path: Option<PathBuf> = None;
```

In the arg-parse `match` (after the `"--reset"` arm), add:

```rust
            "--metrics" => {
                metrics_path = Some(PathBuf::from(it.next().expect("--metrics needs a path")));
            }
```

Update the usage string (the `eprintln!("usage: ...")` for fuzz mode) to include `[--metrics <path>]`:

```rust
            eprintln!("usage: {} --fuzz --initramfs <cpio> [--solutions <dir>] [--seed <path>] [--replay <file>] [--window-mib N] [--reset full|dirty] [--metrics <path>] [--mem MiB] <kernel-Image>", args[0]);
```

- [ ] **Step 3: Thread `metrics_path` through `run_fuzz_mode`**

Change the call site (near line 628):

```rust
        match run_fuzz_mode(&kernel_path, &initramfs, &solutions, seed_path.as_deref(), replay, window_size, ram_size, reset_mode, metrics_path) {
```

Change the `run_fuzz_mode` signature (near line 1004) to add the parameter:

```rust
fn run_fuzz_mode(
    kernel_path: &Path,
    initramfs_path: &Path,
    solutions_dir: &Path,
    seed_path: Option<&Path>,
    replay: Option<Vec<u8>>,
    window_size: u64,
    ram_size: u64,
    reset_mode: ResetMode,
    metrics_path: Option<PathBuf>,
) -> io::Result<()> {
```

- [ ] **Step 4: Pass it into `FuzzController::new`**

At the `FuzzController::new(...)` call (near line 1235), add the final argument:

```rust
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
        metrics_path,
    );
```

- [ ] **Step 5: Build and re-sign**

Run:
```bash
cargo build -p ignition-spike --bin boot && scripts/sign.sh target/debug/boot
```
Expected: builds clean; sign.sh reports success.

- [ ] **Step 6: Run the full workspace test suite**

Run: `cargo test`
Expected: PASS (the prior 159 tests + the 5 new metrics tests = 164).

- [ ] **Step 7: Smoke-test `--metrics` on the existing synthetic initramfs**

Run (writes a metrics file; SIGINT after a few seconds via timeout):
```bash
printf 'FUZ\x01C\x10\x00\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c\x0d\x0e\x0f\x10\x11\x12\x13' > /tmp/m3seed.bin
( target/debug/boot --fuzz --mem 96 --initramfs kimage/out/fuzz-initramfs.cpio \
    --solutions /tmp/m3smoke --reset dirty --seed /tmp/m3seed.bin \
    --metrics /tmp/m3metrics.txt kimage/out/Image & P=$!; sleep 8; kill -INT $P; wait $P ) ; cat /tmp/m3metrics.txt
```
Expected: `/tmp/m3metrics.txt` exists and begins with a `metric iters=... execs_per_sec=...` line, followed by `reset_us_p50=`, `dirty_pages_p50=`, `coverage_final=`, and `covsample` lines. (This synthetic build has the planted bug, so `time_to_crash_s=` is likely a number, not `none`.)

- [ ] **Step 8: Commit**

```bash
git add spike/src/bin/boot.rs crates/vmm/src/vstate/vcpu_manager.rs
git commit -m "fuzz(m3): --metrics flag; write metrics file on clean shutdown"
```

---

### Task 4: libpng decode target (C harness)

**Files:**
- Create: `kimage/build/fuzz-harness/target_png.c`

- [ ] **Step 1: Write the libpng target**

`target_parse` keeps the exact signature the harness expects (`void target_parse(const uint8_t *, unsigned long)`); it decodes the window bytes with libpng's simplified read API.

Create `kimage/build/fuzz-harness/target_png.c`:

```c
/* target_png.c — real-target fuzz body for the M3 benchmark: decode the window
 * bytes as a PNG via libpng's simplified read API. Built with SanCov
 * (-fsanitize-coverage=trace-pc) but WITHOUT AddressSanitizer (the M3 throughput
 * build is coverage-only per spec §12; the ASan build uses the synthetic
 * target.c). Same `target_parse` signature the harness calls. No planted bug —
 * this measures the snapshot machinery against a real decoder. */
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <png.h>

void target_parse(const uint8_t *d, unsigned long n) {
    png_image image;
    memset(&image, 0, sizeof image);
    image.version = PNG_IMAGE_VERSION;

    if (!png_image_begin_read_from_memory(&image, d, (size_t)n)) {
        return;  /* not a PNG / header rejected */
    }
    image.format = PNG_FORMAT_RGBA;

    /* Bound the allocation so a malformed huge dimension does not OOM the guest;
     * 64 MiB ceiling keeps us inside the 96-128 MiB guest RAM. */
    png_alloc_size_t sz = PNG_IMAGE_SIZE(image);
    if (sz == 0 || sz > (64u << 20)) {
        png_image_free(&image);
        return;
    }
    void *buf = malloc((size_t)sz);
    if (!buf) {
        png_image_free(&image);
        return;
    }
    png_image_finish_read(&image, NULL /*background*/, buf, 0 /*row_stride*/, NULL /*colormap*/);
    free(buf);
}
```

- [ ] **Step 2: Verify it is syntactically self-consistent (no compile yet — libpng headers are remote)**

This file is compiled remotely in Task 5. Locally, confirm the function name/signature matches `harness.c`'s extern:

Run: `grep -n "void target_parse" kimage/build/fuzz-harness/harness.c kimage/build/fuzz-harness/target_png.c`
Expected: both show `void target_parse(const uint8_t *... , unsigned long ...)` (the harness declares it `extern`; target_png.c defines it).

- [ ] **Step 3: Commit**

```bash
git add kimage/build/fuzz-harness/target_png.c
git commit -m "fuzz(m3): libpng simplified-read decode target (SanCov, no ASan)"
```

---

### Task 5: libpng initramfs build pipeline

**Files:**
- Modify: `kimage/build/build-fuzz-initramfs.sh`
- Modify: `REBUILD-GUEST-ASSETS.md`

This task produces a second initramfs (`fuzz-initramfs-libpng.cpio`) by building zlib + libpng from source with SanCov in the arm64 alpine container. The existing synthetic build path is preserved unchanged as the default.

- [ ] **Step 1: Add a target-mode dispatch to the build script**

Edit `kimage/build/build-fuzz-initramfs.sh`. Immediately after the `set -euo pipefail` line and the `STAGE`/`OUT`/`HERE` setup (i.e., after `HERE="$(cd "$(dirname "$0")" && pwd)"`), insert the target selector:

```bash
# Target selector: `synthetic` (default, ASan chunk-parser with the planted
# overflow — the correctness build) or `libpng` (SanCov-only real PNG decoder —
# the M3 throughput build). Output name differs so both can coexist in out/.
TARGET="${1:-synthetic}"
case "$TARGET" in
  synthetic) OUT_NAME="fuzz-initramfs.cpio" ;;
  libpng)    OUT_NAME="fuzz-initramfs-libpng.cpio" ;;
  *) echo "usage: $0 [synthetic|libpng]" >&2; exit 2 ;;
esac
```

- [ ] **Step 2: Guard the existing synthetic container run, then add the libpng run**

The existing `docker run ... fuzzinit_build ...` block is the synthetic build. Wrap it so it only runs for the synthetic target, and add a libpng branch. Replace the existing `docker rm -f fuzzinit_build ...` line and the whole `docker run --platform linux/arm64 --name fuzzinit_build ... '` block with:

```bash
docker rm -f fuzzinit_build >/dev/null 2>&1 || true

if [ "$TARGET" = "synthetic" ]; then
docker run --platform linux/arm64 --name fuzzinit_build \
  -v "$HERE/fuzz-harness:/src:ro" \
  alpine:3.19 sh -euxc '
  apk add --no-cache clang compiler-rt musl-dev
  mkdir -p /out/root/lib /out/root/usr/lib /out/root/dev /out/root/proc /out/root/sys
  # Instrument ONLY target.c with trace-pc coverage; harness.c (which defines the
  # __sanitizer_cov_trace_pc callback) must stay uninstrumented or the callback
  # recurses. ASan is applied to both; -O1 + the volatile g_sink keep the planted
  # overflow alive.
  clang -fsanitize=address -fsanitize-coverage=trace-pc -O1 -g -I/src -c /src/target.c -o /tmp/target.o
  clang -fsanitize=address -O1 -g -I/src -c /src/harness.c -o /tmp/harness.o
  clang -fsanitize=address -O1 -g /tmp/target.o /tmp/harness.o -o /out/root/init
  echo "=== ldd /out/root/init ==="
  ldd /out/root/init || true
  cp -L /lib/ld-musl-aarch64.so.1 /out/root/lib/
  cp -L /usr/lib/libgcc_s.so.1 /out/root/usr/lib/
  cd /out/root
  mknod -m 600 dev/mem c 1 1
  mknod -m 622 dev/console c 5 1
  mknod -m 666 dev/null c 1 3
  find . | cpio -o -H newc > /out/fuzz-initramfs.cpio
'
else
# libpng SanCov-only build: compile zlib + libpng from source with trace-pc, link
# target_png.c (instrumented) + harness.c (NOT instrumented; defines the SanCov
# callback) against the static libs. No ASan: this is the throughput build
# (spec §12). Crashes (if any) surface via the harness signal handlers.
ZLIB_VER=1.3.1
PNG_VER=1.6.43
docker run --platform linux/arm64 --name fuzzinit_build \
  -v "$HERE/fuzz-harness:/src:ro" \
  -e ZLIB_VER="$ZLIB_VER" -e PNG_VER="$PNG_VER" \
  alpine:3.19 sh -euxc '
  apk add --no-cache clang compiler-rt musl-dev make wget tar
  mkdir -p /out/root/lib /out/root/usr/lib /out/root/dev /out/root/proc /out/root/sys /build
  cd /build
  COV="-fsanitize-coverage=trace-pc -O1 -g -fPIC"
  # --- zlib (static, SanCov) ---
  wget -O zlib.tar.gz "https://zlib.net/zlib-${ZLIB_VER}.tar.gz"
  tar xf zlib.tar.gz
  cd "zlib-${ZLIB_VER}"
  CC=clang CFLAGS="$COV" ./configure --static
  make -j"$(nproc)" libz.a
  ZDIR="$PWD"
  cd /build
  # --- libpng (static, SanCov, against our zlib) ---
  wget -O libpng.tar.gz "https://download.sourceforge.net/libpng/libpng-${PNG_VER}.tar.gz"
  tar xf libpng.tar.gz
  cd "libpng-${PNG_VER}"
  CC=clang CFLAGS="$COV" CPPFLAGS="-I$ZDIR" LDFLAGS="-L$ZDIR" \
    ./configure --disable-shared --enable-static --host=aarch64-alpine-linux-musl
  make -j"$(nproc)" libpng16.la
  PNGDIR="$PWD"
  PNGLIB="$PNGDIR/.libs/libpng16.a"
  PNGINC="$PNGDIR"   # png.h, pnglibconf.h live in the source/build root
  cd /build
  # --- target_png.c (instrumented) + harness.c (NOT) -> static link ---
  clang -fsanitize-coverage=trace-pc -O1 -g -I"$PNGINC" -I"$ZDIR" -c /src/target_png.c -o target_png.o
  clang -O1 -g -I/src -c /src/harness.c -o harness.o
  clang -O1 -g target_png.o harness.o "$PNGLIB" "$ZDIR/libz.a" -lm -o /out/root/init
  echo "=== ldd /out/root/init ==="
  ldd /out/root/init || true
  cp -L /lib/ld-musl-aarch64.so.1 /out/root/lib/
  cp -L /usr/lib/libgcc_s.so.1 /out/root/usr/lib/
  cd /out/root
  mknod -m 600 dev/mem c 1 1
  mknod -m 622 dev/console c 5 1
  mknod -m 666 dev/null c 1 3
  find . | cpio -o -H newc > /out/fuzz-initramfs.cpio
'
fi
```

(Note: inside the container the artifact is always written to `/out/fuzz-initramfs.cpio`; the host-side copy below renames it to `$OUT_NAME`.)

- [ ] **Step 3: Make the host-side copy use `$OUT_NAME`**

Replace the trailing copy-out block (`DEST="$OUT/fuzz-initramfs.cpio"` … `echo "wrote $DEST"`) with:

```bash
# out/ may be root-owned (left by the kernel build); fall back to the user-owned
# stage dir so `docker cp` (runs as the host user) can always write the artifact.
DEST="$OUT/$OUT_NAME"
if ! ( : >"$DEST" ) 2>/dev/null; then
  DEST="$STAGE/$OUT_NAME"
fi
rm -f "$DEST"
docker cp fuzzinit_build:/out/fuzz-initramfs.cpio "$DEST"
docker rm fuzzinit_build >/dev/null
echo "wrote $DEST"
```

- [ ] **Step 4: Build the libpng initramfs on artemis2 and pull it back**

> If the harness build host (`artemis2`) is unreachable from this session, mark this step BLOCKED and surface it — the remaining steps depend on the artifact. Do not fabricate the artifact.

Run:
```bash
cd kimage
ssh artemis2 'mkdir -p ~/kbuild/fuzz-harness'
scp build/fuzz-harness/harness.c build/fuzz-harness/ignition_fuzz.h build/fuzz-harness/target_png.c artemis2:~/kbuild/fuzz-harness/
scp build/build-fuzz-initramfs.sh artemis2:~/kbuild/
ssh artemis2 'cd ~/kbuild && chmod +x build-fuzz-initramfs.sh && ./build-fuzz-initramfs.sh libpng'
scp artemis2:'~/kbuild/out/fuzz-initramfs-libpng.cpio' out/fuzz-initramfs-libpng.cpio 2>/dev/null \
  || scp artemis2:'~/kbuild/fuzz-initramfs-libpng.cpio' out/fuzz-initramfs-libpng.cpio
head -c 6 out/fuzz-initramfs-libpng.cpio
```
Expected: `head -c 6` prints `070701` (newc cpio magic). The build log should show `ldd /out/root/init` listing only `ld-musl-aarch64.so.1` and `libgcc_s.so.1` (libpng + zlib are static).

> **Build-failure fallbacks (try in order, document whichever you use):**
> 1. If `./configure` for libpng cannot find zlib, confirm `CPPFLAGS=-I$ZDIR` and `LDFLAGS=-L$ZDIR` point at the zlib build dir (it holds the generated `zconf.h`).
> 2. If `make libpng16.la` fails on the `.la` target, use `make -j"$(nproc)"` (default target) and locate the archive with `find . -name 'libpng16.a'`; set `PNGLIB` to that path.
> 3. If `--host=aarch64-alpine-linux-musl` triggers a cross-compile guess that breaks the native arm64 build, drop the `--host=` flag (the container is genuinely arm64 via binfmt, so a native configure is correct).
> 4. If a sourceforge mirror is flaky, fetch libpng from the GitHub release: `https://github.com/pnggroup/libpng/archive/refs/tags/v${PNG_VER}.tar.gz` (note: the GitHub tarball needs `autogen.sh`; prefer the release tarball with the prebuilt `configure`).

- [ ] **Step 5: Document the libpng build in `REBUILD-GUEST-ASSETS.md`**

In the "Rebuild the fuzz initramfs" section, after the existing M2 paragraph, append:

```markdown
### libpng benchmark initramfs (M3)

The M3 throughput benchmark uses a second initramfs whose `/init` decodes PNGs
with **libpng-current** (1.6.43) + zlib (1.3.1), both built from source with
SanCov coverage (`-fsanitize-coverage=trace-pc`) and **no AddressSanitizer**
(spec §12: a coverage-only build isolates throughput/reset/dirty-set from ASan
shadow churn; the synthetic ASan build keeps the deterministic bug-finding
number). `target_png.c` replaces `target.c`; `harness.c` is unchanged.

```bash
cd kimage
ssh artemis2 'mkdir -p ~/kbuild/fuzz-harness'
scp build/fuzz-harness/harness.c build/fuzz-harness/ignition_fuzz.h build/fuzz-harness/target_png.c artemis2:~/kbuild/fuzz-harness/
scp build/build-fuzz-initramfs.sh artemis2:~/kbuild/
ssh artemis2 'cd ~/kbuild && chmod +x build-fuzz-initramfs.sh && ./build-fuzz-initramfs.sh libpng'
scp artemis2:'~/kbuild/out/fuzz-initramfs-libpng.cpio' out/fuzz-initramfs-libpng.cpio 2>/dev/null \
  || scp artemis2:'~/kbuild/fuzz-initramfs-libpng.cpio' out/fuzz-initramfs-libpng.cpio
head -c 6 out/fuzz-initramfs-libpng.cpio   # expect 070701
```

The synthetic ASan build is still the default: `./build-fuzz-initramfs.sh`
(no arg) writes `fuzz-initramfs.cpio` as before.
```

- [ ] **Step 6: Commit**

```bash
git add kimage/build/build-fuzz-initramfs.sh REBUILD-GUEST-ASSETS.md
git commit -m "fuzz(m3): libpng SanCov-only initramfs build target"
```

---

### Task 6: M3 benchmark harness / gate

**Files:**
- Create: `scripts/fuzz_m3_bench.py`

- [ ] **Step 1: Write the benchmark harness**

Create `scripts/fuzz_m3_bench.py`:

```python
#!/usr/bin/env python3
"""M3 benchmark: run the snapshot fuzzer against real libpng-current and capture
the benchmark metrics, plus the deterministic time-to-rediscover number on the
synthetic ASan target.

Runs (all single-core, fixed wall-clock, SIGINT to stop):
  1. libpng / dirty reset  -> execs/sec, reset-latency p50/p99 (page-copy vs
     register split), dirty-set-size distribution, coverage curve.
  2. libpng / full reset   -> execs/sec, for the dirty-vs-full speedup.
  3. synthetic / dirty     -> time-to-rediscover the planted CVE (correctness).

Parses the controller's metrics file (written on clean shutdown via --metrics):
  metric iters=.. elapsed_s=.. execs_per_sec=..
  metric reset_us_p50=.. reset_us_p99=.. reset_us_max=..
  metric restore_us_p50=.. restore_us_p99=..
  metric regs_us_p50=.. regs_us_p99=..
  metric dirty_pages_p50=.. dirty_pages_p99=.. dirty_pages_max=..
  metric coverage_final=.. time_to_crash_s=.. capped=..
  covsample t=.. edges=..

Gate (asserts the machinery produced usable numbers; it does NOT assert specific
latencies, which are host-dependent):
  - libpng dirty run: execs_per_sec > 0, coverage_final > 0, a dirty_pages_p50
    value present, reset_us_p50 present.
  - dirty execs/sec > full execs/sec (the snapshot speedup holds on a real target).
  - synthetic run: time_to_crash_s is a number (planted CVE rediscovered).
"""
import os, re, signal, subprocess, sys, tempfile, time

BOOT = os.environ.get("BOOT_BIN", "target/debug/boot")
KERNEL = os.environ.get("FUZZ_KERNEL", "kimage/out/Image")
PNG_INITRAMFS = os.environ.get("FUZZ_INITRAMFS_LIBPNG", "kimage/out/fuzz-initramfs-libpng.cpio")
SYN_INITRAMFS = os.environ.get("FUZZ_INITRAMFS", "kimage/out/fuzz-initramfs.cpio")
DURATION = float(os.environ.get("M3_DURATION", "60"))
MEM = os.environ.get("M3_MEM", "128")

# A valid 1x1 RGBA PNG seed so libpng gets past the signature/IHDR and the
# decoder body is exercised from iteration 1 (coverage starts nonzero, grows).
PNG_SEED = bytes.fromhex(
    "89504e470d0a1a0a0000000d49484452000000010000000108060000001f15c4"
    "890000000d4944415478da6364f8cf000000ff00ff a2 a25e0000000049454e44ae426082".replace(" ", "")
)

# Synthetic seed: near-boundary 'C' chunk (len == 16); a byte bump overflows.
SYN_SEED = bytes([ord('F'), ord('U'), ord('Z'), 1, ord('C'), 16, 0] + list(range(1, 21)))

METRIC = re.compile(r"^metric (.+)$", re.M)

def parse_metrics(path):
    """Flatten all 'metric k=v ...' lines into a dict of str->str."""
    d = {}
    if not os.path.exists(path):
        return d
    with open(path) as f:
        text = f.read()
    for line in METRIC.findall(text):
        for tok in line.split():
            if "=" in tok:
                k, v = tok.split("=", 1)
                d[k] = v
    return d

def run(initramfs, reset, seed_bytes, duration, metrics_path, sols):
    seed = sols + ".seed"
    with open(seed, "wb") as f:
        f.write(seed_bytes)
    logf = open(sols + ".log", "w+b")
    cmd = [BOOT, "--fuzz", "--mem", MEM, "--initramfs", initramfs,
           "--solutions", sols, "--reset", reset, "--seed", seed,
           "--metrics", metrics_path, KERNEL]
    p = subprocess.Popen(cmd, stdout=logf, stderr=subprocess.STDOUT)
    deadline = time.time() + duration
    while time.time() < deadline and p.poll() is None:
        time.sleep(0.5)
    # SIGINT -> the loop hits the shutdown check, writes metrics, exits cleanly.
    try:
        p.send_signal(signal.SIGINT); p.wait(timeout=10)
    except Exception:
        p.kill()
    logf.close()
    return parse_metrics(metrics_path)

def main():
    for x in (BOOT, KERNEL, PNG_INITRAMFS, SYN_INITRAMFS):
        if not os.path.exists(x):
            print(f"missing artifact: {x}", file=sys.stderr); sys.exit(2)
    d = tempfile.mkdtemp(prefix="fuzz-m3-")

    print(f"[1/3] libpng / dirty reset ({DURATION:.0f}s) ...")
    md = run(PNG_INITRAMFS, "dirty", PNG_SEED, DURATION,
             os.path.join(d, "png_dirty.txt"), os.path.join(d, "png_dirty"))
    print(f"[2/3] libpng / full reset ({DURATION:.0f}s) ...")
    mf = run(PNG_INITRAMFS, "full", PNG_SEED, DURATION,
             os.path.join(d, "png_full.txt"), os.path.join(d, "png_full"))
    print(f"[3/3] synthetic / dirty reset (time-to-rediscover, {DURATION:.0f}s) ...")
    ms = run(SYN_INITRAMFS, "dirty", SYN_SEED, DURATION,
             os.path.join(d, "syn_dirty.txt"), os.path.join(d, "syn_dirty"))

    def num(m, k, default=0.0):
        try:
            return float(m.get(k, default))
        except ValueError:
            return default

    eps_dirty = num(md, "execs_per_sec")
    eps_full = num(mf, "execs_per_sec")
    cov = num(md, "coverage_final")
    rp50, rp99 = num(md, "reset_us_p50"), num(md, "reset_us_p99")
    sp50 = num(md, "restore_us_p50"); gp50 = num(md, "regs_us_p50")
    dp50, dp99, dmax = num(md, "dirty_pages_p50"), num(md, "dirty_pages_p99"), num(md, "dirty_pages_max")
    ttc = ms.get("time_to_crash_s", "none")

    print("\n=== M3 benchmark ===")
    print(f"libpng dirty: {eps_dirty:.0f} execs/sec | coverage={cov:.0f} edges")
    print(f"libpng full : {eps_full:.0f} execs/sec")
    print(f"reset latency (dirty): p50={rp50:.0f}us p99={rp99:.0f}us  (page-copy p50={sp50:.0f}us, regs p50={gp50:.0f}us)")
    print(f"dirty-set size: p50={dp50:.0f} p99={dp99:.0f} max={dmax:.0f} pages (16 KiB each)")
    print(f"time-to-rediscover planted CVE (synthetic): {ttc} s")

    # --- gate ---
    fail = []
    if not (eps_dirty > 0): fail.append(f"libpng dirty execs/sec not positive ({eps_dirty})")
    if not (cov > 0): fail.append(f"libpng coverage did not register ({cov})")
    if "reset_us_p50" not in md: fail.append("reset latency p50 missing")
    if "dirty_pages_p50" not in md: fail.append("dirty-set distribution missing")
    if not (eps_dirty > eps_full > 0): fail.append(f"dirty not faster than full ({eps_dirty:.0f} vs {eps_full:.0f})")
    if ttc == "none": fail.append("synthetic run did not rediscover the planted CVE")
    if fail:
        for f in fail: print("FAIL:", f, file=sys.stderr)
        sys.exit(1)
    print("PASS: M3 benchmark gate")

if __name__ == "__main__":
    main()
```

- [ ] **Step 2: Fix the PNG_SEED bytes to a known-valid 1x1 PNG**

The hex above is a placeholder shape with spaces; replace `PNG_SEED` with a verified valid 1×1 PNG. Generate the canonical bytes locally and paste them in (no Python image libs needed at runtime — embed the literal):

Run:
```bash
python3 - <<'PY'
import struct, zlib, binascii
def chunk(typ, data):
    c = typ + data
    return struct.pack(">I", len(data)) + c + struct.pack(">I", zlib.crc32(c) & 0xffffffff)
sig = b"\x89PNG\r\n\x1a\n"
ihdr = chunk(b"IHDR", struct.pack(">IIBBBBB", 1, 1, 8, 6, 0, 0, 0))  # 1x1, 8-bit, RGBA
raw = b"\x00" + b"\xff\x00\x00\xff"  # one filtered scanline: filter 0 + RGBA pixel
idat = chunk(b"IDAT", zlib.compress(raw))
iend = chunk(b"IEND", b"")
png = sig + ihdr + idat + iend
print(binascii.hexlify(png).decode())
PY
```
Copy the printed hex string and set `PNG_SEED = bytes.fromhex("<that hex>")` in the script (single line, no spaces). Then sanity-check it decodes:
```bash
python3 - <<'PY'
import sys
sys.path.insert(0, "scripts")
# re-import the constant
import importlib.util
spec = importlib.util.spec_from_file_location("m3", "scripts/fuzz_m3_bench.py")
# Avoid running main(): just read PNG_SEED by exec of the assignment line.
src = open("scripts/fuzz_m3_bench.py").read()
ns = {}
for line in src.splitlines():
    if line.startswith("PNG_SEED ="):
        exec(line, ns)
data = ns["PNG_SEED"]
assert data[:8] == b"\x89PNG\r\n\x1a\n", "bad signature"
print("PNG_SEED ok, %d bytes" % len(data))
PY
```
Expected: `PNG_SEED ok, NN bytes` (around 70 bytes). This guarantees the seed is a real PNG so libpng exercises the decode path.

- [ ] **Step 3: Make the script executable**

Run: `chmod +x scripts/fuzz_m3_bench.py`

- [ ] **Step 4: Run the benchmark gate (real HVF + real artifacts)**

> Requires `kimage/out/fuzz-initramfs-libpng.cpio` from Task 5 and a signed `target/debug/boot`. If either is missing, mark BLOCKED.

Run (short duration for the gate; the result-doc run in Task 7 uses the full 60s):
```bash
M3_DURATION=20 python3 scripts/fuzz_m3_bench.py
```
Expected: prints the `=== M3 benchmark ===` block with positive `execs/sec`, nonzero coverage, reset-latency p50/p99 (with the page-copy vs regs split), a dirty-set distribution, a `time-to-rediscover` number, and `PASS: M3 benchmark gate`.

- [ ] **Step 5: Commit**

```bash
git add scripts/fuzz_m3_bench.py
git commit -m "fuzz(m3): benchmark harness/gate (libpng throughput + reset/dirty metrics + time-to-rediscover)"
```

---

### Task 7: Result docs

**Files:**
- Create: `docs/fuzzing-demonstrator-result.md`
- Modify: `docs/benchmarks.md`
- Modify: `docs/superpowers/specs/2026-06-14-snapshot-fuzzing-demonstrator-design.md`

- [ ] **Step 1: Capture the full-duration benchmark numbers**

Run the 60s benchmark and save the output verbatim — these numbers populate the docs:
```bash
M3_DURATION=60 python3 scripts/fuzz_m3_bench.py | tee /tmp/m3-result.txt
```
Expected: `PASS: M3 benchmark gate`. Keep `/tmp/m3-result.txt`; transcribe its `=== M3 benchmark ===` values into the tables below (use the actual numbers from the run, not invented figures).

- [ ] **Step 2: Write `docs/fuzzing-demonstrator-result.md`**

Create the file with this structure, filling the bracketed `«…»` values from `/tmp/m3-result.txt`:

```markdown
# Snapshot-fuzzing demonstrator — result (M3)

In-VMM snapshot fuzzer for `ignition` (Firecracker-modeled microVM on Apple
Hypervisor.framework). The fuzzer parks the guest at a parse entry, injects
inputs into a shared window, runs the target, and resets the guest to the
snapshot every iteration via `hv_vm_protect` dirty-page tracking — all without
leaving the VMM. This is the M3 benchmark: real **libpng-current** as the
target, single core.

Date: 2026-06-14. Host: Apple Silicon, macOS 26.5. Guest: aarch64, «MEM» MiB,
single vCPU, 16 KiB page granule. Target: libpng «PNG_VER» + zlib «ZLIB_VER»,
built with SanCov edge coverage, no AddressSanitizer (see Methodology).

## Throughput and reset

| Metric | libpng (dirty reset) | libpng (full-copy reset) |
|--------|---------------------:|-------------------------:|
| Steady-state execs/sec | «eps_dirty» | «eps_full» |
| Reset latency p50 | «reset_us_p50» µs | — |
| Reset latency p99 | «reset_us_p99» µs | — |
| — page-copy p50 | «restore_us_p50» µs | — |
| — register-restore p50 | «regs_us_p50» µs | — |

Dirty reset is «eps_dirty/eps_full»× the full-copy reset on the same target.

## Dirty-set size (pages dirtied per iteration, 16 KiB each)

| p50 | p99 | max |
|----:|----:|----:|
| «dirty_pages_p50» | «dirty_pages_p99» | «dirty_pages_max» |

The dirty set is what the reset copies back; it explains the page-copy latency
above and feeds the diff-snapshot work (`docs/diff-snapshot-benchmarks.md`).

## Coverage

Distinct edges hit: «coverage_final» (SanCov `trace-pc`, hashed into the
reset-exempt coverage window). The coverage-over-time curve is the `covsample`
series in the metrics file (`--metrics`).

## Correctness (deterministic)

Time-to-rediscover the planted heap overflow (synthetic ASan target,
`CVE`-shaped chunk parser): «ttc» s from an empty/seed corpus, deterministically
replayable from the saved input (`--replay`). This is the M1 correctness number,
re-measured here as the deterministic anchor alongside the throughput numbers.

## Methodology

- **Coverage-only libpng build.** The throughput/reset/dirty-set numbers are from
  a SanCov-only libpng build (no ASan). Per the design's §12 risk note, ASan
  shadow (1/8 of the working set) joins the dirty set and inflates reset; a
  coverage-only build isolates the snapshot machinery. The deterministic
  bug-finding number uses the separate ASan build.
- **Single core, steady state.** execs/sec is measured after warm-up over a fixed
  wall-clock window; SIGINT triggers a clean metrics flush.
- **Reproduce:** `M3_DURATION=60 python3 scripts/fuzz_m3_bench.py` (needs a signed
  `boot`, `kimage/out/Image`, and both fuzz initramfs images — see
  `REBUILD-GUEST-ASSETS.md`).

## Deferred

- **Linux/KVM cross-check.** A side-by-side vs AFL++ persistent / Nyx on the same
  libpng target (spec §11) is deferred; it needs a KVM host and an equivalent
  harness. Tracked as an M3 follow-up.
```

- [ ] **Step 3: Append the M3 section to `docs/benchmarks.md`**

At the end of `docs/benchmarks.md`, append:

```markdown

---

## Snapshot fuzzer (M3) — libpng, single core

In-VMM snapshot fuzzer (`boot --fuzz`), `hv_vm_protect` dirty-page reset, target
= libpng «PNG_VER» (SanCov, no ASan). Full write-up + methodology:
`docs/fuzzing-demonstrator-result.md`.

| Metric | Value |
|--------|------:|
| Steady-state execs/sec (dirty reset) | «eps_dirty» |
| Steady-state execs/sec (full-copy reset) | «eps_full» |
| Reset latency p50 / p99 | «reset_us_p50» / «reset_us_p99» µs |
| — page-copy p50 / register-restore p50 | «restore_us_p50» / «regs_us_p50» µs |
| Dirty-set size p50 / p99 / max (16 KiB pages) | «dirty_pages_p50» / «dirty_pages_p99» / «dirty_pages_max» |
| Distinct edges (coverage) | «coverage_final» |
| Time-to-rediscover planted CVE (synthetic, ASan) | «ttc» s |

Reproduce: `M3_DURATION=60 python3 scripts/fuzz_m3_bench.py`.
```

- [ ] **Step 4: Mark M3 complete in the spec**

In `docs/superpowers/specs/2026-06-14-snapshot-fuzzing-demonstrator-design.md`, change the M3 line in §10 Milestones from `- [ ] **M3 — benchmark.**` to `- [x] **M3 — benchmark.**`. The output path in that line says `docs/fuzzing-demonstrator-result.md` — confirm it matches the file created in Step 2 (it does).

- [ ] **Step 5: Commit**

```bash
git add docs/fuzzing-demonstrator-result.md docs/benchmarks.md docs/superpowers/specs/2026-06-14-snapshot-fuzzing-demonstrator-design.md
git commit -m "fuzz(m3): benchmark result doc + benchmarks.md section; mark M3 complete"
```

---

## Self-Review

**Spec coverage (design §11 metrics):**
- execs/sec (steady-state, single core) — Task 1 `execs_per_sec`, Task 6 runs, Task 7 table. ✓
- Reset latency p50/p99, decomposed register-restore vs page-copy — Task 2 timers, Task 1 `restore_us`/`regs_us`/`reset_us`. ✓
- Dirty-set size distribution — Task 2 `record_dirty`, Task 1 `dirty_pages_*`. ✓
- Time-to-rediscover the planted CVE — Task 2 `record_first_crash`, Task 6 synthetic run. ✓
- Coverage growth curve — Task 2 `sample_coverage`, Task 1 `covsample` lines. ✓
- Cross-check vs Linux/KVM — explicitly DEFERRED (user decision); documented in Task 7. ✓
- Target = libpng current — Tasks 4–5. ✓
- Outputs `docs/fuzzing-demonstrator-result.md` + `docs/benchmarks.md` — Task 7. ✓

**Spec §12 risks addressed:** ASan-shadow inflation → coverage-only libpng build (Tasks 4,5,7). Window-exemption correctness → unchanged from M2 (cov/window below RAM_BASE); not re-litigated here.

**Type consistency:** `FuzzController::new` gains exactly one trailing `metrics_out: Option<PathBuf>` arg (Task 2 def, Task 3 call site). `Metrics` method names (`record_reset`, `record_dirty`, `sample_coverage`, `record_first_crash`, `report`) are identical across Tasks 1–2. The metrics file keys emitted in `Metrics::report` (Task 1) are exactly the keys parsed by `parse_metrics`/`num` in Task 6. `target_parse` signature matches between `harness.c` (extern) and `target_png.c` (def).

**Placeholders:** The only bracketed values are `«…»` in Task 7 docs, which are explicitly populated from the captured `/tmp/m3-result.txt` run output — a doc-generation step, not a code placeholder. The Task 6 `PNG_SEED` placeholder is replaced with verified bytes in Task 6 Step 2.
