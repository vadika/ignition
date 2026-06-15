# virtio-gpu 2D device (M1) + pixels to the window — Design

Date: 2026-06-15. Status: approved design, ready for an implementation plan.

Second milestone of the 2D GUI bring-up (umbrella plan:
`docs/superpowers/specs/2026-06-15-gui-bringup-plan.md`). M2's structural refactor already
landed: `boot --gui` opens a blank 1280x800 `winit`+`softbuffer` window, the VMM runs on a
spawned thread, and a non-blocking coalescing `DisplaySink` seam (`crates/devices/src/display.rs`:
`DisplaySink`, `NoopSink`, `Frame`, `DirtyRect`) plus a `WindowSink` proxy + `run_event_loop`
(`spike/src/bin/display_sink.rs`) are in place. The window currently clears to a solid color
because nothing produces frames.

This milestone adds the **virtio-gpu 2D device** so a guest probes it, gets `/dev/dri/card0`
+ `/dev/fb0`, and its kernel framebuffer console renders **live in the window** — by wiring
`RESOURCE_FLUSH` through the existing `DisplaySink`. Scope is virtio-gpu **2D only**: no
VIRGL/Venus/3D, no blob resources, no `CONTEXT_INIT` (those are GPU-state-aware and break clean
snapshot — a separate project).

## Context & decisions locked (from the umbrella plan + brainstorming)

- **Device:** virtio-gpu, device id **16**, `queue_count = 2` (controlq = 0, cursorq = 1),
  `device_features = 0` (no VIRTIO_GPU_F_* — plain 2D).
- **One scanout, fixed mode 1280x800**, advertised via `GET_DISPLAY_INFO`. No EDID, no resize,
  no hotplug, no multi-scanout.
- **Pixel format B8G8R8A8** (`VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM`), 4 bytes/pixel, throughout.
- **Cursor:** software cursor composited by the guest; `cursorq` commands are parsed and
  ack'd, the cursor image is ignored this milestone.
- **`--gui` gates the device.** No window → no virtio-gpu device. The same `WindowSink` created
  for the window is handed to the device as its `Box<dyn DisplaySink>`; without `--gui` the
  device is absent (not `NoopSink`-backed — there is simply no point showing the guest a GPU
  with no display).
- **Scope this milestone = device + present.** `TRANSFER_TO_HOST_2D` does the real SG-correct
  guest→host copy; `RESOURCE_FLUSH` emits a `Frame` to the sink. (The umbrella doc split this
  as "M1 headless" + "M2 pixel"; the seam exists, so they are done together.)
- **Snapshot/restore of GUI state is out of scope (M5).** See Non-goals.

## Goal

`boot --gui <kernel> <rootfs>` (with a guest kernel built with `CONFIG_DRM`,
`CONFIG_DRM_VIRTIO_GPU`, `CONFIG_DRM_FBDEV_EMULATION`, `CONFIG_FB`, `CONFIG_FRAMEBUFFER_CONSOLE`)
boots with a virtio-gpu device the Linux `virtio_gpu` driver binds: `/dev/dri/card0` and
`/dev/fb0` appear, the framebuffer console takes over, and the guest's console output renders
live in the macOS window. All device-protocol logic is unit-tested on the host with crafted
descriptor chains (no kernel needed). Non-GUI / `--restore` / `--fuzz` are unchanged.

Non-goals (this milestone): VIRGL/Venus/3D/blob/`CONTEXT_INIT`; EDID / dynamic resize /
display hotplug / multi-scanout; a hardware cursor plane; display events
(`VIRTIO_GPU_EVENT_DISPLAY`) beyond leaving `events_read = 0`; **snapshot/restore of GPU state**
(M5 — the device's `save` returns `Null` and `restore` is a no-op this milestone; restoring a
snapshot that contains a gpu record is not supported until M5, and the device is added only in
Boot mode); virtio-input (M3); a compositor/app (M4).

## Architecture — new module `crates/devices/src/virtio/gpu.rs`

A single `VirtioGpu` device implementing `VirtioDevice` (the trait in
`crates/devices/src/virtio/mmio.rs`), modeled on `rng.rs`/`blk.rs`: `handle_notify(queue_idx,
vq, mem)` pops chains with `vq.pop_avail(mem)`, reads request bytes from device-readable
descriptors and writes the response into device-writable descriptors via
`GuestRam::read_slice`/`write_slice`, then `vq.push_used(mem, chain.head, resp_len)`.

