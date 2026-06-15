# ignition: 2D GUI bring-up plan (virtio-gpu + virtio-input)

Goal: an interactive, snapshot/clone-able Linux GUI in a macOS window, software-rendered, with **no host GPU state** so a graphical guest still restores/clones in ~200 ms. Scope is virtio-gpu **2D only** — no VIRGL/Venus/blob resources (that is a separate, GPU-state-aware project that would break clean restore).

The work is five milestones on a single critical path. Each is independently testable; stop after any of them and you have something demonstrable.

---

## Decisions to lock before starting

1. **Host windowing stack.** Recommended v1: `winit` (window + event loop) + `softbuffer` (CPU framebuffer blit). No Metal needed for a software 2D path; `softbuffer` takes a pixel buffer and presents it. Upgrade path later is a `CAMetalLayer` + `IOSurface` upload, but do not start there.
2. **Main-thread ownership.** On macOS the `winit` event loop **must** run on the main thread. Today the `boot` binary's main thread drives the serial console; this must be restructured so **main = UI event loop**, and the VMM/vCPU threads spawn off it. This is the single biggest structural change in the whole plan — confront it in Milestone 2, do not let it surprise you late.
3. **Resolution policy.** v1 = one fixed scanout at a compile/CLI-chosen mode (e.g. 1280×800), advertised via `GET_DISPLAY_INFO`. No EDID, no dynamic resize, no hotplug. Defer all of that.
4. **Pixel format.** B8G8R8A8 (`VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM`), 4 bytes/pixel, throughout. Match what the guest DRM driver prefers to avoid a host-side swizzle.
5. **Guest display stack.** v1 compositor = `cage` (single-app Wayland kiosk) on DRM/KMS + libinput. Simplest thing that yields a real, clickable app. `weston` if you want a desktop shell.
6. **Cursor.** v1 = software cursor composited by the guest compositor (no hardware cursor plane). Wire `cursorq` to accept and ack commands but you can ignore the cursor image until later.

---

## Milestone 1 — Guest probes virtio-gpu and gets a framebuffer (headless)

De-risks the device protocol and the guest driver with **no window involved yet**.

**Tasks**
- New device module `crates/devices/src/virtio/gpu.rs`, implementing `VirtioDevice` (device id 16, `queue_count = 2`: controlq=0, cursorq=1, `device_features = 0`).
- Config space (16 bytes): `events_read`, `events_clear`, `num_scanouts = 1`, `num_capsets = 0`. Serve arbitrary read widths like the other devices.
- Implement the controlq command set well enough for the Linux `virtio_gpu` driver to bind: `GET_DISPLAY_INFO`, `RESOURCE_CREATE_2D`, `RESOURCE_UNREF`, `RESOURCE_ATTACH_BACKING`, `RESOURCE_DETACH_BACKING`, `SET_SCANOUT`, `TRANSFER_TO_HOST_2D`, `RESOURCE_FLUSH`. In this milestone `TRANSFER` may copy into a host-side buffer and `FLUSH` may be a no-op (no sink yet).
- Each command: split the descriptor chain into device-readable (request) and device-writable (response); the response is at minimum a 24-byte `virtio_gpu_ctrl_hdr` with the right `OK_*`/`ERR_*` type. `cursorq` commands: parse and ack (return used, zero-length response is fine).
- Maintain a resource table (`id → {format, w, h, backing SG list, host pixel buffer}`) and a one-entry scanout binding.
- Register the device at the **single DeviceManager wiring site** alongside blk/net/rng/balloon/vsock; emit an `FdtKind::VirtioMmio` node (reuse existing path); allocate its MMIO window + SPI.
- Guest kernel (`kimage/`): enable `CONFIG_DRM=y`, `CONFIG_DRM_VIRTIO_GPU=y`, `CONFIG_DRM_FBDEV_EMULATION=y`, `CONFIG_FB=y`, `CONFIG_FRAMEBUFFER_CONSOLE=y`.
- Unit tests mirroring `rng.rs`/`blk.rs` style: feed a crafted descriptor chain over a `GuestRam` + `Virtqueue`, assert the resource table mutates and the response header type is correct. Cover `CREATE_2D`, `ATTACH_BACKING` (multi-entry), `SET_SCANOUT`, `TRANSFER_TO_HOST_2D`, `RESOURCE_FLUSH`.

