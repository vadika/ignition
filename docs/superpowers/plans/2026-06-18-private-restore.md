# Private-Restore (MAP_PRIVATE over base) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Restore maps guest RAM `MAP_PRIVATE` over the shared immutable base `memory.bin` (the only path) — no per-instance memory clone — cutting restore start latency ~4x and the first-workload page-in tax ~2x, with full reset/checkpoint parity preserved.

**Architecture:** `run_restore` mmaps the root base `memory.bin` `MAP_PRIVATE` (guest writes copy-on-write to anon pages; base untouched; all restores share one cache-warm vnode). The reset pristine is a zero-copy read-only mmap of the base for full snapshots, or a heap copy of the reassembled image for diff chains. Checkpoint always takes an owned heap copy of live RAM (the existing fresh-boot path). Reset (byte-copy rollback) and disk snapshot (reads the live host slice) are unchanged.

**Tech Stack:** Rust (crates/vmm, spike/boot.rs binary), libc mmap, Python stdlib bench. HVF live verification on M-series (signing required after each `cargo build`).

**Build note:** every `cargo build --bin boot` strips the code signature — re-sign with `scripts/sign.sh target/debug/boot` before any live run.

---

### Task 1: `PristineRam::map_file_ro` + delete dead `from_clone`

**Files:**
- Modify: `crates/vmm/src/reset.rs`

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `crates/vmm/src/reset.rs`:

```rust
    #[test]
    fn map_file_ro_round_trips_bytes() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("ignition-mapro-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("memory.bin");
        let bytes = vec![0x7Eu8; PG * 3];
        std::fs::File::create(&src).unwrap().write_all(&bytes).unwrap();

        let p = PristineRam::map_file_ro(&src, bytes.len()).unwrap();
        assert_eq!(p.as_slice(), &bytes[..]);

        drop(p);
        std::fs::remove_dir_all(&dir).ok();
    }
```

- [ ] **Step 2: Run it, verify it fails**

Run: `cargo test -p ignition-vmm reset::tests::map_file_ro_round_trips_bytes 2>&1 | tail -20`
Expected: FAIL to compile — `no function or associated item named 'map_file_ro'`.

- [ ] **Step 3: Implement `map_file_ro`**

Add this method inside `impl PristineRam` (next to `from_clone`):

```rust
    /// Map an existing file read-only (no clone). Used to point the reset
    /// pristine at the immutable base memory.bin directly — zero copy, and it
    /// shares the base's warm page cache.
    pub fn map_file_ro(path: &Path, len: usize) -> io::Result<PristineRam> {
        let f = std::fs::OpenOptions::new().read(true).open(path)?;
        // SAFETY: mapping `len` bytes of a file expected to be at least `len`.
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
            return Err(io::Error::other("mmap of pristine file failed"));
        }
        Ok(PristineRam::Mapped { ptr, len })
    }
```

- [ ] **Step 4: Run test, verify pass**

Run: `cargo test -p ignition-vmm reset::tests::map_file_ro_round_trips_bytes 2>&1 | tail -10`
Expected: PASS.

- [ ] **Step 5: Delete the now-dead `from_clone` + its test**

`from_clone`'s only callers are the two `boot.rs` sites replaced in Task 2, so it
becomes dead. Delete the whole `from_clone` method from `impl PristineRam`:

```rust
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
```

And delete its test `pristine_mapped_round_trips_bytes` from the `tests` module
(the whole `#[test] fn pristine_mapped_round_trips_bytes() { ... }`).

- [ ] **Step 6: Verify the crate builds + tests pass**

Run: `cargo test -p ignition-vmm reset:: 2>&1 | tail -15`
Expected: PASS (map_file_ro + rollback tests); no reference to `from_clone` remains.
Run: `cargo build -p ignition-vmm 2>&1 | tail -3`
Expected: clean (no "unused" warnings for `from_clone`).

- [ ] **Step 7: Commit**

```bash
git add crates/vmm/src/reset.rs
git commit -m "reset: add PristineRam::map_file_ro (zero-copy RO base mmap); drop dead from_clone"
```