```rust
pub struct VirtioGpu {
    /// Fixed advertised mode.
    width: u32,            // 1280
    height: u32,           // 800
    /// id -> 2D resource (host-side).
    resources: HashMap<u32, Resource2D>,
    /// scanout 0 binding: the resource id currently driving the display (0 = none).
    scanout_res: u32,
    /// Where flushed frames go (the window, via WindowSink; or NoopSink in tests).
    sink: Box<dyn DisplaySink>,
}

struct Resource2D {
    format: u32,           // B8G8R8A8
    width: u32,
    height: u32,
    /// Guest backing pages (GPA, len) from RESOURCE_ATTACH_BACKING — a scatter-gather list.
    backing: Vec<(u64, u32)>,
    /// Host pixel buffer, width*height*4 bytes, B8G8R8A8. Shared so FLUSH hands a
    /// handle to the sink without copying (matches `Frame.pixels`).
    pixels: Arc<Mutex<Vec<u8>>>,
}
```

`VirtioGpu::new(width, height, sink: Box<dyn DisplaySink>) -> Self`.

### controlq (queue 0) command set

Every command starts with a 24-byte `virtio_gpu_ctrl_hdr` (type: u32, flags: u32, fence_id:
u64, ctx_id: u32, padding: u32), little-endian. The device parses the request header's `type`,
reads the command-specific request body that follows it (from the device-readable descriptors,
which the device concatenates into one request byte vector), executes, and writes a response
(at minimum a 24-byte response `ctrl_hdr` with the right type) into the device-writable
descriptors. Response `fence_id`/`ctx_id` mirror the request; `flags` = 0 (no fence support
this milestone — `VIRTIO_GPU_FLAG_FENCE` is ignored).

Commands implemented (enough for the Linux `virtio_gpu` 2D driver to bind and paint):

- **`GET_DISPLAY_INFO` (0x0100)** → response `VIRTIO_GPU_RESP_OK_DISPLAY_INFO` (0x1101):
  `ctrl_hdr` + an array of **16** `virtio_gpu_display_one` (each: `rect{x,y,width,height}` =
  4×u32, `enabled`: u32, `flags`: u32 = 24 bytes). Entry 0 = `{0,0,1280,800}`, `enabled = 1`;
  entries 1..16 zeroed (present-but-disabled, or the driver miscounts).
- **`RESOURCE_CREATE_2D` (0x0101)**: req body `{resource_id: u32, format: u32, width: u32,
  height: u32}`. Insert a `Resource2D` with a zeroed `width*height*4` host buffer. Response
  `OK_NODATA` (0x1100).
- **`RESOURCE_UNREF` (0x0102)**: req `{resource_id, padding}`. Remove from the table (clear
  `scanout_res` if it pointed here). `OK_NODATA`.
- **`RESOURCE_ATTACH_BACKING` (0x0106)**: req `{resource_id, nr_entries}` followed by
  `nr_entries` × `virtio_gpu_mem_entry{addr: u64, length: u32, padding: u32}`. Store the
  `(addr, length)` SG list on the resource. `OK_NODATA`.
- **`RESOURCE_DETACH_BACKING` (0x0107)**: req `{resource_id, padding}`. Clear the resource's
  SG list. `OK_NODATA`.
- **`SET_SCANOUT` (0x0103)**: req `{rect, scanout_id: u32, resource_id: u32}`. Bind scanout 0
  to `resource_id` (`scanout_res = resource_id`; `resource_id = 0` disables). `OK_NODATA`.
- **`TRANSFER_TO_HOST_2D` (0x0105)**: req `{rect{x,y,w,h}, offset: u64, resource_id: u32,
  padding}`. Copy the rectangle from the resource's **guest backing SG list** into its host
  pixel buffer (the one real algorithm — see below). `OK_NODATA`.
- **`RESOURCE_FLUSH` (0x0104)**: req `{rect, resource_id, padding}`. If `resource_id` is the
  scanned-out resource, build a `Frame` and call `self.sink.present(frame)` (see Present).
  `OK_NODATA`.

Any unrecognized `type`: respond `VIRTIO_GPU_RESP_ERR_UNSPEC` (0x1200) with a bare `ctrl_hdr`,
and still `push_used` so the guest is not wedged. A malformed/too-short request (missing body):
same `ERR_UNSPEC`, never panic.

