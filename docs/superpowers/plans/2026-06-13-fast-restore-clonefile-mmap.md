# Fast Restore via clonefile + mmap — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Restore a microVM by lazily memory-mapping a copy-on-write clone of an immutable snapshot (fast resume), parametrize guest RAM size, and add a light snapshot-placement convention (named bases under a store, auto-generated names, manifests, re-snapshot).

**Architecture:** A `--store` dir holds `snapshots/<name>/` immutable bases and `instances/<name>-<pid>/` ephemeral CoW clones. Restore `clonefile`s the base memory+disk into an instance and maps `memory.bin` `MAP_SHARED` as guest RAM, so pages fault in lazily and guest writes hit the clone, never the base. `--name` is optional (a fancy `adjective-surname` name is generated when omitted), which makes re-snapshot immutability-safe by construction.

**Tech Stack:** Rust (edition 2024), Hypervisor.framework, macOS `clonefile(2)` + `mmap(2)`, `libc`, `serde`.

**Source of truth:** `docs/superpowers/specs/2026-06-13-fast-restore-clonefile-mmap-design.md`

---

## File structure

- `crates/vmm/src/names.rs` (new) — fancy snapshot-name generator.
- `crates/vmm/src/lib.rs` — export `names`.
- `crates/vmm/src/snapshot.rs` — `clonefile_or_copy` (done), `SnapshotManifest` + manifest I/O, `base_dir`/`instance_dir` path helpers, manifest in `Paths`, disk via clonefile in `write_snapshot`.
- `spike/src/bin/boot.rs` — `--store`/`--name`/`--mem`/`--force` flags; runtime RAM size; store-path resolution; restore via clonefile + `mmap(MAP_SHARED)`; shared snapshot writer; re-snapshot handler on the restore path; instance cleanup.
- `scripts/restore_test.py` + `README.md` — new CLI, latency, immutability, re-snapshot.

---

### Task 1: `clonefile_or_copy` helper — DONE

Already implemented and reviewed (commits `9cbc8c5`, `e4f282e`): `clonefile_or_copy(src,dst)` in `crates/vmm/src/snapshot.rs` (APFS `clonefile(2)`; falls back to `fs::copy` on `ENOTSUP`/`EXDEV`/`ENOSYS`), `libc = "0.2"` added to `crates/vmm/Cargo.toml`, isolation unit test using `tempfile::tempdir()`. No action needed; listed for context.

---

### Task 2: Fancy snapshot-name generator

**Files:**
- Create: `crates/vmm/src/names.rs`
- Modify: `crates/vmm/src/lib.rs`

- [ ] **Step 1: Create `crates/vmm/src/names.rs`**

```rust
//! Memorable snapshot-name generator: `adjective-surname` (e.g. `brave-hopper`).
//! No external RNG — the seed mixes the wall clock with the pid via one splitmix64
//! step, so successive calls in a process differ as the clock advances.

use std::time::{SystemTime, UNIX_EPOCH};

const ADJECTIVES: &[&str] = &[
    "amber", "bold", "brave", "bright", "calm", "clever", "cosmic", "crimson",
    "curious", "daring", "eager", "fancy", "gentle", "golden", "happy", "hidden",
    "jolly", "keen", "lively", "lucid", "mellow", "nimble", "noble", "proud",
    "quiet", "rapid", "shiny", "silent", "smooth", "solar", "spry", "stellar",
    "swift", "tidy", "vivid", "witty", "zesty", "azure", "lunar", "mighty",
];

const SURNAMES: &[&str] = &[
    "archimedes", "babbage", "bohr", "curie", "darwin", "dirac", "einstein", "euler",
    "faraday", "fermi", "feynman", "franklin", "galileo", "galois", "gauss", "goodall",
    "hawking", "heisenberg", "hopper", "hubble", "kepler", "lamarr", "lovelace",
    "maxwell", "mendel", "newton", "noether", "pasteur", "pauli", "planck",
    "ramanujan", "sagan", "shannon", "tesla", "thompson", "turing", "volta",
    "watt", "wozniak", "yonath",
];

/// Mix the wall clock and pid into a 64-bit value via one splitmix64 round.
fn seed() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut z = nanos ^ (std::process::id() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A random `adjective-surname` name.
pub fn generate() -> String {
    let s = seed();
    let adj = ADJECTIVES[(s % ADJECTIVES.len() as u64) as usize];
    let sur = SURNAMES[((s >> 32) % SURNAMES.len() as u64) as usize];
    format!("{adj}-{sur}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_is_well_formed() {
        let n = generate();
        let (a, b) = n.split_once('-').expect("name should contain a hyphen");
        assert!(ADJECTIVES.contains(&a), "adjective part {a:?} not in list");
        assert!(SURNAMES.contains(&b), "surname part {b:?} not in list");
    }
}
```