---

### Task 2: Convert restore to MAP_PRIVATE over base + parity wiring (boot.rs)

This is one coherent unit (the edits must compile together) verified by
`cargo build` + the live tests in Task 3, not unit tests. Apply all steps, then
build, sign, and commit.

**Files:**
- Modify: `spike/src/bin/boot.rs`

- [ ] **Step 1: `ResetWiring` — drop `mem_file` and `inst_dir` fields**

In `struct ResetWiring` (around line 168-182), delete these two fields:

```rust
    mem_file: Option<PathBuf>,
    inst_dir: PathBuf,
```

Update the doc comment above the struct (it currently describes `mem_file`) to:

```rust
/// Resources the checkpoint/reset handlers capture. The checkpoint always takes
/// an owned heap copy of live RAM (works for both MAP_ANON fresh boot and
/// MAP_PRIVATE restore), so no backing file is needed.
```

- [ ] **Step 2: Checkpoint handler — always `from_copy(live)`**

In `install_reset_handlers`, the checkpoint closure currently captures
`mem_file` / `inst_dir` and branches on `mem_file`. Replace the captured-vars
block and the `pristine` computation.

Delete these captures (around lines 192-193):

```rust
        let mem_file = w.mem_file.clone();
        let inst_dir = w.inst_dir.clone();
```

Replace the entire `let pristine = match &mem_file { ... };` block (around lines
204-222) with:

```rust
            // Always an owned heap copy of current RAM. Under MAP_PRIVATE restore
            // there is no file backing live RAM to clonefile; under MAP_ANON fresh
            // boot there never was. One uniform path.
            let pristine = ignition_vmm::reset::PristineRam::from_copy(live);
```

(`live` is already bound just above from `from_raw_parts(host_usize, ...)`.)

- [ ] **Step 3: Fresh-boot `ResetWiring` construction — drop the two fields**

At the fresh-boot `install_reset_handlers` call (around line 1309-1320), delete
these two lines from the `ResetWiring { ... }` literal:

```rust
        mem_file: None,
        inst_dir: std::env::temp_dir(),
```

- [ ] **Step 4: `run_restore` — map the base MAP_PRIVATE instead of clonefile + MAP_SHARED**

In `run_restore`, the memory clone + mmap block (around lines 1914-1944) does:
create instance dir, `clonefile_or_copy(root memory.bin -> inst_mem)`, open
`inst_mem`, mmap it `MAP_SHARED`. Keep the instance dir (the disk clone still
uses it), but remove the memory clone and map the base directly.

Replace the memory-specific lines. Delete the `inst_mem` binding and its clone:

```rust
    let inst_mem = inst_dir.join("memory.bin");
    // Clone the ROOT memory.bin (not the leaf — a Diff leaf's memory.bin is only its
    // packed dirty pages). Diff layers are overlaid onto this clone below.
    snapshot::clonefile_or_copy(&root_paths.memory, &inst_mem)?;
    let t_clone = restore_start.elapsed();
```

with (keep a `t_clone` marker so the later breakdown print still compiles):

```rust
    let t_clone = restore_start.elapsed();
```

Then replace the mmap block (the `let memf = ...; let host = unsafe { mmap(... MAP_SHARED, memf ...) }; ... drop(memf);`) with:

```rust
    // 2. Map the shared, immutable base memory.bin MAP_PRIVATE as guest RAM:
    //    guest writes copy-on-write to anonymous pages (the base is never
    //    modified), and every restore maps the SAME base vnode, so its page
    //    cache stays warm across launches. No per-instance memory clone.
    let basef = fs::File::open(&root_paths.memory)?;
    let host = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            mem_size as usize,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE,
            basef.as_raw_fd(),
            0,
        )
    };
    if host == libc::MAP_FAILED {
        return Err(io::Error::other("mmap of base memory.bin failed"));
    }
    drop(basef); // the mapping keeps the underlying file alive after the fd closes
    let host_addr = host as u64;
    let t_mmap = restore_start.elapsed();
```

