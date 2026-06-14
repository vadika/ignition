#!/usr/bin/env bash
# Build the M1 fuzz initramfs: clang-ASan (dynamic musl) target+harness as /init
# in a minimal cpio, with the ASan runtime baked in and musl loader + libgcc_s
# bundled (see the M1 recipe block below).
# Mirrors build-rootfs.sh (arm64 alpine container, no host toolchain).
# Output: ~/kbuild/out/fuzz-initramfs.cpio (or ~/kbuild/fuzz-initramfs.cpio if
# out/ is root-owned from a prior kernel build and we cannot write into it).
#
# ============================================================================
# M1 SANITIZER-IN-GUEST RECIPE (Task 1 spike result — CONFIRMED 2026-06-14)
# ============================================================================
# Chosen linkage: #1 (alpine/musl + ASan), built DYNAMIC with clang.
# Confirmed in the actual microVM (aarch64 via Hypervisor.framework, not just
# qemu-user): guest console showed "ASAN_SPIKE: start" then
#   "==1==ERROR: AddressSanitizer: heap-buffer-overflow ... WRITE of size 17"
#   "==1==ABORTING"  (then kernel panic: init died) — the "no-detection (BAD)"
# line did NOT appear. Tasks 2/3 use the ASan API path (NOT the guard-page
# fallback).
#
# WHY clang dynamic (not gcc, not -static / -static-pie):
#   - gcc -static-libasan FAILS in alpine: "cannot read spec file
#     'libsanitizer.spec'" (alpine's compiler-rt has no gcc sanitizer specs).
#   - clang -fsanitize=address -static-pie LINKS but SEGV's at ASan shadow init
#     under musl (no "start" printed) — do NOT use -static / -static-pie.
#   - clang -fsanitize=address (dynamic) WORKS: clang links the ASan runtime
#     statically INTO the binary; only the musl loader + libgcc_s are external.
#
# Container:  docker run --platform linux/arm64 alpine:3.19
# Packages:   apk add --no-cache clang compiler-rt        (+ musl-dev for headers)
# Compile (spike, single TU):
#   clang -fsanitize=address -O1 -g asan_spike.c -o /out/root/init
# Compile (M1 target+harness split — see Task 3): instrument target.c with
#   -fsanitize=address; harness.c may be compiled without it but MUST be linked
#   with clang -fsanitize=address so the ASan runtime + death-callback symbols
#   resolve. Use -O1 (not -O2) + a volatile g_sink so the overflow isn't elided.
# M2: target.c is additionally built with -fsanitize-coverage=trace-pc so the
#   harness's __sanitizer_cov_trace_pc callback records edges into the shared
#   coverage region (FUZZ_COV_GPA). Compile target.c and harness.c as separate
#   objects (above) so the callback's TU is NOT itself instrumented.
#
# Bundled runtime libs (from `ldd init`) — copy into the cpio at THESE paths:
#   /lib/ld-musl-aarch64.so.1     (the musl loader; libc.musl-aarch64.so.1 is the
#                                  SAME file — musl is one .so, no separate libc)
#   /usr/lib/libgcc_s.so.1        (clang's unwinder for ASan stack traces)
#   (The ASan runtime itself is NOT a separate lib — it's linked into the binary.)
#   Use `cp -L` to deref symlinks. Resulting cpio ~3.5 MB (vs ~123 KB static M0).
#
# ASAN_OPTIONS (Task 3 sets these via __asan_default_options in harness.c; the
#   env var also works when launching `boot`):
#     abort_on_error=1:halt_on_error=1:detect_leaks=0
#   abort_on_error=1 makes ASan abort() (so __asan_set_death_callback fires);
#   detect_leaks=0 avoids LSan at exit (no atexit in a one-shot harness).
#
# OPERATIONAL NOTE: ASan printed "WARNING: reading executable name failed with
#   errno 2" because the spike rootfs had no /proc mounted. Detection still
#   worked, but the M1 harness should mount /proc (mkdir /proc; the harness can
#   mount("proc","/proc","proc",...) ) for clean symbolization. Not gate-blocking.
#
# Memory: --mem 96 boots fine with the 3.5 MB initramfs (initrd @ 0x44000000,
#   well below the FDT). If the cpio grows, bump --mem.
# ============================================================================
set -euo pipefail
STAGE="$HOME/kbuild"          # user-owned; out/ may be root-owned from kernel build
OUT="$STAGE/out"
mkdir -p "$OUT" 2>/dev/null || true
HERE="$(cd "$(dirname "$0")" && pwd)"

docker rm -f fuzzinit_build >/dev/null 2>&1 || true
docker run --platform linux/arm64 --name fuzzinit_build \
  -v "$HERE/fuzz-harness:/src:ro" \
  alpine:3.19 sh -euxc '
  apk add --no-cache clang compiler-rt musl-dev
  mkdir -p /out/root/lib /out/root/usr/lib /out/root/dev /out/root/proc /out/root/sys
  # Instrument ONLY target.c with trace-pc coverage; harness.c (which defines the
  # __sanitizer_cov_trace_pc callback) must stay uninstrumented or the callback
  # recurses. ASan is applied to both; -O1 + the volatile g_sink keep the planted
  # overflow alive.
  clang -fsanitize=address -fsanitize-coverage=trace-pc -O1 -g -I/src -c /src/target.c -o /tmp/target.o
  clang -fsanitize=address -O1 -g -I/src -c /src/harness.c -o /tmp/harness.o
  clang -fsanitize=address -O1 -g /tmp/target.o /tmp/harness.o -o /out/root/init
  # bundle the dynamic loader + libgcc_s at their ldd paths (Task 1 recipe);
  # re-verify and copy any additional non-virtual deps.
  echo "=== ldd /out/root/init ==="
  ldd /out/root/init || true
  cp -L /lib/ld-musl-aarch64.so.1 /out/root/lib/
  cp -L /usr/lib/libgcc_s.so.1 /out/root/usr/lib/
  cd /out/root
  # initramfs has no devtmpfs auto-mount; pre-create the nodes the harness needs.
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