- [ ] **Step 2: Export the module**

Add to `crates/vmm/src/lib.rs` (next to the other `pub mod` lines):

```rust
pub mod names;
```

- [ ] **Step 3: Test**

Run: `cargo test -p ignition-vmm names::`
Expected: `generate_is_well_formed` PASSES.

- [ ] **Step 4: Clippy**

Run: `cargo clippy -p ignition-vmm -- -D warnings`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add crates/vmm/src/names.rs crates/vmm/src/lib.rs
git commit -m "Add fancy snapshot-name generator (adjective-surname)"
```

---

### Task 3: Snapshot manifest + store-path helpers

**Files:**
- Modify: `crates/vmm/src/snapshot.rs`

- [ ] **Step 1: Add the time import**

At the top of `crates/vmm/src/snapshot.rs`, with the other `use` lines, add:

```rust
use std::time::{SystemTime, UNIX_EPOCH};
```

- [ ] **Step 2: Add the manifest struct + constructor**

After the `VmConfig` struct definition, add:

```rust
/// Human/management metadata for a base snapshot, written as `manifest.json`
/// alongside the machine state. Distinct from `vmstate.json` (the machine state).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotManifest {
    pub name: String,
    pub created: u64, // seconds since the Unix epoch
    pub mem_size: u64,
    pub vcpu_count: u64,
}

impl SnapshotManifest {
    pub fn new(name: String, mem_size: u64, vcpu_count: u64) -> Self {
        let created = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self { name, created, mem_size, vcpu_count }
    }
}
```

- [ ] **Step 3: Add store-path helpers**

Add (near the `paths` function):

```rust
/// `<store>/snapshots/<name>` — the immutable base directory for a named snapshot.
pub fn base_dir(store: &Path, name: &str) -> PathBuf {
    store.join("snapshots").join(name)
}

/// `<store>/instances/<name>-<pid>` — the ephemeral CoW instance directory.
pub fn instance_dir(store: &Path, name: &str, pid: u32) -> PathBuf {
    store.join("instances").join(format!("{name}-{pid}"))
}
```

- [ ] **Step 4: Add `manifest` to `Paths` and write/read helpers**

In the `Paths` struct, add a field:

```rust
    pub manifest: PathBuf,
```

In the `paths` function, add to the returned struct:

```rust
        manifest: dir.join("manifest.json"),
```

Then add the manifest I/O helpers:

```rust
/// Write `manifest.json` into an existing base directory.
pub fn write_manifest(dir: &Path, manifest: &SnapshotManifest) -> io::Result<()> {
    let json = serde_json::to_vec_pretty(manifest).map_err(io::Error::other)?;
    fs::write(paths(dir).manifest, json)
}

/// Read `manifest.json` from a base directory.
pub fn read_manifest(dir: &Path) -> io::Result<SnapshotManifest> {
    let bytes = fs::read(paths(dir).manifest)?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}
```

- [ ] **Step 5: Write tests**

Append to the `#[cfg(test)] mod tests` block in `snapshot.rs`:

```rust
#[test]
fn manifest_roundtrips() {
    let dir = tempfile::tempdir().unwrap();
    let m = SnapshotManifest::new("brave-hopper".to_string(), 1 << 30, 4);
    write_manifest(dir.path(), &m).unwrap();
    let back = read_manifest(dir.path()).unwrap();
    assert_eq!(back, m);
    assert_eq!(back.mem_size, 1 << 30);
    assert_eq!(back.vcpu_count, 4);
}

#[test]
fn store_paths_are_well_formed() {
    let store = Path::new("/tmp/vmstore");
    assert_eq!(base_dir(store, "foo"), Path::new("/tmp/vmstore/snapshots/foo"));
    assert_eq!(
        instance_dir(store, "foo", 1234),
        Path::new("/tmp/vmstore/instances/foo-1234")
    );
}
```