### cursorq (queue 1)

Parse the chain, ignore the `virtio_gpu_update_cursor` payload, and `push_used(head, 0)`
(zero-length response is fine). The guest composites its own cursor in software.

### TRANSFER_TO_HOST_2D — the SG copy (the one real algorithm)

The resource's guest backing is a scatter-gather list of `(gpa, len)` segments that together
form a contiguous logical buffer of `width*height*4` bytes in row-major B8G8R8A8. The transfer
copies a `rect` (x, y, w, h) starting at logical `offset` into the **host** pixel buffer at the
same logical layout. For a full-surface transfer (the common fbcon case) `rect` covers the
whole surface and `offset = 0`.

Implement a **backing cursor** that maps a logical byte range to one or more SG segments:
maintain cumulative segment offsets; for each scanline `y` in `[rect.y, rect.y+rect.h)`, the
logical span is `base = offset + (y * width + rect.x) * 4 .. + rect.w*4`; resolve that span
against the SG list (it can straddle two segments) and issue one `mem.read_slice(seg_gpa +
seg_local, &mut buf[...])` per intersected segment, writing into the host buffer at
`(y*width + rect.x)*4`. A unit test uses a deliberately **fragmented two-entry backing** to
prove a scanline that straddles a segment boundary is reassembled correctly.

Clamp the rect to the resource dimensions; ignore out-of-range (respond OK, copy nothing beyond
bounds) rather than panic.

### Present — RESOURCE_FLUSH → DisplaySink

On `RESOURCE_FLUSH` for the scanned-out resource:

```rust
let r = &self.resources[&id];
let frame = Frame {
    scanout_id: 0,
    width: r.width,
    height: r.height,
    stride: r.width * 4,
    dirty: DirtyRect { x: rect.x, y: rect.y, w: rect.w, h: rect.h },
    pixels: r.pixels.clone(),   // shared handle, no copy
};
self.sink.present(frame);       // non-blocking; WindowSink forwards to the UI thread
```

The UI thread's `run_event_loop` already drains and coalesces frames (from M2). M2's
placeholder cleared the window to a solid color; this milestone the event loop must **blit the
frame**: lock `frame.pixels`, and for B8G8R8A8 source bytes write each pixel into the softbuffer
`u32` as `0RGB` (`(r<<16)|(g<<8)|b`, source order B,G,R,A → R=byte2, G=byte1, B=byte0). Blit the
`dirty` rect (clamped to the surface) rather than the whole buffer when the dirty rect is a
sub-region. This is the one change to `display_sink.rs::App::redraw` from M2.

## Integration in `spike/src/bin/boot.rs`

- The `WindowSink` is currently created in the `--gui` branch and dropped (`_sink`). Change:
  create `(sink, rx) = display_sink::WindowSink::new()` **before** `setup_devices`, thread
  `Box::new(sink)` into the device via a new `DeviceContext` field
  `display_sink: Option<Box<dyn DisplaySink>>`, and keep `rx` for `run_event_loop`.
- In `setup_devices`, add the virtio-gpu device when `ctx.display_sink` is `Some` (Boot mode
  only — see Non-goals): `place::<VirtioMmio, _>(mgr, &mode, "virtio-gpu", layout::MMIO_WINDOW,
  move |irq| VirtioMmio::new("virtio-gpu", Box::new(VirtioGpu::new(1280, 800, sink)), mem,
  irq))?`, mirroring the rng/blk/vsock registrations (MMIO window + SPI alloc + FDT
  `VirtioMmio` node are handled by the existing path).
- The device is gated to Boot mode: in Restore mode it is not added even if a record exists
  (GUI-restore is M5). The `--gui` flag already exists; no new flag.
- Threading order: `--gui` ⇒ create sink/rx → `setup_devices` consumes `sink` → spawn VMM
  thread → `run_event_loop(rx, ...)` on main (unchanged from M2 except the device now feeds rx).

## Guest kernel (built remotely on `artemis2`, `kimage/`)

Add to the kernel config and rebuild (per `docs/src/getting-started/guest-assets.md`):
`CONFIG_DRM=y`, `CONFIG_DRM_VIRTIO_GPU=y`, `CONFIG_DRM_FBDEV_EMULATION=y`, `CONFIG_FB=y`,
`CONFIG_FRAMEBUFFER_CONSOLE=y`. This is a manual step; the device + unit tests do not depend on
it.

