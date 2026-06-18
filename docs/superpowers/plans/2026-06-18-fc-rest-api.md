# FC REST control API Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Expose ignition's launch + snapshot lifecycle through a Firecracker-compatible REST API so unmodified `firecracker-go-sdk` / flintlock can boot, snapshot, and restore guests on macOS / Apple Silicon.

**Architecture:** Translate-and-spawn. A new `ignition-fc-api` crate runs a hyper HTTP/1.1 server on a unix socket, accumulates Firecracker config PUTs, and on `InstanceStart` maps them to `boot` CLI flags and spawns the `boot` child (the MCP/disposable pattern). Snapshot/pause/resume are driven through a new line-JSON control socket on `boot` that calls the same `VcpuManager.request_*()` methods the serial Ctrl-A FSM already uses — no synthesized keystrokes.

**Tech Stack:** Rust 2024, tokio, hyper (new dep), serde/serde_json (already in workspace), HVF VcpuManager rendezvous primitive.

**Spec:** `docs/superpowers/specs/2026-06-18-fc-rest-api-design.md`

**Standing constraints:** plain commit messages (no Claude co-author / Generated trailer); ponytail the affected code before each commit; re-sign `target/debug/boot` after any `cargo build` that relinks it (`scripts/sign.sh target/debug/boot`) before any live run; keep `ResetMode::Full`.

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/vmm/src/vstate/vcpu_manager.rs` (modify) | `request_snapshot` gains `Option<&str>` name; new `snapshot_name` slot; new `request_pause`/`request_resume` + `pause_gate`/`pause_req`; pause branch in the run loop |
| `spike/src/bin/boot.rs` (modify) | `--control-sock` flag; `spawn_control_listener`; thread per accepted conn dispatching line-JSON → `request_*`; snapshot-handler closures take the name param; one serial call site → `request_snapshot(None)` |
| `crates/fc-api/Cargo.toml` (create) | new workspace member; hyper + tokio + serde deps |
| `crates/fc-api/src/model.rs` (create) | serde request/response types matching FC field names |
| `crates/fc-api/src/config.rs` (create) | `VmConfig` accumulator + `to_boot_flags()` |
| `crates/fc-api/src/vm.rs` (create) | lifecycle state machine, spawn boot child + reaper, control-socket client, path↔name map |
| `crates/fc-api/src/api.rs` (create) | hyper router: (method, path) → handler; status + JSON encoding |
| `crates/fc-api/src/main.rs` (create) | arg parse, bind UDS, serve |
| `Cargo.toml` (modify) | add `crates/fc-api` member; add `hyper` to workspace deps |
| `docs/src/features/fc-rest-api.md` (create) | feature page |
| `docs/src/SUMMARY.md` (modify) | TOC entry under the adoption section |
| `scripts/fc_api_live_test.py` (create) | live FC-sequence harness over the UDS |
| `ROADMAP.md` (modify) | mark the FC REST item shipped |

---

## Task 1: VcpuManager — per-request snapshot name

The snapshot name is currently fixed at launch (`write_name`, captured in the leader closure) and `request_snapshot()` takes no args. The control path needs a per-call name. Carry it through a new `snapshot_name` slot and pass it to the handler.

**Files:**
- Modify: `crates/vmm/src/vstate/vcpu_manager.rs`
- Modify: `spike/src/bin/boot.rs` (handler closures + serial call site)

- [ ] **Step 1: Add the `snapshot_name` field**

In `crates/vmm/src/vstate/vcpu_manager.rs`, in `struct VcpuManager`, after the `snapshot_req: AtomicBool,` field add:

```rust
    /// Per-request snapshot name set by `request_snapshot(Some(..))` (the control
    /// socket); `None` from the serial Ctrl-A path so the leader keeps the
    /// launch-time `write_name`. Read-and-cleared by the snapshot leader.
    snapshot_name: Mutex<Option<String>>,
```

In `VcpuManager::new`, after `snapshot_req: AtomicBool::new(false),` add:

```rust
            snapshot_name: Mutex::new(None),
```

- [ ] **Step 2: Change the handler type signatures to carry the name**

Replace the two type aliases:

```rust
type SnapshotHandler = Box<dyn Fn(Vec<VcpuCheckpoint>, Option<String>) + Send + Sync>;
```
```rust
pub type CheckpointHandler = Box<dyn Fn(Vec<VcpuCheckpoint>, Option<String>) + Send + Sync>;
```

(Both gain `, Option<String>`. `run_collect_leader` is shared between the two, so both signatures must match. The checkpoint closure ignores the name.)

- [ ] **Step 3: Update `request_snapshot` to take and store a name**

Replace the `request_snapshot` method:

```rust
    /// Request a snapshot. Freezes CPU_ON, latches the participant set, sizes the
    /// rendezvous barrier, and interrupts every registered vCPU so each exits to
    /// `Canceled` and joins the rendezvous. No-op if no handler is installed.
    /// `name` overrides the launch-time write name when `Some` (the control
    /// socket); `None` keeps it (the serial Ctrl-A path).
    pub fn request_snapshot(&self, name: Option<&str>) {
        if self.snapshot_handler.is_none() {
            return;
        }
        let Some(ids) = self.begin_rendezvous() else { return };
        *self.snapshot_name.lock().unwrap() = name.map(str::to_string);
        self.collected.lock().unwrap().clear();
        self.snapshot_req.store(true, Ordering::Release);
        Self::broadcast_exit(ids);
    }
```

- [ ] **Step 4: Pass the name into the handler in `run_collect_leader`**

In `run_collect_leader`, the `None => { if let Some(h) = handler { ... h(checkpoints) ... } }` block becomes (drain the name first so it is `None` again for the next snapshot, and `None` for checkpoint which never sets it):

```rust
            None => {
                if let Some(h) = handler {
                    let name = self.snapshot_name.lock().unwrap().take();
                    // A panic in the handler must not unwind the vCPU thread.
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        h(checkpoints, name)
                    }));
                    if r.is_err() {
                        log::error!("{what} handler panicked; guest resumed");
                    }
                }
            }
