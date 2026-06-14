#!/usr/bin/env bash
# Build the M0 fuzz initramfs: static-musl harness as /init in a minimal cpio.
# Mirrors build-rootfs.sh (arm64 alpine container, no host toolchain).
# Output: ~/kbuild/out/fuzz-initramfs.cpio (or ~/kbuild/fuzz-initramfs.cpio if
# out/ is root-owned from a prior kernel build and we cannot write into it).
set -euo pipefail
STAGE="$HOME/kbuild"          # user-owned; out/ may be root-owned from kernel build
OUT="$STAGE/out"
mkdir -p "$OUT" 2>/dev/null || true
HERE="$(cd "$(dirname "$0")" && pwd)"

docker rm -f fuzzinit_build >/dev/null 2>&1 || true
docker run --platform linux/arm64 --name fuzzinit_build \
  -v "$HERE/fuzz-harness:/src:ro" \
  alpine:3.19 sh -euxc '
  apk add --no-cache --virtual .build gcc musl-dev
  mkdir -p /out/root
  # -static: no dynamic loader; the harness is PID 1 (/init).
  gcc -O2 -static -I/src /src/harness.c -o /out/root/init
  apk del .build
  cd /out/root
  # initramfs has no devtmpfs auto-mount; pre-create the nodes the harness needs.
  mkdir -p dev proc sys
  mknod -m 600 dev/mem c 1 1
  mknod -m 622 dev/console c 5 1
  mknod -m 666 dev/null c 1 3
  find . | cpio -o -H newc > /out/fuzz-initramfs.cpio
'
# out/ may be root-owned (left by the kernel build); fall back to the user-owned
# stage dir so `docker cp` (runs as the host user) can always write the artifact.
DEST="$OUT/fuzz-initramfs.cpio"
if ! ( : >"$DEST" ) 2>/dev/null; then
  DEST="$STAGE/fuzz-initramfs.cpio"
fi
rm -f "$DEST"
docker cp fuzzinit_build:/out/fuzz-initramfs.cpio "$DEST"
docker rm fuzzinit_build >/dev/null
echo "wrote $DEST"
