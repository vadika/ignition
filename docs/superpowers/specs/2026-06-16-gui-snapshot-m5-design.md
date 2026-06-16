# M5 — GUI snapshot / restore + fan-out (design)

**Status:** approved 2026-06-16
**Predecessors:** M2 (display seam), M1 (virtio-gpu 2D), M3 (virtio-input), M4 (cage+foot compositor)

## Goal

Snapshot a *live* cage+foot desktop, `--restore` it into a reopened macOS
window with the desktop resuming where it left off, then clone N independent
GUI desktops from one warm base snapshot.

Splits into two implementation plans:

- **Plan A (Phase 1):** GPU + input snapshot state, and a GUI-aware restore path
  that reopens the window and repaints the resumed desktop.
- **Plan B (Phase 2):** fan-out — launch N `boot --gui --restore base`
  processes from one warm base; doc + shell helper, no new Rust.

## Background (current state)

Snapshot/restore, diff-snapshots, and APFS-CoW fan-out already work for
non-GUI guests. The device-state plumbing exists:

- `crates/devices/src/device.rs:60` — `MmioDevice::save() -> serde_json::Value`
  and `restore(&mut self, &Value)`.
- `crates/devices/src/virtio/mmio.rs:117` — `VirtioMmioState` already
  round-trips transport state: `status`, `queue_sel`, per-queue addrs/ready,
  `interrupt_status`, plus the inner device's `save()` blob.
- `crates/vmm/src/device_manager.rs:180` — `save()` snapshots every device;
  `:113` `add_restored()` restores at saved base/SPI + applies state.

Gaps M5 closes:

- `crates/devices/src/virtio/gpu.rs` — uses the default `save`=Null /
  `restore`=no-op. Host-side resource table and scanout binding are lost on
  restore. (`// No snapshot of GPU state (that is M5)`.)
- `crates/devices/src/virtio/input.rs` — same; config `select`/`subsel` lost.
- `spike/src/bin/boot.rs run_restore` (~:1473) rebuilds devices in
  `Mode::Restore` **headless** — no window, no event-loop/main-thread split,
  no input wiring.

## Plan A — Phase 1: GPU/input snapshot + GUI restore

### A1. virtio-gpu save/restore (metadata only)

`VirtioGpu` (`gpu.rs:52`) holds `width`, `height`,
`resources: HashMap<u32, Resource2D>`, `scanout_res`, `sink`. `Resource2D`
(`gpu.rs:42`) holds `format`, `width`, `height`, a backing SG list of
`(gpa, len)`, and `pixels: Arc<Mutex<Vec<u8>>>`.

- `save()` serializes **metadata only**:
  ```json
  { "resources": [ {"id":N,"format":F,"width":W,"height":H,
                    "backing":[{"gpa":G,"len":L}, ...]} ],
    "scanout_res": S }
  ```
  No pixel bytes — pixels are reconstructable from guest RAM backing.
- `restore(v)` rebuilds the `resources` map and `scanout_res`. Each resource's
  `pixels` buffer is allocated zeroed at `width*height*4`. Backing SG list is
  restored verbatim.

### A2. virtio-gpu present-after-restore

New one-shot method:

```rust
/// Re-read the scanout resource's backing from (restored) guest RAM into its
/// pixel buffer and present one frame. Called once after restore, before the
/// guest resumes, so the window shows the resumed desktop immediately.
pub fn present_scanout(&self, ram: &GuestRam)
```

- Look up `scanout_res` in `resources`; if absent, no-op (graceful — the
  compositor repaints on its next FLUSH).
- Re-read backing SG into `pixels` using the **same checked SG walk** as the
  M1 `transfer_2d`/`flush` path (out-of-range or overflowing `gpa+len`
  skipped, never panics).
- Build a full-surface `Frame { scanout_id, width, height, stride, dirty=full,
  pixels }` and call `sink.present(frame)`.

### A3. virtio-input save/restore

`VirtioInput` (`input.rs:61`) holds `flavor` (Keyboard | Tablet), `select`,
`subsel`. `flavor` is construction-time (rebuilt by `setup_devices` in device
order), so it is **not** serialized.