- [ ] **Step 6: Test + clippy**

Run: `cargo test -p ignition-vmm` then `cargo clippy -p ignition-vmm -- -D warnings`
Expected: all pass, clippy clean.

- [ ] **Step 7: Commit**

```bash
git add crates/vmm/src/snapshot.rs
git commit -m "Add snapshot manifest and store-path helpers"
```

---

### Task 4: Write snapshot disk artifact via clonefile

**Files:**
- Modify: `crates/vmm/src/snapshot.rs` — `write_snapshot`.

- [ ] **Step 1: Swap the disk copy**

In `write_snapshot`, change:

```rust
    fs::copy(disk_src, &p.disk)?;
```

to:

```rust
    clonefile_or_copy(disk_src, &p.disk)?;
```

- [ ] **Step 2: Test**

Run: `cargo test -p ignition-vmm`
Expected: all snapshot tests pass.

- [ ] **Step 3: Commit**

```bash
git add crates/vmm/src/snapshot.rs
git commit -m "Write snapshot disk artifact via clonefile"
```

---

### Task 5: Restore guest RAM via clonefile + mmap(MAP_SHARED) — FEASIBILITY GATE

The load-bearing change. Maps a file-backed (`MAP_SHARED`) clone as guest RAM and is
live-validated immediately. The instance dir is a temp path here; Task 8 moves it
under the store.

**Files:**
- Modify: `spike/src/bin/boot.rs` — `run_restore` steps 1–2 and the in-`run_restore` `RAM_SIZE` uses.

- [ ] **Step 1: Add the `AsRawFd` import**

At the top of `spike/src/bin/boot.rs`, with the other `use` lines:

```rust
use std::os::unix::io::AsRawFd;
```

- [ ] **Step 2: Replace metadata read + RAM allocation in `run_restore`**

In `run_restore`, replace everything from `// 1. Read the snapshot metadata.` through
the end of the old `// 2.` block (the `read_snapshot` call, the
`assert_eq!(snap.config.mem_size, RAM_SIZE, ...)`, the anonymous `mmap`, the
`fs::read(&paths.memory)` + `copy_from_slice` + `drop(mem_bytes)`) with:

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
    // running guest never writes back into the base. (Task 8 moves this under the store.)
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

- `DeviceContext { ... ram_size: RAM_SIZE, ... }` → `ram_size: mem_size,`.
- `vm.map_memory(host_addr, layout::RAM_BASE, RAM_SIZE)` → `... mem_size)`.

(The restore disk block still uses `fs::copy` to `temp_dir()`; Task 6 changes it.)

- [ ] **Step 4: Build**

Run: `cargo build -p ignition-spike --bin boot`
Expected: builds (a `RAM_SIZE` "never used"-style warning may remain until Task 7 — acceptable for now; do NOT silence it by deleting the const yet, the boot path still uses it).

- [ ] **Step 5: Live feasibility gate**

```bash
scripts/sign.sh target/debug/boot
python3 scripts/restore_test.py
```

Expected: `responsive=True`, low CPU. **If the restored guest does not resume
(responsive=False), STOP** — the `MAP_SHARED`-as-HVF-guest-RAM assumption failed; do
not proceed. Report for reassessment.

- [ ] **Step 6: Commit**

```bash
git add spike/src/bin/boot.rs
git commit -m "Restore guest RAM via clonefile + mmap(MAP_SHARED), sized from snapshot"
```

---

### Task 6: Restore disk via clonefile into the instance dir

**Files:**
- Modify: `spike/src/bin/boot.rs` — restore disk block in `run_restore`.

- [ ] **Step 1: Replace the disk copy**

Replace:

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

with:

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

- [ ] **Step 2: Build + live re-check**

```bash
cargo build -p ignition-spike --bin boot && scripts/sign.sh target/debug/boot
python3 scripts/restore_test.py
```

Expected: `responsive=True` (rootfs mounts from the cloned disk).

- [ ] **Step 3: Commit**

```bash
git add spike/src/bin/boot.rs
git commit -m "Restore disk via clonefile into the instance dir"
```

---

### Task 7: `--mem` flag + parametrize the boot path

**Files:**
- Modify: `spike/src/bin/boot.rs` — arg parsing, boot-path `RAM_SIZE` uses, snapshot-handler closure, the `RAM_SIZE` const.

