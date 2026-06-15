# GUI compositor (M4) — cage + foot — Implementation Plan

> **For agentic workers:** This milestone is a guest-image build + live-iteration loop, NOT host code with unit tests. Use **superpowers:executing-plans** (inline, with checkpoints) rather than subagent-driven TDD — there is no host Rust to test-drive. Steps use checkbox (`- [ ]`) syntax. Any host device fix that live testing turns up gets a `crates/devices` unit test at that point.

**Goal:** `boot --gui Image rootfs-gui.ext4` runs a cage (wlroots, pixman software renderer) Wayland kiosk with a foot terminal, rendered into the macOS window and driven by the virtio-input keyboard + pointer.

**Architecture:** A new, separate `kimage/build/build-rootfs-gui.sh` builds a GUI rootfs (`rootfs-gui.ext4`) = the base provisioning (from `build-rootfs.sh`) + cage/foot/seatd/font + a `cage-kiosk` OpenRC service. The minimal `build-rootfs.sh`/`rootfs.ext4` is untouched. No host VMM change is expected; the existing virtio-gpu (2D) + virtio-input devices already satisfy wlroots' pixman path.

**Tech Stack:** Alpine 3.19 arm64 (built remotely on `artemis2` via Docker), cage + foot + seatd + wlroots (pixman), the existing ignition VMM (`--gui`).

---

## File Structure

- `kimage/build/build-rootfs-gui.sh` — **create.** Self-contained GUI rootfs builder → `out/rootfs-gui.ext4`. Mirrors `build-rootfs.sh`'s base provisioning, adds the GUI layer + cage-kiosk service, packs a larger (768 MiB) ext4.
- `docs/src/getting-started/guest-assets.md` — **modify.** "Rebuild the GUI rootfs" section.
- `docs/src/features/devices.md`, `ROADMAP.md` — **modify.** Note M4.
- `crates/devices/src/virtio/gpu.rs` (+tests) — **only if** live testing reveals a controlq gap wlroots needs.

---

## Task 1: Write `build-rootfs-gui.sh`

**Files:** Create `kimage/build/build-rootfs-gui.sh`.

- [ ] **Step 1: Create the script** with exactly this content:

