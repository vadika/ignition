# M5 Phase 1 — GUI snapshot/restore Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Snapshot a live cage+foot desktop and `--restore` it into a reopened macOS window with the desktop resuming where it left off.

**Architecture:** Give virtio-gpu and virtio-input `save`/`restore` (metadata only — pixels are reconstructed from the restored guest-RAM backing). Add a one-shot `present_scanout` that re-reads the scanout backing and presents a frame. Extend `run_restore` to honor `--gui`: same main-thread/event-loop split as fresh `--gui` boot, with `present_scanout` painting the resumed desktop before vCPUs resume.

**Tech Stack:** Rust (edition 2024), serde_json for device state, virtio-mmio transport, winit/softbuffer event loop, HVF.

**Spec:** `docs/superpowers/specs/2026-06-16-gui-snapshot-m5-design.md`

---

## Orientation (read before starting)

Key facts the tasks rely on (already verified against the code):

- The inner-device snapshot hook is the `VirtioDevice` trait in
  `crates/devices/src/virtio/mmio.rs:74-81`:
  `fn save(&self) -> serde_json::Value` (default `Null`) and
  `fn restore(&mut self, &serde_json::Value) -> Result<(), String>` (default
  `Ok(())`). `VirtioMmio::save_state`/`restore_state` already wrap these and
  round-trip transport/queue state; balloon
  (`crates/devices/src/virtio/balloon.rs:104-119`) is the reference pattern.
- `VirtioGpu` (`crates/devices/src/virtio/gpu.rs:52-58`): `width`, `height`,
  `resources: HashMap<u32, Resource2D>`, `scanout_res: u32`,
  `sink: Box<dyn DisplaySink>`. `Resource2D` (`gpu.rs:42-49`): `format: u32`,
  `width: u32`, `height: u32`, `backing: Vec<(u64, u32)>`,
  `pixels: Arc<Mutex<Vec<u8>>>`. `MAX_RESOURCE_BYTES = 256 MiB` (`gpu.rs:36`).
  `read_backing` (`gpu.rs:85`) does the checked SG walk. `Frame`/`DirtyRect`
  come from `crate::display` (`gpu.rs:9`).
- `VirtioInput` (`crates/devices/src/virtio/input.rs:61-65`): `flavor`,
  `select: u8`, `subsel: u8`. `flavor` is construction-time (rebuilt by
  `setup_devices`), so it is NOT serialized.
- `setup_devices` (`spike/src/bin/boot.rs:397`) is the single device-wiring
  site for both boot and restore. The gpu/input block (`boot.rs:488-515`)
  currently only fires under `Mode::Boot`. `DeviceContext`
  (`boot.rs:339-358`) carries `display_sink`, `keyboard_mmio`, `tablet_mmio`.
- `KNOWN_DEVICE_IDS` (`boot.rs:317-319`) gates restore; it currently lacks the
  GUI device ids.
- `run_restore` (`boot.rs:1473`) runs `manager.run_restored(...)` on the main
  thread (`boot.rs:1904`). The fresh `--gui` split (spawn VMM, run event loop
  on main) is `boot.rs:1092-1124`. `run_event_loop` signature is
  `display_sink.rs:300-309`.
- CLI: `--gui` sets `gui` (`boot.rs:678`); `run_restore` is called at
  `boot.rs:741` and does NOT currently receive `gui`.

Run the existing device tests any time with:
`cargo test -p ignition-devices` (218 workspace tests currently pass).

---

## Task 1: virtio-gpu save/restore (metadata only)

**Files:**
- Modify: `crates/devices/src/virtio/gpu.rs` (add `save`/`restore` to the
  `impl VirtioDevice for VirtioGpu` block at `gpu.rs:350`; add a test in the
  `#[cfg(test)] mod tests` block)

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `gpu.rs` (after `flush_of_non_scanned_out_resource_presents_nothing`):

