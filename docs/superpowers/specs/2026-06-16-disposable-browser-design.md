# Disposable browser microVM (design)

**Status:** approved 2026-06-16 (architecture + components + defaults)
**Track:** disposable-sandbox showcase. Sub-project B of the "disposable browser" idea.
**Predecessors:** GUI M1–M5 (cage + virtio-gpu/input, `present_scanout`), snapshot/restore
+ diff-snapshots + dirty-tracking, vmnet `--net` + the netwatch carrier-poller, and
**sub-project A: interactive reset-to-checkpoint** (`Ctrl-A c` / `Ctrl-A r`, merged).

## Goal

A throwaway web browser in a microVM: a GUI guest running **Firefox ESR `--kiosk`**
under cage, cloned fresh per session from a **warm snapshot** (Firefox already
launched, homepage loaded), with networking, reset to its clean state in place via
`Ctrl-A r`, fanned out N at a time, and torn down on window close.

This is the showcase that motivated sub-project A. The interactive-reset mechanism
already exists; this sub-project supplies the rootfs, the warm-base workflow, and the
launch tooling that make the disposable browser real.

## The load-bearing constraint (from sub-project A)

`Ctrl-A r` rolls back RAM + vCPU + GIC + virtio-device state but **does not rewind the
disk**. It is sound only if the disk does not diverge between checkpoint and reset.
This design satisfies that **by construction**: the guest root is an **overlay**
(read-only ext4 lower + tmpfs upper), so every write lands in tmpfs (guest RAM) and the
block device is never written. RAM rollback therefore rolls back all mutable state, and
the disk is immutable. No per-path enumeration, no disk-divergence footgun.

## Boot model

```
COLD BOOT (one-time, builds the warm-base):
  boot --gui --net --track-dirty --mem 1024 --append "init=/sbin/overlay-init" \
       kimage/out/Image kimage/out/rootfs-browser.ext4
   - kernel mounts ext4 root read-only, runs /sbin/overlay-init as PID1:
       tmpfs upper -> overlay(lower=ro-ext4, upper=tmpfs) -> switch_root into openrc
   - openrc: udev / seatd / cage; cage runs `firefox-esr --kiosk <homepage>`
   - readiness hook prints BROWSER_READY on /dev/ttyS0
   - Ctrl-A s -> snapshot "browser-base"   (disk = RO ext4; all writes are in tmpfs/RAM)

DISPOSABLE SESSION (every use, fast):
  disposable-browser.sh [-n N] [browser-base]
   - boot --gui --net --mem 1024 --track-dirty --restore browser-base   (x N)
   - overlay is already mounted (it lives in the restored RAM image) -> no kernel boot,
     no overlay-setup; the reset point is auto-seeded from the snapshot
   - browse; Ctrl-A r snaps back to the warm homepage; close window = teardown
     (per-pid instance dir cleaned on exit)
```

Three facts this rests on:
- The overlay makes every write go to RAM, so the disk never diverges and `Ctrl-A r` is
  safe with no caveat.
- The kernel/overlay/`init=` machinery is exercised **only at the cold boot** that builds
  the warm-base. Restore reconstructs the mounted overlay from `memory.bin`, so fan-out,
  restore, and reset use none of it.
- `Ctrl-A r` rolls back RAM including the overlay's tmpfs upper, so Firefox returns to the
  warm homepage with history/cookies/cache gone. That is the disposable reset.

## Components

### 1. Kernel — `kimage/build/build-kernel.sh`
Add to the `scripts/config` block (before `olddefconfig`):
- `CONFIG_OVERLAY_FS=y` — the overlay root.
- `CONFIG_TMPFS=y` — the upper layer (assert; likely already enabled).

Rebuild on artemis2, pull `Image`, verify the ARMd magic. Additive only — existing GUI/base
guests still boot. Matters solely for the cold boot that builds the warm-base (restore does
not reload the kernel).