- `save()` → `{ "select": s, "subsel": ss }`.
- `restore(v)` → set `select`, `subsel`.

### A4. GUI-aware restore path (boot.rs)

`run_restore` must honor `--gui`, mirroring the fresh `--gui` boot split:

- When `--gui` is set on restore: run the VMM (vCPU threads, serial reader,
  vsock reactor, vmnet RX, GPU present) on **spawned threads**; run the winit
  event loop on the **main thread** (macOS requirement). Without `--gui`,
  restore stays exactly as today (headless, NoopSink, serial console).
- Construct the `WindowSink` and inject it into the restored GPU device (same
  seam as fresh boot; sink is stateless, attached at setup).
- After devices are restored, RAM is mapped, and the sink is attached: call
  `gpu.present_scanout(ram)` **once** so the resumed desktop paints before the
  guest is unpaused.
- Wire the virtio-input inject path (keyboard + tablet) from the winit event
  loop into the restored input devices, identical to the M3 fresh-boot path.

### A5. Snapshotting a live GUI guest

Snapshot trigger (Ctrl-A s) already exists. Under `--gui` the VMM runs on
spawned threads while winit owns main; the existing snapshot handler latches
the vCPUs and `device_manager.save()` now captures GPU table + input config.
Verify the handler fires correctly while the GUI is live (no main-thread
deadlock with the event loop).

### A6. Error handling

- Restored resource whose backing `gpa+len` exceeds guest RAM → checked walk
  skips it (reuse M1 guards). No panic.
- `scanout_res` not present in the restored table → `present_scanout` no-ops;
  compositor repaints on next frame.
- `--restore` of a GUI snapshot **without** `--gui` → headless restore works;
  GPU sink is `NoopSink`, present is discarded, serial console available.

### A7. Tests (Plan A)

Unit (`crates/devices`):

- gpu `save()` → `restore()` round-trips the resource table + `scanout_res`
  (build device with N resources, save, restore into a fresh device, assert
  equal metadata).
- `present_scanout`: synthetic `GuestRam` with known bytes at a resource's
  backing → after `present_scanout`, the captured `Frame` pixels match the
  expected BGRA→buffer read.
- `present_scanout` with `scanout_res` absent → no frame presented, no panic.
- input `save()` → `restore()` round-trips `select`/`subsel`.

Live eyeball (operator):

- Boot `--gui` GUI rootfs, log in, run something visible in foot, Ctrl-A s.
- `boot --gui --restore <name>` → window reopens, desktop resumes with the
  same screen content, typing works, pointer tracks.

## Plan B — Phase 2: fan-out

No new Rust. Each `boot --gui --restore base` already gets its own CoW
instance dir (`<store>/instances/<name>-<pid>`), window, and (with `--net`)
MAC/IP.

### B1. Shell helper + docs

- Small helper script (e.g. `scripts/fanout-gui.sh N <base>`) that launches N
  `boot --gui --restore <base>` in the background, optionally offsetting/tiling
  windows, and tears them down on exit.
- Doc the fan-out flow: take one warm-base GUI snapshot, fan out N clones, each
  an independent desktop sharing the immutable base via APFS CoW.

### B2. Docs to update

- `docs/src/features/devices.md` — GPU display + compositor sections: note
  GPU/input state survives snapshot, restore reopens the window, fan-out.
- `docs/src/features/snapshot-restore.md` — GUI snapshot/restore + fan-out.
- `docs/src/getting-started/guest-assets.md` — GUI snapshot/restore run notes.
- `ROADMAP.md` — mark M5 done.

### B3. Tests (Plan B)

Live eyeball: take a warm-base snapshot of a logged-in desktop, fan out N=3,
confirm 3 independent windows/desktops (type different text in each; each
isolated, base never mutated).

## Out of scope (YAGNI)

- 3D/VIRGL/Venus GPU state (2D only, as M1).
- Serializing host pixel buffers (reconstructed from backing instead).
- `--clone N` orchestrator flag (plain N processes + helper script instead).
- Display resize/hotplug across snapshot.
- Live migration / cross-host restore.
