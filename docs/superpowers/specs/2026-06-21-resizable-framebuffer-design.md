# Resizable framebuffer — design

Date: 2026-06-21
Status: approved (design), pending implementation plan

## Goal

Let the macOS host window be resized, and propagate the new size to the guest so
the guest display reconfigures to match. Today the framebuffer is fixed at
`GUI_W×GUI_H` (1400×880, `spike/src/bin/boot.rs:59`) and the window is created
`.with_resizable(false)` (`spike/src/bin/display_sink.rs:423`).

End state: drag the window edge → after a short idle the guest re-modesets to the
new logical size → cage resizes its output → firefox reflows → frames arrive at
the new resolution.

## Why this is small

The virtio-gpu device already has the native reverse path; it just isn't wired:

- `display_info()` (`gpu.rs:336`) already reports `self.width/self.height` to
  `GET_DISPLAY_INFO`. Change those fields and the next query returns new dims.
- `VirtioMmio::signal_config_change()` (`mmio.rs:357`) already raises
  `INT_STATUS_CONFIG`. It's used today for net link / balloon target.
- The GUI event-loop thread already holds `Arc<Mutex<VirtioMmio>>` for the
  tablet/keyboard and calls `inject_input` under the lock
  (`display_sink.rs:272,383`). The resize poke uses the identical pattern.
- Host blit (`blit_frame`, `display_sink.rs:228`) already scales an
  arbitrary-sized guest frame into the window surface. No change.

Guest side: cage is wlroots-based. The Linux `virtio_gpu` DRM driver turns
`VIRTIO_GPU_EVENT_DISPLAY` into a KMS hotplug event; wlroots re-queries modes,
atomic-modesets to the new preferred mode, and firefox (a Wayland client) gets an
`xdg_surface` reconfigure. No guest-side helper, no vsock, no new device.

## The chain

```
host window WindowEvent::Resized
  → debounce (~150ms idle, decision: one modeset per resize, not per drag-tick)
  → GUI thread locks gpu transport, calls VirtioMmio::display_set_mode(w,h):
        gpu.width/height = w,h
        gpu.events_read |= VIRTIO_GPU_EVENT_DISPLAY (0x0001)
        self.signal_config_change()
  → guest reads config events_read, sees EVENT_DISPLAY, writes events_clear,
    issues GET_DISPLAY_INFO → learns new w,h
  → virtio_gpu DRM hotplug → wlroots modeset → SET_SCANOUT new resource at w,h
  → cage output resized → firefox xdg reconfigure → repaint
  → RESOURCE_FLUSH at new dims → host blit (already arbitrary-size) → window
```

## Changes

### Device: `crates/devices/src/virtio/gpu.rs`
- Add `events_read: u32` field to `VirtioGpu` (init 0).
- `config_read` offset 0 (`events_read`): return `self.events_read` instead of 0.
- Impl `config_write`: on a write to offset 4 (`events_clear`), clear the written
  bits from `self.events_read`. (Guest acks the event this way.)
- Add `set_display_mode(&mut self, w: u32, h: u32)` (override of a new default-noop
  `VirtioDevice` trait method): set `width/height`, set
  `events_read |= VIRTIO_GPU_EVENT_DISPLAY`. Returns `bool` (true = gpu handled).
- `const VIRTIO_GPU_EVENT_DISPLAY: u32 = 0x0001;`

### Transport: `crates/devices/src/virtio/mmio.rs`
- Add `pub fn display_set_mode(&mut self, w, h)`: call `self.dev.set_display_mode(w,h)`;
  if it returns true, `self.signal_config_change()`. Mirrors `net_set_link`.
- Add `fn set_display_mode(&mut self, _w, _h) -> bool { false }` default to the
  `VirtioDevice` trait (like `present_scanout`).

