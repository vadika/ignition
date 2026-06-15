# GUI compositor + app (M4) — cage + foot on virtio-gpu 2D — Design

Date: 2026-06-15. Status: approved design, ready for an implementation plan.

Fourth milestone of the 2D GUI bring-up (umbrella plan:
`docs/superpowers/specs/2026-06-15-gui-bringup-plan.md`). M1 (virtio-gpu 2D), M2 (winit
event loop + DisplaySink), and M3 (virtio-input keyboard + tablet) are shipped and verified
live: `boot --gui` renders the guest framebuffer console in a macOS window and is interactive.
This milestone runs a real **Wayland compositor + app** in that window instead of the bare
framebuffer console.

## Context & decisions locked (brainstorming)

- **cage + foot.** `cage` (a wlroots-based kiosk compositor: one app, fullscreen) running
  `foot` (a lightweight Wayland terminal) — the "disposable Linux desktop / throwaway shell"
  story. Smallest reliable software-rendered target.
- **Software rendering, no GL.** wlroots uses its **pixman** renderer (`WLR_RENDERER=pixman`)
  and foot uses `wl_shm` — a fully software path, no EGL/GL/mesa. This is mandatory: our
  virtio-gpu is **2D only** (no VIRGL/3D), so a GL compositor cannot work. pixman draws into a
  DRM dumb buffer the virtio-gpu device presents.
- **Seat via `seatd`.** The Alpine `seatd` daemon (OpenRC service) provides seat management;
  cage runs as root and uses libseat→seatd to open `/dev/dri/card0` + `/dev/input/*`. (If
  seatd proves troublesome live, the fallback is `LIBSEAT_BACKEND=builtin` as root — an
  iteration detail, not a design change.)
- **Separate GUI rootfs.** The GUI userspace is built by a **new, separate** script
  `kimage/build/build-rootfs-gui.sh` producing a **distinct artifact `rootfs-gui.ext4`**. The
  minimal base (`build-rootfs.sh` → `rootfs.ext4`) is left untouched, so base and GUI rootfs
  variants coexist and can be built independently. `boot --gui` is pointed at
  `rootfs-gui.ext4`.
- **Auto-start kiosk, serial fallback.** An init service launches cage at boot; the serial
  console (ttyS0) always keeps its getty as the debug fallback if the compositor fails.
- **No host VMM code anticipated.** The existing virtio-gpu + virtio-input protocol already
  gives the compositor everything it needs. Live testing may surface a device gap; see Risks.

## Goal

`boot --gui kimage/out/Image kimage/out/rootfs-gui.ext4` boots the GUI rootfs, cage takes the
virtio-gpu scanout, and **foot renders fullscreen in the macOS window** with a working shell.
Typing (virtio-input keyboard) reaches the shell in foot; the pointer (virtio-input tablet)
drives a compositor-rendered software cursor. The minimal `rootfs.ext4` and all non-`--gui`
paths are unchanged. Acceptance is live/manual (no host code to unit-test).

Non-goals (M4): GL/3D acceleration (software only — llvmpipe/GL deliberately avoided); a
browser or multi-window desktop shell (cage is single-app kiosk); audio, clipboard,
drag-and-drop; hardware cursor plane (software cursor); snapshot/clone of the GUI session (M5);
multi-monitor / resize / hotplug.

## Architecture

### New build script `kimage/build/build-rootfs-gui.sh` → `out/rootfs-gui.ext4`

Self-contained (its own container build + ext4 pack), mirroring `build-rootfs.sh`'s base
provisioning (busybox/openrc init, ttyS0 + tty1 getty, no-password root, networking, boot-timer,
net-watch, devmem) so the GUI rootfs is a superset of the base, then adds the GUI layer:

- **Packages:** `apk add cage foot seatd font-terminus` (apk pulls wlroots, libinput,
  libxkbcommon, wayland, pixman, libdrm as deps — **no mesa/GL needed** for the pixman path).
  `font-terminus` (or `font-misc-misc`) gives foot a monospace font.
- **seatd:** `rc-update add seatd default` so the seat daemon runs at boot.
- **cage-kiosk service:** an OpenRC service (`/etc/init.d/cage-kiosk`) added to the `default`
  runlevel, ordered after `seatd` and `udev`/devfs. Its start:
  - if `/dev/dri/card0` is absent (booted without `--gui`), log and exit 0 (no-op).
  - `export XDG_RUNTIME_DIR=/run/user/0` (create `0700` root-owned), `WLR_RENDERER=pixman`,
    `WLR_RENDERER_ALLOW_SOFTWARE=1`, `XKB_DEFAULT_LAYOUT=us`.
  - run `cage -- foot` as root (cage acquires DRM master on card0 and launches foot fullscreen).
  - on cage exit, the service ends; the serial console remains for debugging.
- **Larger image:** the GUI tree is much bigger than 96 MiB; size the ext4 generously
  (`768M`) in the `mke2fs -d` step. (Trim later if desired.)
- A distinct container name (`fcroot_gui_build`) and tar staging path so it can build alongside
  the base without collision.

### Boot