**Acceptance**
- Guest boots; `dmesg` shows `virtio_gpu` probing and a `/dev/dri/card0` (+ `/dev/fb0`) appears.
- The kernel framebuffer console binds (you can see fbcon take over if you later attach a sink); device logs show the create/attach/set_scanout/transfer/flush sequence.
- Device unit tests pass.

**Risks / gotchas**
- `TRANSFER_TO_HOST_2D` SG correctness is the one real algorithm here. The backing is a scatter-gather list; a single scanline can straddle two entries. Implement a backing cursor that resolves `offset + y*stride + x*bpp .. +row_bytes` against cumulative SG lengths and issues one `read_slice` per segment. Build a unit test with a deliberately fragmented two-entry backing.
- `GET_DISPLAY_INFO` response is `ctrl_hdr` + an array of 16 `virtio_gpu_display_one` (each: rect + enabled + flags). Only entry 0 is enabled; the rest must be present and zeroed or the driver miscounts.

---

## Milestone 2 — Pixels reach a macOS window

**Tasks**
- Define a `DisplaySink` seam mirroring the existing `IrqLine`/`NoopIrq` pattern: a trait with a non-blocking `present(frame)` and a `NoopSink` for the manager/tests.
- In the `spike`/`boot` binary, implement the real sink: a `winit` window + `softbuffer` surface. The sink sends "present(scanout, geometry, dirty rect, pixel-buffer handle)" to the UI thread; the UI thread blits the dirty region.
- **Restructure threading:** main thread runs the `winit` event loop; spawn the VMM (vCPU threads, reactor, serial) off it. Move the serial console onto its existing thread model. The present channel is an `mpsc` (or triple-buffer) drained on the UI thread; coalesce to the latest frame so a slow window never backpressures the guest.
- Share each resource's host pixel buffer with the UI thread (e.g. behind an `Arc<Mutex<…>>`); `FLUSH` hands over the handle + dirty rect rather than copying.
- `TRANSFER_TO_HOST_2D` now does the real guest→host copy; `FLUSH` emits a present.

**Acceptance**
- Boot to shell with the GUI window open: the guest's fbcon (kernel console) renders live in the macOS window, scrolling as you type on the serial side.
- vCPUs never block on the window: kill the UI / freeze presents and the guest keeps running.

**Risks / gotchas**
- Main-thread event-loop ownership vs. the current console-driven main is the crux; budget for a real refactor of `boot`'s startup, not a patch.
- `softbuffer` expects a specific channel order/stride; reconcile with the B8G8R8A8 choice once, centrally.
- Re-sign after every relink (`scripts/sign.sh`) — adding windowing deps changes nothing about the hypervisor entitlement requirement.

---

## Milestone 3 — Input (the GUI becomes interactive)

**Tasks**
- New device module `crates/devices/src/virtio/input.rs`: virtio-input (device id 18), two queues (eventq=0, statusq=1). Config space exposes device name, `EV` bits, and `ABS` axis ranges via the `select`/`subsel` config protocol.
- Provide **two** input devices (or one combined): a keyboard (EV_KEY) and an **absolute** pointer/tablet (EV_ABS x/y + buttons). Absolute positioning is what makes the cursor track the macOS pointer cleanly; relative motion is much worse UX.
- Translate `winit` input events → Linux `input_event` records (type/code/value triples, with `EV_SYN`/`SYN_REPORT` framing) → eventq. Map `winit` keycodes to Linux evdev keycodes (a static table) and pointer position to the absolute axis range matching the scanout resolution.
- Guest kernel: `CONFIG_VIRTIO_INPUT=y` (plus `CONFIG_INPUT_EVDEV=y`).
- Register both at the wiring site; FDT `VirtioMmio` nodes.
- Unit tests: assert a synthesized host keypress produces the correct evdev triple sequence on eventq.

**Acceptance**
- With only the GUI window (serial unused), you can log in at the guest's fbcon by typing in the macOS window.
- Pointer position in the window maps 1:1 to an absolute guest pointer.

**Risks / gotchas**
- The config-space `select/subsel` protocol for advertising capabilities is finicky; get the `ABS` axis min/max right or the guest clamps the pointer to a corner.
- Key repeat / modifier state: let the guest handle repeat; just send key-down/up faithfully.

---

## Milestone 4 — A real GUI (compositor + app)