- [ ] **Step 1: Parse `--mem`**

Alongside the other flag locals in `main` (next to `let mut smp: u64 = 1;`), add:

```rust
    let mut mem_mib: u64 = 512; // default 512 MiB (the historical RAM_SIZE)
```

In the arg-parsing `match`, add (next to the `"--smp"` arm):

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

After the arg-parsing loop and before the `if let Some(dir) = restore_dir` block, add:

```rust
    let ram_size: u64 = mem_mib << 20; // MiB -> bytes
```

- [ ] **Step 2: Replace boot-path `RAM_SIZE` with `ram_size`**

In `main` (fresh-boot path), change `RAM_SIZE` → `ram_size` in: the guest-RAM
`libc::mmap` length; the `from_raw_parts_mut(...)` length; `layout::fdt_addr(RAM_SIZE)`;
the `DeviceContext { ... ram_size: RAM_SIZE, ... }` field (→ `ram_size,` shorthand);
the `FdtConfig { ... mem_size: RAM_SIZE, ... }` field (→ `mem_size: ram_size,`); and
`vm.map_memory(host_addr, layout::RAM_BASE, RAM_SIZE)`.

- [ ] **Step 3: Thread `ram_size` into the snapshot-handler closure**

Just before `manager.set_snapshot_handler(Box::new(move |checkpoints...` (next to
`let host_usize = host as usize;`), add:

```rust
        let ram_size_snap = ram_size;
```

Inside the closure, change `mem_size: RAM_SIZE` → `mem_size: ram_size_snap` and
`from_raw_parts(host_usize as *const u8, RAM_SIZE as usize)` →
`... ram_size_snap as usize)`.

- [ ] **Step 4: Remove the `RAM_SIZE` const**

Delete `const RAM_SIZE: u64 = 0x2000_0000; // 512 MiB`.

Run: `grep -n "RAM_SIZE" spike/src/bin/boot.rs`
Expected: no matches.

- [ ] **Step 5: Clippy + live check**

```bash
cargo clippy -p ignition-spike --bin boot -- -D warnings && scripts/sign.sh target/debug/boot
timeout 25 target/debug/boot --mem 1024 --snap-dir /tmp/ign-mem-test kimage/out/Image kimage/out/rootfs.ext4 2>&1 | grep -E "dtb|gic|login" | head
```

Expected: clippy clean; boots to a `login:` prompt at 1 GiB. (NOTE: `--snap-dir` still
exists at this point; Task 8 replaces it. This step is just the size check.)

- [ ] **Step 6: Commit**

```bash
git add spike/src/bin/boot.rs
git commit -m "Add --mem flag; parametrize guest RAM size on the boot path"
```

---

### Task 8: Store/name CLI + path resolution + shared snapshot writer

Introduces `--store`/`--name`/`--force`, generates a name when omitted, routes all
snapshot reads/writes through `base_dir`/`instance_dir`, replaces `--snap-dir`, and
changes `--restore` to take a `<name>`. Extracts a shared writer both paths use.

**Files:**
- Modify: `spike/src/bin/boot.rs`

- [ ] **Step 1: Update imports**

