# Faster Restore/Resume Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Attribute the ~245 ms host-side `Restore-time` to its stages, then apply one data-gated optimization that measurably lowers it.

**Architecture:** Three phases. (1) Add per-stage timers to `restore_snapshot` and emit a `Restore-breakdown` line. (2) Parse it in the bench, record the attribution. (3) A decision gate reads the data, then — if HVF object creation and/or clonefile dominate — restructure restore so the heavy RAM I/O (clonefile root RAM + mmap + diff overlay) runs on a side thread concurrently with `Vm::new` + `HvfGicV3::new`. If the data points elsewhere, stop and re-plan against the spec's fallback levers.

**Tech Stack:** Rust (edition 2024), Apple Hypervisor.framework, `libc`, Python 3 bench/test harness.

Spec: `docs/superpowers/specs/2026-06-14-faster-restore-design.md`.

**Background the implementer needs:**
- The runnable binary is `boot`. It needs the hypervisor entitlement; **re-sign after every build**: `cargo build -p ignition-spike --bin boot && scripts/sign.sh target/debug/boot`.
- `restore_snapshot` lives in `spike/src/bin/boot.rs`. The restore clock is `let restore_start = std::time::Instant::now();` near line 905; the matching `log::info!("Restore-time = {} ms", restore_start.elapsed().as_millis());` is near line 1262. All stages to time sit between them.
- `clonefile` is O(1) in file size but **not free** (inode + extent-map copy); two calls exist (root RAM at ~`:965`, disk at ~`:1059`). No stage is assumed negligible.
- `hv_vm_create` is a per-process singleton (`crates/hvf/src/lib.rs:293`). One VM per process. Do not attempt an in-process VM pool.
- Headless test drivers: `python3 scripts/restore_test.py` (boot → snapshot → restore; prints CPU% + latency + immutability) and `python3 scripts/restore_clone_test.py` (login + command + two clones). Both need a prebuilt+signed `target/debug/boot` and `kimage/out/Image` + `kimage/out/rootfs.ext4`.
- Bench: `scripts/diff_snapshot_bench.py`. Restore phase is `measure_restore(name, samples)` (~line 273); regex/parse helpers are at ~lines 110–126.

---

### Task 1: Restore sub-timing instrumentation

**Files:**
- Modify: `spike/src/bin/boot.rs` (inside `restore_snapshot`, between ~`:905` and ~`:1262`)

This task adds cumulative-elapsed snapshots after each restore stage and emits one machine-parseable line. Stages skipped at runtime (diff overlay when the chain is a single layer; protect when `--track-dirty` is off) record the same cumulative value as the previous stage, so their delta is 0.

- [ ] **Step 1: Add a cumulative snapshot after each stage**

After each stage's existing code, capture `restore_start.elapsed()` into a named local. Insert these lines at the points indicated (the *function call* each follows already exists in the file). **Stages must be captured in chronological file order so deltas never go negative** (`Duration` subtraction panics on underflow). Chronological order in the file is: resolve_chain+validate (ends ~`:932`) → `read_snapshot`+`base_len` check (ends ~`:955`) → clonefile → mmap → ...

After the chain-shape validation loop (the `for m in &chain[1..]` block, ~`:932`), before `read_snapshot`:
```rust
let t_chain = restore_start.elapsed();
```
After the `base_len` validation block (the `if base_len != mem_size` check, ~`:955`) — this delta covers `read_snapshot` + the length checks:
```rust
let t_read = restore_start.elapsed();
```
After the `clonefile_or_copy(&root_paths.memory, &inst_mem)?` call (~`:965`):
```rust
let t_clone = restore_start.elapsed();
```
After the `mmap` block sets `host_addr` (~`:985`):
```rust
let t_mmap = restore_start.elapsed();
```
After the diff-overlay `if chain.len() > 1 { ... }` block (~`:1005`):
```rust
let t_diff = restore_start.elapsed();
```
After `let mut vm = Vm::new(false)...?;` (~`:1008`):
```rust
let t_vm = restore_start.elapsed();
```
After the `HvfGicV3::new(...)` `gic` binding (~`:1016`):
```rust
let t_gic = restore_start.elapsed();
```
After `vm.map_memory(host_addr, layout::RAM_BASE, mem_size)...?;` (~`:1020`):
```rust
let t_map = restore_start.elapsed();
```
After the `dirty_tracker` `if track_dirty { ... } else { None };` block (~`:1043`):
```rust
let t_protect = restore_start.elapsed();
```
After `setup_devices(&mut mgr, &mut ctx, Mode::Restore(&snap.devices))?;` (~`:1074`):
```rust
let t_dev = restore_start.elapsed();
```

