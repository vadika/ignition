#!/usr/bin/env bash
# Build minimal aarch64 rootfs (busybox shell + openrc init) and pack to ext4.
# Output: ~/kbuild/out/rootfs.ext4
set -euo pipefail

OUT="$HOME/kbuild/out"
STAGE="$HOME/kbuild"          # user-owned; out/ may be root-owned from kernel build
mkdir -p "$OUT"
TAR="$STAGE/rootfs.tar"

# 1. Provision rootfs inside an arm64 alpine container (init, console, dirs).
docker rm -f fcroot_build >/dev/null 2>&1 || true
docker run --platform linux/arm64 --name fcroot_build \
  -v "$(cd "$(dirname "$0")" && pwd)/devmem.c:/devmem.c:ro" \
  alpine:3.19 sh -euxc '
  # socat provides a userspace AF_VSOCK client (VSOCK-CONNECT) for testing the
  # virtio-vsock device end to end (alpine 3.19 ships socat 1.8 with VSOCK support).
  apk add --no-cache openrc util-linux ifupdown-ng socat

  # devmem: alpine busybox has no devmem applet, so compile a tiny static one
  # (musl) for the boot-timer MMIO poke. Toolchain is removed afterwards.
  apk add --no-cache --virtual .build gcc musl-dev
  gcc -O2 -static /devmem.c -o /usr/bin/devmem
  apk del .build

  # serial console on ttyS0 (Firecracker default)
  ln -sf agetty /etc/init.d/agetty.ttyS0
  echo ttyS0 > /etc/securetty
  rc-update add agetty.ttyS0 default
  rc-update add devfs boot
  rc-update add procfs boot
  rc-update add sysfs boot

  # root has no password for console login
  passwd -d root || true

  # /dev/tty -> /dev/ttyS0 so programs that open the controlling terminal work
  # on the serial console. /dev is a fresh devtmpfs each boot, so create the
  # link at early init via a busybox sysinit action (runs before getty/login).
  grep -q "ln -sf /dev/ttyS0 /dev/tty" /etc/inittab ||
    printf "::sysinit:/bin/ln -sf /dev/ttyS0 /dev/tty\n" >> /etc/inittab

  # Automatic networking: bring eth0 up via DHCP at boot. ifupdown-ng runs
  # busybox udhcpc (with its default.script) to apply address/route/DNS.
  # vmnet shared mode (and most Firecracker TAP setups) serve DHCP.
  mkdir -p /etc/network /etc/local.d
  printf "auto lo\niface lo inet loopback\n\nauto eth0\niface eth0 inet dhcp\n" > /etc/network/interfaces
  # No /etc/init.d/networking ships on alpine, so drive ifup from the openrc
  # local service (runs at boot, after device nodes exist).
  printf "#!/bin/sh\nifup -a\n" > /etc/local.d/network.start
  chmod +x /etc/local.d/network.start
  # boot-timer: signal boot-complete to the VMM by writing the magic byte 123 to
  # the boot-timer MMIO address (out-of-band fixed address; see layout::BOOT_TIMER_ADDR).
  # Uses /usr/bin/devmem (compiled above) + kernel CONFIG_DEVMEM=y, STRICT_DEVMEM=n.
  printf "#!/bin/sh\ndevmem 0x091FF000 8 123\n" > /etc/local.d/boottime.start
  chmod +x /etc/local.d/boottime.start
  rc-update add local boot
'

# Export the built container filesystem to a tarball (host-user writable path).
docker export fcroot_build -o "$TAR"
docker rm fcroot_build >/dev/null

# 2. Unpack tar into a dir and create runtime mountpoints, then build ext4
#    image with mke2fs -d (no privileged mount needed). Container runs as root,
#    so it can write rootfs.ext4 into a root-owned out/.
docker run --rm -v "$STAGE:/work" ubuntu:22.04 bash -euxc '
  export DEBIAN_FRONTEND=noninteractive
  apt-get update -qq
  apt-get install -y -qq --no-install-recommends e2fsprogs >/dev/null

  rm -rf /tmp/rootfs && mkdir -p /tmp/rootfs
  tar xf /work/rootfs.tar -C /tmp/rootfs
  # Docker export leaves these as files; ensure they are dirs.
  rm -f /tmp/rootfs/.dockerenv
  for d in dev proc run sys tmp mnt; do mkdir -p /tmp/rootfs/$d; done

  # 96 MiB image, sized for the staged tree.
  rm -f /work/out/rootfs.ext4
  mke2fs -q -t ext4 -d /tmp/rootfs -L rootfs /work/out/rootfs.ext4 96M
  ls -la /work/out/rootfs.ext4
'

rm -f "$TAR"