(The diff-overlay block that follows writes into `host` unchanged — those writes
become private anon pages.)

- [ ] **Step 5: `run_restore` — seed the reset pristine without a clone**

Replace the reset-point seed block (around lines 2253-2265):

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
```

with:

```rust
    // Seed the reset point: the restored snapshot IS the default Ctrl-A r target.
    // Full snapshot (single layer): the immutable base file IS the post-restore
    // image, so map it read-only (zero copy, shares the warm vnode). Diff chain:
    // the reassembled image lives only in the MAP_PRIVATE pages, so take an owned
    // heap copy of live RAM.
    {
        let pristine = if chain.len() == 1 {
            ignition_vmm::reset::PristineRam::map_file_ro(&root_paths.memory, mem_size as usize)
                .map_err(|e| io::Error::other(format!("seed pristine map_file_ro: {e}")))?
        } else {
            let live: &[u8] =
                unsafe { std::slice::from_raw_parts(host as *const u8, mem_size as usize) };
            ignition_vmm::reset::PristineRam::from_copy(live)
        };
        *manager.reset_point().lock().unwrap() = Some(ignition_vmm::reset::ResetPoint {
            pristine,
            vcpus: snap.vcpus.clone(),
            gic_blob: gic_blob.clone(),
            devices: snap.devices.clone(),
        });
    }
```

- [ ] **Step 6: `run_restore` — drop `mem_file`/`inst_dir` from the restore `ResetWiring`**

At the restore `install_reset_handlers` call (around line 2268-2279), delete
these two lines from the `ResetWiring { ... }` literal:

```rust
        mem_file: Some(inst_mem.clone()),
        inst_dir: inst_dir.clone(),
```

- [ ] **Step 7: Build, sign, and confirm no leftover references**

Run: `cargo build --bin boot 2>&1 | tail -20`
Expected: clean build. If it errors about `inst_mem` used elsewhere or unused
imports (`clonefile_or_copy` may still be used by the disk clone at ~line 2041 —
leave that), fix only the memory-path references. `t_clone` and `t_mmap` must
still be defined (the breakdown print at ~line 2329 uses them).

Run: `rg -n "inst_mem|mem_file|from_clone|pristine_dst" spike/src/bin/boot.rs`
Expected: no matches (all removed).

Run: `scripts/sign.sh target/debug/boot && echo signed`
Expected: `signed`.

- [ ] **Step 8: Commit**

```bash
git add spike/src/bin/boot.rs
git commit -m "restore: map guest RAM MAP_PRIVATE over shared base (no per-instance memory clone); reset pristine = RO base mmap (full) or live copy (diff); checkpoint = owned copy"
```

---

### Task 3: Live HVF verification (run by the human operator / main thread)

Subagents cannot run HVF or sign binaries. These steps are executed in the main
session. Prereq: `target/debug/boot` built + signed (Task 2 Step 7), the
`tools-base` snapshot exists in `mcp-store` (else `scripts/make-tools-base.sh`).

- [ ] **Step 1: Launch perf + correctness**

Run: `python3 scripts/sandbox_bench.py -n 20 --concurrency 1 --mode hot`
Expected: `ok=20 failed=0`, numpy output correct, ready p50 ~60-80 ms (was ~245),
exec1 p50 well under the old ~740 ms (~330 ms). Records the win.

- [ ] **Step 2: Fan-out still passes**

Run: `python3 scripts/fanout_demo.py --base tools-base -n 8`
Expected: `=> PASS` (randoms distinct, cow isolated). Confirms MAP_PRIVATE clones
still diverge and isolate.

- [ ] **Step 3: Re-snapshot integrity (live-slice disk snapshot path)**

Restore tools-base interactively with a vsock console, snapshot it under a new
name, restore that, run a workload:

```bash
# in one shell, drive via the existing make-tools-base style or manually:
target/debug/boot --restore tools-base --store mcp-store --mem 1024 \
  --vsock-uds /tmp/resnap.sock --name resnap-test --force \
  kimage/out/Image kimage/out/rootfs-tools.ext4
