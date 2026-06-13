# Fast Restore via clonefile + mmap — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Restore a microVM by lazily memory-mapping a copy-on-write clone of an immutable snapshot, instead of eagerly reading the whole RAM dump, and parametrize guest RAM size.

**Architecture:** A snapshot dir is read-only. Each `--restore` makes a per-process instance dir whose `memory.bin`/`disk.img` are APFS `clonefile(2)` clones of the base. The instance `memory.bin` is mapped `MAP_SHARED` as guest RAM, so pages fault in lazily and guest writes land in the clone — never the base. Guest RAM size becomes a `--mem` flag and is read back from the snapshot on restore.

**Tech Stack:** Rust (edition 2024), Hypervisor.framework, macOS `clonefile(2)` + `mmap(2)`, `libc` crate.

**Source of truth:** `docs/superpowers/specs/2026-06-13-fast-restore-clonefile-mmap-design.md`

---

## File structure

- `crates/vmm/Cargo.toml` — add `libc` dependency (the helper needs `clonefile`/`mmap` errno constants).
- `crates/vmm/src/snapshot.rs` — new `clonefile_or_copy` helper + unit test; `write_snapshot` uses it for the disk artifact.
- `spike/src/bin/boot.rs` — restore path rewritten to clone + `mmap(MAP_SHARED)` and to read RAM size from the snapshot; boot path gains `--mem` and threads the runtime size through; instance-dir cleanup.
- `scripts/restore_test.py` — restore-latency timing + base-immutability assertions.

---

### Task 1: `clonefile_or_copy` helper in the vmm crate

**Files:**
- Modify: `crates/vmm/Cargo.toml` (add `libc`)
- Modify: `crates/vmm/src/snapshot.rs` (add helper + test)

- [ ] **Step 1: Add the `libc` dependency**

In `crates/vmm/Cargo.toml`, under `[dependencies]` (after the `log = "0.4"` line):

```toml
libc = "0.2"
```

- [ ] **Step 2: Write the failing test**

Append to the `#[cfg(test)] mod tests` block in `crates/vmm/src/snapshot.rs`:

```rust
#[test]
fn clonefile_or_copy_duplicates_and_isolates() {
    let dir = std::env::temp_dir().join(format!("ign-clone-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let src = dir.join("src.bin");
    let dst = dir.join("dst.bin");
    fs::write(&src, b"hello world").unwrap();

    clonefile_or_copy(&src, &dst).unwrap();
    assert_eq!(fs::read(&dst).unwrap(), b"hello world");

    // Editing the clone must NOT change the source (CoW / copy isolation).
    fs::write(&dst, b"CHANGED!!!!").unwrap();
    assert_eq!(fs::read(&src).unwrap(), b"hello world");

    let _ = fs::remove_dir_all(&dir);
}
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test -p ignition-vmm clonefile_or_copy_duplicates_and_isolates`
Expected: FAIL — compile error, `cannot find function clonefile_or_copy`.

- [ ] **Step 4: Implement the helper**

Near the top of `crates/vmm/src/snapshot.rs`, after the existing `use` lines, add:

```rust
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;

// macOS APFS copy-on-write clone. `<sys/clonefile.h>`; flags are clonefile_flags_t (u32).
unsafe extern "C" {
    fn clonefile(src: *const libc::c_char, dst: *const libc::c_char, flags: u32) -> libc::c_int;
}
```

Then add the public helper (place it above `write_snapshot`):

```rust
/// Copy `src` to `dst` using APFS `clonefile(2)` (O(1), copy-on-write) when
/// possible, falling back to a byte copy on filesystems that don't support it.
/// `dst` must not already exist. The result is always an independent file: writing
/// to it never mutates `src`.
pub fn clonefile_or_copy(src: &Path, dst: &Path) -> io::Result<()> {
    let csrc = CString::new(src.as_os_str().as_bytes()).map_err(io::Error::other)?;
    let cdst = CString::new(dst.as_os_str().as_bytes()).map_err(io::Error::other)?;
    let rc = unsafe { clonefile(csrc.as_ptr(), cdst.as_ptr(), 0) };
    if rc == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        // Not APFS, or src and dst are on different filesystems: fall back.
        Some(libc::ENOTSUP) | Some(libc::EXDEV) => {
            log::warn!(
                "clonefile unsupported ({err}); falling back to byte copy: {} -> {}",
                src.display(),
                dst.display()
            );
            fs::copy(src, dst)?;
            Ok(())
        }
        _ => Err(err),
    }
}
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p ignition-vmm clonefile_or_copy_duplicates_and_isolates`
Expected: PASS.