Ensure these are imported at the top of `boot.rs` (add what's missing):

```rust
use ignition_vmm::names;
use ignition_vmm::snapshot::{self, VcpuCheckpoint, VmConfig, VmSnapshot, SnapshotManifest};
```

(`names` is new; add `SnapshotManifest` to the existing `snapshot::{...}` import.)

- [ ] **Step 2: Replace the snapshot/restore flags in `main`**

Remove the `let mut snap_dir: PathBuf = PathBuf::from("./snapshot");` and
`let mut restore_dir: Option<PathBuf> = None;` locals and their arg-parsing arms
(`"--snap-dir"` and the old `"--restore"`). Add these locals (with the others):

```rust
    let mut store: PathBuf = PathBuf::from("./vmstore");
    let mut name: Option<String> = None;
    let mut force = false;
    let mut restore_name: Option<String> = None;
```

Add these arg-parsing arms:

```rust
            "--store" => {
                store = PathBuf::from(it.next().expect("--store needs a path"));
            }
            "--name" => {
                name = Some(it.next().expect("--name needs a value").to_string());
            }
            "--force" => {
                force = true;
            }
            "--restore" => {
                restore_name = Some(it.next().expect("--restore needs a snapshot name").to_string());
            }
```

Update the usage string:

```rust
        eprintln!("usage: {} [--smp N] [--mem MiB] [--net] [--vsock-uds <path>] [--store <dir>] [--name <name>] [--force] [--restore <name>] <kernel-Image> [rootfs-disk]", args[0]);
```

- [ ] **Step 3: Update the restore dispatch in `main`**

Replace:

```rust
    // Restore path: skip normal boot entirely.
    if let Some(dir) = restore_dir {
        match run_restore(&dir, vsock_uds) {
            Ok(()) => eprintln!("\n[restore exited cleanly]"),
            Err(e) => {
                eprintln!("\n[restore error: {e}]");
                process::exit(1);
            }
        }
        return;
    }
```

with:

```rust
    // Restore path: skip normal boot entirely.
    if let Some(rname) = restore_name {
        match run_restore(&store, &rname, name.clone(), force, vsock_uds) {
            Ok(()) => eprintln!("\n[restore exited cleanly]"),
            Err(e) => {
                eprintln!("\n[restore error: {e}]");
                process::exit(1);
            }
        }
        return;
    }
```

- [ ] **Step 4: Add the shared snapshot writer (free function)**

Add this free function to `boot.rs` (above `main` or near the other helpers). It owns
the write+manifest+print logic both paths share:

```rust
/// Write a named base snapshot into `<store>/snapshots/<write_name>/`, plus its
/// manifest, and print the resolved name. Shared by the boot and restore handlers.
#[allow(clippy::too_many_arguments)]
fn write_named_snapshot(
    store: &Path,
    write_name: &str,
    ram: &[u8],
    gic_blob: &[u8],
    disk_src: &Path,
    checkpoints: Vec<VcpuCheckpoint>,
    devices: Vec<ignition_vmm::device_manager::DeviceRecord>,
    mem_size: u64,
) -> io::Result<()> {
    let base = snapshot::base_dir(store, write_name);
    let config = VmConfig { mem_size, vcpu_count: checkpoints.len() as u64 };
    let vcpu_count = config.vcpu_count;
    let snap = VmSnapshot::new(config, checkpoints, devices);
    snapshot::write_snapshot(&base, &snap, ram, gic_blob, disk_src)?;
    let manifest = SnapshotManifest::new(write_name.to_string(), mem_size, vcpu_count);
    snapshot::write_manifest(&base, &manifest)?;
    eprintln!("[snapshot] '{write_name}' written to {}", base.display());
    Ok(())
}
```

(If `DeviceRecord` is already imported under a shorter path in `boot.rs`, use that path instead of the fully-qualified one.)

- [ ] **Step 5: Resolve the boot-path write name + rework the boot handler**

In `main` (boot path), before installing the handler, add:

```rust
    let write_name = name.clone().unwrap_or_else(names::generate);
```

Rework the boot snapshot-handler closure so that, instead of computing `gic_blob`,
`devices`, `config`, `snap` and calling `snapshot::write_snapshot(&snap_dir_snap, ...)`,
it computes `gic_blob` and `devices`, quiesces the vmnet RX feeder (unchanged), builds
the `ram_slice` (unchanged), resolves `disk_src` (unchanged), then calls the shared
writer. Concretely, the captured `snap_dir_snap` becomes a captured `store` + `write_name`:

- Replace the capture `let snap_dir_snap = snap_dir.clone();` with:
  ```rust
        let store_snap = store.clone();
        let write_name_snap = write_name.clone();
  ```
- Replace the body's snapshot-write portion (from `let devices = ...` through the
  `match snapshot::write_snapshot(...) { ... }` block) with:
  ```rust
            let devices = snap_devices.save();

            // Quiesce the vmnet RX feeder so it can't write guest RAM mid-read.
            if let Some(stop) = &rx_stop_snap {
                stop.store(true, Ordering::Release);
                if let Some(net) = &net_mmio_snap {
                    drop(net.lock().unwrap());
                }
            }

            let ram_slice: &[u8] = unsafe {
                std::slice::from_raw_parts(host_usize as *const u8, ram_size_snap as usize)
            };

            let disk_src = match &disk_path_snap {
                Some(p) => PathBuf::from(p),
                None => {
                    let placeholder = std::env::temp_dir()
                        .join(format!("ignition-empty-disk-{}", process::id()));
                    let _ = std::fs::write(&placeholder, b"");
                    placeholder
                }
            };

            match write_named_snapshot(
                &store_snap, &write_name_snap, ram_slice, &gic_blob, &disk_src,
                checkpoints, devices, ram_size_snap,
            ) {
                Ok(()) => {}
                Err(e) => eprintln!("[snapshot] write failed: {e}"),
            }

            if let Some(stop) = &rx_stop_snap {
                stop.store(false, Ordering::Release);
            }
  ```
  (The empty-disk placeholder moves to a temp path because there is no longer a
  `snap_dir` to drop it into; the base dir is created by `write_snapshot`.)