# in the console: Ctrl-A s  (writes snapshot 'resnap-test'), then Ctrl-A x
target/debug/boot --restore resnap-test --store mcp-store --mem 1024 \
  --vsock-uds /tmp/resnap2.sock kimage/out/Image kimage/out/rootfs-tools.ext4
# confirm it resumes and a vsock exec works
```
Expected: both restores resume cleanly; workload output correct. Proves
re-snapshotting a MAP_PRIVATE guest captures correct RAM.

- [ ] **Step 4: In-place reset (Ctrl-A r) on a restored guest**

```bash
target/debug/boot --restore tools-base --store mcp-store --mem 1024 \
  --vsock-uds /tmp/reset.sock kimage/out/Image kimage/out/rootfs-tools.ext4
```
In the guest console: create a marker (`echo HELLO > /tmp/marker`), then press
`Ctrl-A r`. Expected: `[reset] Reset-time = ... ms` printed, guest stays alive,
and `cat /tmp/marker` shows the file is gone (rolled back to the post-restore
state). Exercises the new `map_file_ro` pristine + unchanged byte-copy rollback.

- [ ] **Step 5: Diff-chain restore (from_copy pristine path)**

If a diff-chain snapshot is available (or create one: restore tools-base,
`Ctrl-A s` twice with `--track-dirty` to build a diff), restore the leaf and run
a workload. Expected: correct reassembly + workload output. Exercises the
`chain.len() > 1` `from_copy` pristine branch. If no diff snapshot is readily
available, note it as not-exercised rather than fabricating one.

- [ ] **Step 6: Record results**

No commit (verification only). Capture the bench numbers for the docs in Task 5.
If any step fails, STOP and treat as a Task 2 defect (systematic-debugging).

---

### Task 4: Bench `--prefetch` flag

Re-add the cold-cache prefetch knob (useful to front-load the one-time base read
from a truly cold cache; a no-op when the base is already warm). The base is a
shared vnode now, so one prefetch warms it for all subsequent restores.

**Files:**
- Modify: `scripts/sandbox_bench.py`

- [ ] **Step 1: Add the `--prefetch` argument**

In `main()`'s argparse block (after the `--mem` argument), add:

```python
    ap.add_argument("--prefetch", action="store_true",
                    help="hot mode: read the base memory.bin once before launching, to warm the page cache")
```

- [ ] **Step 2: Prefetch before the hot run**

In `main()`, replace the modes loop body so a `hot` run with `--prefetch` reads
the base image first. Replace:

```python
    for m in modes:
        if not args.json:
            print(f"running {args.count} {m} sandboxes (concurrency {args.concurrency}) ...",
                  file=sys.stderr)
        recs, wall = run_mode(m, args, run_tok)
        summaries.append((summarize(m, recs, wall), recs))
```

with:

```python
    for m in modes:
        if m == "hot" and args.prefetch:
            base_mem = os.path.join(args.store, "snapshots", args.base, "memory.bin")
            t = time.monotonic()
            with open(base_mem, "rb", buffering=0) as f:
                while f.read(8 << 20):  # read the base once to warm the page cache
                    pass
            if not args.json:
                print(f"prefetched {base_mem} in {round((time.monotonic()-t)*1000)} ms",
                      file=sys.stderr)
        if not args.json:
            print(f"running {args.count} {m} sandboxes (concurrency {args.concurrency}) ...",
                  file=sys.stderr)
        recs, wall = run_mode(m, args, run_tok)
        summaries.append((summarize(m, recs, wall), recs))