```rust
#[test]
fn save_restore_roundtrips_resource_table_and_scanout() {
    let mut gpu = new_gpu();
    let mut backing = vec![0u8; 0x4000];
    submit(&mut gpu, &mut backing, &create_2d_req(7, 8, 4));
    submit(&mut gpu, &mut backing, &attach_backing_req(7, &[(0x1000, 64), (0x2000, 64)]));
    submit(&mut gpu, &mut backing, &set_scanout_req(0, 7));
    let saved = gpu.save();

    let mut gpu2 = new_gpu();
    gpu2.restore(&saved).expect("restore ok");
    assert_eq!(gpu2.scanout_res, 7);
    let r = gpu2.resources.get(&7).expect("resource 7 restored");
    assert_eq!((r.format, r.width, r.height), (FORMAT_B8G8R8A8_UNORM, 8, 4));
    assert_eq!(r.backing, vec![(0x1000, 64), (0x2000, 64)]);
    assert_eq!(r.pixels.lock().unwrap().len(), 8 * 4 * 4); // rebuilt zeroed
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ignition-devices save_restore_roundtrips_resource_table_and_scanout`
Expected: FAIL — `save` returns `Null`, so `restore` leaves `scanout_res == 0` and resource 7 absent.

- [ ] **Step 3: Implement save/restore**

Add to the `impl VirtioDevice for VirtioGpu` block in `gpu.rs` (alongside the existing methods):

```rust
fn save(&self) -> serde_json::Value {
    let resources: Vec<serde_json::Value> = self
        .resources
        .iter()
        .map(|(id, r)| {
            let backing: Vec<serde_json::Value> = r
                .backing
                .iter()
                .map(|&(gpa, len)| serde_json::json!({ "gpa": gpa, "len": len }))
                .collect();
            serde_json::json!({
                "id": id,
                "format": r.format,
                "width": r.width,
                "height": r.height,
                "backing": backing,
            })
        })
        .collect();
    serde_json::json!({ "resources": resources, "scanout_res": self.scanout_res })
}

fn restore(&mut self, v: &serde_json::Value) -> Result<(), String> {
    let arr = v
        .get("resources")
        .and_then(|x| x.as_array())
        .ok_or("gpu: missing resources")?;
    let mut resources = HashMap::new();
    for e in arr {
        let id = e.get("id").and_then(|x| x.as_u64()).ok_or("gpu: resource missing id")? as u32;
        let format =
            e.get("format").and_then(|x| x.as_u64()).ok_or("gpu: resource missing format")? as u32;
        let width =
            e.get("width").and_then(|x| x.as_u64()).ok_or("gpu: resource missing width")? as u32;
        let height =
            e.get("height").and_then(|x| x.as_u64()).ok_or("gpu: resource missing height")? as u32;
        // Reuse the create-time bound: a restored resource must allocate a sane
        // pixel buffer, never wrap usize or drive a multi-GiB allocation.
        let size = (width as usize)
            .checked_mul(height as usize)
            .and_then(|n| n.checked_mul(4))
            .filter(|&n| n <= MAX_RESOURCE_BYTES)
            .ok_or("gpu: restored resource size invalid")?;
        let backing_arr =
            e.get("backing").and_then(|x| x.as_array()).ok_or("gpu: resource missing backing")?;
        let mut backing = Vec::with_capacity(backing_arr.len());
        for b in backing_arr {
            let gpa = b.get("gpa").and_then(|x| x.as_u64()).ok_or("gpu: backing missing gpa")?;
            let len =
                b.get("len").and_then(|x| x.as_u64()).ok_or("gpu: backing missing len")? as u32;
            backing.push((gpa, len));
        }
        resources.insert(id, Resource2D {
            format,
            width,
            height,
            backing,
            pixels: Arc::new(Mutex::new(vec![0u8; size])),
        });
    }
    let scanout_res =
        v.get("scanout_res").and_then(|x| x.as_u64()).ok_or("gpu: missing scanout_res")? as u32;
    self.resources = resources;
    self.scanout_res = scanout_res;
    Ok(())
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p ignition-devices save_restore_roundtrips_resource_table_and_scanout`
Expected: PASS

