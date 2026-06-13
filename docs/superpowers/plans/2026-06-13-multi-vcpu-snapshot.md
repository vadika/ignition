# Multi-vCPU Snapshot/Restore Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Lift the single-vCPU snapshot restriction so an `--smp N` microVM (including `--smp N --net`) can be snapshotted and restored, recreating every online core at its saved PC.

**Architecture:** A stop-the-world rendezvous among the vCPU threads. On snapshot, `request_snapshot` interrupts every vCPU; each thread saves its own registers (HVF thread-affinity) into a shared `Vec`, all meet at a `std::sync::Barrier`, and the elected leader writes the snapshot (RAM + GIC + device records + per-vCPU state) while peers wait at a second barrier. Restore mirrors it: spawn one thread per saved core, barrier so all redistributors exist, leader runs `gic_restore`, second barrier, then each thread restores its own registers and resumes.

**Tech Stack:** Rust 2024, Apple Hypervisor.framework (`hv_vcpu_*`, in-kernel `hv_gic_*`), `std::sync::Barrier`, serde/serde_json.

---

## File Structure

- `crates/vmm/src/snapshot.rs` — snapshot schema. Replace the single `vcpu: VcpuState` with `vcpus: Vec<VcpuCheckpoint>`; define `VcpuCheckpoint`.
- `crates/vmm/src/vstate/vcpu_manager.rs` — the coordination. New barrier/collection fields, all-vCPU exit broadcast, `run_loop` rendezvous, `snapshot_active` CPU_ON freeze, multi-vCPU `run_restored`.
- `spike/src/bin/boot.rs` — wiring. Drop the `smp == 1` snapshot gate; handler takes `Vec<VcpuCheckpoint>`; restore reads `snap.vcpus` and drops the `vcpu_count == 1` assert.
- `scripts/restore_smp_test.py` — new headless driver: boot `--smp 4`, snapshot, restore, assert responsive + `nproc == 4`.

---

## Task 1: Snapshot schema — `VcpuCheckpoint` + `Vec`

**Files:**
- Modify: `crates/vmm/src/snapshot.rs`

- [ ] **Step 1: Add a failing round-trip test for a multi-vCPU snapshot**

Add this test inside the `#[cfg(test)] mod tests` block in `crates/vmm/src/snapshot.rs` (after `snapshot_roundtrips_with_device_records`):

```rust
    #[test]
    fn snapshot_roundtrips_multiple_vcpus() {
        let snap = VmSnapshot::new(
            VmConfig { mem_size: 0x2000_0000, vcpu_count: 4 },
            vec![
                VcpuCheckpoint { mpidr: 0, state: sample_vcpu() },
                VcpuCheckpoint { mpidr: 1, state: sample_vcpu() },
                VcpuCheckpoint { mpidr: 2, state: sample_vcpu() },
                VcpuCheckpoint { mpidr: 3, state: sample_vcpu() },
            ],
            vec![],
        );
        let json = serde_json::to_string(&snap).unwrap();
        let back: VmSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.config.vcpu_count, 4);
        assert_eq!(back.vcpus.len(), 4);
        let mpidrs: Vec<u64> = back.vcpus.iter().map(|c| c.mpidr).collect();
        assert_eq!(mpidrs, vec![0, 1, 2, 3]);
    }
```

- [ ] **Step 2: Run it; verify it fails to compile**

Run: `cargo test -p vmm snapshot_roundtrips_multiple_vcpus`
Expected: FAIL — `cannot find type VcpuCheckpoint` / `VmSnapshot::new` arity mismatch / no field `vcpus`.

- [ ] **Step 3: Define `VcpuCheckpoint` and switch `VmSnapshot` to a `Vec`**

In `crates/vmm/src/snapshot.rs`, add the new struct just above `VmSnapshot` (after the `VmConfig` struct, line ~18):

```rust
/// One vCPU's saved state plus the MPIDR identifying which core it is. A
/// multi-vCPU snapshot carries one of these per online core.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VcpuCheckpoint {
    pub mpidr: u64,
    pub state: VcpuState,
}
```

Replace the `vcpu` field in `VmSnapshot`:

```rust
#[derive(Debug, Serialize, Deserialize)]
pub struct VmSnapshot {
    pub magic: String,
    pub version: u32,
    pub config: VmConfig,
    pub vcpus: Vec<VcpuCheckpoint>,
    pub devices: Vec<crate::device_manager::DeviceRecord>,
}
```

Update the `new` constructor:

```rust
impl VmSnapshot {
    pub fn new(
        config: VmConfig,
        vcpus: Vec<VcpuCheckpoint>,
        devices: Vec<crate::device_manager::DeviceRecord>,
    ) -> Self {
        Self {
            magic: SNAP_MAGIC.to_string(),
            version: SNAP_VERSION,
            config,
            vcpus,
            devices,
        }
    }
}
```

- [ ] **Step 4: Fix the existing tests that pass a bare `vcpu`**

Three existing tests call `VmSnapshot::new(.., sample_vcpu(), ..)`. Update each to wrap in a one-element `Vec`. In `snapshot_roundtrips_with_device_records`, change the second argument from `sample_vcpu(),` to:

```rust
            vec![VcpuCheckpoint { mpidr: 0, state: sample_vcpu() }],
```

In `write_then_read_validates_magic` and `read_snapshot_rejects_bad_magic`, change `sample_vcpu(),` (the second `new` argument) to:

```rust
            vec![VcpuCheckpoint { mpidr: 0, state: sample_vcpu() }],
```

In `check_version_rejects_old`, the JSON literal has a `"vcpu"` key. Replace that line:

```rust
            "vcpu": serde_json::to_value(sample_vcpu()).unwrap(), "devices": []
```

with:

```rust
            "vcpus": [{"mpidr": 0, "state": serde_json::to_value(sample_vcpu()).unwrap()}], "devices": []
```

- [ ] **Step 5: Run the full snapshot test module; verify PASS**

Run: `cargo test -p vmm --lib snapshot`
Expected: PASS — all snapshot tests green, including `snapshot_roundtrips_multiple_vcpus`.

- [ ] **Step 6: Commit**

```bash
git add crates/vmm/src/snapshot.rs
git commit -m "snapshot: carry Vec<VcpuCheckpoint> for multi-vCPU state"
```

---

## Task 2: Manager snapshot coordination (stop-the-world)

**Files:**
- Modify: `crates/vmm/src/vstate/vcpu_manager.rs`

This task makes `request_snapshot` interrupt every vCPU, has each vCPU thread save its own state and rendezvous at a barrier, and elects a leader to invoke the handler. It also adds the `snapshot_active` CPU_ON freeze. The threading itself is verified live in Task 5; this task is gated by compile + clippy + the new pure-state unit test.

- [ ] **Step 1: Add a failing test for the CPU_ON freeze**

Add to the `#[cfg(test)] mod tests` block at the bottom of `crates/vmm/src/vstate/vcpu_manager.rs`:

```rust
    #[test]
    fn claim_rejected_while_snapshot_active() {
        let m = mgr(4);
        m.snapshot_active.store(true, Ordering::Relaxed);
        assert_eq!(m.claim(1), Claim::Frozen);
    }
```

- [ ] **Step 2: Run it; verify it fails to compile**

Run: `cargo test -p vmm claim_rejected_while_snapshot_active`
Expected: FAIL — no field `snapshot_active`, no variant `Claim::Frozen`.

- [ ] **Step 3: Update imports and the `Claim` enum**

At the top of `crates/vmm/src/vstate/vcpu_manager.rs`, change the `std::sync` import line:

```rust
use std::sync::{Arc, Barrier, Mutex};
```

Add an import for the checkpoint type (after the `use hvf::...` line):

```rust
use crate::snapshot::VcpuCheckpoint;
```

Add the `Frozen` variant to `Claim`:

```rust
#[derive(Debug, PartialEq, Eq)]
pub enum Claim {
    /// Newly claimed — caller should spawn the vCPU.
    Claimed,
    /// Not one of the configured MPIDRs — reject.
    Unknown,
    /// Already running — duplicate CPU_ON, reject.
    AlreadyRunning,
    /// A snapshot is in progress — CPU_ON is frozen, reject.
    Frozen,
}
```

- [ ] **Step 4: Add the new manager fields and change the handler type**

Replace the `SnapshotHandler` type alias:

```rust
/// A snapshot handler: invoked on the elected leader vCPU thread once every
/// vCPU has saved its register state. Receives the per-vCPU checkpoints and
/// performs the global capture (RAM + GIC + device records) and file write.
type SnapshotHandler = Box<dyn Fn(Vec<VcpuCheckpoint>) + Send + Sync>;
```

In `struct VcpuManager`, replace the `snapshot_req` / `snapshot_handler` fields with these (keep all other fields):

```rust
    /// Set by `request_snapshot`; cleared by the leader inside `run_loop`.
    snapshot_req: AtomicBool,
    /// True for the duration of a snapshot rendezvous; freezes CPU_ON. Read and
    /// written only while holding the `running` lock, so it cannot race a claim.
    snapshot_active: AtomicBool,
    /// Per-snapshot barrier sized to the participant count, published by
    /// `request_snapshot` and read by each vCPU thread at the rendezvous.
    snap_barrier: Mutex<Option<Arc<Barrier>>>,
    /// Each participating vCPU thread pushes `(mpidr, save_state())` here; the
    /// leader drains it after the barrier.
    collected: Mutex<Vec<(u64, Result<hvf::VcpuState, hvf::Error>)>>,
    /// Installed by the boot harness before `run`; invoked on the leader thread.
    snapshot_handler: Option<SnapshotHandler>,
```

Update `VcpuManager::new` to initialize them (replace the `snapshot_req: AtomicBool::new(false), snapshot_handler: None,` lines):

```rust
            snapshot_req: AtomicBool::new(false),
            snapshot_active: AtomicBool::new(false),
            snap_barrier: Mutex::new(None),
            collected: Mutex::new(Vec::new()),
            snapshot_handler: None,
```

- [ ] **Step 5: Drop the single-vCPU assert from `set_snapshot_handler`**

Replace the body of `set_snapshot_handler`:

```rust
    /// Install a snapshot handler. MUST be called before `run`. The handler is
    /// invoked on the leader vCPU thread (HVF thread-affinity) once every vCPU
    /// has rendezvoused and saved its state.
    pub fn set_snapshot_handler(
        self: &mut Arc<Self>,
        handler: Box<dyn Fn(Vec<VcpuCheckpoint>) + Send + Sync>,
    ) {
        let me = Arc::get_mut(self).expect("set_snapshot_handler must be called before run");
        me.snapshot_handler = Some(handler);
    }
```

- [ ] **Step 6: Rewrite `request_snapshot` to broadcast to every vCPU**

Replace the whole `request_snapshot` method:

```rust
    /// Request a snapshot. Freezes CPU_ON, latches the participant set, sizes the
    /// rendezvous barrier, and interrupts every registered vCPU so each exits to
    /// `Canceled` and joins the rendezvous. No-op if no handler is installed.
    pub fn request_snapshot(&self) {
        if self.snapshot_handler.is_none() {
            return;
        }
        // Freeze CPU_ON under the `running` lock so no claim races the latch.
        {
            let _running = self.running.lock().unwrap();
            self.snapshot_active.store(true, Ordering::Relaxed);
        }
        // Participants = the vCPUs already registered (running their loop). A
        // CPU_ON mid-spawn (claimed but not yet registered) is the documented
        // mid-boot exclusion; snapshots are taken after boot.
        let ids: Vec<u64> = self.vcpuids.lock().unwrap().clone();
        *self.snap_barrier.lock().unwrap() = Some(Arc::new(Barrier::new(ids.len())));
        self.collected.lock().unwrap().clear();
        self.snapshot_req.store(true, Ordering::Release);
        for id in ids {
            let _ = hvf::vcpu_request_exit(id);
        }
    }
```

- [ ] **Step 7: Freeze CPU_ON in `claim`**

In `claim`, add the frozen check after taking the lock (before the `running.contains` check):

```rust
    fn claim(&self, mpidr: u64) -> Claim {
        if !self.mpidrs.contains(&mpidr) {
            return Claim::Unknown;
        }
        let mut running = self.running.lock().unwrap();
        if self.snapshot_active.load(Ordering::Relaxed) {
            return Claim::Frozen;
        }
        if running.contains(&mpidr) {
            Claim::AlreadyRunning
        } else {
            running.insert(mpidr);
            Claim::Claimed
        }
    }
```