```

- [ ] **Step 3: Verify it parses + runs the prefetch**

Run: `python3 scripts/sandbox_bench.py --help 2>&1 | grep prefetch`
Expected: the `--prefetch` help line prints.
Run: `python3 scripts/sandbox_bench.py -n 1 --concurrency 1 --mode hot --prefetch 2>&1 | grep prefetched`
Expected: a `prefetched .../memory.bin in <N> ms` line (requires the tools-base
snapshot + signed boot; if absent, skip this run and note it).

- [ ] **Step 4: Commit**

```bash
git add scripts/sandbox_bench.py
git commit -m "sandbox_bench: add --prefetch (warm the shared base page cache once before hot launches)"
```

---

### Task 5: Docs — describe the MAP_PRIVATE-over-base restore model

**Files:**
- Modify: `docs/src/features/snapshot-restore.md`

- [ ] **Step 1: Read the current restore description**

Run: `rg -n "clonefile|MAP_SHARED|instance|memory.bin|mmap|restore" docs/src/features/snapshot-restore.md`
Identify the paragraph(s) describing how restore maps memory (per-instance
clonefile + MAP_SHARED). 

- [ ] **Step 2: Update the prose**

Edit the restore-memory description to state: restore mmaps the shared, immutable
base `memory.bin` `MAP_PRIVATE`; guest writes copy-on-write to anonymous pages so
the base is never modified and N concurrent restores share one cache-warm vnode;
there is no per-instance memory clone (the disk still clones per instance). Note
the measured effect (start latency ~4x lower, first-workload page-in ~2x lower —
use the Task 3 Step 1 numbers). Note reset rolls back by byte-copy from a
pristine that is a read-only base mmap (full snapshot) or a live heap copy (diff
chain), and checkpoint takes an owned heap copy of live RAM. Match the house
style (no em dashes in prose; concrete numbers). Cross-link `vmid.md` and the
fan-out / MCP pages if relevant.

- [ ] **Step 3: Build the book if mdbook is available**

Run: `command -v mdbook >/dev/null && (cd docs && mdbook build >/dev/null && echo OK) || echo "mdbook absent, skip"`
Expected: `OK` or the skip line; no broken-link errors if it ran.

- [ ] **Step 4: Commit**

```bash
git add docs/src/features/snapshot-restore.md
git commit -m "docs: restore now maps guest RAM MAP_PRIVATE over the shared base"
```

---

## Self-Review

**Spec coverage:**
- Guest RAM MAP_PRIVATE over base → Task 2 Step 4. ✓
- Remove per-instance memory clone → Task 2 Step 4 (delete `inst_mem` + clone). ✓
- Pristine seed: map_file_ro (full) / from_copy (diff) → Task 2 Step 5 + Task 1 (map_file_ro). ✓
- Checkpoint always from_copy; drop mem_file branch + field → Task 2 Steps 1-3. ✓
- Reset handler unchanged → not touched (verified by Task 3 Step 4). ✓
- Disk snapshot unchanged → not touched (verified by Task 3 Step 3). ✓
- `map_file_ro` constructor + delete dead `from_clone` → Task 1. ✓
- Remove `--restore-private` prototype flag → already reverted to clean base (not in HEAD); nothing to remove. ✓ (noted)
- Bench keeps `--prefetch` → Task 4 (re-added; was prototype-only). ✓
- Docs update → Task 5. ✓
- Unit test for map_file_ro + existing rollback tests green → Task 1. ✓
- Live: launch perf, fan-out, re-snapshot, reset, diff chain → Task 3. ✓

**Placeholder scan:** No TBD/TODO. Every code step shows the exact before/after. The one judgment call (Task 5 prose) is a doc-writing task with explicit content requirements, not a code placeholder. Task 3 diff-chain step explicitly allows "note as not-exercised" rather than fabricating.

**Type consistency:** `PristineRam::map_file_ro(&Path, usize) -> io::Result<PristineRam>` defined in Task 1, called in Task 2 Step 5 with `(&root_paths.memory, mem_size as usize)` — matches. `from_copy(&[u8]) -> PristineRam` (existing) used in Task 2 Steps 2 and 5 — matches. `ResetWiring` field removals (`mem_file`, `inst_dir`) are consistent across the struct def (Step 1) and both construction sites (Steps 3, 6). `t_clone`/`t_mmap` markers preserved (Task 2 Step 4) so the breakdown print still compiles.

**Execution note:** Tasks 1, 4, 5 are subagent-friendly (unit-tested / mechanical / docs). Task 2 (boot.rs core) and Task 3 (live HVF) require local build+sign+HVF and are executed in the main session.