### Input decoupling: `crates/devices/src/virtio/input.rs`
- Change tablet ABS_X/ABS_Y reported max from `w-1/h-1` to a fixed
  `ABS_MAX = 32767` (QEMU virtio-tablet convention). libinput maps the absolute
  axis range onto the current output extent, so the pointer stays correct at any
  resolution. `VirtioInput::tablet` keeps its signature for now but the dims stop
  affecting absinfo (or drop the args — implementer's call, smallest diff).
- Rationale: EV_BITS/absinfo are probed only at fresh boot
  ([[virtio-input-evbits-probe-at-boot]]); a resolution-tied range cannot change
  live. A fixed normalized range is probed once and is resolution-independent
  forever. Requires a one-time base rebuild.

### Host pointer: `spike/src/bin/display_sink.rs`
- `scale_pos` scales window-physical → `0..ABS_MAX` (32767) instead of
  `0..gw/gh`. Drops the dependency on guest resolution for pointer mapping.

### Host window + debounce: `spike/src/bin/display_sink.rs`
- Window: `.with_resizable(true)`.
- `WindowSink` gains `gpu: Option<Arc<Mutex<VirtioMmio>>>` (same shape as the
  existing `tablet` handle).
- On `WindowEvent::Resized`: store pending target dims + a "last resize" instant.
  Target guest dims = window **logical** inner size (physical / scale_factor),
  clamped to `[MIN, host work area]`, even-rounded. Use logical (not physical) so
  HiDPI scale does not double the guest resolution.
- Debounce: `ControlFlow::WaitUntil` / `about_to_wait`: when pending and idle
  ≥ ~150ms, lock the gpu transport and call `display_set_mode(w,h)`, clear pending.
- Surface (`surf_w/surf_h`) already tracks physical window size and blit upscales
  the (stale) guest frame during the drag — slightly soft mid-drag, sharp once the
  guest catches up. Acceptable.

### Wiring: `spike/src/bin/boot.rs`
- Pass the gpu `Arc<Mutex<VirtioMmio>>` into the `WindowSink` (alongside the
  existing tablet/keyboard handles).
- `GUI_W/GUI_H` stay as the initial/boot size. Min clamp constant lives here too
  (e.g. `MIN_W=320, MIN_H=240`).

## Decisions (from brainstorming)

- **Granularity: debounced.** One modeset when the drag settles (~150ms), not per
  Resized tick. Cheap on the no-GL pixman path; avoids firefox relayout churn.
- **Snapshot/restore: base dims, no format change.** A restored guest comes back at
  base `GUI_W/GUI_H` (gpu `width/height` are reconstructed from `VirtioGpu::new`,
  not saved). The window opens to match; resize-after-restore uses the same live
  path. No new snapshot state. Per-session size memory deferred (YAGNI).
- **No new IPC / device / vsock.** Reuse virtio-gpu config-change + GET_DISPLAY_INFO.

## Constraints / risks

- **Base rebuild required** for the tablet ABS range change (probed at boot only).
- **wlroots must follow the hotplug.** cage/wlroots + Linux `virtio_gpu` support
  dynamic modeset; if the guest doesn't reflow on the first try, verify the
  `virtio_gpu` driver emits the hotplug uevent and that cage isn't pinned to a
  fixed output mode. Fallback if needed: nudge via `wlr-randr`, but expected
  unnecessary.
- **Stride.** B8G8R8A8 stride = `w*4`, always 4-aligned for any width; even-round
  is belt-and-suspenders, no hard requirement.
- **Resource churn.** Each modeset allocs a new host pixel buffer (bounded by the
  existing `MAX_RESOURCE_BYTES` cap). Debounce keeps this rare.

## Test (one runnable check each non-trivial unit)

- `gpu.rs`: unit test — after `set_display_mode(w,h)`, `config_read` of events_read
  returns `EVENT_DISPLAY`; `config_write` of events_clear clears it;
  `display_info` reports new `w,h`.
- `input.rs`: unit test — tablet absinfo max == 32767 for ABS_X/ABS_Y regardless
  of constructor dims.
- `display_sink.rs`: unit test — `scale_pos` maps surface extremes to `0` and
  `32767`; debounce emits exactly one mode update for a burst of Resized events
  within the window (pure-logic check, no winit).

## Out of scope

- Multi-scanout / multi-monitor.
- 3D/VIRGL, blob resources.
- Per-session persisted window size.
- Live (per-tick) resize.