## Error handling

- Malformed/short requests, unknown command types, out-of-range resource ids, unbound
  scanouts: respond with the appropriate `ERR_*` (or `OK_NODATA` where the operation is a
  legitimate no-op), always `push_used`, never panic. A guest cannot wedge or crash the device.
- `TRANSFER` rect/offset out of the resource's bounds: clamp; copy only the in-bounds part.
- `present` is non-blocking (the sink drops/coalesces); a frozen window never backpressures the
  controlq.

## Testing

Unit (`crates/devices`, crafted descriptor chains over `GuestRam`+`Virtqueue`, no kernel, run in
CI — mirror `rng.rs` test style):
1. **Identity** — `device_id() == 16`, `queue_count() == 2`, `device_features(_) == 0`.
2. **`GET_DISPLAY_INFO`** — response type is `OK_DISPLAY_INFO`; 16 display_one entries; entry 0
   is `{0,0,1280,800}` enabled, entries 1..16 disabled/zeroed.
3. **`RESOURCE_CREATE_2D` + `ATTACH_BACKING`** (multi-entry) — resource table gains the id with
   the right dims; the SG list has all entries; response `OK_NODATA`.
4. **`SET_SCANOUT`** — `scanout_res` becomes the resource id; `resource_id = 0` disables.
5. **`TRANSFER_TO_HOST_2D` over a fragmented two-entry backing** — a known pixel pattern laid
   out across a segment boundary lands correctly in the host buffer (the SG-straddle test).
6. **`RESOURCE_FLUSH` presents** — with a **capturing test sink** (`struct CapSink(Arc<Mutex<
   Vec<Frame>>>)` implementing `DisplaySink`), a flush of the scanned-out resource pushes one
   `Frame` with the right `scanout_id`/dims/dirty rect and pixels matching the host buffer; a
   flush of a non-scanned-out resource pushes nothing.
7. **Unknown command** — an unrecognized `type` yields `ERR_UNSPEC` and a used buffer (no panic).
8. **cursorq** — a cursor command on queue 1 is ack'd with a zero-length used buffer.

Integration / manual (macOS, needs the hypervisor entitlement + a kernel rebuilt with the DRM
configs; documented as the milestone's acceptance):
- `boot --gui <kernel> <rootfs>`: `dmesg | grep -i "virtio_gpu\|drm"` shows the driver probing;
  `/dev/dri/card0` and `/dev/fb0` exist; the framebuffer console renders in the macOS window and
  scrolls as the guest prints. Re-sign after relink (`scripts/sign.sh`).
- Device-log the create/attach/set_scanout/transfer/flush sequence to confirm ordering.

## File structure

- Create `crates/devices/src/virtio/gpu.rs` — `VirtioGpu`, `Resource2D`, command parse/dispatch,
  the SG transfer, the FLUSH→present wiring, unit tests (incl. `CapSink`).
- Modify `crates/devices/src/virtio/mod.rs` — `pub mod gpu;`.
- Modify `spike/src/bin/display_sink.rs` — `App::redraw` blits a real `Frame` (B8G8R8A8 → 0RGB,
  dirty-rect aware) instead of always clearing; keep the clear path for the no-frame case.
- Modify `spike/src/bin/boot.rs` — create the `WindowSink` before `setup_devices`; add
  `display_sink: Option<Box<dyn DisplaySink>>` to `DeviceContext`; register the virtio-gpu device
  (Boot + `--gui` only); keep `rx` for `run_event_loop`.
- Modify `docs/src/features/devices.md` — note virtio-gpu (2D) and that `--gui` now shows the
  guest framebuffer; mention the required kernel configs.
- Modify `docs/src/getting-started/guest-assets.md` — the DRM/virtio-gpu/fbcon kernel configs.
- Modify `ROADMAP.md` — GUI M1 progress line.

## End state

A guest under `boot --gui` binds the Linux `virtio_gpu` driver, its framebuffer console renders
live in the macOS window via the M2 `DisplaySink` seam, and the whole virtio-gpu 2D protocol is
covered by host unit tests including the SG-straddle transfer and a capturing-sink flush. No 3D,
no snapshot-of-GPU (M5), no input yet (M3). This is the first milestone with a visible result on
screen and the foundation the compositor (M4) will drive.