```bash
#!/usr/bin/env bash
# Build a GUI aarch64 rootfs: base (busybox+openrc, getty, net, boot-timer) PLUS a
# cage (wlroots, pixman software renderer) Wayland kiosk running foot. Output:
# ~/kbuild/out/rootfs-gui.ext4. The minimal base lives in build-rootfs.sh; this is
# a separate, larger artifact so the two rootfs variants build independently.
set -euo pipefail

OUT="$HOME/kbuild/out"
STAGE="$HOME/kbuild"
mkdir -p "$OUT"
TAR="$STAGE/rootfs-gui.tar"

# 1. Provision the GUI rootfs inside an arm64 alpine container.
docker rm -f fcroot_gui_build >/dev/null 2>&1 || true
docker run --platform linux/arm64 --name fcroot_gui_build \
  -v "$(cd "$(dirname "$0")" && pwd)/devmem.c:/devmem.c:ro" \
  alpine:3.19 sh -euxc '
  # --- base provisioning (kept in sync with build-rootfs.sh) ---
  apk add --no-cache openrc util-linux ifupdown-ng socat

  apk add --no-cache --virtual .build gcc musl-dev
  gcc -O2 -static /devmem.c -o /usr/bin/devmem
  apk del .build

  ln -sf agetty /etc/init.d/agetty.ttyS0
  echo ttyS0 > /etc/securetty
  rc-update add agetty.ttyS0 default
  ln -sf agetty /etc/init.d/agetty.tty1
  echo tty1 >> /etc/securetty
  rc-update add agetty.tty1 default
  rc-update add devfs boot
  rc-update add procfs boot
  rc-update add sysfs boot

  passwd -d root || true

  grep -q "ln -sf /dev/ttyS0 /dev/tty" /etc/inittab ||
    printf "::sysinit:/bin/ln -sf /dev/ttyS0 /dev/tty\n" >> /etc/inittab

  mkdir -p /etc/network /etc/local.d
  printf "auto lo\niface lo inet loopback\n\nauto eth0\niface eth0 inet dhcp\n" > /etc/network/interfaces
  printf "#!/bin/sh\nifup -a\n" > /etc/local.d/network.start
  chmod +x /etc/local.d/network.start
  printf "#!/bin/sh\ndevmem 0x091FF000 8 123\n" > /etc/local.d/boottime.start
  chmod +x /etc/local.d/boottime.start
  rc-update add local boot

  # --- GUI layer: cage + foot + seatd over the virtio-gpu/input devices ---
  # cage/foot/seatd live in the alpine community repo; enable it.
  echo "https://dl-cdn.alpinelinux.org/alpine/v3.19/community" >> /etc/apk/repositories
  apk update
  # pixman software path: no mesa/GL. cage pulls wlroots/libinput/wayland/pixman/libdrm.
  apk add --no-cache cage foot seatd font-terminus

  # seat daemon for cage to open DRM + input devices.
  rc-update add seatd default

  # cage-kiosk service: launch cage(foot) once a virtio-gpu scanout exists. Runs as
  # root, software renderer, logs to /var/log/cage.log (read via the serial console
  # for debugging). No-ops cleanly when booted without --gui (no /dev/dri/card0).
  cat > /etc/init.d/cage-kiosk <<'"'"'CAGEEOF'"'"'
#!/sbin/openrc-run
description="cage kiosk (foot) on the virtio-gpu framebuffer"

export XDG_RUNTIME_DIR=/run/user/0
export WLR_RENDERER=pixman
export WLR_RENDERER_ALLOW_SOFTWARE=1
export XKB_DEFAULT_LAYOUT=us

command="/usr/bin/cage"
command_args="-- /usr/bin/foot"
command_background=true
pidfile="/run/cage-kiosk.pid"
output_log="/var/log/cage.log"
error_log="/var/log/cage.log"

depend() {
    need seatd
    after udev devfs
}

start_pre() {
    if [ ! -e /dev/dri/card0 ]; then
        ewarn "no /dev/dri/card0 (booted without --gui); not starting cage"
        return 1
    fi
    mkdir -p "$XDG_RUNTIME_DIR"
    chmod 0700 "$XDG_RUNTIME_DIR"
}
CAGEEOF
  chmod +x /etc/init.d/cage-kiosk
  rc-update add cage-kiosk default
'

# Export the container filesystem to a tarball (host-user writable path).
docker export fcroot_gui_build -o "$TAR"
docker rm fcroot_gui_build >/dev/null

# 2. Pack the tree into a 768 MiB ext4 (GUI tree is far larger than the base 96M).
docker run --rm -v "$STAGE:/work" ubuntu:22.04 bash -euxc '
  export DEBIAN_FRONTEND=noninteractive
  apt-get update -qq
  apt-get install -y -qq --no-install-recommends e2fsprogs >/dev/null

  rm -rf /tmp/rootfs && mkdir -p /tmp/rootfs
  tar xf /work/rootfs-gui.tar -C /tmp/rootfs
  rm -f /tmp/rootfs/.dockerenv
  for d in dev proc run sys tmp mnt; do mkdir -p /tmp/rootfs/$d; done

  rm -f /work/out/rootfs-gui.ext4
  mke2fs -q -t ext4 -d /tmp/rootfs -L rootfs-gui /work/out/rootfs-gui.ext4 768M
  ls -la /work/out/rootfs-gui.ext4
'

rm -f "$TAR"
```

- [ ] **Step 2: Make it executable + sanity-check the shell syntax locally** (no Docker run, just parse):

Run: `chmod +x kimage/build/build-rootfs-gui.sh && bash -n kimage/build/build-rootfs-gui.sh && echo "syntax ok"`
Expected: `syntax ok` (the `bash -n` parse must pass — the nested single-quote/heredoc escaping is the fragile part).

- [ ] **Step 3: Commit.**

```bash
git add kimage/build/build-rootfs-gui.sh
git commit -m "kimage: build-rootfs-gui.sh — separate GUI rootfs (cage + foot + seatd)"
```

---

## Task 2: Build the GUI rootfs on artemis2 and pull it back

**Files:** none (remote build + artifact pull to `kimage/out/rootfs-gui.ext4`).

- [ ] **Step 1: Copy the script + devmem.c to the build host.**

Run: `scp kimage/build/build-rootfs-gui.sh kimage/build/devmem.c artemis2:~/kbuild/`
Expected: scp completes.

- [ ] **Step 2: Run the build (foreground or backgrounded waiter).** The GUI rootfs pulls a large package set; allow several minutes.