- [ ] **Step 6: Verify the whole crate still builds clean**

Run: `cargo clippy -p ignition-vmm -- -D warnings`
Expected: no warnings, no errors.

- [ ] **Step 7: Commit**

```bash
git add crates/vmm/Cargo.toml crates/vmm/src/snapshot.rs Cargo.lock
git commit -m "Add clonefile_or_copy helper for CoW snapshot artifacts"
```

---

### Task 2: Restore memory via clonefile + mmap(MAP_SHARED) — feasibility gate

This is the load-bearing change. It also parametrizes the restore RAM size from the
snapshot (dropping the compiled-in `RAM_SIZE` assumption on the restore path).

**Files:**
- Modify: `spike/src/bin/boot.rs` — `run_restore`, steps 1–2 and the `RAM_SIZE` uses inside `run_restore`.

- [ ] **Step 1: Add the `AsRawFd` import**

At the top of `spike/src/bin/boot.rs`, with the other `use` statements, add:

```rust
use std::os::unix::io::AsRawFd;
```

- [ ] **Step 2: Replace metadata read + RAM allocation in `run_restore`**

In `run_restore`, replace the current block (the `read_snapshot` call, the
`assert_eq!(snap.config.mem_size, RAM_SIZE, ...)`, the anonymous `mmap`, the
`fs::read(&paths.memory)` + `copy_from_slice` + `drop(mem_bytes)`) — i.e. everything
from `// 1. Read the snapshot metadata.` through the end of the old `// 2.` block — with:

```rust
    // 1. Read the snapshot metadata. RAM size comes from the snapshot, not a const.
    let (snap, gic_blob, paths) = snapshot::read_snapshot(dir)?;
    let mem_size = snap.config.mem_size;

    // The base memory image must match the recorded size before we map it.
    let base_len = fs::metadata(&paths.memory)?.len();
    if base_len != mem_size {
        return Err(io::Error::other(format!(
            "memory.bin length {base_len} != snapshot mem_size {mem_size}"
        )));
    }

    // Per-restore instance dir: CoW clones of the immutable base live here, so the
    // running guest never writes back into the snapshot dir.
    let inst_dir = std::env::temp_dir().join(format!("ignition-inst-{}", process::id()));
    let _ = fs::remove_dir_all(&inst_dir);
    fs::create_dir_all(&inst_dir)?;
    let inst_mem = inst_dir.join("memory.bin");
    snapshot::clonefile_or_copy(&paths.memory, &inst_mem)?;

    // 2. Map the instance memory.bin as guest RAM. MAP_SHARED: pages fault in lazily
    //    from the clone, and guest writes land in the clone (APFS copy-on-writes the
    //    block off the base on first write) — the base is never touched.
    let memf = fs::OpenOptions::new().read(true).write(true).open(&inst_mem)?;
    let host = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            mem_size as usize,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            memf.as_raw_fd(),
            0,
        )
    };
    if host == libc::MAP_FAILED {
        return Err(io::Error::other("mmap of instance memory.bin failed"));
    }
    drop(memf); // the mapping keeps the underlying file alive after the fd closes
    let host_addr = host as u64;
```

- [ ] **Step 3: Replace the remaining `RAM_SIZE` uses inside `run_restore`**

Still in `run_restore`, change the two remaining `RAM_SIZE` references to `mem_size`:

- The `DeviceContext { ... ram_size: RAM_SIZE, ... }` field becomes `ram_size: mem_size,`.
- The `vm.map_memory(host_addr, layout::RAM_BASE, RAM_SIZE)` call becomes
  `vm.map_memory(host_addr, layout::RAM_BASE, mem_size)`.

(The old restore disk block still uses `fs::copy` into `std::env::temp_dir()`; Task 3 changes it. Leave it for now.)

- [ ] **Step 4: Build**