- [ ] **Step 2: Emit the breakdown line just before the `Restore-time` log**

Immediately above the existing `log::info!("Restore-time = ...")` line (~`:1262`), insert:
```rust
let total = restore_start.elapsed();
let us = |d: std::time::Duration| d.as_micros();
log::info!(
    "Restore-breakdown = chain:{}us read:{}us clone:{}us mmap:{}us diff:{}us \
     vm:{}us gic:{}us map:{}us protect:{}us dev:{}us total:{}us",
    us(t_chain),
    us(t_read - t_chain),
    us(t_clone - t_read),
    us(t_mmap - t_clone),
    us(t_diff - t_mmap),
    us(t_vm - t_diff),
    us(t_gic - t_vm),
    us(t_map - t_gic),
    us(t_protect - t_map),
    us(t_dev - t_protect),
    us(total),
);
```

- [ ] **Step 3: Build and re-sign**

Run: `cargo build -p ignition-spike --bin boot && scripts/sign.sh target/debug/boot`
Expected: builds clean, `boot` re-signed.

- [ ] **Step 4: Verify the line appears on a real restore**

Run: `python3 scripts/restore_test.py 2>&1 | grep -E "Restore-breakdown|Restore-time"`
Expected: one `Restore-breakdown = chain:... total:...` line and one `Restore-time = N ms` line. The breakdown `total:` in µs ÷ 1000 should be within ~1 ms of `Restore-time`'s ms value.

- [ ] **Step 5: Clippy**

Run: `cargo clippy -p ignition-spike --bin boot`
Expected: `no issues` (no new warnings).

- [ ] **Step 6: Commit**

```bash
git add spike/src/bin/boot.rs
git commit -m "restore: per-stage Restore-breakdown timing line"
```

---

### Task 2: Bench parses the breakdown + records the attribution

**Files:**
- Modify: `scripts/diff_snapshot_bench.py` (regex block ~`:110`, `measure_restore` ~`:273`, restore reporting in `main`)
- Modify: `scripts/restore_test.py` (add a breakdown-present assertion)
- Modify: `docs/diff-snapshot-benchmarks.md` (§3 attribution table)

- [ ] **Step 1: Add a breakdown regex + parser to the bench**

In `scripts/diff_snapshot_bench.py`, after `RE_SNAPW` (~`:112`) add:
```python
RE_RBREAK = re.compile(
    rb"Restore-breakdown = chain:(\d+)us read:(\d+)us clone:(\d+)us mmap:(\d+)us "
    rb"diff:(\d+)us vm:(\d+)us gic:(\d+)us map:(\d+)us protect:(\d+)us dev:(\d+)us total:(\d+)us"
)
RBREAK_STAGES = ["chain","read","clone","mmap","diff","vm","gic","map","protect","dev","total"]

def parse_rbreak_us(buf):
    """Return {stage: microseconds} from the last Restore-breakdown line, or None."""
    m = None
    for m in RE_RBREAK.finditer(buf):
        pass
    if not m:
        return None
    return {s: int(m.group(i + 1)) for i, s in enumerate(RBREAK_STAGES)}
```

- [ ] **Step 2: Capture the breakdown in `measure_restore`**

In `measure_restore` (~`:273`), change the signature comment and collect a third list. Replace the function's `wall, internal = [], []` line with:
```python
    wall, internal, breakdowns = [], [], []
```
After `internal_ms = parse_restore_ms(b"".join(sink))` (~`:297`) add:
```python
        bd = parse_rbreak_us(b"".join(sink))
        if bd is not None:
            breakdowns.append(bd)
```
Change the `return wall, internal` line (~`:302`) to:
```python
    return wall, internal, breakdowns
```

- [ ] **Step 3: Aggregate per-stage medians and update callers**

Add this helper next to `parse_rbreak_us`:
```python
def median_breakdown(breakdowns):
    """Per-stage median µs across runs; {} if none collected."""
    if not breakdowns:
        return {}
    return {s: int(statistics.median(b[s] for b in breakdowns)) for s in RBREAK_STAGES}
```
Every call site of `measure_restore` in `main` now unpacks three values. Find each `wall, internal = measure_restore(...)` and change it to `wall, internal, bds = measure_restore(...)`, then store `R[...] = median_breakdown(bds)` alongside the existing `med_spread` lines for that restore target (use a `_breakdown` suffix key, e.g. `R["restore_full_breakdown"] = median_breakdown(bds)`).