Run: `ssh artemis2 'cd ~/kbuild && chmod +x build-rootfs-gui.sh && ./build-rootfs-gui.sh' 2>&1 | tail -30`
Expected: ends with `... /work/out/rootfs-gui.ext4` listed (the `ls -la` line). If `apk add cage` fails with "unable to select package", the community repo line didn't take — verify the `echo ... community >> /etc/apk/repositories` + `apk update` ran (re-read the log).

- [ ] **Step 3: Pull the artifact + verify the ext4 magic.**

Run:
```bash
scp artemis2:'~/kbuild/out/rootfs-gui.ext4' kimage/out/rootfs-gui.ext4
ls -la kimage/out/rootfs-gui.ext4
xxd -s $((0x438)) -l 2 kimage/out/rootfs-gui.ext4
```
Expected: `rootfs-gui.ext4` present (~hundreds of MB used within the 768 MiB image); the 2 bytes at 0x438 are `53ef` (ext4 magic).

- [ ] **Step 4: No commit** (the artifact is gitignored under `kimage/out/`).

---

## Task 3: Live boot + iterate until foot renders

**Files:** `kimage/build/build-rootfs-gui.sh` (iterate if needed); `crates/devices/src/virtio/gpu.rs` (only if a device gap appears).

This task is a live loop. The VMM `boot` binary should already be built + signed from M3; if not: `PATH="$HOME/.cargo/bin:$PATH" cargo build -p ignition-spike --bin boot && ./scripts/sign.sh target/debug/boot`.

- [ ] **Step 1: Boot the GUI rootfs with extra RAM.**

Run (backgrounded, serial → log):
```bash
rm -f /tmp/ign-gui.log
target/debug/boot --gui --mem 512 kimage/out/Image kimage/out/rootfs-gui.ext4 > /tmp/ign-gui.log 2>&1 &
```
Then watch the serial log for boot + the cage-kiosk service + any cage error:
```bash
grep -iE 'cage|seatd|/dev/dri|wlr|login:|panic|soft lockup' /tmp/ign-gui.log | tail -40
cat /var/log/... # (inside guest) — or, since serial has a getty, log in on serial and `cat /var/log/cage.log`
```
Expected: cage starts; foot renders fullscreen in the macOS window. The user eyeballs the window.

- [ ] **Step 2: Verify interactivity (manual, in the window).** Type in foot → shell echoes; run `ls`, `uname -a`. Move the pointer → a software cursor tracks. Confirm with the user.

- [ ] **Step 3: Iterate on failure using the watch-list.** Apply these concrete tweaks (edit `build-rootfs-gui.sh`, rebuild via Task 2, re-boot) as the symptom dictates:
  - **cage can't get a seat / "failed to open seat":** change the service to use the builtin libseat backend — add `export LIBSEAT_BACKEND=builtin` to the top of `/etc/init.d/cage-kiosk` (cage runs as root, so builtin can open devices without seatd). Keep seatd enabled too; builtin just bypasses it.
  - **cage exits "could not create renderer" / GL error:** confirm `WLR_RENDERER=pixman` + `WLR_RENDERER_ALLOW_SOFTWARE=1` are exported in the service (they are); if wlroots still wants GL, also set `WLR_BACKENDS=drm,libinput`.
  - **"no DRM master" / cage can't take the display:** stop the tty1 getty competing for the VT — remove the `agetty.tty1` lines from this GUI script (serial getty stays for debug), rebuild. Optionally add `export WLR_DRM_NO_ATOMIC=1`.
  - **GET_EDID / probe failure in the guest virtio_gpu logs:** the host returns ERR_UNSPEC for unimplemented controlq commands; if wlroots needs a specific command (check the dmesg/cage.log), add it to `crates/devices/src/virtio/gpu.rs` dispatch with a unit test, rebuild + re-sign the VMM.
  - **OOM / cage killed:** raise `--mem` (e.g. `--mem 768`).
  - **No font / foot fails "can't load font":** ensure `font-terminus` installed; if foot wants a different family, add `ttf-dejavu` and set `foot -f "monospace:size=12"` in `command_args` (`-- /usr/bin/foot -f monospace:size=12`).

- [ ] **Step 4: When foot renders + is interactive, confirm the regression paths.** Boot the **base** rootfs still works:
```bash
target/debug/boot kimage/out/Image kimage/out/rootfs.ext4   # no --gui, base rootfs: serial shell as before
```
and the GUI rootfs without `--gui` falls back to a console (cage-kiosk no-ops on missing card0):
```bash
target/debug/boot kimage/out/Image kimage/out/rootfs-gui.ext4   # serial/tty1 console, cage skipped
```
Expected: both boot to a working shell; no cage crash-loop.