`boot --gui <Image> rootfs-gui.ext4` — virtio-gpu + virtio-input are registered under `--gui`
(M1/M3, unchanged). The guest boots, seatd starts, cage-kiosk starts, cage opens card0 as DRM
master (the fbcon hands off the single scanout), pixman composites foot's surface into a DRM
dumb buffer, the virtio-gpu driver `TRANSFER`s + `FLUSH`es it, and the device presents it to the
window (M1's FLUSH→present→blit, with the M3 full-reread-on-FLUSH so any compositor damage/flip
pattern renders correctly).

### Host (no changes anticipated)

The virtio-gpu device already implements the controlq command set the wlroots DRM backend uses
(GET_DISPLAY_INFO, CREATE_2D, ATTACH/DETACH_BACKING, SET_SCANOUT, TRANSFER_TO_HOST_2D,
RESOURCE_FLUSH) and ack's cursorq (so wlroots falls back to a software cursor). virtio-input
provides the keyboard + absolute pointer libinput consumes. No host code change is expected; if
live testing reveals a gap, it is fixed in `crates/devices` with a unit test (see Risks).

## Risks / live-testing watch-list

These are the things most likely to need a host fix or guest config tweak, discovered live:

- **Double-buffering / scanout flips.** wlroots may create two resources and flip `SET_SCANOUT`
  per frame. Our FLUSH presents the currently-bound resource and re-reads its full backing, so
  this should render; watch for tearing or a stale half if the flip/flush ordering differs.
- **Hardware cursor on cursorq.** wlroots may try `UPDATE_CURSOR`; we ack and ignore the image,
  so wlroots should fall back to compositing a software cursor into the surface. If the cursor
  is invisible, confirm the software-cursor fallback (or accept no cursor for M4).
- **GET_EDID.** wlroots may issue `VIRTIO_GPU_CMD_GET_EDID`; we return ERR_UNSPEC (no EDID),
  and wlroots should fall back to `GET_DISPLAY_INFO`'s mode. Watch for a probe failure.
- **seatd vs builtin libseat.** If cage can't get a seat via seatd, switch to
  `LIBSEAT_BACKEND=builtin` (root) — a one-line env change in the service.
- **DRM master handoff from fbcon.** cage must become DRM master while fbcon holds the console.
  If cage fails to acquire the master, may need `WLR_DRM_NO_ATOMIC=1` or to stop the tty1 getty
  on the GUI VT. Iterate live.
- **Memory.** cage+foot+wlroots want more RAM than the console-only guest; boot the GUI rootfs
  with a larger `--mem` (e.g. `--mem 512`) if the default is tight.

Each of these is a documented iteration point, not a blocker to the plan.

## Implementation / execution model (differs from M1–M3)

This milestone has **no host Rust code and no unit tests** to drive via subagent-TDD. It is a
guest-image + live-iteration loop, executed directly:

1. Write `build-rootfs-gui.sh` (base provisioning + GUI layer).
2. Build it on `artemis2` (`scp` the script + `devmem.c`, run, pull `rootfs-gui.ext4` to
   `kimage/out/`).
3. `boot --gui Image rootfs-gui.ext4 --mem 512`; observe the window + serial log; iterate on
   the cage-kiosk service / env / package set until foot renders and is interactive.
4. If a device gap appears, fix it in `crates/devices` (with a unit test) and rebuild the VMM.
5. Document the GUI rootfs build + the `--gui` GUI usage.

The "plan" is this sequence of concrete steps + the live acceptance checks, not a set of
TDD tasks.

## Testing

- **Automated:** none new on the host (no host code unless a device gap is found, which then
  gets a `crates/devices` unit test). The existing 216 workspace tests must stay green.
- **Live / manual (the acceptance):**
  - `boot --gui Image rootfs-gui.ext4 --mem 512` → cage starts (serial log shows the cage-kiosk
    service), foot renders fullscreen in the window.
  - Type in the window → input reaches the shell in foot (echoes, runs commands).
  - Move the pointer → a software cursor tracks the macOS cursor.
  - Booting the **base** `rootfs.ext4` (or any rootfs without cage) still works; non-`--gui`
    boot of the GUI rootfs falls back to the serial/tty1 console (cage-kiosk no-ops on missing
    card0).

## File structure

- Create `kimage/build/build-rootfs-gui.sh` — the GUI rootfs builder (base provisioning + cage
  /foot/seatd/font + seatd enable + cage-kiosk service), outputs `out/rootfs-gui.ext4`.
- Modify `docs/src/getting-started/guest-assets.md` — a "Rebuild the GUI rootfs" section
  (remote build + pull, mirrors the base rootfs section; note the larger artifact).
- Modify `docs/src/features/devices.md` — note that `--gui` with `rootfs-gui.ext4` runs a
  cage+foot Wayland kiosk (software-rendered).
- Modify `ROADMAP.md` — mark GUI M4.
- `crates/devices/src/virtio/gpu.rs` (and tests) — **only if** live testing reveals a device
  gap (e.g. a controlq command wlroots needs that we ERR today).

## End state

`boot --gui Image rootfs-gui.ext4` runs a real Wayland compositor (cage) with a terminal (foot)
software-rendered into the macOS window, interactive via the virtio-input keyboard + pointer —
a usable throwaway Linux desktop on bare HVF. The base minimal rootfs and all non-`--gui` paths
are untouched, and GUI vs base rootfs variants build independently. Only snapshot/clone of the
live GUI session (M5) remains.