**Tasks**
- Build a guest userspace image containing a Wayland compositor on DRM/KMS + libinput: start with `cage` (kiosk: launches exactly one app fullscreen). Add `seatd`/`libseat` or run as root for seat access.
- Ship one demonstrative app: a terminal emulator (foot) for the "disposable Linux desktop" story, or a browser for the "open the sketchy link in a throwaway box" story.
- Confirm the compositor enumerates `/dev/dri/card0`, sets the mode from `GET_DISPLAY_INFO`, and drives `TRANSFER`/`FLUSH` with damage rects (not full-frame every time).

**Acceptance**
- A graphical app renders in the macOS window and responds to keyboard + pointer.
- Damage-driven flushes (only changed regions transfer) — verify by logging flush rect sizes during partial-screen updates.

**Risks / gotchas**
- Software rendering (llvmpipe) is fine for a terminal/light UI, sluggish for animation. That's expected and acceptable for the target use cases; don't chase it with Venus.
- libinput seat/permissions in a minimal rootfs is a common stall — validate the input path (Milestone 3) before adding the compositor on top.

---

## Milestone 5 — Snapshot / restore / clone with the GUI live

This is the payoff and the ignition-specific differentiator.

**Tasks**
- `gpu.rs` `save()`: emit **metadata only** — per-resource `{id, format, w, h, backing SG list}`, scanout bindings, advertised mode. **Never** serialize host pixel buffers.
- `gpu.rs` `restore()`: rebuild the resource table with empty host buffers sized from geometry; reattach the SG lists (the GPAs are valid because guest RAM returns via `memory.bin`); rebind scanouts. Then **force a full repaint**: for each enabled scanout, run `transfer_to_host(full rect)` from the backing and emit a full-surface `present`.
- Host sink: the window is recreated fresh on restore (it is host runtime state, exactly like the new vmnet interface) — `restore` must not assume a live sink; it queues a full present that the freshly-created window picks up.
- `input.rs` save/restore: minimal (drop in-flight events; reset to a known idle state). Match the existing connection-reset-on-restore convention.
- Extend the headless drivers (`scripts/restore_clone_test.py` analog) to: snapshot a running GUI, restore, assert the window repaints to the same frame and idles at ~0% CPU; then clone N and assert N independent windows.

**Acceptance**
- Snapshot a running GUI session, restore → window repaints to the identical frame in ~200 ms with the guest idle.
- Clone the same base into N guests → N independent GUI windows, each network-distinct (existing carrier-watch behavior unaffected).
- Snapshot artifacts contain **zero** pixel data attributable to the framebuffer (it all lives in `memory.bin`).

**Risks / gotchas**
- Ordering on restore: rebind scanouts *before* the repaint, and ensure the sink exists before the queued present fires. Treat the repaint as the GPU analogue of the net link-bounce/vsock-RST already in the restore path.
- If a clone's compositor doesn't immediately redraw, the forced full-surface transfer from backing covers it — that's why the repaint is mandatory, not opportunistic.

---

## Explicitly deferred (do not build in v1)

- VIRGL / Venus / 3D, blob resources, `CONTEXT_INIT` — separate project; breaks clean snapshot.
- EDID, dynamic resize, display hotplug, multi-scanout / multi-monitor.
- Hardware cursor plane (software cursor via the compositor is fine).
- Clipboard, drag-and-drop, audio, GPU-accelerated video.
- Display events (`VIRTIO_GPU_EVENT_DISPLAY`) beyond leaving `events_read = 0`.

## Critical path & sizing (relative)

```
M1 device+kernel ─► M2 window (+ main-thread refactor) ─► M3 input ─► M4 compositor ─► M5 snapshot/clone
   medium               large (refactor risk)              medium      small-medium     medium
```

M2's main-thread/event-loop refactor is the schedule risk; everything else is bounded, well-specified protocol work that follows the shape of the devices already in the tree.

## Validation harness (cross-cutting)

- Per-device unit tests in the `rng.rs`/`blk.rs` style (crafted chains over `GuestRam`/`Virtqueue`).
- A "test pattern" guest program that opens `/dev/dri/card0`, allocates a DRM dumb buffer, and paints animated bars — use it to validate M1–M3 before introducing a real compositor in M4.
- Headless snapshot/clone drivers extended from the existing `restore_*` scripts for M5.