- [ ] **Step 5: Commit any iteration changes** to `build-rootfs-gui.sh` (and any `gpu.rs` device fix as its own commit with its test):

```bash
git add kimage/build/build-rootfs-gui.sh
git commit -m "kimage: cage-kiosk tweaks for live cage+foot bring-up"
```

---

## Task 4: Documentation

**Files:** `docs/src/getting-started/guest-assets.md`, `docs/src/features/devices.md`, `ROADMAP.md`.

- [ ] **Step 1: Add a "Rebuild the GUI rootfs" section to `docs/src/getting-started/guest-assets.md`** (after the base "Rebuild the rootfs" section):

```markdown
## Rebuild the GUI rootfs

A separate, larger rootfs (`rootfs-gui.ext4`) adds a cage (wlroots, pixman software
renderer) Wayland kiosk running foot, for the `--gui` window. Built by its own script
so the minimal base rootfs stays untouched.

```bash
cd kimage
scp build/build-rootfs-gui.sh build/devmem.c artemis2:~/kbuild/
ssh artemis2 'cd ~/kbuild && chmod +x build-rootfs-gui.sh && ./build-rootfs-gui.sh'
scp artemis2:'~/kbuild/out/rootfs-gui.ext4' out/rootfs-gui.ext4
# verify ext4 magic 53ef at 0x438:
dd if=out/rootfs-gui.ext4 bs=1 skip=$((0x438)) count=2 2>/dev/null | xxd
```

Run it: `boot --gui --mem 512 out/Image out/rootfs-gui.ext4`. Without `--gui` (no
`/dev/dri/card0`) the cage service no-ops and the guest falls back to the serial/tty1
console.
```

- [ ] **Step 2: Note M4 in `docs/src/features/devices.md`** (append to the GUI display section):

```markdown
With the GUI rootfs (`rootfs-gui.ext4`, built by `kimage/build/build-rootfs-gui.sh`),
`--gui` runs a **cage** Wayland kiosk (wlroots **pixman** software renderer — no GL,
matching the 2D-only virtio-gpu) hosting a **foot** terminal, driven by the virtio-input
keyboard + pointer. The minimal base rootfs has no compositor and uses the framebuffer
console directly.
```

- [ ] **Step 3: Update `ROADMAP.md`** — mark M4 shipped:

Replace the `- [ ] **M4 compositor/app**, **M5 ...**` line with:

```markdown
- [x] **M4 compositor/app** — cage (wlroots, pixman software renderer) + foot terminal in the `--gui` window, on a separate `rootfs-gui.ext4`. `docs/superpowers/specs/2026-06-15-gui-compositor-m4-design.md`
- [ ] **M5 snapshot/clone with the GUI live** — final GUI milestone.
```

- [ ] **Step 4: Verify the book builds.**

Run: `PATH="$HOME/.cargo/bin:$PATH" mdbook build docs 2>&1 | tail -3`
Expected: success, no broken links.

- [ ] **Step 5: Commit.**

```bash
git add docs/src/getting-started/guest-assets.md docs/src/features/devices.md ROADMAP.md
git commit -m "docs: GUI compositor (M4) — cage+foot rootfs + roadmap"
```

---

## Self-Review Notes

- **Spec coverage:** separate `build-rootfs-gui.sh` → `rootfs-gui.ext4`, base untouched (Task 1) ✓; cage+foot+seatd+font + pixman + cage-kiosk service guarded on card0, serial fallback (Task 1) ✓; remote build + pull (Task 2) ✓; live boot + iterate with the spec's watch-list as concrete tweaks (Task 3) ✓; regression of base + non-`--gui` (Task 3 Step 4) ✓; docs + roadmap (Task 4) ✓. Host device fix only if a gap appears (Task 3 Step 3, with a unit test) ✓.
- **No placeholders:** the full script content is inline; iteration tweaks are concrete env/flag/package changes, not "handle errors". The one inherently open part (which watch-list tweak is needed) is the nature of live bring-up, enumerated with exact fixes.
- **Execution model:** inline (executing-plans), not subagent-TDD — there is no host code to test-drive; live acceptance is the gate. Flagged in the header.