- [ ] **Step 4: Print the per-stage medians**

In `main`, after the restore phase computes its medians, print the attribution so it lands in the run log:
```python
    for key in [k for k in R if k.endswith("_breakdown")]:
        bd = R[key]
        if bd:
            print(f"   [{key}] " + " ".join(f"{s}={bd[s]}us" for s in RBREAK_STAGES), flush=True)
```

- [ ] **Step 5: Assert the breakdown line in `restore_test.py`**

In `scripts/restore_test.py`, after the restore phase has drained console output into its buffer (after the `[restore -> first output latency...]` print, ~`:109`), add an assertion. Use the buffer variable already holding restore console bytes (named `resp` in that script):
```python
import re as _re
_bd = _re.search(
    rb"Restore-breakdown = chain:(\d+)us .* total:(\d+)us", resp)
print(f"[Restore-breakdown present: {_bd is not None}]", flush=True)
assert _bd is not None, "Restore-breakdown line missing from restore output"
```
If `resp` does not contain the full setup log (it may hold only post-Enter output), instead search the combined buffer the script already accumulates during the restore spawn; pick the buffer variable that includes the `Restore-time` line (grep the file for `Restore-time` usage — if the script does not currently capture it, capture the spawn output into a buffer and search that). The assertion must operate on the same bytes where `Restore-time` appears.

- [ ] **Step 6: Run the bench restore phase only is not supported; run the full bench at low sample count to confirm parsing**

Run: `cargo build -p ignition-spike --bin boot && scripts/sign.sh target/debug/boot && python3 scripts/diff_snapshot_bench.py --boot-samples 1 --dd-samples 1 --snap-samples 1 --restore-samples 3 2>&1 | grep -E "_breakdown|restore .* internal="`
Expected: per-stage `[restore_*_breakdown] chain=.. read=.. ... total=..` lines print, with non-zero `vm`, `gic`, `clone`, `map` values.

- [ ] **Step 7: Verify the assertion in restore_test.py passes**

Run: `python3 scripts/restore_test.py 2>&1 | grep "Restore-breakdown present"`
Expected: `[Restore-breakdown present: True]` and the script exits 0.

- [ ] **Step 8: Record the attribution in the benchmark doc**

In `docs/diff-snapshot-benchmarks.md` §3 (Restore latency, ~`:112`), add a stage-attribution table built from the printed medians, for the Full-only restore and a deep-chain (golden+3) restore. Use the actual numbers from Step 6's run. Format:
```markdown
**Where the ~245 ms goes (per-stage median, µs):**

| stage | Full-only | golden+3 |
|---|---|---|
| chain resolve+validate | … | … |
| read leaf state | … | … |
| clonefile root RAM | … | … |
| mmap | … | … |
| diff overlay | … | … |
| Vm::new (hv_vm_create) | … | … |
| HvfGicV3::new (hv_gic_create) | … | … |
| map_memory (hv_vm_map) | … | … |
| protect (n/a here) | … | … |
| device wiring | … | … |
| **total** | … | … |
```
Fill every `…` with the measured µs. Add one sentence naming the dominant stage(s).

- [ ] **Step 9: Commit**

```bash
git add scripts/diff_snapshot_bench.py scripts/restore_test.py docs/diff-snapshot-benchmarks.md
git commit -m "bench: parse + record Restore-breakdown stage attribution"
```

---

### Task 3: Decision gate (analysis, no code)

