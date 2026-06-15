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
