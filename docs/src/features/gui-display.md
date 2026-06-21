# GUI display (software-rendered)

`boot --gui <kernel> <rootfs>` opens a macOS window backed by a CPU
framebuffer (`winit` + `softbuffer`, no Metal). The Linux guest renders into the
window through a virtio-gpu device; a pair of virtio-input devices make the
window interactive; and the GUI rootfs runs a cage Wayland kiosk for a full
software-rendered desktop.

## The macOS window

On macOS the winit event loop must own the main thread. Under `--gui` the entire
VMM — vCPU threads, the serial console reader, the vsock reactor, the vmnet RX
feeder — runs on spawned threads while the event loop runs on main. The window
title is "ignition". The guest boots at an initial scanout (`GUI_W`×`GUI_H`,
currently 1400×880), chosen a touch under the host's work area so the window's
**logical** size equals the scanout. On a 2× Retina display the physical surface
is then exactly 2× the scanout, so the blit path upscales by an integer factor
(sharp pixel-doubling, no fractional blur). The window is centered in the work
area, leaving a symmetric gap on each side and room for the title bar. On a
display smaller than the scanout it downscales to fit.

The window is **resizable**: dragging it re-modesets the guest so the desktop
reflows to the new size, rather than just rescaling a fixed scanout (see [Runtime
resize](#runtime-resize) below). The boot scanout is still baked into a snapshot,
so changing `GUI_W`/`GUI_H` requires rebuilding the base; a restored guest reopens
at the base size and resizes live from there (see [Disposable
browser](disposable-browser.md)).

The present path is non-blocking: frames arrive over an mpsc channel and are
coalesced to the latest before each blit, so a slow or frozen window never
backpressures the guest. The window holds its last frame between guest flushes
(no flash to a clear color on idle redraws). Closing the window ends the session
— the process exits and tears the disposable guest down. The serial console keeps
working alongside the window throughout.

Without `--gui` (the default) — and for `--restore` and `--fuzz` — no window
opens and the vCPU loop runs on the main thread as before.

## virtio-gpu (2D)

A **virtio-gpu** device (device id 16) is added under `--gui`. The Linux
`virtio_gpu` driver binds it, `/dev/dri/card0` and `/dev/fb0` appear, and the
kernel framebuffer console renders live in the macOS window. Two commands drive
the display path:

- `TRANSFER_TO_HOST_2D` — copies guest pixels (scatter-gather correct) from
  guest RAM into a host-side buffer.
- `RESOURCE_FLUSH` — presents the scanned-out resource through the display sink,
  forwarding the frame to the winit event loop.

Runtime resize is supported via the config-change path (see [Runtime
resize](#runtime-resize)); `GET_DISPLAY_INFO` reports the live mode and an
`events_read`/`events_clear` register pair carries `VIRTIO_GPU_EVENT_DISPLAY`. No
3D, VIRGL, or Venus support. GPU resource table and scanout binding are serialized
as part of snapshot state (see below).

The guest kernel must be built with:

```
CONFIG_DRM=y
CONFIG_DRM_VIRTIO_GPU=y
CONFIG_DRM_FBDEV_EMULATION=y
CONFIG_FB=y
CONFIG_FRAMEBUFFER_CONSOLE=y
```

## virtio-input

Under `--gui`, two **virtio-input** devices (device id 18) make the window
interactive: a keyboard (`EV_KEY`) and an absolute tablet (`EV_ABS` x/y +
buttons). The winit event loop translates host key/pointer/click events into
Linux evdev events and injects them into the guest's eventq (`inject_rx`-style
path), so typing logs in at the console and the pointer tracks the macOS cursor
1:1 over the scanout.

Mouse position is scaled from the physical surface size into the tablet's fixed
absolute axis range (`0..32767`, the QEMU virtio-tablet convention); libinput maps
that range onto the current guest output extent, so the pointer stays correct at
any resolution and a [runtime resize](#runtime-resize) never touches input. Button
events map to `BTN_LEFT`/`BTN_RIGHT`/`BTN_MIDDLE`.
Scroll is supported: the tablet advertises `EV_REL`/`REL_WHEEL`, and the window
translates `MouseWheel` events into `REL_WHEEL` (trackpad sub-notch `PixelDelta`
is accumulated so slow scrolls aren't rounded away). Physical key codes map to
Linux evdev scan codes; unmapped keys are dropped silently.

Note: the wheel axis is registered by the guest driver only at probe (guest
boot). A guest restored from a snapshot taken before scroll support was added has
no wheel axis, so injected `REL_WHEEL` events are dropped — rebuild the base
snapshot to pick it up.

The guest kernel needs:

```
CONFIG_VIRTIO_INPUT=y
CONFIG_INPUT_EVDEV=y
```

## Runtime resize

The `--gui` window is resizable, and dragging it makes the **guest reflow** to the
new size — not just rescale a fixed scanout. The chain is the standard virtio-gpu
display-info path plus one wrinkle for the kiosk compositor:

1. **Host** — `WindowEvent::Resized` is debounced (~150 ms after the drag settles);
   the target guest size is the window's logical size, clamped to `[320×240,
   GUI_W×GUI_H]` and even-rounded. During the drag the blit just rescales the last
   frame, so the window stays live.
2. **Device → guest** — the host sets the device's advertised mode and raises a
   config-change interrupt (`VIRTIO_GPU_EVENT_DISPLAY` in `events_read`). The Linux
   `virtio_gpu` driver acks (`events_clear`), re-queries `GET_DISPLAY_INFO`, and
   raises a DRM hotplug.
3. **Connector-cycle** — cage (wlroots) picks its output mode **only** when an
   output is created; it never re-picks on a bare mode-list change. So a resize is
   driven as a **disconnect → reconnect**: phase 1 reports the scanout connector
   disabled (cage destroys the output); after a short gap, phase 2 re-enables it at
   the new mode (cage's `handle_new_output` adopts the new preferred mode and the
   desktop reflows). The transition shows a brief (~100 ms) blank.

Pointer mapping is resolution-independent (fixed tablet range, see virtio-input
above), so input needs no per-resize update.

**Compositor floor:** this needs **cage ≥ 0.2.0**. cage 0.1.5 (Alpine 3.19)
calls `wl_display_terminate` when the sole output is destroyed, so the disconnect
phase would kill the kiosk; cage 0.2.0 (Alpine 3.21) terminates only for *nested*
backends, so the DRM-backed kiosk survives the cycle. The browser rootfs is pinned
to Alpine 3.21 for this (`kimage/build/build-rootfs-browser.sh`).

## Wayland compositor (cage + foot)

With the GUI rootfs (`rootfs-gui.ext4`, built by
`kimage/build/build-rootfs-gui.sh`), `--gui` runs a **cage** Wayland kiosk
(wlroots **pixman** software renderer — no GL, matching the 2D-only virtio-gpu)
hosting a **foot** terminal: an interactive software-rendered Linux desktop in
the macOS window, driven by the virtio-input keyboard + pointer.

The compositor path exercises fenced virtio-gpu commands — page-flips set
`VIRTIO_GPU_FLAG_FENCE`, and the device signals the fence in its response so
wlroots's render loop keeps producing frames. Without fence signaling the
compositor renders one frame then stalls.

The minimal base rootfs has no compositor and uses the framebuffer console
directly. The [disposable browser](disposable-browser.md) swaps foot for Firefox
ESR, with cage fullscreening the single browser window.

## GUI snapshot, restore & fan-out

A `--gui` guest snapshots and restores like any other. `Ctrl-A s` writes a
complete snapshot of the live desktop (RAM, GIC, vCPU registers, device state),
and `boot --gui --restore <name>` reopens a window with the desktop resuming
where it left off. The virtio-gpu resource table and scanout binding plus the
virtio-input config cursor are serialized; pixels are not — on restore the device
re-reads the scanout from the restored guest-RAM backing and presents one frame,
so the window paints the resumed screen before the guest runs again.

Because each restore clones the immutable base into its own copy-on-write
instance directory (keyed by pid), one warm-base snapshot fans out into N
independent desktops, each with its own window:

```console
# take one warm-base snapshot of a logged-in desktop (Ctrl-A s), then:
scripts/fanout-gui.sh 3 warm-base
# -> 3 boot --gui --restore processes, 3 windows, 3 isolated guests
```

Networking fans out too: with `--net` (needs `sudo` for vmnet shared mode) each
clone gets its own MAC and DHCP lease, since the GUI rootfs runs the same
`netwatch` carrier-poller as the base rootfs — every restore starts a fresh
vmnet interface, bounces the virtio-net link, and re-runs DHCP. Without the
poller a restored guest would keep the snapshot's MAC.

```console
sudo scripts/fanout-gui.sh 3 warm-base --net
```

See [Snapshot & restore](snapshot-restore.md) for the full mechanism, the
`--track-dirty` incremental path, and the read-only-disk requirement.

## GUI window hotkeys

The focused window swallows keyboard input, so the serial `Ctrl-A` chords do not
reach the serial console FSM from the GUI window (they still work on a foreground
serial console when the window is not focused). Three `Ctrl+Alt+<letter>` chords
are intercepted by the window before the key reaches the guest:

| Hotkey | Action |
|--------|--------|
| `Ctrl+Alt+R` | **Cold reset (relaunch):** the process exits with a sentinel code; a launcher (e.g. `disposable-browser.sh`) re-`--restore`s it from the snapshot. The window blinks and reopens at the warm state. Prints `[gui] reset: relaunching clone from snapshot`. |
| `Ctrl+Alt+S` | Write a disk snapshot of the current desktop state. |
| `Ctrl+Alt+X` | Close the window and end the session. |

`Ctrl+Alt+R` deliberately does **not** roll back in place under `--gui`. An in-place
rollback of a live, actively-rendering desktop cannot reconcile the running GIC and
virtio devices (net, vtimer, and the virtio-gpu fence pipeline) with the rolled-back
guest — `hv_gic_set_state` is create-time-only on HVF, so in-flight interrupt state
wedges the display/network under load. A fresh `--restore` (the relaunch) builds clean
device instances and the guest re-initialises, so it is reliable. The in-place reset
(`Ctrl-A r` on a serial console) is retained for headless guests, where it works.

The serial console still uses `Ctrl-A x` (quit), `Ctrl-A s` (snapshot), `Ctrl-A
b` (reboot), `Ctrl-A c` (mark in-memory checkpoint), and `Ctrl-A r` (roll back
to checkpoint). See [Snapshot & restore — interactive reset](snapshot-restore.md#interactive-reset-to-checkpoint)
for the full `Ctrl-A c`/`r` behaviour and the dirty-tracking detail.

## Related

- [Devices, SMP & networking](devices.md) — the virtio transport and device trait
  these devices build on.
- [Snapshot & restore](snapshot-restore.md) — full snapshot/restore/fan-out
  mechanism and interactive reset-to-checkpoint.
- [Disposable browser](disposable-browser.md) — cage + Firefox over the same
  virtio-gpu/virtio-input stack.
- [Device model](../concepts/device-model.md) — the `MmioDevice` trait.