- [ ] **Step 5: Add a malformed-state test**

```rust
#[test]
fn restore_rejects_absurd_resource_size() {
    let mut gpu = new_gpu();
    // 65536*65536*4 = 16 GiB: exceeds MAX_RESOURCE_BYTES → restore errors.
    let bad = serde_json::json!({
        "resources": [{ "id": 1, "format": 1, "width": 0x10000, "height": 0x10000, "backing": [] }],
        "scanout_res": 0
    });
    assert!(gpu.restore(&bad).is_err());
}
```

Run: `cargo test -p ignition-devices restore_rejects_absurd_resource_size`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add crates/devices/src/virtio/gpu.rs
git commit -m "feat(devices): virtio-gpu snapshot save/restore (resource table + scanout)"
```

---

## Task 2: virtio-gpu present_scanout (re-read backing, present one frame)

**Files:**
- Modify: `crates/devices/src/virtio/mmio.rs` (add a default trait method to
  `VirtioDevice`; add a `VirtioMmio` wrapper)
- Modify: `crates/devices/src/virtio/gpu.rs` (override the trait method; add a test)

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `gpu.rs` (the `CapSink` helper already exists at `gpu.rs:642`):

```rust
#[test]
fn present_scanout_reads_backing_and_presents() {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let mut gpu = VirtioGpu::new(1280, 800, Box::new(CapSink(captured.clone())));
    let mut backing = vec![0u8; 0x8000];
    submit(&mut gpu, &mut backing, &create_2d_req(1, 4, 1)); // 4x1 = 16 bytes
    submit(&mut gpu, &mut backing, &attach_backing_req(1, &[(BASE + 0x4000, 16)]));
    submit(&mut gpu, &mut backing, &set_scanout_req(0, 1));
    // Paint a known pattern into the backing, as a resumed guest's framebuffer would hold.
    let m = GuestRam::new(backing.as_mut_ptr(), backing.len(), BASE);
    let pat: Vec<u8> = (0..16u8).collect();
    m.write_slice(BASE + 0x4000, &pat);

    gpu.present_scanout(&m);

    let frames = captured.lock().unwrap();
    assert_eq!(frames.len(), 1, "one frame presented");
    assert_eq!((frames[0].width, frames[0].height), (4, 1));
    assert_eq!(&frames[0].pixels.lock().unwrap()[..], &pat[..], "scanout re-read from backing");
}

