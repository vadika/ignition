#!/usr/bin/env bash
# Cross-compile aarch64 Firecracker guest kernel (6.1) inside an x86_64 container.
# Output: ~/kbuild/out/Image
set -euo pipefail

KVER=6.1
LINUX_TARBALL="linux-${KVER}.tar.xz"
CFG_URL="https://raw.githubusercontent.com/firecracker-microvm/firecracker/main/resources/guest_configs/microvm-kernel-ci-aarch64-6.1.config"

docker run --rm \
  -v "$HOME/kbuild:/work" \
  -w /work \
  -e KVER="$KVER" -e LINUX_TARBALL="$LINUX_TARBALL" -e CFG_URL="$CFG_URL" \
  ubuntu:22.04 bash -euxc '
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq
    apt-get install -y -qq --no-install-recommends \
      gcc-aarch64-linux-gnu build-essential flex bison bc \
      libssl-dev libelf-dev cpio kmod wget xz-utils ca-certificates >/dev/null

    if [ ! -d "linux-${KVER}" ]; then
      wget -q "https://cdn.kernel.org/pub/linux/kernel/v6.x/${LINUX_TARBALL}"
      tar xf "${LINUX_TARBALL}"
    fi
    cd "linux-${KVER}"
    wget -qO .config "${CFG_URL}"

    # Extra guest features on top of the Firecracker CI config. Built-in (=y)
    # so they need no module loading. VSOCKETS/VHOST are deps required for the
    # *_VSOCK symbols to survive olddefconfig.
    #   VIRTIO_BALLOON  - guest memory balloon
    #   VIRTIO_VSOCKETS - guest vsock transport (what Firecracker needs)
    #   VHOST_VSOCK     - host-side vsock; inert in a guest image, added on request
    ./scripts/config \
      --enable VIRTIO_BALLOON \
      --enable VSOCKETS \
      --enable VIRTIO_VSOCKETS \
      --enable VHOST \
      --enable VHOST_VSOCK \
      --enable DEVMEM \
      --disable STRICT_DEVMEM \
      --disable IO_STRICT_DEVMEM

    export ARCH=arm64 CROSS_COMPILE=aarch64-linux-gnu-
    make olddefconfig
    echo "=== requested configs after olddefconfig ==="
    grep -E "CONFIG_(VIRTIO_BALLOON|VSOCKETS|VIRTIO_VSOCKETS|VHOST|VHOST_VSOCK|DEVMEM|STRICT_DEVMEM)=" .config || true
    grep -E "CONFIG_STRICT_DEVMEM" .config || echo "CONFIG_STRICT_DEVMEM not set (good)"
    make -j"$(nproc)" Image

    # NOTE: never strip the arm64 Image. It is a valid PE/COFF binary, so
    # binutils strip will silently rewrite it and destroy the arm64 boot
    # header (magic 0x644d5241 "ARMd" at offset 0x38). Copy it verbatim.
    mkdir -p /work/out
    cp arch/arm64/boot/Image /work/out/Image
    ls -la /work/out/Image
  '