Add a one-line notice near the console-attach `eprintln!` so the operator knows the
name that a snapshot will use:

```rust
    eprintln!("--- snapshots will be saved as '{write_name}' under {} ---", store.display());
```

- [ ] **Step 6: Point `run_restore` at the store; update its signature + base read + instance dir**

Change the `run_restore` signature to:

```rust
fn run_restore(
    store: &Path,
    restore_name: &str,
    name: Option<String>,
    force: bool,
    vsock_uds: Option<PathBuf>,
) -> io::Result<()> {
```

At the top of `run_restore`, derive the base dir from the store and read from it:

```rust
    let dir = snapshot::base_dir(store, restore_name);
    let dir = dir.as_path();
```

(Leave the existing `let (snap, gic_blob, paths) = snapshot::read_snapshot(dir)?;` line
as-is — it now reads `<store>/snapshots/<restore_name>/`.)

Change the instance dir line (added in Task 5) from the temp path to the store:

```rust
    let inst_dir = snapshot::instance_dir(store, restore_name, process::id());
```

For now, silence the not-yet-used params so the crate compiles (Task 9 uses them):

```rust
    let _ = (&name, force); // used by the re-snapshot handler in the next task
```

- [ ] **Step 7: Clippy + live check (new CLI)**

```bash
cargo clippy -p ignition-spike --bin boot -- -D warnings && scripts/sign.sh target/debug/boot
# boot, snapshot (the printed name), then restore it by name:
rm -rf /tmp/vmstore-t
timeout 30 target/debug/boot --store /tmp/vmstore-t kimage/out/Image kimage/out/rootfs.ext4 &
BP=$!; sleep 18; ls -R /tmp/vmstore-t/snapshots 2>/dev/null; kill $BP 2>/dev/null
```

Expected: clippy clean; after boot, `/tmp/vmstore-t/snapshots/<generated-name>/`
exists with `memory.bin`/`gic.bin`/`disk.img`/`vmstate.json`/`manifest.json` once a
snapshot is taken. (A full scripted restore-by-name is exercised in Task 11.)

- [ ] **Step 8: Commit**

```bash
git add spike/src/bin/boot.rs
git commit -m "Route snapshots through a --store/--name convention with generated names"
```

---

### Task 9: Re-snapshot from a restored guest (+ same-name guard)

**Files:**
- Modify: `spike/src/bin/boot.rs` — `run_restore`.

- [ ] **Step 1: Resolve the write name + guard, install the handler**

In `run_restore`, remove the `let _ = (&name, force);` placeholder from Task 8. After
the `VcpuManager::new(...)` line (where `manager` is created) and before
`manager.run_restored(...)`, add the re-snapshot handler. It mirrors the boot handler
but uses the restore-path captures and refuses to overwrite the restored-from base:

```rust
    // Re-snapshot: a restored guest can be snapshotted into a NEW base. An omitted
    // --name generates a fresh one (never collides with the source). An explicit
    // --name equal to the restored-from name is refused unless --force.
    let write_name = name.unwrap_or_else(names::generate);
    {
        let store_snap = store.to_path_buf();
        let write_name_snap = write_name.clone();
        let restored_from = restore_name.to_string();
        let gic_snap = gic.clone();
        let snap_devices = frozen.clone();
        let disk_snap = disk.clone();
        let host_usize = host as usize;
        let mem_size_snap = mem_size;
        manager.set_snapshot_handler(Box::new(move |checkpoints: Vec<VcpuCheckpoint>| {
            if write_name_snap == restored_from && !force {
                eprintln!(
                    "[snapshot] refusing to overwrite the base '{write_name_snap}' you are \
                     restored from; pass --force or --name <other>"
                );
                return;
            }
            let gic_blob = match gic_snap.save_state() {
                Ok(b) => b,
                Err(e) => { eprintln!("[snapshot] gic save_state failed: {e}"); return; }
            };
            let devices = snap_devices.save();
            let ram_slice: &[u8] = unsafe {
                std::slice::from_raw_parts(host_usize as *const u8, mem_size_snap as usize)
            };
            let disk_src = match &disk_snap {
                Some(p) => p.clone(),
                None => {
                    let placeholder = std::env::temp_dir()
                        .join(format!("ignition-empty-disk-{}", process::id()));
                    let _ = std::fs::write(&placeholder, b"");
                    placeholder
                }
            };
            match write_named_snapshot(
                &store_snap, &write_name_snap, ram_slice, &gic_blob, &disk_src,
                checkpoints, devices, mem_size_snap,
            ) {
                Ok(()) => {}
                Err(e) => eprintln!("[snapshot] write failed: {e}"),
            }
        }));
    }
```

Notes for the implementer:
- `frozen` in `run_restore` is currently `let frozen = mgr.freeze();` (owned). The
  handler needs to share it, so change it to `let frozen = Arc::new(mgr.freeze());`
  and update the immediately following `let bus = frozen.bus();` (it already works
  through the `Arc` deref). `Arc` is already imported in `boot.rs`.
- The restore path has no vmnet RX feeder to quiesce (net is re-wired fresh on
  restore), so the boot handler's `rx_stop`/`net_mmio` quiesce block is intentionally
  omitted here.
- Place this block AFTER `disk` and `frozen` and `host` are in scope.

- [ ] **Step 2: Clippy**

Run: `cargo clippy -p ignition-spike --bin boot -- -D warnings`
Expected: clean (no unused `name`/`force`).

- [ ] **Step 3: Build + sign**

Run: `cargo build -p ignition-spike --bin boot && scripts/sign.sh target/debug/boot`
Expected: builds. (Functional re-snapshot is verified in Task 11.)

- [ ] **Step 4: Commit**

```bash
git add spike/src/bin/boot.rs
git commit -m "Wire re-snapshot onto the restore path with a same-name guard"
```

---

### Task 10: Clean up the instance dir on clean exit

**Files:**
- Modify: `spike/src/bin/boot.rs` — end of `run_restore`.

- [ ] **Step 1: Remove the instance dir after the guest exits**

Replace the tail of `run_restore`:

```rust
    match manager.run_restored(snap.vcpus, Some(gic_blob)) {
        Ok(()) => {}
        Err(e) => return Err(io::Error::other(format!("run_restored: {e}"))),
    }
    Ok(())
}
```

with:

```rust
    let run_result = manager.run_restored(snap.vcpus, Some(gic_blob));

    // Best-effort cleanup of the CoW instance dir (memory.bin + disk.img clones).
    // A Ctrl-A x exit calls process::exit and intentionally skips this — a leftover
    // instance dir is harmless because the base is never mutated.
    let _ = fs::remove_dir_all(&inst_dir);

    run_result.map_err(|e| io::Error::other(format!("run_restored: {e}")))?;
    Ok(())
}
```

- [ ] **Step 2: Clippy**

Run: `cargo clippy -p ignition-spike --bin boot -- -D warnings`
Expected: clean.

- [ ] **Step 3: Commit**

```bash
git add spike/src/bin/boot.rs
git commit -m "Remove restore instance dir on clean guest exit"
```

---

### Task 11: Update the live driver + README

**Files:**
- Modify: `scripts/restore_test.py`
- Modify: `README.md`

- [ ] **Step 1: Switch `restore_test.py` to the `--store`/`--name` CLI**

At the top of `scripts/restore_test.py`, replace the `SNAP = ...` line with a fixed
store + name (so the test reads the base by name):

```python
STORE = os.path.join(ROOT, "vmstore-test")
NAME = "test-snap"
SNAP = os.path.join(STORE, "snapshots", NAME)  # base dir for immutability checks
```

In Phase A, change the boot spawn + cleanup to use the store/name and capture the base
by `NAME`:

```python
os.system(f"rm -rf {STORE}")
pidA, fdA = spawn(["--store", STORE, "--name", NAME, KERNEL, ROOTFS])
```