#[test]
fn present_scanout_with_no_scanout_presents_nothing() {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let gpu = VirtioGpu::new(1280, 800, Box::new(CapSink(captured.clone())));
    let mut backing = vec![0u8; 0x1000];
    let m = GuestRam::new(backing.as_mut_ptr(), backing.len(), BASE);
    gpu.present_scanout(&m); // scanout_res == 0
    assert!(captured.lock().unwrap().is_empty());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ignition-devices present_scanout`
Expected: FAIL — `present_scanout` does not exist (compile error).

- [ ] **Step 3: Add the default trait method**

In `crates/devices/src/virtio/mmio.rs`, in the `VirtioDevice` trait (after the
`save`/`restore` defaults at `mmio.rs:74-81`), add:

```rust
/// Re-read the scanout from guest RAM and present one frame (virtio-gpu only,
/// used once after restore to repaint the resumed desktop). Default: no-op.
fn present_scanout(&self, _mem: &GuestRam) {}
```

- [ ] **Step 4: Add the VirtioMmio wrapper**

In `mmio.rs`, alongside `inject_input` (`mmio.rs:285`), add:

```rust
/// Re-read the GPU scanout from guest RAM and present one frame. No-op for
/// non-GPU devices. Called once after a GUI restore.
pub fn present_scanout(&self) {
    self.dev.present_scanout(&self.mem);
}
```

- [ ] **Step 5: Override in VirtioGpu**

In `gpu.rs`, in the `impl VirtioDevice for VirtioGpu` block, add:

```rust
fn present_scanout(&self, mem: &GuestRam) {
    if self.scanout_res == 0 {
        return;
    }
    let Some(r) = self.resources.get(&self.scanout_res) else { return };
    {
        let mut host = r.pixels.lock().unwrap();
        let len = host.len();
        read_backing(&r.backing, mem, 0, &mut host[..len]);
    }
    let frame = Frame {
        scanout_id: 0,
        width: r.width,
        height: r.height,
        stride: r.width * 4,
        dirty: DirtyRect { x: 0, y: 0, w: r.width, h: r.height },
        pixels: r.pixels.clone(),
    };
    self.sink.present(frame);
}
```

- [ ] **Step 6: Run test to verify it passes**

Run: `cargo test -p ignition-devices present_scanout`
Expected: PASS (both tests)

- [ ] **Step 7: Commit**

```bash
git add crates/devices/src/virtio/gpu.rs crates/devices/src/virtio/mmio.rs
git commit -m "feat(devices): virtio-gpu present_scanout — repaint resumed scanout after restore"
```

---

## Task 3: virtio-input save/restore (select/subsel)

**Files:**
- Modify: `crates/devices/src/virtio/input.rs` (add `save`/`restore` to
  `impl VirtioDevice for VirtioInput` at `input.rs:158`; add a test)

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `input.rs`:

```rust
#[test]
fn save_restore_roundtrips_select_subsel() {
    let mut kbd = VirtioInput::keyboard();
    kbd.config_write(0, &[CFG_EV_BITS]); // select
    kbd.config_write(1, &[EV_KEY as u8]); // subsel
    let saved = kbd.save();

    let mut kbd2 = VirtioInput::keyboard();
    kbd2.restore(&saved).expect("restore ok");
    assert_eq!(kbd2.select, CFG_EV_BITS);
    assert_eq!(kbd2.subsel, EV_KEY as u8);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ignition-devices save_restore_roundtrips_select_subsel`
Expected: FAIL — default `save` is `Null`, so `restore` leaves `select`/`subsel` at 0.

- [ ] **Step 3: Implement save/restore**

Add to the `impl VirtioDevice for VirtioInput` block in `input.rs`:

```rust
fn save(&self) -> serde_json::Value {
    // flavor is construction-time (rebuilt by setup_devices); only the config
    // protocol cursor is dynamic.
    serde_json::json!({ "select": self.select, "subsel": self.subsel })
}

fn restore(&mut self, v: &serde_json::Value) -> Result<(), String> {
    self.select = v.get("select").and_then(|x| x.as_u64()).ok_or("input: missing select")? as u8;
    self.subsel = v.get("subsel").and_then(|x| x.as_u64()).ok_or("input: missing subsel")? as u8;
    Ok(())
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p ignition-devices save_restore_roundtrips_select_subsel`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/devices/src/virtio/input.rs
git commit -m "feat(devices): virtio-input snapshot save/restore (select/subsel)"
```

---

## Task 4: restore-mode device wiring (boot.rs setup_devices)

**Files:**
- Modify: `spike/src/bin/boot.rs` — `KNOWN_DEVICE_IDS` (`:317`),
  `DeviceContext` (`:339`), both `DeviceContext` literals (`:808` boot,
  `:1655` restore), the gpu/input block in `setup_devices` (`:488-515`)

This task is integration wiring (no unit test is feasible for the device
manager + HVF setup). Gate on: `cargo build -p boot`, the existing
`check_known_ids` test, and `cargo test --workspace` showing no regressions.

- [ ] **Step 1: Add the GUI device ids to the known set**

Replace `KNOWN_DEVICE_IDS` (`boot.rs:317-319`) with:

```rust
const KNOWN_DEVICE_IDS: &[&str] = &[
    "serial", "virtio-blk", "virtio-rng", "rtc", "virtio-balloon", "vsock", "virtio-net",
    "virtio-gpu", "virtio-keyboard", "virtio-tablet",
];
```

- [ ] **Step 2: Add `gpu_mmio` to DeviceContext**

In the `DeviceContext` struct (`boot.rs:339-358`), after the `tablet_mmio`
field, add:

```rust
    /// virtio-gpu handle (Some when a GPU device was wired), used to repaint the
    /// scanout once after a GUI restore.
    gpu_mmio: Option<Arc<Mutex<VirtioMmio>>>,
```

- [ ] **Step 3: Initialize `gpu_mmio: None` in both DeviceContext literals**

In the boot-path literal (`boot.rs:808-819`) and the restore-path literal
(`boot.rs:1655-1666`), add `gpu_mmio: None,` next to `tablet_mmio: None,`.

- [ ] **Step 4: Rewrite the gpu/input block in setup_devices**

Replace the whole `if let (Mode::Boot, Some(sink)) = ...` block
(`boot.rs:488-515`) with:

```rust
    // virtio-gpu + virtio-input — wired under --gui boot (a sink was provided) or
    // whenever a restore record exists (a GUI snapshot). In a headless restore
    // (record present, no sink) the GPU gets a NoopSink so its state restores
    // consistently while presented frames are discarded.
    let want_gpu = match &mode {
        Mode::Boot => ctx.display_sink.is_some(),
        Mode::Restore(recs) => recs.iter().any(|r| r.id == "virtio-gpu"),
    };
    if want_gpu {
        let sink = ctx
            .display_sink
            .take()
            .unwrap_or_else(|| Box::new(ignition_devices::display::NoopSink));
        let mem = ctx.guest_ram();
        if let Some(h) = place::<VirtioMmio, _>(mgr, &mode, "virtio-gpu", layout::MMIO_WINDOW,
            move |irq| VirtioMmio::new(
                "virtio-gpu",
                Box::new(ignition_devices::virtio::gpu::VirtioGpu::new(1280, 800, sink)),
                mem,
                irq,
            ))? {
            ctx.gpu_mmio = Some(h);
        }
        let mem_kbd = ctx.guest_ram();
        if let Some(h) = place::<VirtioMmio, _>(mgr, &mode, "virtio-keyboard", layout::MMIO_WINDOW,
            move |irq| VirtioMmio::new(
                "virtio-keyboard",
                Box::new(ignition_devices::virtio::input::VirtioInput::keyboard()),
                mem_kbd, irq))? {
            ctx.keyboard_mmio = Some(h);
        }
        let mem_tab = ctx.guest_ram();
        if let Some(h) = place::<VirtioMmio, _>(mgr, &mode, "virtio-tablet", layout::MMIO_WINDOW,
            move |irq| VirtioMmio::new(
                "virtio-tablet",
                Box::new(ignition_devices::virtio::input::VirtioInput::tablet(1280, 800)),
                mem_tab, irq))? {
            ctx.tablet_mmio = Some(h);
        }
    }
```

- [ ] **Step 5: Build + test**

Run: `cargo build -p boot && cargo test --workspace`
Expected: build OK; all tests pass (the `check_known_ids` test still passes —
it asserts known ids are accepted and `mystery-device` rejected, both still
true).

- [ ] **Step 6: Commit**

```bash
git add spike/src/bin/boot.rs
git commit -m "feat(boot): restore virtio-gpu/input devices on restore; NoopSink fallback headless"
```

---

## Task 5: GUI-aware run_restore (thread split + present + event loop)

**Files:**
- Modify: `spike/src/bin/boot.rs` — `run_restore` signature (`:1473`), its
  call site (`:741`), and the run tail (`:1895-1904`)

Integration wiring. Gate on `cargo build -p boot` and `cargo clippy -p boot`.
Real verification is Task 6 (live eyeball).

- [ ] **Step 1: Thread the `gui` flag into run_restore**

Add a `gui: bool` parameter to `run_restore` (`boot.rs:1473-1481`), as the last
parameter:

```rust
fn run_restore(
    store: &Path,
    restore_name: &str,
    name: Option<String>,
    force: bool,
    track_dirty: bool,
    vsock_uds: Option<PathBuf>,
    no_sandbox: bool,
    gui: bool,
) -> io::Result<()> {
```

- [ ] **Step 2: Create the sink/receiver pair before setup_devices (restore)**

In `run_restore`, immediately BEFORE the `let mut ctx = DeviceContext {`
(`boot.rs:1655`), add:

```rust
    // Under --gui, wire a WindowSink so the restored virtio-gpu presents into the
    // event loop; without it the restore stays headless (NoopSink in setup_devices).
    let (gui_sink, gui_rx): (
        Option<Box<dyn ignition_devices::display::DisplaySink>>,
        Option<std::sync::mpsc::Receiver<ignition_devices::display::Frame>>,
    ) = if gui {
        let (sink, rx) = display_sink::WindowSink::new();
        (Some(Box::new(sink)), Some(rx))
    } else {
        (None, None)
    };
```

Then set `display_sink: gui_sink,` (instead of `display_sink: None,`) in the
restore `DeviceContext` literal (`boot.rs:1663`).

- [ ] **Step 3: Capture the input + gpu handles after setup_devices (restore)**

Immediately AFTER `setup_devices(&mut mgr, &mut ctx, Mode::Restore(&snap.devices))?;`
(`boot.rs:1667`), add:

```rust
    let kbd_handle = ctx.keyboard_mmio.clone();
    let tab_handle = ctx.tablet_mmio.clone();
    let gpu_handle = ctx.gpu_mmio.clone();
```

- [ ] **Step 4: Split the run tail for GUI**

Replace the final run section (`boot.rs:1895-1904`, from `let sb_paths =` through
`let run_result = manager.run_restored(snap.vcpus, Some(gic_blob));`) with:

```rust
    let sb_paths = ignition_sandbox::SandboxPaths {
        readable: vec![store.to_path_buf()],
        writable: [Some(store.to_path_buf()), Some(std::env::temp_dir()),
                   vsock_uds.as_ref().and_then(|u| u.parent().map(PathBuf::from))]
            .into_iter().flatten().collect(),
    };

    // 8. Run. Under --gui the winit event loop must own the main thread (macOS), so
    //    the VMM moves to a spawned thread and the event loop runs on main — mirroring
    //    the fresh-boot --gui split. The restored scanout is repainted once up front so
    //    the window shows the resumed desktop before the guest produces its next FLUSH.
    let run_result = if gui {
        let rx = gui_rx.expect("gui implies a receiver was created");
        // Repaint the resumed desktop from the restored backing into the event loop.
        if let Some(gpu) = &gpu_handle {
            gpu.lock().unwrap().present_scanout();
        }
        let done = std::sync::Arc::new(AtomicBool::new(false));
        let done_vmm = done.clone();
        let mgr_run = manager.clone();
        let gic_for_run = Some(gic_blob);
        let vcpus = snap.vcpus;
        std::thread::spawn(move || {
            struct SignalOnExit(std::sync::Arc<AtomicBool>);
            impl Drop for SignalOnExit {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::Release);
                }
            }
            let _signal = SignalOnExit(done_vmm);
            apply_or_exit(&sb_paths, no_sandbox);
            let _ = mgr_run.run_restored(vcpus, gic_for_run);
        });
        display_sink::run_event_loop(rx, done, 1280, 800, kbd_handle, tab_handle, 1280, 800);
        Ok(())
    } else {
        apply_or_exit(&sb_paths, no_sandbox);
        // VcpuManager creates + restores the vCPU on the vCPU thread (thread-affinity).
        manager.run_restored(snap.vcpus, Some(gic_blob))
    };
```

Note: `snap.vcpus` and `gic_blob` are moved into the spawned closure in the GUI
branch; they are used by value in the non-GUI branch — the `if/else` keeps both
uses exclusive, so no clone is needed. If the borrow checker objects to
`manager.run_restored` consuming `manager` after `manager.clone()`, the clone in
the GUI branch (`manager.clone()`) already gives the thread its own handle; the
non-GUI branch owns the original.

- [ ] **Step 5: Pass `gui` at the call site**

At `boot.rs:741`, change the call to pass `gui`:

```rust
        match run_restore(&store, &rname, name.clone(), force, track_dirty, vsock_uds, no_sandbox, gui) {
```

- [ ] **Step 6: Build + clippy**

Run: `cargo build -p boot && cargo clippy -p boot`
Expected: builds clean, no clippy warnings. Resolve any borrow-checker issue per
the note in Step 4 (the run-result branches must keep `snap.vcpus`/`gic_blob`
uses mutually exclusive).

- [ ] **Step 7: Run full workspace tests**

Run: `cargo test --workspace`
Expected: all pass, no regressions.

- [ ] **Step 8: Commit**

```bash
git add spike/src/bin/boot.rs
git commit -m "feat(boot): --gui restore — reopen window, repaint scanout, wire input"
```

---

## Task 6: Live eyeball verification (operator, no code)

**Files:** none (manual hardware test on the macOS host).

This is the real integration gate. The implementer reports the commands; the
operator runs them and confirms. Build/sign first.

- [ ] **Step 1: Build + sign the boot binary**

```bash
cargo build -p boot
./scripts/sign.sh target/debug/boot
```

- [ ] **Step 2: Boot the GUI rootfs and snapshot a live desktop**

```bash
target/debug/boot --gui --track-dirty --mem 512 kimage/out/Image kimage/out/rootfs-gui.ext4
```

Wait for the cage+foot desktop in the window; type something visible in foot
(e.g. `ls -la /`). Then press `Ctrl-A s` (in the controlling terminal) to write
a snapshot. Note the generated snapshot name printed as
`[snapshot] full '<name>' written to ...`. Quit with `Ctrl-A x`.

- [ ] **Step 3: Restore into a reopened window**

```bash
target/debug/boot --gui --restore <name> kimage/out/Image kimage/out/rootfs-gui.ext4
```

Expected: a new 1280x800 window opens and immediately shows the resumed desktop
(the same screen content captured at snapshot — `present_scanout` paints it
before the guest resumes). Typing in foot works; the pointer tracks.

- [ ] **Step 4: Confirm headless restore still works**

```bash
target/debug/boot --restore <name> kimage/out/Image kimage/out/rootfs-gui.ext4
```

Expected: no window; the serial console attaches and the guest resumes (GPU
device restored with a NoopSink, frames discarded). `Ctrl-A x` to quit.

- [ ] **Step 5: Record the result**

Report to the operator: did the GUI restore window repaint the resumed desktop,
and does input work? If yes, Phase 1 is complete. (Fan-out is Plan B.)

---

## Self-Review notes

- **Spec coverage:** A1 (gpu save/restore) → Task 1; A2 (present_scanout) →
  Task 2; A3 (input save/restore) → Task 3; A4 (GUI restore path) → Tasks 4+5;
  A5 (snapshot a live GUI guest) → exercised in Task 6 Step 2 (the existing
  snapshot handler already calls `device_manager.save()`, which now captures GPU
  + input state via Tasks 1/3 — no new code); A6 (error handling: absurd size →
  Task 1 Step 5; missing scanout → Task 2 `present_scanout_with_no_scanout`;
  headless restore → Task 4 NoopSink fallback + Task 6 Step 4); A7 (tests) →
  Tasks 1-3 unit tests + Task 6 eyeball.
- **Type consistency:** `present_scanout` is `&self`-by-`GuestRam` on the device
  and arg-less on `VirtioMmio` throughout. `gpu_mmio` field name consistent in
  Tasks 4/5. Backing serialized as `{gpa,len}` objects in both save (Task 1
  Step 3) and restore (same).
- **No placeholders:** every code step shows the full code.