### 2. boot.rs — `--append <str>` cmdline knob
`layout::default_cmdline()` is currently a fixed `root=/dev/vda rw rootwait reboot=k panic=1`.
Add a `--append "<args>"` flag that concatenates onto the default cmdline for the normal/GUI
path (plumbed into the `FdtConfig.cmdline` built in `main`). Used here to pass
`init=/sbin/overlay-init`. General and minimal: one arg-parse arm, append in cmdline
construction, default behavior unchanged when the flag is absent. Unit test: the appended
string is present in the produced cmdline; absence reproduces the current default.

### 3. `/sbin/overlay-init` (shipped in the browser rootfs)
The kernel's PID1 at cold boot. busybox shell; sets up the overlay and `switch_root`s into
openrc:
```sh
#!/bin/sh
mount -t proc proc /proc
mount -t sysfs sys /sys
mount -t tmpfs tmpfs /mnt
mkdir -p /mnt/up /mnt/work /mnt/root /mnt/lower
mount --bind / /mnt/lower
mount -t overlay overlay -o lowerdir=/mnt/lower,upperdir=/mnt/up,workdir=/mnt/work /mnt/root
exec switch_root /mnt/root /sbin/init
```
After `switch_root` the merged `/` is writable (tmpfs upper) and the ext4 lower is read-only.
Exact `/proc`,`/sys` re-mount handling across the pivot is finalized during implementation;
this is the shape. NOTE: this script is shell heredoc'd inside the rootfs build — no
apostrophes in its comments (the build wraps the provisioning in `sh -euxc '...'`).

### 4. Browser rootfs — `kimage/build/build-rootfs-browser.sh`
Derived from `build-rootfs-gui.sh`:
- Keep: openrc base, udev/seatd, the netwatch carrier-poller (so each clone re-DHCPs with a
  fresh MAC), ttyS0 getty, the cage + libinput/xkb stack.
- Add packages: `firefox-esr`, `mesa-dri-gallium` + `mesa-gl` (llvmpipe software GL —
  Firefox's WebRender needs real GL, unlike foot's pixman path), `ca-certificates`,
  `font-dejavu`.
- Swap the `cage-kiosk` service command from `foot` to `firefox-esr --kiosk <homepage>`,
  run with a clean kiosk profile: a baked `user.js`/`policies.json` disabling first-run,
  telemetry, and update checks and setting the homepage; software-GL env
  (`LIBGL_ALWAYS_SOFTWARE=1`, plus the Moz software-render env as needed). Confirm the exact
  Moz env during bring-up.
- Ship `/sbin/overlay-init` (component 3).
- Bump the ext4 image size (firefox + mesa is far larger than the foot rootfs); pack with
  `mke2fs -d` as before.
- Output `rootfs-browser.ext4`.
- **Homepage** is a build-time arg/env, default `https://duckduckgo.com` (a usable search
  start; `about:blank` available for pure-neutral).

### 5. Readiness hook (in the rootfs)
After cage launches Firefox, a small loop waits until the `firefox` process exists and its
Wayland surface is mapped, settles briefly, then `echo BROWSER_READY > /dev/ttyS0`. Consumed
only by `make-browser-base.sh`; harmless on a normal interactive boot. The settle may need
tuning per homepage weight; the manual warm-base flow does not depend on it.

### 6. `scripts/make-browser-base.sh`
Automates warm-base creation. Cold-boots the rootfs with
`--gui --net --track-dirty --mem 1024 --append init=/sbin/overlay-init`, feeds boot's stdin
from a pipe, watches serial for `BROWSER_READY`, then writes `Ctrl-A s` (the escape FSM maps
`\x01 s` to `Action::Snapshot`) to snapshot `browser-base`, awaits the `[snapshot ...]`
confirmation line, and writes `Ctrl-A x` to quit. The script's header documents the manual
equivalent (boot the same command, watch the window paint the homepage, press `Ctrl-A s`).