In `spawn`, handle the new variant in the `match self.claim(mpidr)` arm list (add after the `AlreadyRunning` arm):

```rust
            Claim::Frozen => {
                log::warn!("CPU_ON for mpidr {mpidr:#x} ignored: snapshot in progress");
                return;
            }
```

- [ ] **Step 8: Thread `mpidr` through `run_loop` and add the rendezvous**

Change the `run_loop` signature to take the mpidr:

```rust
    fn run_loop(self: &Arc<Self>, mpidr: u64, mut vcpu: HvfVcpu) -> Result<(), hvf::Error> {
```

Replace the `VcpuExit::Canceled => { ... }` arm with the rendezvous:

```rust
                VcpuExit::Canceled => {
                    if self.snapshot_req.load(Ordering::Acquire) {
                        // Save our own registers (HVF thread-affinity) and meet
                        // every other vCPU at the barrier.
                        let st = vcpu.save_state();
                        self.collected.lock().unwrap().push((mpidr, st));
                        let bar = self
                            .snap_barrier
                            .lock()
                            .unwrap()
                            .clone()
                            .expect("snap_barrier set when snapshot_req is set");
                        // Barrier 1: a full happens-before edge — every push is
                        // visible to the leader after this returns.
                        if bar.wait().is_leader() {
                            self.run_snapshot_leader();
                        }
                        // Barrier 2: peers wait here while the leader writes;
                        // release together and resume.
                        bar.wait();
                        continue;
                    }
                    return Ok(());
                }
```

- [ ] **Step 9: Add the leader routine**

Add this method to `impl VcpuManager` (place it right after `run_loop`):

```rust
    /// Runs on the single leader thread between the two rendezvous barriers, with
    /// every other vCPU parked. Drains the collected per-vCPU states, aborts on
    /// any save failure (no torn snapshot), else invokes the handler. Always
    /// clears the snapshot flags before returning so the second barrier resumes a
    /// clean state.
    fn run_snapshot_leader(self: &Arc<Self>) {
        let mut items = std::mem::take(&mut *self.collected.lock().unwrap());
        items.sort_by_key(|(mpidr, _)| *mpidr);

        let mut checkpoints = Vec::with_capacity(items.len());
        let mut failed = None;
        for (mpidr, res) in items {
            match res {
                Ok(state) => checkpoints.push(VcpuCheckpoint { mpidr, state }),
                Err(e) => {
                    failed = Some((mpidr, e));
                    break;
                }
            }
        }

        match failed {
            Some((mpidr, e)) => {
                log::error!("snapshot aborted: vcpu {mpidr:#x} save_state failed: {e}");
            }
            None => {
                if let Some(h) = &self.snapshot_handler {
                    // A panic in the handler must not unwind the vCPU thread.
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        h(checkpoints)
                    }));
                    if r.is_err() {
                        log::error!("snapshot handler panicked; guest resumed");
                    }
                }
            }
        }

        self.snapshot_req.store(false, Ordering::Release);
        self.snapshot_active.store(false, Ordering::Relaxed);
    }
```

- [ ] **Step 10: Update the `run_loop` call sites**

In `run_primary`, change the final line `self.run_loop(vcpu)` to:

```rust
        self.run_loop(mpidr, vcpu)
```

In `run_secondary`, change `self.run_loop(vcpu)` to:

```rust
        self.run_loop(mpidr, vcpu)
```

