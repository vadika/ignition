#!/usr/bin/env bash
# Build the MCP "tools base" rootfs: alpine + python3 + git + a C toolchain, with
# the ign-exec agent behind a socat vsock listener, on an overlay root (immutable
# ext4 lower + tmpfs upper via /sbin/overlay-init). Output: ~/kbuild/out/rootfs-tools.ext4
# Copy the result to kimage/out/rootfs-tools.ext4 before running scripts/make-tools-base.sh.
set -euo pipefail

OUT="$HOME/kbuild/out"
STAGE="$HOME/kbuild"
mkdir -p "$OUT"
TAR="$STAGE/rootfs-tools.tar"

docker rm -f fcroot_tools_build >/dev/null 2>&1 || true
docker run --platform linux/arm64 --name fcroot_tools_build \
  -v "$(cd "$(dirname "$0")" && pwd)/devmem.c:/devmem.c:ro" \
  -v "$(cd "$(dirname "$0")" && pwd)/vmid-reseed.c:/vmid-reseed.c:ro" \
  -v "$(cd "$(dirname "$0")" && pwd)/ign-exec.py:/ign-exec.py:ro" \
  alpine:3.19 sh -euxc '
  apk add --no-cache openrc util-linux ifupdown-ng socat python3 py3-pip py3-numpy git gcc musl-dev linux-headers coreutils

  # The tools base intentionally keeps the C toolchain and kernel headers so
  # sandboxed agents can compile (including against linux headers) with no
  # network. Build the static host helpers with it; it stays in the image.
  gcc -O2 -static /devmem.c -o /usr/bin/devmem
  gcc -O2 -static /vmid-reseed.c -o /usr/bin/vmid-reseed

  install -m 0755 /ign-exec.py /usr/bin/ign-exec

  ln -sf agetty /etc/init.d/agetty.ttyS0
  echo ttyS0 > /etc/securetty
  rc-update add agetty.ttyS0 default
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

  printf "#!/bin/sh\nsocat VSOCK-LISTEN:9000,fork EXEC:/usr/bin/vmid-reseed &\n" > /etc/local.d/vmid.start
  chmod +x /etc/local.d/vmid.start

  printf "#!/bin/sh\nsocat VSOCK-LISTEN:7000,fork EXEC:/usr/bin/ign-exec &\n" > /etc/local.d/ign-exec.start
  chmod +x /etc/local.d/ign-exec.start

  cat > /etc/local.d/tools-ready.start <<'"'"'RDYEOF'"'"'
#!/bin/sh
( i=0
  while [ "$i" -lt 60 ]; do
    if pgrep -f "VSOCK-LISTEN:7000" >/dev/null 2>&1; then
      sleep 1
      echo TOOLS_READY > /dev/ttyS0
      exit 0
    fi
    sleep 1
    i=$((i + 1))
  done
  echo TOOLS_TIMEOUT > /dev/ttyS0 ) &
RDYEOF
  chmod +x /etc/local.d/tools-ready.start

  cat > /sbin/overlay-init <<'"'"'OVLEOF'"'"'
#!/bin/sh
mount -t proc proc /proc 2>/dev/null
mount -t sysfs sys /sys 2>/dev/null
mount -t tmpfs tmpfs /mnt
mkdir -p /mnt/up /mnt/work /mnt/root /mnt/lower
mount --bind / /mnt/lower
mount -o remount,ro /mnt/lower 2>/dev/null || true
mount -t overlay overlay -o lowerdir=/mnt/lower,upperdir=/mnt/up,workdir=/mnt/work /mnt/root
exec switch_root /mnt/root /sbin/init
OVLEOF
  chmod +x /sbin/overlay-init

  rc-update add local boot
'

docker export fcroot_tools_build -o "$TAR"
docker rm fcroot_tools_build >/dev/null

docker run --rm -v "$STAGE:/work" ubuntu:22.04 bash -euxc '
  export DEBIAN_FRONTEND=noninteractive
  apt-get update -qq
  apt-get install -y -qq --no-install-recommends e2fsprogs >/dev/null
  rm -rf /tmp/rootfs && mkdir -p /tmp/rootfs
  tar xf /work/rootfs-tools.tar -C /tmp/rootfs
  rm -f /tmp/rootfs/.dockerenv
  for d in dev proc run sys tmp mnt; do mkdir -p /tmp/rootfs/$d; done
  rm -f /work/out/rootfs-tools.ext4
  mke2fs -q -t ext4 -d /tmp/rootfs -L rootfs-tools /work/out/rootfs-tools.ext4 768M
  ls -la /work/out/rootfs-tools.ext4
'

rm -f "$TAR"