### 7. `scripts/disposable-browser.sh`
Mirrors `scripts/fanout-gui.sh`: usage/validation, default snapshot name `browser-base`,
optional `-n N` fan-out; launches N × `boot --gui --net --mem 1024 --track-dirty --restore
<name>` in the background with a trap that kills all on EXIT/INT/TERM, then `wait`. `--net`
(shared/NAT vmnet) needs sudo; the script notes this and the base snapshot is never mutated
(each restore gets its own CoW instance).

### 8. Docs
- New `docs/src/features/disposable-browser.md`: the showcase end to end — build the rootfs,
  create the warm-base (manual + helper), run a disposable session, `Ctrl-A r` reset, fan-out
  with `--net`, and the RO-overlay / disk-safety story tying back to interactive-reset.
- `docs/src/getting-started/guest-assets.md`: a "Rebuild the browser rootfs" section
  (build-rootfs-browser.sh) + the `CONFIG_OVERLAY_FS` kernel note.
- Cross-link from `snapshot-restore.md` / `devices.md` where the GUI rootfs is described.

## Error handling & edge cases
- **Firefox software-GL bring-up** is the main unknown: WebRender on llvmpipe may need
  specific Moz env (`MOZ_WEBRENDER`, `MOZ_ACCELERATED`, `LIBGL_ALWAYS_SOFTWARE`). Resolved by
  live eyeball during the rootfs task; documented once it renders.
- **overlay `switch_root`** is unproven on this kernel/boot path — validated at the cold boot
  that creates the warm-base. If the pivot fails the guest drops to the ttyS0 console with a
  clear error (overlay-init logs each mount step).
- **`--mem`**: Firefox needs ~1 GiB; `--mem 1024` is the documented default for both the
  cold boot and the restore. Fan-out of N clones costs ~N GiB host RAM (CoW disk is shared,
  RAM is per-clone) — the wrapper notes this.
- **Net**: each clone re-DHCPs via the netwatch poller (carried over from the GUI rootfs);
  distinct MAC/IP per clone, already proven for fan-out.

## Out of scope (YAGNI)
- Per-session start URL (the snapshot is frozen at the baked homepage; a different start page
  means a new warm-base).
- Multiple browser profiles / persistent profile across sessions (the point is ephemerality).
- A non-Firefox browser, GPU acceleration, audio, or a download-to-host path.
- Headless (no-window) warm-base creation (cage needs `/dev/dri/card0`, which exists only
  under `--gui`, so a window appears during the one-time creation).
- Automating disk rollback (not needed — the overlay makes the disk immutable).

## Testing
- **Unit (Rust):** boot.rs `--append` lands the extra args in the produced cmdline; absent
  flag reproduces `default_cmdline()` exactly. (Mirror the existing `default_cmdline` test.)
- **Build verification:** `rootfs-browser.ext4` packs and carries `firefox-esr`,
  `/sbin/overlay-init`, mesa-dri-gallium; `Image` rebuilt with `CONFIG_OVERLAY_FS=y`
  (`zcat`/config check on artemis2).
- **Live eyeball (the real gate):**
  1. Cold boot with `--append init=/sbin/overlay-init`: overlay mounts, `mount` shows
     `overlay on /`, Firefox kiosk paints the homepage in the window.
  2. `make-browser-base.sh` produces a `browser-base` snapshot (serial shows `BROWSER_READY`
     then `[snapshot ...]`).
  3. `disposable-browser.sh`: window opens with Firefox at the homepage; browse to another
     site.
  4. `Ctrl-A r`: snaps back to the warm homepage, history/cookies cleared, still interactive.
  5. Verify the disk did NOT diverge: after several `Ctrl-A r`, no ext4 errors; `mount`
     still shows the overlay; the ext4 lower is read-only.
  6. `disposable-browser.sh -n 3 --net` (sudo): three windows, three browsers, distinct IPs;
     resetting one does not affect the others; closing a window tears only that clone down.