Run: `cargo build -p ignition-spike --bin boot`
Expected: builds (there may be a `RAM_SIZE` "constant is never used" warning if the boot path still uses it — that's fine; Task 5 removes the const).

- [ ] **Step 5: Sign and run the live feasibility gate**

```bash
scripts/sign.sh target/debug/boot
python3 scripts/restore_test.py
```

Expected: `RESULT: snapshot=True restore_cpu=<low>% responsive=True`. The restored
guest must reach a responsive prompt. **If `responsive=False` or the guest does not
resume, STOP** — the `MAP_SHARED` file-backed guest-RAM approach is the spec's one
unverified assumption; do not proceed. Report the failure for reassessment.

- [ ] **Step 6: Commit**

```bash
git add spike/src/bin/boot.rs
git commit -m "Restore guest RAM via clonefile + mmap(MAP_SHARED), sized from snapshot"
```

---

### Task 3: Restore disk via clonefile into the instance dir

**Files:**
- Modify: `spike/src/bin/boot.rs` — the restore disk block in `run_restore`.

- [ ] **Step 1: Replace the disk copy**

In `run_restore`, replace the existing private-disk block:

```rust
    // Private disk instance so clones are independent (only if the snapshot has a disk).
    let disk = if snap.devices.iter().any(|r| r.id == "virtio-blk") {
        let instance_disk = std::env::temp_dir()
            .join(format!("ignition-instance-{}.img", process::id()));
        fs::copy(&paths.disk, &instance_disk)?;
        Some(instance_disk)
    } else {
        None
    };
```

with (clone into the same `inst_dir` created in Task 2, via the CoW helper):

```rust
    // Private CoW disk instance so clones are independent and the base disk.img is
    // never mutated (only if the snapshot has a disk).
    let disk = if snap.devices.iter().any(|r| r.id == "virtio-blk") {
        let instance_disk = inst_dir.join("disk.img");
        snapshot::clonefile_or_copy(&paths.disk, &instance_disk)?;
        Some(instance_disk)
    } else {
        None
    };
```

- [ ] **Step 2: Build**

Run: `cargo build -p ignition-spike --bin boot`
Expected: builds clean.

- [ ] **Step 3: Live re-check (rootfs restore still works)**

```bash
scripts/sign.sh target/debug/boot
python3 scripts/restore_test.py
```

Expected: `responsive=True` (the restored guest mounts its rootfs from the cloned disk).

- [ ] **Step 4: Commit**

```bash
git add spike/src/bin/boot.rs
git commit -m "Restore disk via clonefile into the instance dir"
```

---

### Task 4: Snapshot-write disk via clonefile

**Files:**
- Modify: `crates/vmm/src/snapshot.rs` — `write_snapshot`.

- [ ] **Step 1: Replace the disk copy in `write_snapshot`**

In `crates/vmm/src/snapshot.rs::write_snapshot`, change:

```rust
    fs::copy(disk_src, &p.disk)?;
```

to:

```rust
    clonefile_or_copy(disk_src, &p.disk)?;
```

- [ ] **Step 2: Build + run existing snapshot tests**

Run: `cargo test -p ignition-vmm`
Expected: all snapshot tests pass (they exercise `read_snapshot`/version/round-trip; `write_snapshot`'s disk path is now `clonefile_or_copy`, covered by Task 1's isolation test).

- [ ] **Step 3: Commit**

```bash
git add crates/vmm/src/snapshot.rs
git commit -m "Write snapshot disk artifact via clonefile"
```

---

### Task 5: `--mem` flag + parametrize the boot path

**Files:**
- Modify: `spike/src/bin/boot.rs` — arg parsing, the boot-path `RAM_SIZE` uses, the snapshot-handler closure, and the now-unused `RAM_SIZE` const.

- [ ] **Step 1: Parse `--mem` and compute the runtime RAM size**

In `main`, alongside the other flag locals (next to `let mut smp: u64 = 1;`), add:

```rust
    let mut mem_mib: u64 = 512; // default 512 MiB (the historical RAM_SIZE)
```

In the arg-parsing `match`, add a new arm (next to the `"--smp"` arm):

```rust
            "--mem" => {
                let n = it
                    .next()
                    .expect("--mem needs a value")
                    .parse::<u64>()
                    .expect("--mem value must be a number (MiB)");
                assert!((1..=65536).contains(&n), "--mem must be 1..=65536 MiB");
                mem_mib = n;
            }
```

Immediately after the arg-parsing loop and before the `if let Some(dir) = restore_dir`
block, add:

```rust
    let ram_size: u64 = mem_mib << 20; // MiB -> bytes
```

Update the usage string in `main` (the `eprintln!("usage: ...")` line) to include `[--mem MiB]`:

```rust
        eprintln!("usage: {} [--smp N] [--mem MiB] [--net] [--vsock-uds <path>] [--snap-dir <dir>] <kernel-Image> [rootfs-disk]", args[0]);
```

- [ ] **Step 2: Replace boot-path `RAM_SIZE` uses with `ram_size`**

In `main` (the fresh-boot path), change every `RAM_SIZE` to `ram_size` in these five places:

- the guest-RAM `libc::mmap(... RAM_SIZE as usize ...)` length;
- the `from_raw_parts_mut(host as *mut u8, RAM_SIZE as usize)` slice length;
- `let fdt_addr = layout::fdt_addr(RAM_SIZE);` → `layout::fdt_addr(ram_size)`;
- the `DeviceContext { ... ram_size: RAM_SIZE, ... }` field → `ram_size,` (shorthand);
- the `FdtConfig { ... mem_size: RAM_SIZE, ... }` field → `mem_size: ram_size,`;
- the `vm.map_memory(host_addr, layout::RAM_BASE, RAM_SIZE)` call → `... ram_size)`.

- [ ] **Step 3: Thread `ram_size` into the snapshot-handler closure**

The closure captures guest RAM as `host_usize`; it must also know the size. Just
before the `manager.set_snapshot_handler(Box::new(move |checkpoints...` line (next to
`let host_usize = host as usize;`), add:

```rust
        let ram_size_snap = ram_size; // u64 is Copy + Send; captured by the closure
```

Inside the closure, change the two `RAM_SIZE` uses:

- `let config = VmConfig { mem_size: RAM_SIZE, vcpu_count: ... };` → `mem_size: ram_size_snap,`;
- `std::slice::from_raw_parts(host_usize as *const u8, RAM_SIZE as usize)` → `... ram_size_snap as usize)`.

- [ ] **Step 4: Remove the now-unused `RAM_SIZE` const**

Delete the line `const RAM_SIZE: u64 = 0x2000_0000; // 512 MiB` near the top of the file.

Run: `grep -n "RAM_SIZE" spike/src/bin/boot.rs`
Expected: **no matches** (every use was replaced in Tasks 2 and 5).

- [ ] **Step 5: Build clean**

Run: `cargo clippy -p ignition-spike --bin boot -- -D warnings`
Expected: no warnings, no errors.

- [ ] **Step 6: Live check — non-default size boots and round-trips**

```bash
scripts/sign.sh target/debug/boot
# Boot with 1 GiB; the banner's dtb/gic lines should reflect a 0x40000000 RAM size.
timeout 25 target/debug/boot --mem 1024 --snap-dir /tmp/ign-mem-test kimage/out/Image kimage/out/rootfs.ext4 2>&1 | grep -E "dtb|gic|login" | head
```

Expected: kernel boots to a `login:` prompt (a too-small `--mem` would fail to boot; 1024 is ample). This confirms the runtime size flows through the mmap, FDT, and map_memory.

- [ ] **Step 7: Commit**

```bash
git add spike/src/bin/boot.rs
git commit -m "Add --mem flag; parametrize guest RAM size on the boot path"
```

---

### Task 6: Clean up the instance dir on clean exit

**Files:**
- Modify: `spike/src/bin/boot.rs` — end of `run_restore`.

- [ ] **Step 1: Remove the instance dir after the guest exits**

In `run_restore`, the tail is:

```rust
    // 8. Run: VcpuManager creates + restores the vCPU on the vCPU thread (thread-affinity).
    match manager.run_restored(snap.vcpus, Some(gic_blob)) {
        Ok(()) => {}
        Err(e) => return Err(io::Error::other(format!("run_restored: {e}"))),
    }
    Ok(())
}
```

Replace it with (best-effort cleanup after a clean guest exit; a Ctrl-A x exit calls
`process::exit` and intentionally leaves the dir — a tempdir artifact, not a
correctness issue, since the base is never mutated):

```rust
    // 8. Run: VcpuManager creates + restores the vCPU on the vCPU thread (thread-affinity).
    let run_result = manager.run_restored(snap.vcpus, Some(gic_blob));

    // Best-effort cleanup of the CoW instance dir (memory.bin + disk.img clones).
    let _ = fs::remove_dir_all(&inst_dir);

    run_result.map_err(|e| io::Error::other(format!("run_restored: {e}")))?;
    Ok(())
}
```

- [ ] **Step 2: Build**

Run: `cargo clippy -p ignition-spike --bin boot -- -D warnings`
Expected: no warnings, no errors.

- [ ] **Step 3: Live check — instance dir removed after clean exit**

```bash
scripts/sign.sh target/debug/boot
python3 scripts/restore_test.py   # spawns + SIGKILLs; for clean-exit check use the guest's own poweroff if available
ls -d /tmp/ignition-inst-* 2>/dev/null || echo "no leftover instance dirs"
```

Expected: `restore_test.py` still reports `responsive=True`. (Note: the script
SIGKILLs the restore process, which skips the cleanup — leftover dirs from a
SIGKILL run are expected and harmless. The cleanup covers the normal guest-exit
path.)

- [ ] **Step 4: Commit**

```bash
git add spike/src/bin/boot.rs
git commit -m "Remove restore instance dir on clean guest exit"
```

---

### Task 7: Restore-latency benchmark + base-immutability assertions

**Files:**
- Modify: `scripts/restore_test.py`

- [ ] **Step 1: Add an md5 helper**

Near the top of `scripts/restore_test.py`, after the imports, add:

```python
import hashlib
def md5(path):
    h = hashlib.md5()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()
```

- [ ] **Step 2: Hash the base artifacts before restore**

In Phase B, immediately after `time.sleep(1)` and before `pidB, fdB = spawn([...])`, add:

```python
base_mem = os.path.join(SNAP, "memory.bin")
base_disk = os.path.join(SNAP, "disk.img")
base_mem_md5 = md5(base_mem)
base_disk_md5 = md5(base_disk) if os.path.exists(base_disk) and os.path.getsize(base_disk) > 0 else None
t_restore = time.time()
```

- [ ] **Step 3: Record resume latency**

Replace the existing Phase B settle line:

```python
drain(fdB, 3, echo=False)            # let it settle
```

with a drain that waits for the prompt and times it:

```python
warmup = drain(fdB, 6, echo=False, until=b"login:")
restore_latency_ms = (time.time() - t_restore) * 1000.0
print(f"[restore -> prompt latency: {restore_latency_ms:.0f} ms]", flush=True)
```

- [ ] **Step 4: Assert base immutability after the guest is killed**

Replace the final result line:

```python
print(f"\nRESULT: snapshot={ok_snap} restore_cpu={avg_cpu:.1f}% responsive={responsive}")
```

with (re-hash the base after the restored guest ran and was killed):

```python
mem_unchanged = (md5(base_mem) == base_mem_md5)
disk_unchanged = (base_disk_md5 is None) or (md5(base_disk) == base_disk_md5)
print(f"[base memory.bin unchanged: {mem_unchanged}]", flush=True)
print(f"[base disk.img unchanged: {disk_unchanged}]", flush=True)
print(
    f"\nRESULT: snapshot={ok_snap} restore_cpu={avg_cpu:.1f}% "
    f"responsive={responsive} latency_ms={restore_latency_ms:.0f} "
    f"immutable_mem={mem_unchanged} immutable_disk={disk_unchanged}"
)
if not (mem_unchanged and disk_unchanged):
    sys.exit(1)
```

- [ ] **Step 5: Run the full driver**

```bash
scripts/sign.sh target/debug/boot
python3 scripts/restore_test.py
```

Expected: `RESULT: snapshot=True restore_cpu=<low>% responsive=True latency_ms=<N> immutable_mem=True immutable_disk=True`, exit 0. Note `latency_ms` — compare it against a pre-change run (e.g. `git stash` the boot.rs changes, rebuild, run, observe the old eager-read latency) to quantify the win.

- [ ] **Step 6: Commit**

```bash
git add scripts/restore_test.py
git commit -m "restore_test: measure resume latency + assert base immutability"
```

---

## Self-review

**Spec coverage:**
- Instance = CoW clone of immutable base → Tasks 2 (memory) + 3 (disk).
- Restore memory via clonefile + mmap(MAP_SHARED), lazy fault-in → Task 2.
- Restore disk via clonefile → Task 3.
- RAM size parametrized (`--mem`, read from `snap.config.mem_size`, size guard) → Tasks 2 (restore side + guard) + 5 (boot side + flag).
- Snapshot-write disk via clonefile → Task 4.
- `clonefile_or_copy` helper with ENOTSUP/EXDEV fallback → Task 1.
- Feasibility gate (MAP_SHARED file-backed guest RAM) validated before the rest → Task 2 Step 5 (earliest point a file mapping exists; Task 1 is a pure, risk-free prerequisite).
- Instance dir cleanup → Task 6.
- Unit tests (clonefile isolation, VmConfig already round-trips non-default sizes via existing tests) + live latency/immutability driver → Tasks 1 + 7.

**Placeholder scan:** none — every code step shows full code.

**Type consistency:** `clonefile_or_copy(&Path, &Path) -> io::Result<()>` defined in Task 1, called identically in Tasks 2/3/4. `mem_size` (restore) and `ram_size`/`ram_size_snap` (boot) are `u64` byte counts throughout. `inst_dir` created in Task 2 is reused in Tasks 3 and 6.

**Note on the `--mem` flag:** flag parsing lives in `main` (matching the existing untested `--smp` pattern), so it is validated by the live boot check in Task 5 Step 6 rather than a unit test.
