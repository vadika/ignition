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
  # NOTE: no agetty on tty1 in the GUI rootfs — cage owns the framebuffer VT, and a
  # getty there competes with cage for the keyboard (events go to the VT, not
  # libinput). Serial (ttyS0) stays as the debug login. Alpine ALSO spawns gettys on
  # tty1..tty6 from busybox /etc/inittab; strip those VT gettys so none grabs the
  # keyboard from cage (ttyS0 line, if present, is kept — [0-9] does not match "S").
  sed -i "/^tty[0-9].*getty/d" /etc/inittab
  rc-update add devfs boot
  rc-update add procfs boot
  rc-update add sysfs boot

  passwd -d root || true

  # NOTE: the base rootfs symlinks /dev/tty -> /dev/ttyS0 for serial-console programs.
  # The GUI rootfs MUST NOT: foot runs its app on a pty and the app opens /dev/tty as
  # its controlling terminal; if /dev/tty is a symlink to the serial port the app gets
  # the wrong device (the cannot-access-tty error, no echo). Leave /dev/tty as the real
  # kernel ctty node (c 5 0) from devtmpfs so it resolves to foots pty.

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
  # libinput-tools: `libinput list-devices` / `debug-events` for bring-up diagnosis.
  # xkeyboard-config: XKB layout data — without it libxkbcommon compiles an empty
  # keymap, so cage focuses the window but keystrokes map to nothing (verified: key
  # events reach the guest, but no characters appear until this is installed).
  apk add --no-cache cage foot seatd font-terminus libinput-tools xkeyboard-config wev

  # udev (eudev): wlroots libinput discovers /dev/input/event* via udev. Without it
  # cage aborts ("libinput initialization failed, no input devices"). Run at sysinit
  # so input + DRM nodes are enumerated before cage starts.
  apk add --no-cache eudev
  rc-update add udev sysinit
  rc-update add udev-trigger sysinit
  rc-update add udev-settle sysinit

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
# NOTE: deliberately NOT setting WLR_LIBINPUT_NO_DEVICES — that flag makes wlroots
# skip enumerating the already-present (cold-boot) input devices and only listen for
# new hotplug uevents, which never fire for devices present before cage starts, so
# cage ends up with no keyboard (no wl_keyboard -> the app never gets focus). Instead
# start_pre waits until libinput can actually see the keyboard, then cage enumerates
# it normally at startup.

command="/usr/bin/cage"
command_args="-- /usr/bin/foot"
command_background=true
pidfile="/run/cage-kiosk.pid"
output_log="/var/log/cage.log"
error_log="/var/log/cage.log"

depend() {
    need seatd
    after udev-settle udev-trigger udev devfs
}

start_pre() {
    if [ ! -e /dev/dri/card0 ]; then
        ewarn "no /dev/dri/card0 (booted without --gui); not starting cage"
        return 1
    fi
    mkdir -p "$XDG_RUNTIME_DIR"
    chmod 0700 "$XDG_RUNTIME_DIR"
    # Wait until libinput can enumerate the keyboard (udev has finished tagging
    # /dev/input/event*). cage enumerates input once at startup; if it starts before
    # tagging, it gets no keyboard and the app never receives focus.
    i=0
    while [ "$i" -lt 50 ]; do
        if libinput list-devices 2>/dev/null | grep -qi keyboard; then
            break
        fi
        sleep 0.2
        i=$((i + 1))
    done
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