(The snapshot it writes lands in `SNAP` because `--name test-snap` is explicit.) The
existing `ok_snap` check on `os.path.join(SNAP, "memory.bin")` / `vmstate.json` still
works since `SNAP` now points at the base dir.

In Phase B, change the restore spawn to restore by name:

```python
pidB, fdB = spawn(["--store", STORE, "--restore", NAME])
```

- [ ] **Step 2: Add the md5 helper + latency timing + immutability assertions**

After the imports, add:

```python
import hashlib
def md5(path):
    h = hashlib.md5()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()
```

In Phase B, immediately before `pidB, fdB = spawn([...])`, add:

```python
base_mem = os.path.join(SNAP, "memory.bin")
base_disk = os.path.join(SNAP, "disk.img")
base_mem_md5 = md5(base_mem)
base_disk_md5 = md5(base_disk) if os.path.exists(base_disk) and os.path.getsize(base_disk) > 0 else None
t_restore = time.time()
```

Replace the settle line `drain(fdB, 3, echo=False)            # let it settle` with:

```python
warmup = drain(fdB, 6, echo=False, until=b"login:")
restore_latency_ms = (time.time() - t_restore) * 1000.0
print(f"[restore -> prompt latency: {restore_latency_ms:.0f} ms]", flush=True)
```

Replace the final result print with:

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

- [ ] **Step 3: Run the driver**

```bash
scripts/sign.sh target/debug/boot
python3 scripts/restore_test.py
```

Expected: `RESULT: snapshot=True ... responsive=True latency_ms=<N> immutable_mem=True immutable_disk=True`, exit 0.

- [ ] **Step 4: Update README**

In `README.md`, update the snapshot/restore command examples to the new CLI. Replace
`--snap-dir <dir>` usage with `--store <dir>` + optional `--name`, and `--restore <dir>`
with `--restore <name>`. Add a short note describing the store layout
(`<store>/snapshots/<name>/`, `<store>/instances/<name>-<pid>/`), the auto-generated
names, and re-snapshot (a restored guest's `Ctrl-A s` writes a new named base; same
name as the source is refused without `--force`). Update any `--snap-dir`/`--restore`
references in the snapshot section and the "console keys" lines accordingly.

- [ ] **Step 5: Commit**

```bash
git add scripts/restore_test.py README.md
git commit -m "restore_test + README: store/name CLI, latency, immutability"
```

---

## Self-review

**Spec coverage:**
- clonefile helper → Task 1 (done). Fancy names → Task 2. Manifest + store paths → Task 3.
- Snapshot-write disk via clonefile → Task 4. Manifest write → Task 8 (`write_named_snapshot`).
- Restore memory via clonefile + mmap(MAP_SHARED), sized from snapshot, feasibility gate → Task 5. Restore disk via clonefile → Task 6.
- `--mem` + boot parametrization → Task 7. Store/name/force CLI + path resolution + name generation + instance under store → Task 8. Re-snapshot + same-name guard → Task 9. Instance cleanup → Task 10. Live latency/immutability/re-snapshot + README → Task 11.

**Placeholder scan:** none — full code in every code step; prose-only steps (Task 8 handler rework, Task 11 README) reference exact existing strings to change.

**Type consistency:** `clonefile_or_copy(&Path,&Path)->io::Result<()>`, `base_dir`/`instance_dir` (Task 3) used in Tasks 5/6/8. `SnapshotManifest::new(String,u64,u64)` (Task 3) used by `write_named_snapshot` (Task 8). `write_named_snapshot(store,write_name,ram,gic_blob,disk_src,checkpoints,devices,mem_size)` defined Task 8, reused Task 9. `run_restore(store,restore_name,name,force,vsock)` signature set Task 8, used Task 9. `inst_dir` created Task 5, repointed to the store Task 8, cleaned Task 10. `ram_size`/`ram_size_snap` (boot) and `mem_size`/`mem_size_snap` (restore) are `u64` byte counts.

**Sequencing note:** Tasks 2–4 are pure-vmm (unit-testable, no entitlement). Task 5 is the live feasibility gate. Tasks 6–11 build on it. The `frozen` value in `run_restore` becomes `Arc`-wrapped in Task 9 (flagged inline).

**Untested-by-unit note:** flag parsing and the boot/restore handlers live in `main`/`run_restore` (matching the existing untested `--smp` pattern); they are validated by the live checks in Tasks 5/7/8/11.