**Files:** none (reads Task 2's recorded table)

- [ ] **Step 1: Classify the dominant stage from the §3 table**

Read the attribution table written in Task 2 Step 8. Sum the reducible-in-process stages and identify the largest:
- `vm` + `gic` (HVF object creation), and/or `clone` (root RAM clonefile) dominant → **proceed to Task 4** (the concurrent restructure targets exactly these by overlapping HVF creation with RAM I/O).
- `protect` dominant → STOP. The fix is lazy/deferred write-protect (spec fallback). Report to the human and request a re-plan; do not start Task 4.
- `dev` (device wiring) dominant → STOP. The fix is parallelizing the disk clonefile / lazy device init (spec fallback). Report and request a re-plan.
- `map` (`hv_vm_map`) dominant alone → STOP. Likely irreducible HVF cost; report findings, no further code.

- [ ] **Step 2: Record the decision**

Append one line to `docs/diff-snapshot-benchmarks.md` §3 stating which stage dominated and which lever was selected (or that work stopped and why). Commit:
```bash
git add docs/diff-snapshot-benchmarks.md
git commit -m "bench: record restore-optimization decision from breakdown"
```

---

### Task 4: Concurrent restructure — RAM I/O on a side thread (only if Task 3 says proceed)

**Files:**
- Modify: `spike/src/bin/boot.rs` (`restore_snapshot`, the block ~`:957`–`:1020`)

Run the heavy RAM I/O (clonefile root `memory.bin` + `mmap` + diff overlay) on a spawned thread while the main thread runs `Vm::new` + `HvfGicV3::new`. The HVF objects never cross a thread boundary (they are created and owned on the main thread); only the mmap host address (as `usize`) and plain data return from the side thread, so no `Send` impl is required on `Vm`/`HvfGicV3`. `read_snapshot` stays on the main thread (it is a small read, already before this block, and supplies `snap.config.vcpu_count` for the GIC). `map_memory` runs after the join because it needs both the VM handle and the host address.

- [ ] **Step 1: Confirm the baseline is green before refactoring**

Run: `cargo build -p ignition-spike --bin boot && scripts/sign.sh target/debug/boot && python3 scripts/restore_test.py 2>&1 | tail -5`
Expected: restore succeeds — `responsive: True`, low avg CPU, `base memory.bin unchanged: True`, `base disk.img unchanged: True`.

- [ ] **Step 2: Replace the serial clone+mmap+diff+Vm+GIC block with a concurrent one**

Replace the existing code that runs from the instance-dir creation (`let inst_dir = ...` ~`:959`) through the `HvfGicV3::new` `gic` binding (~`:1016`) with the following. (The `t_clone`/`t_mmap`/`t_diff`/`t_vm`/`t_gic` snapshots from Task 1 are superseded here; see Step 3.)
```rust
    // Per-restore instance dir for CoW clones (base never written back).
    let inst_dir = snapshot::instance_dir(store, restore_name, process::id());
    let _ = fs::remove_dir_all(&inst_dir);
    fs::create_dir_all(&inst_dir)?;
    let inst_mem = inst_dir.join("memory.bin");

    // Side thread: heavy RAM I/O. clonefile the ROOT memory.bin, map it MAP_SHARED,
    // overlay each diff layer's packed pages. Returns the mmap address as usize so no
    // raw pointer crosses the thread boundary. Runs concurrently with HVF object
    // creation on the main thread below.
    let io_handle = {
        let root_mem = root_paths.memory.clone();
        let inst_mem_io = inst_mem.clone();
        let store_io = store.to_path_buf();
        let diff_names: Vec<String> = chain[1..].iter().map(|m| m.name.clone()).collect();
        let mem_size_io = mem_size;
        std::thread::spawn(move || -> io::Result<usize> {
            snapshot::clonefile_or_copy(&root_mem, &inst_mem_io)?;
            let memf = fs::OpenOptions::new().read(true).write(true).open(&inst_mem_io)?;
            let host = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    mem_size_io as usize,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    memf.as_raw_fd(),
                    0,
                )
            };
            if host == libc::MAP_FAILED {
                return Err(io::Error::other("mmap of instance memory.bin failed"));
            }
            drop(memf);
            if !diff_names.is_empty() {
                let ram: &mut [u8] = unsafe {
                    std::slice::from_raw_parts_mut(host as *mut u8, mem_size_io as usize)
                };
                for name in &diff_names {
                    let d = snapshot::base_dir(&store_io, name);
                    let (idx, packed) = snapshot::read_diff_pages(&d)?;
                    snapshot::apply_diff(ram, &idx, &packed)?;
                }
            }
            Ok(host as usize)
        })
    };

    // 3. Main thread, concurrent with the I/O above: create the HVF VM + in-kernel GIC.
    let mut vm = Vm::new(false).map_err(|e| io::Error::other(format!("Vm::new: {e}")))?;
    let gic = Arc::new(
        HvfGicV3::new(snap.config.vcpu_count, layout::RAM_BASE)
            .map_err(|e| io::Error::other(format!("GIC create: {e}")))?,
    );

    // Join the RAM I/O. Double `?`: outer = thread panic, inner = io::Result.
    let host_usize = io_handle
        .join()
        .map_err(|_| io::Error::other("restore RAM I/O thread panicked"))??;
    let host = host_usize as *mut libc::c_void;
    let host_addr = host_usize as u64;
    if chain.len() > 1 {
        eprintln!(
            "[restore] reassembled chain: root '{}' + {} diff layer(s) -> leaf '{}'",
            root.name,
            chain.len() - 1,
            leaf.name
        );
    }
```

- [ ] **Step 3: Replace the superseded per-stage timers with overlap-aware ones**

The serial `t_clone`/`t_mmap`/`t_diff`/`t_vm`/`t_gic` snapshots no longer mark distinct serial points. Delete those five `let t_… = restore_start.elapsed();` lines (they were added in Task 1) and, immediately after the `host_addr` binding from Step 2, add two snapshots that bracket the concurrent region as a whole:
```rust
    let t_io_join = restore_start.elapsed(); // end of concurrent HVF-create || RAM-I/O region
```
Then change the `Restore-breakdown` log (Task 1 Step 2) to the overlap-aware shape. Replace its format string + args with:
```rust
    let total = restore_start.elapsed();
    let us = |d: std::time::Duration| d.as_micros();
    log::info!(
        "Restore-breakdown = chain:{}us read:{}us concurrent:{}us map:{}us protect:{}us dev:{}us total:{}us",
        us(t_chain),
        us(t_read - t_chain),
        us(t_io_join - t_read),
        us(t_map - t_io_join),
        us(t_protect - t_map),
        us(t_dev - t_protect),
        us(total),
    );
```
(`t_map`, `t_protect`, `t_dev` from Task 1 remain valid — they follow the join.) Update the bench `RE_RBREAK`/`RBREAK_STAGES` in `scripts/diff_snapshot_bench.py` to the new field set `["chain","read","concurrent","map","protect","dev","total"]` and regex accordingly.

- [ ] **Step 4: Build, re-sign, clippy**

Run: `cargo build -p ignition-spike --bin boot && scripts/sign.sh target/debug/boot && cargo clippy -p ignition-spike --bin boot`
Expected: builds clean; clippy `no issues`. If the build fails with a `Send`/`Sync` error on the spawned closure, the captured data is the cause (all captures here are `PathBuf`/`String`/`u64`, which are `Send` — re-check no raw pointer or HVF object was captured into the closure). Do not make `Vm`/`HvfGicV3` `Send` to work around it.

- [ ] **Step 5: Correctness gate — restore + clones still work**

Run: `python3 scripts/restore_test.py 2>&1 | tail -6 && python3 scripts/restore_clone_test.py 2>&1 | tail -6`
Expected: `restore_test.py` → `responsive: True`, low avg CPU, `base memory.bin unchanged: True`, `base disk.img unchanged: True`. `restore_clone_test.py` → both clones come up independent. Any failure here means the restructure broke resume; revert and report.

- [ ] **Step 6: Win gate — Restore-time dropped**

Run: `python3 scripts/diff_snapshot_bench.py --boot-samples 1 --dd-samples 1 --snap-samples 1 --restore-samples 5 2>&1 | grep -E "restore .* internal=|_breakdown"`
Expected: median `internal=` (Restore-time) for the Full-only restore is measurably below the 245 ms baseline recorded in §3. If it did not move, revert this task's changes and record why (Step 8 of Task 2 still stands as the deliverable).

- [ ] **Step 7: Update the benchmark doc with the new numbers**

In `docs/diff-snapshot-benchmarks.md` §3, add the post-restructure `Restore-time` median (n=5) next to the 245 ms baseline, and a one-line note on the overlap-aware breakdown (`concurrent` stage now folds clone+mmap+diff+vm+gic).

- [ ] **Step 8: Commit**

```bash
git add spike/src/bin/boot.rs scripts/diff_snapshot_bench.py docs/diff-snapshot-benchmarks.md
git commit -m "restore: overlap HVF object creation with RAM I/O on a side thread"
```

---

## Final verification

- [ ] `cargo test` passes (workspace).
- [ ] `cargo clippy` clean for the touched crates.
- [ ] `python3 scripts/restore_test.py` and `python3 scripts/restore_clone_test.py` pass.
- [ ] `docs/diff-snapshot-benchmarks.md` §3 has: the stage-attribution table, the decision line, and (if Task 4 ran) the new `Restore-time` median.
- [ ] Dispatch a final code reviewer over the full diff, then use superpowers:finishing-a-development-branch.