`run_restored_primary` is replaced wholesale in Task 3, so leave it for now (it will not compile against the new `run_loop` arity until Task 3 — that is expected; Task 2's build target is the test binary path, not the restore path). To keep the crate compiling at the end of Task 2, update `run_restored_primary`'s final call too:

```rust
        self.run_loop(mpidr, vcpu)
```

- [ ] **Step 11: Build, clippy, and run the unit test**

Run: `cargo test -p vmm --lib vcpu_manager && cargo clippy -p vmm -- -D warnings`
Expected: PASS — `claim_rejected_while_snapshot_active` and the existing `claim_*` / `mpidr_*` tests green; clippy clean.

- [ ] **Step 12: Commit**

```bash
git add crates/vmm/src/vstate/vcpu_manager.rs
git commit -m "vcpu_manager: stop-the-world snapshot rendezvous across all vCPUs"
```

---

## Task 3: Manager restore mirror (multi-vCPU `run_restored`)

**Files:**
- Modify: `crates/vmm/src/vstate/vcpu_manager.rs`

Restore spawns one thread per saved core, barriers so every redistributor exists, the leader runs `gic_restore`, a second barrier orders it before any per-vCPU register restore, then each thread restores itself and runs. Deadlock-free: even a thread whose `HvfVcpu::new` fails still reaches both barriers, sets `shutdown`, and all peers bail.

- [ ] **Step 1: Replace `run_restored` and `run_restored_primary`**

Replace the existing `run_restored` method and the `run_restored_primary` method with:

```rust
    /// Run the restore path for N cores. Spawns one thread per checkpoint, pre-
    /// seeds `running` with every restored MPIDR (so a later stray CPU_ON is
    /// rejected `AlreadyRunning`), restores the GIC once all redistributors
    /// exist, then resumes each core at its saved PC. Returns the first error.
    pub fn run_restored(
        self: &Arc<Self>,
        checkpoints: Vec<VcpuCheckpoint>,
        gic_blob: Option<Vec<u8>>,
    ) -> Result<(), hvf::Error> {
        let barrier = Arc::new(Barrier::new(checkpoints.len()));
        let gic_blob = Arc::new(gic_blob);
        {
            let mut running = self.running.lock().unwrap();
            for cp in &checkpoints {
                running.insert(cp.mpidr);
            }
        }
        for cp in checkpoints {
            let me = Arc::clone(self);
            let bar = Arc::clone(&barrier);
            let blob = Arc::clone(&gic_blob);
            let handle = thread::spawn(move || me.run_restored_one(cp, bar, blob));
            self.threads.lock().unwrap().push(handle);
        }
        self.join_all()
    }

    /// One restored vCPU thread. Two barriers bracket the GIC restore so it runs
    /// exactly once, after every redistributor exists and before any per-vCPU
    /// register restore (which writes per-cpu ICC state). A creation or GIC
    /// failure sets `shutdown`; every thread still reaches both barriers, so no
    /// peer deadlocks, and they all bail after the second barrier.
    fn run_restored_one(
        self: &Arc<Self>,
        cp: VcpuCheckpoint,
        barrier: Arc<Barrier>,
        gic_blob: Arc<Option<Vec<u8>>>,
    ) -> Result<(), hvf::Error> {
        let vcpu = HvfVcpu::new(cp.mpidr, false);
        match &vcpu {
            Ok(v) => self.vcpuids.lock().unwrap().push(v.id()),
            Err(_) => self.shutdown.store(true, Ordering::Release),
        }

        // Barrier 1: every redistributor now exists (or someone failed).
        let mut gic_err = None;
        if barrier.wait().is_leader() && !self.shutdown.load(Ordering::Acquire) {
            if let Some(blob) = gic_blob.as_ref() {
                if let Err(e) = hvf::gic::gic_restore(blob) {
                    self.shutdown.store(true, Ordering::Release);
                    gic_err = Some(e);
                }
            }
        }
        // Barrier 2: GIC restore (if any) is complete before any register restore.
        barrier.wait();

        if self.shutdown.load(Ordering::Acquire) {
            // Some thread failed creation or the GIC restore. Surface our own
            // error; otherwise bail cleanly so the failing thread's error wins.
            return match vcpu {
                Err(e) => Err(e),
                Ok(_) => gic_err.map_or(Ok(()), Err),
            };
        }

        let vcpu = vcpu.expect("not shutdown implies every vcpu was created");
        vcpu.restore_state(&cp.state)?;
        self.run_loop(cp.mpidr, vcpu)
    }
```

- [ ] **Step 2: Build the crate**

Run: `cargo build -p vmm`
Expected: PASS — `run_restored` now takes `Vec<VcpuCheckpoint>`; the crate compiles (the boot binary still references the old API and is fixed in Task 4).

- [ ] **Step 3: Clippy + existing tests**

Run: `cargo clippy -p vmm -- -D warnings && cargo test -p vmm --lib`
Expected: PASS — clippy clean, all `vmm` lib tests green.

- [ ] **Step 4: Commit**

```bash
git add crates/vmm/src/vstate/vcpu_manager.rs
git commit -m "vcpu_manager: multi-vCPU restore via two-barrier GIC ordering"
```

---

## Task 4: Boot wiring — drop the `smp == 1` gates

**Files:**
- Modify: `spike/src/bin/boot.rs`

- [ ] **Step 1: Import `VcpuCheckpoint`**

Find the `use vmm::...` imports near the top of `spike/src/bin/boot.rs`. The snapshot types come in via a `use vmm::snapshot::{...}` (or `use vmm::{... snapshot ...}`) line that already brings `VmSnapshot` and `VmConfig`. Add `VcpuCheckpoint` to that set. For example, if the line is:

```rust
use vmm::snapshot::{self, VmConfig, VmSnapshot};
```

change it to:

```rust
use vmm::snapshot::{self, VcpuCheckpoint, VmConfig, VmSnapshot};
```

(If `VmConfig`/`VmSnapshot` are imported from a different path, add `VcpuCheckpoint` alongside them at that same path — they all live in `vmm::snapshot`.)

- [ ] **Step 2: Replace the snapshot-handler install block**

Replace the entire `if smp == 1 { ... } else { eprintln!(...) }` block (the `manager.set_snapshot_handler(...)` install and its `else`) with an unconditional install. The handler closure now takes `Vec<VcpuCheckpoint>` and no longer calls `vcpu.save_state()` (the manager already collected per-vCPU state):

```rust
    // Install the snapshot handler for any vCPU count. The manager rendezvouses
    // every vCPU and hands us their checkpoints; we capture the global state
    // (GIC + RAM + device records) and write the snapshot.
    {
        let gic_snap = gic.clone();
        let snap_devices = frozen.clone();
        let disk_path_snap = disk_path.clone();
        let snap_dir_snap = snap_dir.clone();
        // The guest RAM base pointer captured as usize: raw *const u8 is neither
        // Send nor Sync, but usize is. Sound because the closure only reads the
        // slice at the rendezvous, when every vCPU is parked at the barrier. The
        // vmnet RX feeder is quiesced below before RAM is read. usize avoids the
        // 2021+ partial-capture seeing through a newtype to the *const u8 field.
        let host_usize = host as usize;

        manager.set_snapshot_handler(Box::new(move |checkpoints: Vec<VcpuCheckpoint>| {
            // Runs on the leader vCPU thread with all vCPUs parked.
            let gic_blob = match gic_snap.save_state() {
                Ok(b) => b,
                Err(e) => { eprintln!("[snapshot] gic save_state failed: {e}"); return; }
            };

            let devices = snap_devices.save();
            let config = VmConfig { mem_size: RAM_SIZE, vcpu_count: checkpoints.len() as u64 };
            let snap = VmSnapshot::new(config, checkpoints, devices);

            // Quiesce the vmnet RX feeder so it can't write guest RAM mid-read.
            if let Some(stop) = &rx_stop_snap {
                stop.store(true, Ordering::Release);
                if let Some(net) = &net_mmio_snap {
                    drop(net.lock().unwrap()); // drain any in-flight inject
                }
            }

            // The RAM slice — host_usize round-trip avoids capturing *const u8.
            let ram_slice: &[u8] = unsafe {
                std::slice::from_raw_parts(host_usize as *const u8, RAM_SIZE as usize)
            };

            let disk_src = match &disk_path_snap {
                Some(p) => PathBuf::from(p),
                None => {
                    let placeholder = snap_dir_snap.join("disk.img");
                    let _ = std::fs::write(&placeholder, b"");
                    placeholder
                }
            };

            match snapshot::write_snapshot(&snap_dir_snap, &snap, ram_slice, &gic_blob, &disk_src) {
                Ok(()) => eprintln!("[snapshot] written to {}", snap_dir_snap.display()),
                Err(e) => eprintln!("[snapshot] write failed: {e}"),
            }

            if let Some(stop) = &rx_stop_snap {
                stop.store(false, Ordering::Release);
            }
        }));
    }
```

- [ ] **Step 3: Drop the single-vCPU restore assert and use the saved vCPU count**

In `run_restore`, delete the `assert_eq!(snap.config.vcpu_count, 1, ...)` block entirely (keep the `mem_size` assert that follows it).

- [ ] **Step 4: Build the manager with the saved vCPU count and restore all cores**

In `run_restore`, find `let manager = VcpuManager::new(1, bus);` and change it to:

```rust
    let manager = VcpuManager::new(snap.config.vcpu_count, bus);
```

Then change the run call `manager.run_restored(snap.vcpu, Some(gic_blob))` to:

```rust
    match manager.run_restored(snap.vcpus, Some(gic_blob)) {
```

(keep the surrounding `match { Ok(()) => {} Err(e) => return Err(...) }`).

- [ ] **Step 5: Build, sign, clippy**

Run: `cargo build -p hvf-spike --bin boot && cargo clippy -p hvf-spike -- -D warnings`
Expected: PASS — boot binary compiles, clippy clean.

- [ ] **Step 6: Commit**

```bash
git add spike/src/bin/boot.rs
git commit -m "boot: install multi-vCPU snapshot handler; restore all saved cores"
```

---

## Task 5: Headless SMP restore driver + live verification

**Files:**
- Create: `scripts/restore_smp_test.py`

- [ ] **Step 1: Write the SMP restore driver**

Create `scripts/restore_smp_test.py`:

```python
#!/usr/bin/env python3
# Drive boot through a pty with --smp 4: boot -> snapshot (Ctrl-A s) -> restore,
# asserting the restored guest is responsive and sees all 4 cores (nproc == 4).
# Not a unit test (needs the hypervisor entitlement + a real kernel/rootfs).
import os, pty, sys, time, select, subprocess, signal

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BOOT = os.path.join(ROOT, "target/debug/boot")
KERNEL = os.path.join(ROOT, "kimage/out/Image")
ROOTFS = os.path.join(ROOT, "kimage/out/rootfs.ext4")
SNAP = os.path.join(ROOT, "snapshot_smp")
SMP = "4"

def spawn(args):
    pid, fd = pty.fork()
    if pid == 0:
        os.execv(BOOT, [BOOT] + args)
        os._exit(127)
    return pid, fd

def drain(fd, seconds, echo=False, until=None):
    out = b""
    end = time.time() + seconds
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.2)
        if r:
            try:
                d = os.read(fd, 4096)
            except OSError:
                break
            if not d:
                break
            out += d
            if echo:
                sys.stdout.buffer.write(d); sys.stdout.flush()
            if until and until in out:
                break
    return out

def cpu_pct(pid):
    try:
        o = subprocess.check_output(["ps", "-o", "%cpu=", "-p", str(pid)]).decode().strip()
        return float(o)
    except Exception:
        return -1.0

# ---- Phase A: boot --smp 4 to login, snapshot via Ctrl-A s ----
os.system(f"rm -rf {SNAP}")
pidA, fdA = spawn(["--smp", SMP, "--snap-dir", SNAP, KERNEL, ROOTFS])
print(f"=== boot phase: --smp {SMP}, waiting for login prompt ===", flush=True)
buf = drain(fdA, 30, echo=False, until=b"login:")
print(f"[boot reached login: {b'login:' in buf}]", flush=True)
time.sleep(1)
os.write(fdA, b"\x01s")
print("[sent Ctrl-A s, waiting for snapshot write]", flush=True)
drain(fdA, 8, echo=False)
ok_snap = os.path.exists(os.path.join(SNAP, "memory.bin")) and os.path.exists(os.path.join(SNAP, "vmstate.json"))
print(f"[snapshot written: {ok_snap}]", flush=True)
os.kill(pidA, signal.SIGKILL); os.waitpid(pidA, 0)
os.close(fdA)
if not ok_snap:
    print("RESULT: snapshot FAILED, abort"); sys.exit(1)

# ---- Phase B: restore, check responsiveness + core count ----
time.sleep(1)
pidB, fdB = spawn(["--restore", SNAP])
print("=== restore phase ===", flush=True)
drain(fdB, 3, echo=False)
samples = [cpu_pct(pidB) for _ in range(5) if not time.sleep(0.5)]
ok = [s for s in samples if s >= 0]
avg_cpu = sum(ok) / max(1, len(ok))
print(f"[restore CPU% samples: {samples}  avg={avg_cpu:.1f}]", flush=True)

# Log in (root, no password) and ask the guest how many cores it sees.
os.write(fdB, b"\r"); time.sleep(0.5)
drain(fdB, 2, echo=False)
os.write(fdB, b"root\r"); time.sleep(0.8)
drain(fdB, 2, echo=False)
os.write(fdB, b"nproc\r"); time.sleep(0.8)
resp = drain(fdB, 3, echo=False)
responsive = len(resp.strip()) > 0
nproc4 = b"4" in resp
print(f"[responsive: {responsive}  nproc==4: {nproc4}]", flush=True)
if resp:
    print("---- restore console after nproc ----")
    sys.stdout.buffer.write(resp[-400:]); print("\n----")
os.kill(pidB, signal.SIGKILL); os.waitpid(pidB, 0)
os.close(fdB)

print(f"\nRESULT: snapshot={ok_snap} restore_cpu={avg_cpu:.1f}% responsive={responsive} nproc4={nproc4}")
sys.exit(0 if (ok_snap and responsive and nproc4) else 1)
```

- [ ] **Step 2: Make it executable**

Run: `chmod +x scripts/restore_smp_test.py`
Expected: no output.

- [ ] **Step 3: Re-sign the boot binary (relinking strips the entitlement)**

Run: `scripts/sign.sh target/debug/boot`
Expected: codesign succeeds (the binary was rebuilt in Task 4).

- [ ] **Step 4: Run the live SMP snapshot/restore test**

Run: `python3 scripts/restore_smp_test.py`
Expected: final line `RESULT: snapshot=True restore_cpu=<low>% responsive=True nproc4=True` and exit 0. The restored guest idles at low CPU and reports 4 cores.

> If `nproc4` is False but the guest is responsive, capture the console tail and the `[restore CPU%]` line and report — this is the signal that a secondary core did not resume (rendezvous/restore ordering), not a test-harness issue.

- [ ] **Step 5: Commit**

```bash
git add scripts/restore_smp_test.py
git commit -m "scripts: headless --smp 4 snapshot/restore driver (nproc check)"
```

---

## Final verification (after all tasks)

- [ ] Run `cargo test --workspace` — all unit tests pass.
- [ ] Run `cargo clippy --workspace -- -D warnings` — clean.
- [ ] Run `python3 scripts/restore_smp_test.py` — `nproc4=True`, responsive, low CPU.
- [ ] Manual: `cargo build -p hvf-spike --bin boot && scripts/sign.sh target/debug/boot && sudo target/debug/boot --smp 2 --net kimage/out/Image kimage/out/rootfs.ext4`, snapshot with `Ctrl-A s`, then `sudo target/debug/boot --restore snapshot_smp` (or the `--snap-dir` used) — confirm both cores resume and the guest reaches the internet after the link-bounce re-init.
- [ ] Update `README.md` snapshot bullets: snapshot/restore is no longer single-vCPU-only; `--smp N` (including `--smp N --net`) is supported.
- [ ] Update the spec status note in `docs/superpowers/specs/2026-06-13-multi-vcpu-snapshot-design.md` to record the live result.

---

## Notes for the implementer

- **HVF thread-affinity:** a vCPU's registers can only be read/written on its own thread. This is *why* every vCPU thread must participate in the snapshot rendezvous — the leader cannot save another core's registers.
- **GIC blob is global:** `hv_gic_state_*` already captures the distributor plus all per-cpu redistributors. There is no per-vCPU GIC blob; per-vCPU CPU-interface (ICC) registers live inside each `VcpuState`. Order on restore: `gic_restore` (barrier 2) strictly before any `restore_state` (which writes ICC).
- **`Barrier::wait().is_leader()`** returns `true` for exactly one thread — use it to elect the single thread that writes the snapshot (Task 2) or restores the GIC (Task 3). No separate primary/secondary distinction is needed.
- **Re-sign after every build:** relinking `target/debug/boot` strips the hypervisor entitlement; run `scripts/sign.sh target/debug/boot` before any live run.
- **Snapshot after boot:** the CPU_ON freeze plus latched participant set means a snapshot taken mid-bring-up may miss a just-spawned core. This is the documented mid-boot exclusion; the live test snapshots after `login:`.