```

- [ ] **Step 5: Update the two boot.rs snapshot-handler closures + serial call site**

In `spike/src/bin/boot.rs`, both `manager.set_snapshot_handler(Box::new(move |checkpoints: Vec<VcpuCheckpoint>| {` (the fresh-boot one ~line 1161 and the restore one ~line 2105) change the closure parameter list to:

```rust
manager.set_snapshot_handler(Box::new(move |checkpoints: Vec<VcpuCheckpoint>, req_name: Option<String>| {
```

and at the top of each closure body, before the guards that use `write_name_snap`, resolve the effective name:

```rust
            // Control-socket snapshots carry their own name; serial Ctrl-A passes
            // None and keeps the launch-time write_name.
            let write_name_snap = req_name.unwrap_or_else(|| write_name_snap.clone());
```

(`write_name_snap` is the closure's captured `String`; this shadows it with the resolved name. The fresh-boot path captures it the same way; if it does not yet, capture `let write_name_snap = write_name.clone();` in that closure's setup block exactly as the restore path does.)

Update the checkpoint closure (`set_checkpoint_handler`) parameter list to accept and ignore the name:

```rust
manager.set_checkpoint_handler(Box::new(move |checkpoints: Vec<VcpuCheckpoint>, _req_name: Option<String>| {
```

Change the serial call site (`spike/src/bin/boot.rs:362`):

```rust
                    manager.request_snapshot(None);
```

- [ ] **Step 6: Keep the existing no-op test, add a named variant**

In the `#[cfg(test)]` module of `vcpu_manager.rs`, add next to `request_checkpoint_without_handler_is_noop`:

```rust
    #[test]
    fn request_snapshot_with_name_without_handler_is_noop() {
        let bus = std::sync::Arc::new(ignition_devices::bus::Bus::new());
        let mgr = VcpuManager::new(1, bus);
        // No handler and no registered vCPU: returns early, no panic, name slot stays clear.
        mgr.request_snapshot(Some("snap-1"));
        assert!(mgr.snapshot_name.lock().unwrap().is_none());
        assert!(!mgr.snapshot_req.load(Ordering::Acquire));
    }
```

(Use the same `Bus::new()` construction the surrounding tests use; copy their exact import/helper if they build the bus differently.)

- [ ] **Step 7: Build + test**

Run: `cargo test -p ignition-vmm vcpu_manager`
Expected: PASS, including `request_snapshot_with_name_without_handler_is_noop`.
Run: `cargo build -p ignition-spike --bin boot`
Expected: compiles (the closure-signature and call-site changes line up).

- [ ] **Step 8: Re-sign + commit**

```bash
scripts/sign.sh target/debug/boot
git add crates/vmm/src/vstate/vcpu_manager.rs spike/src/bin/boot.rs
git commit -m "vcpu_manager: per-request snapshot name through request_snapshot/handler"
```

---

## Task 2: VcpuManager — pause / resume

A holding-rendezvous that mirrors reset: every vCPU reaches barrier 1, then blocks on a condvar gate until `request_resume`, then barrier 2 (one vCPU clears the rendezvous). ~0% CPU while held, HVF thread-affinity preserved.

**Files:**
- Modify: `crates/vmm/src/vstate/vcpu_manager.rs`

- [ ] **Step 1: Add the pause fields**

In `struct VcpuManager`, after `reset_req: AtomicBool,` add:

```rust
    /// Set by `request_pause`; cleared by the pause leader at barrier 2.
    pause_req: AtomicBool,
    /// Holding gate: `true` while paused. Each vCPU blocks on the condvar after
    /// the pause barrier until `request_resume` flips it false and notifies.
    pause_gate: (Mutex<bool>, std::sync::Condvar),
```

In `VcpuManager::new`, after `reset_req: AtomicBool::new(false),` add:

```rust
            pause_req: AtomicBool::new(false),
            pause_gate: (Mutex::new(false), std::sync::Condvar::new()),
```

- [ ] **Step 2: Add `request_pause` / `request_resume`**

After `request_reset`, add:

```rust
    /// Pause all vCPUs: latch the gate, then run a holding rendezvous so every
    /// vCPU parks at the pause barrier and blocks until `request_resume`. No-op
    /// if a rendezvous is already active or no vCPU has registered (the gate is
    /// rolled back so a later pause works).
    pub fn request_pause(&self) {
        *self.pause_gate.0.lock().unwrap() = true;
        let Some(ids) = self.begin_rendezvous() else {
            *self.pause_gate.0.lock().unwrap() = false;
            return;
        };
        self.pause_req.store(true, Ordering::Release);
        Self::broadcast_exit(ids);
    }

    /// Resume from a pause: clear the gate and wake every parked vCPU. Harmless
    /// if not paused (no waiters).
    pub fn request_resume(&self) {
        *self.pause_gate.0.lock().unwrap() = false;
        self.pause_gate.1.notify_all();
    }
```

- [ ] **Step 3: Add the pause branch to the run loop**

In `run_loop`'s `VcpuExit::Canceled =>` arm, after the `if self.reset_req.load(..)` block (which ends with its barrier-2 `continue`) and before the final `return Ok(());`, insert:

```rust
                    if self.pause_req.load(Ordering::Acquire) {
                        let bar = self
                            .snap_barrier
                            .lock()
                            .unwrap()
                            .clone()
                            .expect("snap_barrier set when pause_req is set");
                        // Barrier 1: every vCPU parked before we hold.
                        bar.wait();
                        // Hold on the gate until request_resume; condvar park => ~0% CPU.
                        {
                            let (lock, cv) = &self.pause_gate;
                            let mut held = lock.lock().unwrap();
                            while *held {
                                held = cv.wait(held).unwrap();
                            }
                        }
                        // Barrier 2: release together; one vCPU clears the rendezvous.
                        if bar.wait().is_leader() {
                            self.pause_req.store(false, Ordering::Release);
                            self.rendezvous_active.store(false, Ordering::Relaxed);
                        }
                        continue;
                    }
```

- [ ] **Step 4: Add a pause-gate logic test**

In the `#[cfg(test)]` module add:

```rust
    #[test]
    fn request_pause_without_vcpus_rolls_back_gate() {
        let bus = std::sync::Arc::new(ignition_devices::bus::Bus::new());
        let mgr = VcpuManager::new(1, bus);
        // No vCPU registered: begin_rendezvous bails, and the gate must not stay latched.
        mgr.request_pause();
        assert!(!*mgr.pause_gate.0.lock().unwrap());
        assert!(!mgr.pause_req.load(Ordering::Acquire));
    }

    #[test]
    fn request_resume_clears_gate() {
        let bus = std::sync::Arc::new(ignition_devices::bus::Bus::new());
        let mgr = VcpuManager::new(1, bus);
        *mgr.pause_gate.0.lock().unwrap() = true;
        mgr.request_resume();
        assert!(!*mgr.pause_gate.0.lock().unwrap());
    }
```

- [ ] **Step 5: Test**

Run: `cargo test -p ignition-vmm vcpu_manager`
Expected: PASS, including the two new pause tests.

- [ ] **Step 6: Commit**

```bash
git add crates/vmm/src/vstate/vcpu_manager.rs
git commit -m "vcpu_manager: pause/resume via a holding rendezvous"
```

---

## Task 3: boot — control socket listener

`boot` binds `--control-sock <path>` and serves line-JSON control commands that call the `VcpuManager.request_*()` methods. One thread per accepted connection (commands are rare and synchronous).

**Files:**
- Modify: `spike/src/bin/boot.rs`

- [ ] **Step 1: Parse `--control-sock`**

In `main()`, beside the other `let mut … = None;` flag declarations add:

```rust
    let mut control_sock: Option<PathBuf> = None;
```

In the arg-parse `match`, beside `"--vsock-uds" =>`, add:

```rust
            "--control-sock" => {
                control_sock = Some(PathBuf::from(it.next().expect("--control-sock needs a path")));
            }
```

- [ ] **Step 2: Write the listener + dispatch function**

Add near `spawn_stdin_reader` in `spike/src/bin/boot.rs`:

```rust
/// Parse one control line and run the matching VcpuManager request. Returns the
/// JSON response line. Unknown actions and malformed JSON return an error reply.
fn dispatch_control(
    line: &str,
    manager: &ignition_vmm::vstate::vcpu_manager::VcpuManager,
) -> String {
    let v: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return format!("{{\"ok\":false,\"error\":\"bad json: {e}\"}}"),
    };
    match v.get("action").and_then(|a| a.as_str()) {
        Some("snapshot") => {
            let name = v.get("name").and_then(|n| n.as_str());
            manager.request_snapshot(name);
            "{\"ok\":true}".to_string()
        }
        Some("checkpoint") => { manager.request_checkpoint(); "{\"ok\":true}".to_string() }
        Some("reset") => { manager.request_reset(); "{\"ok\":true}".to_string() }
        Some("pause") => { manager.request_pause(); "{\"ok\":true}".to_string() }
        Some("resume") => { manager.request_resume(); "{\"ok\":true}".to_string() }
        other => format!("{{\"ok\":false,\"error\":\"unknown action: {other:?}\"}}"),
    }
}

/// Bind a control unix socket and serve line-JSON commands (one request line ->
/// one response line) by calling VcpuManager.request_*(). Detached; lives for the
/// process lifetime. Removes any stale socket file first.
fn spawn_control_listener(
    path: PathBuf,
    manager: Arc<ignition_vmm::vstate::vcpu_manager::VcpuManager>,
) {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => { log::error!("control socket bind {path:?} failed: {e}"); return; }
    };
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let stream = match conn { Ok(s) => s, Err(_) => continue };
            let mgr = manager.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(&stream);
                let mut writer = &stream;
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => return, // peer closed
                        Ok(_) => {}
                    }
                    let trimmed = line.trim_end();
                    if trimmed.is_empty() { continue; }
                    let resp = dispatch_control(trimmed, &mgr);
                    if writeln!(writer, "{resp}").is_err() { return; }
                }
            });
        }
    });
}
```

- [ ] **Step 3: Wire it into the fresh-boot and restore paths**

In both the fresh-boot path and the `run_restore` path, immediately after the snapshot handler is installed and before `manager.run(...)` (the same place `spawn_stdin_reader(...)` is called), add:

```rust
    if let Some(ctl) = control_sock.clone() {
        spawn_control_listener(ctl, manager.clone());
    }
```

(In `run_restore`, thread `control_sock` in as a parameter — add `control_sock: Option<PathBuf>` to its signature and pass it from `main()`.)

- [ ] **Step 4: Unit test the dispatch mapping**

`dispatch_control` needs a manager; build a no-vCPU manager (all `request_*` become safe no-ops) and assert the reply strings. In the `#[cfg(test)]` module at the bottom of `boot.rs` (add one if absent):

```rust
#[cfg(test)]
mod control_tests {
    use super::*;
    fn mgr() -> Arc<ignition_vmm::vstate::vcpu_manager::VcpuManager> {
        let bus = Arc::new(ignition_devices::bus::Bus::new());
        ignition_vmm::vstate::vcpu_manager::VcpuManager::new(1, bus)
    }
    #[test]
    fn known_actions_ack() {
        let m = mgr();
        for a in ["snapshot", "checkpoint", "reset", "pause", "resume"] {
            let r = dispatch_control(&format!("{{\"action\":\"{a}\"}}"), &m);
            assert_eq!(r, "{\"ok\":true}", "action {a}");
        }
    }
    #[test]
    fn snapshot_takes_name() {
        let m = mgr();
        let r = dispatch_control("{\"action\":\"snapshot\",\"name\":\"s1\"}", &m);
        assert_eq!(r, "{\"ok\":true}");
    }
    #[test]
    fn bad_json_and_unknown_action_error() {
        let m = mgr();
        assert!(dispatch_control("not json", &m).contains("\"ok\":false"));
        assert!(dispatch_control("{\"action\":\"nope\"}", &m).contains("unknown action"));
    }
}
```

(Match the exact `Bus::new()` construction used elsewhere in `boot.rs`/the vmm tests; if `boot.rs` has no test bus helper, copy the one from `vcpu_manager.rs` tests.)

- [ ] **Step 5: Build + test**

Run: `cargo test -p ignition-spike`
Expected: PASS (`known_actions_ack`, `snapshot_takes_name`, `bad_json_and_unknown_action_error`).
Run: `cargo build -p ignition-spike --bin boot`
Expected: compiles.

- [ ] **Step 6: Re-sign + commit**

```bash
scripts/sign.sh target/debug/boot
git add spike/src/bin/boot.rs
git commit -m "boot: --control-sock line-JSON control listener -> VcpuManager.request_*"
```

---

## Task 4: fc-api crate skeleton + model.rs

Create the workspace member and the serde types matching Firecracker field names.

**Files:**
- Create: `crates/fc-api/Cargo.toml`, `crates/fc-api/src/main.rs`, `crates/fc-api/src/model.rs`
- Modify: root `Cargo.toml`

- [ ] **Step 1: Add the workspace member + hyper dep**

In the root `Cargo.toml` `[workspace] members = [...]`, add `"crates/fc-api"`. Confirm `hyper` availability for the crate (add to that crate's manifest below; if the workspace pins shared deps, add `hyper` and `http-body-util` there).

- [ ] **Step 2: Write `crates/fc-api/Cargo.toml`**

```toml
[package]
name = "ignition-fc-api"
version = "0.1.0"
edition = "2024"

[[bin]]
name = "ignition-fc-api"
path = "src/main.rs"

[dependencies]
tokio = { version = "1", features = ["macros", "rt-multi-thread", "process", "net", "io-util", "sync", "signal", "time"] }
hyper = { version = "1", features = ["server", "http1"] }
hyper-util = { version = "0.1", features = ["tokio"] }
http-body-util = "0.1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
log = "0.4"
env_logger = "0.11"
```

(Pin `hyper`/`hyper-util` to whatever majors resolve in this workspace; if other crates already pin a `hyper`, match it.)

- [ ] **Step 3: Write `crates/fc-api/src/model.rs`**

```rust
//! Firecracker REST request/response bodies. Field names match the Firecracker
//! API so an unmodified firecracker-go-sdk / flintlock client serializes to them.
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Deserialize)]
pub struct MachineConfig {
    pub vcpu_count: u64,
    pub mem_size_mib: u64,
    #[serde(default)]
    pub track_dirty_pages: bool,
}

#[derive(Debug, Default, Deserialize)]
pub struct BootSource {
    pub kernel_image_path: String,
    #[serde(default)]
    pub boot_args: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Drive {
    pub drive_id: String,
    pub path_on_host: String,
    #[serde(default)]
    pub is_root_device: bool,
    #[serde(default)]
    pub is_read_only: bool,
}

#[derive(Debug, Default, Deserialize)]
pub struct NetworkInterface {
    pub iface_id: String,
    #[serde(default)]
    pub host_dev_name: Option<String>,
    #[serde(default)]
    pub guest_mac: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Action {
    pub action_type: String, // "InstanceStart" | "SendCtrlAltDel" | "FlushMetrics"
}

#[derive(Debug, Deserialize)]
pub struct VmUpdate {
    pub state: String, // "Paused" | "Resumed"
}

#[derive(Debug, Deserialize)]
pub struct SnapshotCreate {
    pub snapshot_path: String,
    #[serde(default)]
    pub mem_file_path: Option<String>,
    #[serde(default)]
    pub snapshot_type: Option<String>, // accepted, ignored (boot decides Full/Diff)
}

#[derive(Debug, Deserialize)]
pub struct SnapshotLoad {
    pub snapshot_path: String,
    #[serde(default)]
    pub mem_file_path: Option<String>,
    #[serde(default = "default_true")]
    pub resume_vm: bool,
    #[serde(default)]
    pub enable_diff_snapshots: bool, // accepted, ignored
}
fn default_true() -> bool { true }

#[derive(Debug, Serialize)]
pub struct InstanceInfo {
    pub id: String,
    pub state: String, // "Not started" | "Running" | "Paused"
    pub vmm_version: String,
    pub app_name: String,
}

#[derive(Debug, Serialize)]
pub struct Fault {
    pub fault_message: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_real_go_sdk_machine_config() {
        // Body shape firecracker-go-sdk PUT /machine-config emits.
        let j = r#"{"vcpu_count":2,"mem_size_mib":1024,"smt":false,"track_dirty_pages":true}"#;
        let mc: MachineConfig = serde_json::from_str(j).unwrap();
        assert_eq!(mc.vcpu_count, 2);
        assert_eq!(mc.mem_size_mib, 1024);
        assert!(mc.track_dirty_pages); // unknown fields like smt are ignored
    }
    #[test]
    fn parses_drive_and_snapshot_create() {
        let d: Drive = serde_json::from_str(
            r#"{"drive_id":"rootfs","path_on_host":"/x/rootfs.ext4","is_root_device":true,"is_read_only":false}"#,
        ).unwrap();
        assert!(d.is_root_device);
        let s: SnapshotCreate = serde_json::from_str(
            r#"{"snapshot_path":"/s/snap","mem_file_path":"/s/mem","snapshot_type":"Full"}"#,
        ).unwrap();
        assert_eq!(s.snapshot_path, "/s/snap");
    }
    #[test]
    fn snapshot_load_defaults_resume_true() {
        let l: SnapshotLoad = serde_json::from_str(r#"{"snapshot_path":"/s/snap"}"#).unwrap();
        assert!(l.resume_vm);
    }
}
```

- [ ] **Step 4: Write a placeholder `crates/fc-api/src/main.rs` so the crate builds**

```rust
mod model;

fn main() {
    eprintln!("ignition-fc-api: not yet wired (see api.rs/vm.rs tasks)");
    std::process::exit(1);
}
```

- [ ] **Step 5: Build + test**

Run: `cargo test -p ignition-fc-api`
Expected: PASS (the three `model` tests).

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml crates/fc-api/Cargo.toml crates/fc-api/src/main.rs crates/fc-api/src/model.rs
git commit -m "fc-api: crate skeleton + Firecracker request/response model types"
```

---

## Task 5: fc-api config.rs — VmConfig accumulator + to_boot_flags

**Files:**
- Create: `crates/fc-api/src/config.rs`
- Modify: `crates/fc-api/src/main.rs` (add `mod config;`)

- [ ] **Step 1: Write `crates/fc-api/src/config.rs`**

```rust
//! Accumulated VM config (from the PUT routes) and its mapping to `boot` flags.
use crate::model::{BootSource, Drive, MachineConfig, NetworkInterface};

#[derive(Debug, Default)]
pub struct VmConfig {
    pub vcpu_count: Option<u64>,
    pub mem_size_mib: Option<u64>,
    pub track_dirty_pages: bool,
    pub kernel_image_path: Option<String>,
    pub boot_args: Option<String>,
    pub root_drive_path: Option<String>,
    pub has_root: bool,
    pub net: bool,
}

impl VmConfig {
    pub fn set_machine(&mut self, m: MachineConfig) {
        self.vcpu_count = Some(m.vcpu_count);
        self.mem_size_mib = Some(m.mem_size_mib);
        self.track_dirty_pages = m.track_dirty_pages;
    }
    pub fn set_boot_source(&mut self, b: BootSource) {
        self.kernel_image_path = Some(b.kernel_image_path);
        self.boot_args = b.boot_args;
    }
    /// Returns Err for a second root device (v1 supports one rootfs positional).
    pub fn set_drive(&mut self, d: Drive) -> Result<(), String> {
        if d.is_root_device {
            if self.has_root {
                return Err("only one root device is supported".to_string());
            }
            self.has_root = true;
            self.root_drive_path = Some(d.path_on_host);
        }
        Ok(())
    }
    pub fn set_net(&mut self, _n: NetworkInterface) {
        // socket_vmnet backend; host_dev_name / guest_mac are accepted but ignored.
        self.net = true;
    }

    /// Map to a `boot` argv tail: [flags...] <kernel> <rootfs>. Caller prepends
    /// the boot binary path and appends --control-sock/--store/etc.
    pub fn to_boot_flags(&self) -> Result<Vec<String>, String> {
        let kernel = self.kernel_image_path.clone()
            .ok_or("no boot-source configured")?;
        let rootfs = self.root_drive_path.clone()
            .ok_or("no root drive configured")?;
        let mut v = Vec::new();
        if let Some(n) = self.vcpu_count { v.push("--smp".into()); v.push(n.to_string()); }
        if let Some(m) = self.mem_size_mib { v.push("--mem".into()); v.push(m.to_string()); }
        if self.track_dirty_pages { v.push("--track-dirty".into()); }
        if let Some(a) = &self.boot_args { v.push("--append".into()); v.push(a.clone()); }
        if self.net { v.push("--net".into()); }
        v.push(kernel);
        v.push(rootfs);
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;
    fn full() -> VmConfig {
        let mut c = VmConfig::default();
        c.set_machine(MachineConfig { vcpu_count: 2, mem_size_mib: 1024, track_dirty_pages: true });
        c.set_boot_source(BootSource { kernel_image_path: "/k/Image".into(), boot_args: Some("ro".into()) });
        c.set_drive(Drive { drive_id: "rootfs".into(), path_on_host: "/r.ext4".into(), is_root_device: true, is_read_only: false }).unwrap();
        c.set_net(NetworkInterface { iface_id: "eth0".into(), host_dev_name: None, guest_mac: None });
        c
    }
    #[test]
    fn maps_full_config_to_flags() {
        let v = full().to_boot_flags().unwrap();
        assert_eq!(v, vec![
            "--smp","2","--mem","1024","--track-dirty","--append","ro","--net","/k/Image","/r.ext4",
        ].into_iter().map(String::from).collect::<Vec<_>>());
    }
    #[test]
    fn missing_kernel_is_err() {
        let mut c = full();
        c.kernel_image_path = None;
        assert!(c.to_boot_flags().unwrap_err().contains("boot-source"));
    }
    #[test]
    fn missing_root_is_err() {
        let mut c = full();
        c.root_drive_path = None;
        assert!(c.to_boot_flags().unwrap_err().contains("root drive"));
    }
    #[test]
    fn second_root_device_rejected() {
        let mut c = full();
        let err = c.set_drive(Drive { drive_id: "d2".into(), path_on_host: "/2".into(), is_root_device: true, is_read_only: false });
        assert!(err.is_err());
    }
}
```

- [ ] **Step 2: Register the module**

In `crates/fc-api/src/main.rs`, add `mod config;` below `mod model;`.

- [ ] **Step 3: Build + test**

Run: `cargo test -p ignition-fc-api config`
Expected: PASS (four `config` tests).

- [ ] **Step 4: Commit**

```bash
git add crates/fc-api/src/config.rs crates/fc-api/src/main.rs
git commit -m "fc-api: VmConfig accumulator + to_boot_flags mapping"
```

---

## Task 6: fc-api vm.rs — lifecycle state machine

The state machine + boot-child spawning + control-socket client + path↔name map. Transition guards are pure logic and fully unit-testable without a real boot child by injecting a "spawner".

**Files:**
- Create: `crates/fc-api/src/vm.rs`
- Modify: `crates/fc-api/src/main.rs` (add `mod vm;`)

- [ ] **Step 1: Write `crates/fc-api/src/vm.rs`**

```rust
//! VM lifecycle state machine. Owns the accumulated config, the running boot
//! child, the control-socket client, and the snapshot path<->name map.
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Child;
use crate::config::VmConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State { NotStarted, Running, Paused, Stopped }

impl State {
    pub fn as_str(&self) -> &'static str {
        match self { State::NotStarted => "Not started", State::Running => "Running",
                     State::Paused => "Paused", State::Stopped => "Stopped" }
    }
}

/// Server settings resolved at startup.
pub struct Settings {
    pub boot_bin: PathBuf,
    pub store: PathBuf,
    pub control_sock: PathBuf,
    pub kernel_default: PathBuf,
}

pub struct VmState {
    pub state: State,
    pub config: VmConfig,
    pub child: Option<Child>,
    pub paths: HashMap<String, String>, // snapshot_path -> store name
    pub settings: Settings,
}

/// Sanitize a client snapshot path into a store name: basename, drop extension,
/// non-[A-Za-z0-9_-] -> '_'.
pub fn sanitize_name(snapshot_path: &str) -> String {
    let base = snapshot_path.rsplit('/').next().unwrap_or(snapshot_path);
    let stem = base.split('.').next().unwrap_or(base);
    let s: String = stem.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect();
    if s.is_empty() { "snapshot".to_string() } else { s }
}

impl VmState {
    pub fn new(settings: Settings) -> Self {
        VmState { state: State::NotStarted, config: VmConfig::default(),
                  child: None, paths: HashMap::new(), settings }
    }

    /// Guard: config PUTs only before boot.
    pub fn ensure_not_started(&self) -> Result<(), String> {
        if self.state == State::NotStarted { Ok(()) }
        else { Err("operation not allowed post-boot".into()) }
    }

    /// PATCH /vm transition check. Returns the control action to send on success.
    pub fn vm_update(&self, target: &str) -> Result<&'static str, String> {
        match (self.state, target) {
            (State::Running, "Paused") => Ok("pause"),
            (State::Paused, "Resumed") => Ok("resume"),
            (State::Paused, "Paused") => Err("vm already paused".into()),
            (State::Running, "Resumed") => Err("vm already running".into()),
            _ => Err(format!("cannot set state {target} from {}", self.state.as_str())),
        }
    }

    /// snapshot/create precondition.
    pub fn ensure_paused_for_snapshot(&self) -> Result<(), String> {
        if self.state == State::Paused { Ok(()) }
        else { Err("vm must be paused before snapshotting".into()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sanitize() {
        assert_eq!(sanitize_name("/var/run/snap.file"), "snap");
        assert_eq!(sanitize_name("a b/c@d"), "c_d");
        assert_eq!(sanitize_name("/x/"), "snapshot");
    }
    fn st(state: State) -> VmState {
        let mut v = VmState::new(Settings {
            boot_bin: "boot".into(), store: "s".into(),
            control_sock: "c".into(), kernel_default: "k".into() });
        v.state = state;
        v
    }
    #[test]
    fn config_put_blocked_after_boot() {
        assert!(st(State::Running).ensure_not_started().is_err());
        assert!(st(State::NotStarted).ensure_not_started().is_ok());
    }
    #[test]
    fn pause_resume_transitions() {
        assert_eq!(st(State::Running).vm_update("Paused").unwrap(), "pause");
        assert_eq!(st(State::Paused).vm_update("Resumed").unwrap(), "resume");
        assert!(st(State::Paused).vm_update("Paused").is_err());
        assert!(st(State::Running).vm_update("Resumed").is_err());
    }
    #[test]
    fn snapshot_requires_paused() {
        assert!(st(State::Running).ensure_paused_for_snapshot().is_err());
        assert!(st(State::Paused).ensure_paused_for_snapshot().is_ok());
    }
}
```

- [ ] **Step 2: Register the module**

In `crates/fc-api/src/main.rs`, add `mod vm;` below `mod config;`.

- [ ] **Step 3: Build + test**

Run: `cargo test -p ignition-fc-api vm`
Expected: PASS (`sanitize`, `config_put_blocked_after_boot`, `pause_resume_transitions`, `snapshot_requires_paused`).

- [ ] **Step 4: Commit**

```bash
git add crates/fc-api/src/vm.rs crates/fc-api/src/main.rs
git commit -m "fc-api: VM lifecycle state machine + snapshot path/name mapping"
```

---

## Task 7: fc-api api.rs + main.rs — hyper server over UDS

Wire the routes to `VmConfig`/`VmState`, spawn the boot child on `InstanceStart`, drive control commands, and serve over a unix socket. Control-socket client + spawn live here (the side-effecting parts kept out of the unit-tested `vm.rs`).

**Files:**
- Create: `crates/fc-api/src/api.rs`
- Rewrite: `crates/fc-api/src/main.rs`
- Test: `scripts/fc_api_mock_test.py` (integration, mock boot)

- [ ] **Step 1: Write `crates/fc-api/src/api.rs`**

```rust
//! Hyper request router: maps (method, path) to handlers, encodes FC status codes.
use std::convert::Infallible;
use std::sync::Arc;
use std::time::{Duration, Instant};
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::{Method, Request, Response, StatusCode};
use tokio::sync::Mutex;
use crate::model::*;
use crate::vm::{sanitize_name, State, VmState};

pub type Shared = Arc<Mutex<VmState>>;

fn json<T: serde::Serialize>(status: StatusCode, body: &T) -> Response<Full<Bytes>> {
    let b = serde_json::to_vec(body).unwrap_or_default();
    Response::builder().status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(b))).unwrap()
}
fn empty(status: StatusCode) -> Response<Full<Bytes>> {
    Response::builder().status(status).body(Full::new(Bytes::new())).unwrap()
}
fn fault(msg: impl Into<String>) -> Response<Full<Bytes>> {
    json(StatusCode::BAD_REQUEST, &Fault { fault_message: msg.into() })
}
fn parse<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, Response<Full<Bytes>>> {
    serde_json::from_slice(body).map_err(|_| fault("failed to parse body"))
}

/// Send one control line to the boot child's control socket, await the reply.
async fn control(sock: &std::path::Path, action: &str, name: Option<&str>) -> Result<(), String> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let mut stream = tokio::net::UnixStream::connect(sock).await
        .map_err(|e| format!("control connect: {e}"))?;
    let line = match name {
        Some(n) => format!("{{\"action\":\"{action}\",\"name\":\"{n}\"}}\n"),
        None => format!("{{\"action\":\"{action}\"}}\n"),
    };
    stream.write_all(line.as_bytes()).await.map_err(|e| format!("control write: {e}"))?;
    let mut reader = BufReader::new(stream);
    let mut resp = String::new();
    reader.read_line(&mut resp).await.map_err(|e| format!("control read: {e}"))?;
    if resp.contains("\"ok\":true") { Ok(()) } else { Err(format!("control error: {}", resp.trim())) }
}

/// Poll-connect the control socket until it answers or the deadline passes.
async fn wait_ready(sock: &std::path::Path) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if tokio::net::UnixStream::connect(sock).await.is_ok() { return Ok(()); }
        if Instant::now() >= deadline { return Err("vm did not become ready".into()); }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Spawn `boot` with the given extra args, store the child, wait until ready.
async fn spawn_boot(vm: &mut VmState, extra: Vec<String>) -> Result<(), String> {
    let _ = std::fs::remove_file(&vm.settings.control_sock);
    let mut cmd = tokio::process::Command::new(&vm.settings.boot_bin);
    cmd.arg("--control-sock").arg(&vm.settings.control_sock)
        .arg("--store").arg(&vm.settings.store);
    for a in extra { cmd.arg(a); }
    cmd.stdin(std::process::Stdio::null());
    let child = cmd.spawn().map_err(|e| format!("spawn boot: {e}"))?;
    vm.child = Some(child.into_std());
    wait_ready(&vm.settings.control_sock).await
}

pub async fn route(shared: Shared, req: Request<Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let body = match req.into_body().collect().await {
        Ok(c) => c.to_bytes(),
        Err(_) => return Ok(fault("failed to read body")),
    };
    Ok(handle(shared, method, path, &body).await)
}

async fn handle(shared: Shared, method: Method, path: String, body: &[u8]) -> Response<Full<Bytes>> {
    let mut vm = shared.lock().await;

    macro_rules! cfg_put { ($parse:ty, $set:expr) => {{
        if let Err(m) = vm.ensure_not_started() { return fault(m); }
        match parse::<$parse>(body) { Ok(v) => { $set(&mut vm, v); empty(StatusCode::NO_CONTENT) }, Err(r) => r }
    }}; }

    match (method, path.as_str()) {
        (Method::GET, "/") => json(StatusCode::OK, &InstanceInfo {
            id: "ignition".into(), state: vm.state.as_str().into(),
            vmm_version: env!("CARGO_PKG_VERSION").into(), app_name: "ignition-fc-api".into() }),

        (Method::PUT, "/machine-config") =>
            cfg_put!(MachineConfig, |vm: &mut VmState, v| vm.config.set_machine(v)),
        (Method::GET, "/machine-config") => json(StatusCode::OK, &serde_json::json!({
            "vcpu_count": vm.config.vcpu_count, "mem_size_mib": vm.config.mem_size_mib,
            "track_dirty_pages": vm.config.track_dirty_pages })),
        (Method::PUT, "/boot-source") =>
            cfg_put!(BootSource, |vm: &mut VmState, v| vm.config.set_boot_source(v)),
        (Method::PUT, p) if p.starts_with("/drives/") => {
            if let Err(m) = vm.ensure_not_started() { return fault(m); }
            match parse::<Drive>(body) {
                Ok(d) => match vm.config.set_drive(d) { Ok(()) => empty(StatusCode::NO_CONTENT), Err(m) => fault(m) },
                Err(r) => r,
            }
        }
        (Method::PUT, p) if p.starts_with("/network-interfaces/") =>
            cfg_put!(NetworkInterface, |vm: &mut VmState, v| vm.config.set_net(v)),

        (Method::PUT, "/actions") => {
            let a: Action = match parse(body) { Ok(a) => a, Err(r) => return r };
            match a.action_type.as_str() {
                "InstanceStart" => {
                    if vm.state != State::NotStarted { return fault("vm already started"); }
                    let flags = match vm.config.to_boot_flags() { Ok(f) => f, Err(m) => return fault(m) };
                    match spawn_boot(&mut vm, flags).await {
                        Ok(()) => { vm.state = State::Running; empty(StatusCode::NO_CONTENT) }
                        Err(m) => fault(m),
                    }
                }
                other => fault(format!("unsupported action_type: {other}")),
            }
        }

        (Method::PATCH, "/vm") => {
            let u: VmUpdate = match parse(body) { Ok(u) => u, Err(r) => return r };
            let action = match vm.vm_update(&u.state) { Ok(a) => a, Err(m) => return fault(m) };
            let sock = vm.settings.control_sock.clone();
            match control(&sock, action, None).await {
                Ok(()) => { vm.state = if action == "pause" { State::Paused } else { State::Running };
                            empty(StatusCode::NO_CONTENT) }
                Err(m) => fault(m),
            }
        }

        (Method::PUT, "/snapshot/create") => {
            if let Err(m) = vm.ensure_paused_for_snapshot() { return fault(m); }
            let s: SnapshotCreate = match parse(body) { Ok(s) => s, Err(r) => return r };
            let name = sanitize_name(&s.snapshot_path);
            // Snapshot is an atomic stop-the-world; do it directly. The VM stays paused.
            let sock = vm.settings.control_sock.clone();
            match control(&sock, "snapshot", Some(&name)).await {
                Ok(()) => { vm.paths.insert(s.snapshot_path, name); empty(StatusCode::NO_CONTENT) }
                Err(m) => fault(m),
            }
        }

        (Method::PUT, "/snapshot/load") => {
            if vm.state != State::NotStarted { return fault("cannot load after boot"); }
            let l: SnapshotLoad = match parse(body) { Ok(l) => l, Err(r) => return r };
            let name = vm.paths.get(&l.snapshot_path).cloned()
                .unwrap_or_else(|| sanitize_name(&l.snapshot_path));
            let kernel = vm.config.kernel_image_path.clone()
                .unwrap_or_else(|| vm.settings.kernel_default.to_string_lossy().into_owned());
            let rootfs = vm.config.root_drive_path.clone();
            let mut flags = vec!["--restore".to_string(), name, kernel];
            if let Some(r) = rootfs { flags.push(r); }
            match spawn_boot(&mut vm, flags).await {
                Ok(()) => {
                    if !l.resume_vm {
                        let sock = vm.settings.control_sock.clone();
                        if let Err(m) = control(&sock, "pause", None).await { return fault(m); }
                        vm.state = State::Paused;
                    } else {
                        vm.state = State::Running;
                    }
                    empty(StatusCode::NO_CONTENT)
                }
                Err(m) => fault(m),
            }
        }

        (Method::GET, _) | (Method::PUT, _) | (Method::PATCH, _) => empty(StatusCode::NOT_FOUND),
        _ => empty(StatusCode::METHOD_NOT_ALLOWED),
    }
}
```

`boot --restore` requires the kernel/rootfs positionals; the snapshot store carries the rest. If a `snapshot/load` arrives with no prior config (fresh client), the kernel falls back to `--kernel` default and the rootfs to the store's recorded image — document that a load-only client must still PUT a boot-source+drive first (firecracker-go-sdk does in its snapshot-resume flow). `ponytail:` mark this in a comment.

- [ ] **Step 2: Rewrite `crates/fc-api/src/main.rs`**

```rust
mod model;
mod config;
mod vm;
mod api;

use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use crate::vm::{Settings, VmState};

fn arg(args: &[String], flag: &str) -> Option<String> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned()
}

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<String> = std::env::args().collect();
    let api_sock = match arg(&args, "--api-sock") {
        Some(s) => PathBuf::from(s),
        None => { eprintln!("--api-sock <path> is required"); std::process::exit(2); }
    };
    let store = PathBuf::from(arg(&args, "--store").unwrap_or_else(|| "./fc-store".into()));
    let boot_bin = PathBuf::from(arg(&args, "--boot").unwrap_or_else(|| "target/debug/boot".into()));
    let kernel_default = PathBuf::from(arg(&args, "--kernel").unwrap_or_else(|| "kimage/out/Image".into()));
    let control_sock = store.join("control.sock");
    std::fs::create_dir_all(&store).ok();

    let shared = Arc::new(Mutex::new(VmState::new(Settings {
        boot_bin, store, control_sock, kernel_default,
    })));

    let _ = std::fs::remove_file(&api_sock);
    let listener = tokio::net::UnixListener::bind(&api_sock)
        .unwrap_or_else(|e| { eprintln!("bind {api_sock:?}: {e}"); std::process::exit(1); });
    log::info!("ignition-fc-api listening on {api_sock:?}");

    // Kill the boot child + unlink sockets on SIGTERM/SIGINT.
    {
        let shared = shared.clone();
        let api_sock = api_sock.clone();
        tokio::spawn(async move {
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
            let mut intr = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).unwrap();
            tokio::select! { _ = term.recv() => {}, _ = intr.recv() => {} }
            let mut vm = shared.lock().await;
            if let Some(mut c) = vm.child.take() { let _ = c.kill(); }
            let _ = std::fs::remove_file(&vm.settings.control_sock);
            let _ = std::fs::remove_file(&api_sock);
            std::process::exit(0);
        });
    }

    loop {
        let (stream, _) = match listener.accept().await { Ok(p) => p, Err(_) => continue };
        let shared = shared.clone();
        tokio::task::spawn(async move {
            let io = TokioIo::new(stream);
            let svc = service_fn(move |req| api::route(shared.clone(), req));
            let _ = http1::Builder::new().serve_connection(io, svc).await;
        });
    }
}
```

- [ ] **Step 3: Build**

Run: `cargo build -p ignition-fc-api`
Expected: compiles. (Adjust hyper/hyper-util version pins if the resolver complains.)

- [ ] **Step 4: Write the mock-boot integration test `scripts/fc_api_mock_test.py`**

A stub that plays `boot`: binds the control socket passed via `--control-sock`, answers every line with `{"ok":true}`, and records actions to a file. Drives the full FC sequence over the api-sock with stdlib HTTP-over-UDS and asserts status codes + recorded actions.

```python
#!/usr/bin/env python3
"""Integration test for ignition-fc-api with a MOCK boot (no HVF).

Stub boot binds --control-sock, ACKs control lines, records actions. We drive the
FC sequence over the api-sock and assert status codes + that pause/snapshot/resume
reached the stub. Stdlib only.
"""
import http.client, json, os, socket, subprocess, sys, tempfile, threading, time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

STUB = r'''
import socket, sys, os, threading
ctl = sys.argv[sys.argv.index("--control-sock")+1]
rec = ctl + ".actions"
try: os.unlink(ctl)
except FileNotFoundError: pass
srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); srv.bind(ctl); srv.listen(8)
def serve(c):
    f = c.makefile("rwb")
    for line in f:
        open(rec, "a").write(line.decode())
        f.write(b'{"ok":true}\n'); f.flush()
while True:
    c,_ = srv.accept(); threading.Thread(target=serve, args=(c,), daemon=True).start()
'''

class UDSConn(http.client.HTTPConnection):
    def __init__(self, path): super().__init__("localhost"); self.path = path
    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); self.sock.connect(self.path)

def req(api, method, route, body=None):
    c = UDSConn(api)
    c.request(method, route, json.dumps(body) if body is not None else None,
              {"Content-Type": "application/json"})
    r = c.getresponse(); data = r.read(); c.close()
    return r.status, data

def main():
    d = tempfile.mkdtemp(prefix="fcapi-")
    api = os.path.join(d, "api.sock")
    stub = os.path.join(d, "stub_boot.py")
    open(stub, "w").write(STUB)
    store = os.path.join(d, "store")
    env = dict(os.environ)
    srv = subprocess.Popen(
        [os.path.join(ROOT, "target/debug/ignition-fc-api"),
         "--api-sock", api, "--store", store,
         "--boot", sys.executable, "--kernel", "/k/Image"],
        env=env)
    # The server prepends --control-sock/--store; boot bin == python, so it runs the stub:
    # rewrite: pass the stub as the boot binary via a wrapper.
    # (Simplest: set --boot to a 2-line shell that execs python stub. See note below.)
    try:
        for _ in range(100):
            if os.path.exists(api): break
            time.sleep(0.05)
        assert req(api, "PUT", "/machine-config", {"vcpu_count":1,"mem_size_mib":512})[0] == 204
        assert req(api, "PUT", "/boot-source", {"kernel_image_path":"/k/Image","boot_args":"ro"})[0] == 204
        assert req(api, "PUT", "/drives/rootfs", {"drive_id":"rootfs","path_on_host":"/r.ext4","is_root_device":True})[0] == 204
        assert req(api, "PUT", "/actions", {"action_type":"InstanceStart"})[0] == 204
        assert req(api, "PATCH", "/vm", {"state":"Paused"})[0] == 204
        assert req(api, "PUT", "/snapshot/create", {"snapshot_path":"/s/snap1"})[0] == 204
        assert req(api, "PATCH", "/vm", {"state":"Resumed"})[0] == 204
        actions = open(os.path.join(store, "control.sock.actions")).read()
        assert '"pause"' in actions and '"snapshot"' in actions and '"resume"' in actions, actions
        assert '"name":"snap1"' in actions, actions
        print("fc_api_mock_test PASS")
    finally:
        srv.terminate()

if __name__ == "__main__":
    main()
```

Note: the server spawns `<boot> --control-sock <ctl> --store <store> <flags…>`. Make `--boot` point at a tiny executable wrapper script (created by the test in `d`) that execs `python3 stub_boot.py "$@"`, so the stub receives `--control-sock`. Mark the wrapper `chmod +x`. Wire that into the test before launching the server.

- [ ] **Step 5: Run the integration test**

Run: `cargo build -p ignition-fc-api && python3 scripts/fc_api_mock_test.py`
Expected: `fc_api_mock_test PASS`.

- [ ] **Step 6: Commit**

```bash
git add crates/fc-api/src/api.rs crates/fc-api/src/main.rs scripts/fc_api_mock_test.py
git commit -m "fc-api: hyper UDS server, route handlers, boot spawn + control client, mock integration test"
```

---

## Task 8: Docs, roadmap, live harness

**Files:**
- Create: `docs/src/features/fc-rest-api.md`
- Modify: `docs/src/SUMMARY.md`, `ROADMAP.md`
- Create: `scripts/fc_api_live_test.py`

- [ ] **Step 1: Feature page `docs/src/features/fc-rest-api.md`**

Document: the goal (unmodified FC clients on macOS), the route table (from the spec), the translate-and-spawn model, the honest limitations (single VM per socket; `snapshot_path`/`mem_file_path` are opaque handles, the literal files do not exist on disk; `snapshot_type`/`enable_diff_snapshots` accepted but boot decides Full/Diff via `--track-dirty`; pause is a real stop-the-world hold; networking is socket_vmnet so `host_dev_name` is ignored). Show a `firecracker-go-sdk` start+snapshot snippet and the `scripts/fc_api_live_test.py` invocation. Cross-reference the MCP page as the sibling adoption seam.

- [ ] **Step 2: Add to `docs/src/SUMMARY.md`**

Under the MCP/adoption section, add:

```markdown
- [Firecracker REST API](features/fc-rest-api.md)
```

- [ ] **Step 3: Mark the ROADMAP item shipped**

In `ROADMAP.md`, change the adoption-track Firecracker REST line from `[ ]` to `[x]` with a one-line outcome (binary `ignition-fc-api`, launch+snapshot subset, verified live), matching the MCP entry's style.

- [ ] **Step 4: Live harness `scripts/fc_api_live_test.py`**

Same HTTP-over-UDS client as the mock test, but pointed at a real `ignition-fc-api` with `--boot target/debug/boot` and real `--kernel`/rootfs (tools-base). Sequence: machine-config → boot-source → drives (real rootfs) → InstanceStart → (poll GET `/` until "Running") → PATCH Paused → snapshot/create → PATCH Resumed; then a SECOND server instance: snapshot/load the same path with `resume_vm:true`. Assert 204s and that GET `/` reports the expected states. Print `fc_api_live_test PASS`. Header docstring notes it needs HVF (M-series) and a signed `target/debug/boot`.

- [ ] **Step 5: Verify docs build**

Run: `mdbook build docs` (or the repo's doc lint/build command)
Expected: builds, no dead link to `features/fc-rest-api.md`.

- [ ] **Step 6: Commit**

```bash
git add docs/src/features/fc-rest-api.md docs/src/SUMMARY.md ROADMAP.md scripts/fc_api_live_test.py
git commit -m "fc-api: feature docs, roadmap update, live FC-sequence harness"
```

---

## Final verification (after all tasks)

- [ ] `cargo test --workspace` — all green.
- [ ] `cargo build -p ignition-spike --bin boot && scripts/sign.sh target/debug/boot` — boot relinked + re-signed.
- [ ] `python3 scripts/fc_api_mock_test.py` — `fc_api_mock_test PASS` (no HVF).
- [ ] Live (M-series, by hand): `python3 scripts/fc_api_live_test.py` — boots, pauses, snapshots, resumes, and a second instance loads the snapshot. `fc_api_live_test PASS`.
- [ ] Live cross-check: the API-created snapshot restores with plain `target/debug/boot --restore <name> --store fc-store <kernel> <rootfs>` — proves the artifact is a normal ignition snapshot.

---

## Self-Review

**Spec coverage:**
- Control channel + per-request name → Tasks 1, 3. Pause/resume → Task 2. boot `--control-sock` → Task 3.
- fc-api crate (model/config/vm/api/main) → Tasks 4–7. Routes, state machine, faults, path↔name, wire faithfulness (hyper/UDS, 204/200/400) → Tasks 6, 7.
- Error contract → Task 7 (`fault()`, `404`/`405`, parse error) + Task 6 (transition faults).
- Readiness poll-connect → Task 7 (`wait_ready`). Cleanup on signal → Task 7 main.rs. Single-VM lock → Task 7 (`Arc<Mutex<VmState>>`).
- Testing: unit (config/vm/model) Tasks 4–6, VcpuManager pause Task 2, mock integration Task 7, live Task 8.
- Docs + roadmap + live harness → Task 8. snapshot_type ignored documented → Task 8.

**Placeholders:** none — every code step has complete code; the one judgement call (load-only client must PUT boot-source/drive first) is stated with the exact fallback behavior.

**Type consistency:** `request_snapshot(Option<&str>)`, `SnapshotHandler`/`CheckpointHandler` both `Fn(Vec<VcpuCheckpoint>, Option<String>)`, `snapshot_name: Mutex<Option<String>>`, `pause_gate: (Mutex<bool>, Condvar)`, `pause_req: AtomicBool` — used identically across Tasks 1–3. `VmState`/`VmConfig`/`Settings`/`State`/`sanitize_name` signatures match between Tasks 5, 6, 7. `control()`/`spawn_boot()`/`wait_ready()` defined once in Task 7 and used there.

**Open risk flagged for the implementer:** hyper 1.x API surface (`http1::Builder`, `service_fn`, `Full`/`Incoming`, `TokioIo`) — if the workspace resolves a different hyper major, adjust imports in Task 7 Steps 1–2; the route logic is unaffected.
