# Building guest assets

Agent playbook for rebuilding the aarch64 Firecracker guest kernel and rootfs in
`kimage/`. Both artifacts are built **on the remote Linux host `artemis2`** (it
has Docker but no host toolchain — everything runs in containers) and pulled
back to `kimage/out/`. Full background lives in `kimage/README.md`; this file is
the operational checklist.

## Mental model

- **Sources** you edit live locally in `kimage/build/`:
  - `build-kernel.sh` — cross-compiles Linux 6.1 aarch64 (`ubuntu:22.04` +
    `gcc-aarch64-linux-gnu`). Config = Firecracker CI config, fetched at build
    time, plus `scripts/config` tweaks, then `make olddefconfig && make Image`.
  - `build-rootfs.sh` — provisions `alpine:3.19` arm64, exports the fs, packs
    ext4 with `mke2fs -d` (no mount/sudo).
  - `devmem.c` — static `/dev/mem` poke tool compiled into the rootfs.
- **Artifacts** land in `kimage/out/` (gitignored): `Image`, `rootfs.ext4`.
- The build runs in `~/kbuild/` on artemis2. Kernel source/object tree is cached
  under `~/kbuild/linux-6.1`, so kernel rebuilds are incremental.

## Workflow (every rebuild)

1. Edit the script(s) locally under `kimage/build/`.
2. `scp` the changed scripts to `artemis2:~/kbuild/`.
3. Run the build over `ssh` on artemis2.
4. `scp` the artifact(s) back to `kimage/out/`.
5. Verify magic bytes (below).
6. Commit per the repo convention (plain message, no co-author trailer).

## Rebuild the rootfs

```bash
cd kimage
scp build/build-rootfs.sh build/devmem.c artemis2:~/kbuild/
ssh artemis2 'cd ~/kbuild && chmod +x build-rootfs.sh && ./build-rootfs.sh'
scp artemis2:'~/kbuild/out/rootfs.ext4' out/rootfs.ext4
# verify ext4 magic 53ef at 0x438:
dd if=out/rootfs.ext4 bs=1 skip=$((0x438)) count=2 2>/dev/null | xxd
```

## Rebuild the kernel

```bash
cd kimage
scp build/build-kernel.sh artemis2:~/kbuild/
ssh artemis2 'cd ~/kbuild && chmod +x build-kernel.sh && ./build-kernel.sh'
scp artemis2:'~/kbuild/out/Image' out/Image
# verify arm64 boot magic "ARMd" (4152 4d64) at 0x38:
xxd -s 56 -l 4 out/Image
```

For the GUI (virtio-gpu) milestone, the kernel config also needs `CONFIG_DRM=y`,
`CONFIG_DRM_VIRTIO_GPU=y`, `CONFIG_DRM_FBDEV_EMULATION=y`, `CONFIG_FB=y`, and
`CONFIG_FRAMEBUFFER_CONSOLE=y` so `/dev/dri/card0` + `/dev/fb0` appear and fbcon binds.
The GUI compositor (M4) also needs `CONFIG_VIRTIO_INPUT=y` and `CONFIG_INPUT_EVDEV=y`.

The browser rootfs additionally requires `CONFIG_OVERLAY_FS=y` and `CONFIG_TMPFS=y`.
These are needed only for the one-time warm-base cold boot (which passes
`--append "ro init=/sbin/overlay-init"` to set up the overlay root over a
read-only lower); restoring a
browser-base snapshot does not reload the kernel or re-run the overlay pivot.

## Rebuild the GUI rootfs

A separate, larger rootfs (`rootfs-gui.ext4`) adds a cage (wlroots, pixman software
renderer) Wayland kiosk running foot, plus eudev/seatd/xkeyboard-config, for the `--gui`
window. It also carries the same `netwatch` carrier-poller as the base rootfs, so a
restored or cloned GUI guest rebinds virtio-net on restore and re-DHCPs with its fresh
MAC. Built by its own script so the minimal base rootfs stays untouched.

```bash
cd kimage
scp build/build-rootfs-gui.sh build/devmem.c artemis2:~/kbuild/
ssh artemis2 'cd ~/kbuild && chmod +x build-rootfs-gui.sh && ./build-rootfs-gui.sh'
scp artemis2:'~/kbuild/out/rootfs-gui.ext4' out/rootfs-gui.ext4
```

Run it: `boot --gui --mem 512 out/Image out/rootfs-gui.ext4`. The compositor takes the
framebuffer VT and foot renders fullscreen; type to drive the shell, move the pointer for
a software cursor. Without `--gui` (no `/dev/dri/card0`) the cage service no-ops and the
guest falls back to the serial console.

To snapshot and restore the live desktop, add `--track-dirty`, press `Ctrl-A s` to write
a snapshot, then `boot --gui --restore <name>` to reopen it. Fan out N clones from one
base with `scripts/fanout-gui.sh N <name>`. Add `--net` (under `sudo`) on both the
snapshot and the fan-out for networked clones — the GUI rootfs carries the `netwatch`
carrier-poller, so each clone rebinds virtio-net on restore and gets its own MAC + DHCP
lease. For in-memory reset without writing a new snapshot, `Ctrl-A c` marks the current
running desktop as a reset point and `Ctrl-A r` rolls it back in place (distinct from the
`Ctrl-A s` disk snapshot); this requires that the rootfs not diverge between checkpoint
and reset — mount it read-only with tmpfs for all writable state.

## Rebuild the browser rootfs

A third rootfs (`rootfs-browser.ext4`) adds Firefox ESR in a kiosk configuration
plus `/sbin/overlay-init`, which the cold boot uses to mount the ext4 image
read-only as the lower overlay layer and a tmpfs as the upper layer before
handing off to init. The homepage URL is set at build time via `HOMEPAGE`; the
default is DuckDuckGo. The base is pinned to **Alpine 3.21** for **cage 0.2.0**,
which the [runtime window resize](../features/gui-display.md#runtime-resize)
requires (cage 0.1.5 terminates when the sole output is cycled); do not downgrade.

```bash
cd kimage
scp build/build-rootfs-browser.sh build/devmem.c build/vmid-reseed.c artemis2:~/kbuild/
ssh artemis2 'cd ~/kbuild && chmod +x build-rootfs-browser.sh && HOMEPAGE=https://duckduckgo.com ./build-rootfs-browser.sh'
scp artemis2:'~/kbuild/out/rootfs-browser.ext4' out/rootfs-browser.ext4
```

See [Disposable browser](../features/disposable-browser.md) for warm-base
creation and session management.

## Rebuild the fuzz initramfs

The snapshot fuzzer (`boot --fuzz`) uses a separate minimal initramfs whose
`/init` is the static-musl harness in `kimage/build/fuzz-harness/`. Built the
same way (arm64 alpine container), packed as a newc cpio.

```bash
cd kimage
ssh artemis2 'mkdir -p ~/kbuild/fuzz-harness'
scp build/fuzz-harness/harness.c build/fuzz-harness/ignition_fuzz.h artemis2:~/kbuild/fuzz-harness/
scp build/build-fuzz-initramfs.sh artemis2:~/kbuild/
ssh artemis2 'cd ~/kbuild && chmod +x build-fuzz-initramfs.sh && ./build-fuzz-initramfs.sh'
# the script writes ~/kbuild/out/fuzz-initramfs.cpio, falling back to
# ~/kbuild/fuzz-initramfs.cpio if out/ is root-owned from a prior kernel build —
# pull from whichever exists:
scp artemis2:'~/kbuild/out/fuzz-initramfs.cpio' out/fuzz-initramfs.cpio 2>/dev/null \
  || scp artemis2:'~/kbuild/fuzz-initramfs.cpio' out/fuzz-initramfs.cpio
# verify newc cpio magic "070701" at byte 0:
head -c 6 out/fuzz-initramfs.cpio
```

The M2 build instruments `target.c` with `-fsanitize-coverage=trace-pc` and the
harness adds a third `/dev/mem` mapping for the coverage region at `0x09404000`
(64 KiB); no new device nodes are needed (it reuses `/dev/mem`).

After editing `harness.c` (e.g. swapping the M0 stub target for a real one),
rebuild and re-pull. Keep `ignition_fuzz.h` in sync with
`crates/devices/src/fuzz/protocol.rs`.

### libpng benchmark initramfs (M3)

The default `./build-fuzz-initramfs.sh` (no arg → `synthetic`) keeps the ASan
chunk-parser with the planted overflow — that build owns the bug-finding number.
M3 adds a second target, `libpng`, that decodes real PNGs through libpng's
simplified API (`build/fuzz-harness/target_png.c`).

The `libpng` target builds **libpng 1.6.43 + zlib 1.3.1 from source with
`-fsanitize-coverage=trace-pc` only (no ASan)**. Rationale (spec §12): the
throughput / reset-latency / dirty-set numbers must isolate the snapshot machinery
from ASan's shadow-memory churn, so the coverage-only build strips ASan while
keeping edge coverage. Crashes (if any) surface via the harness signal handlers
rather than ASan. The synthetic ASan build stays the default and unchanged.

Build notes (encoded in the script):
- `configure`'s "can the compiler link an executable?" probe compiles a trivial
  `main` with `$CFLAGS`; with `trace-pc` that emits an unresolved
  `__sanitizer_cov_trace_pc` (the callback lives in `harness.c`, which configure
  never sees), so the probe is handed a no-op definition via `LDFLAGS`
  (`/build/covstub.o`). It never enters `libz.a` / `libpng16.a`, so the shipped
  library code stays fully instrumented.
- `harness.c` is shared with the synthetic build and references
  `__asan_set_death_callback`; the no-ASan link supplies a no-op
  `/build/asanstub.o` for it (never called here). `harness.c` is unchanged.
- zlib is fetched from the GitHub release tarball
  (`github.com/madler/zlib/releases/...`); `zlib.net/zlib-<ver>.tar.gz` 404s for
  non-current versions.

Rebuild + pull `fuzz-initramfs-libpng.cpio` (distinct output name, coexists with
the synthetic cpio in `out/`):

```bash
cd kimage
ssh artemis2 'mkdir -p ~/kbuild/fuzz-harness'
scp build/fuzz-harness/harness.c build/fuzz-harness/ignition_fuzz.h build/fuzz-harness/target_png.c artemis2:~/kbuild/fuzz-harness/
scp build/build-fuzz-initramfs.sh artemis2:~/kbuild/
ssh artemis2 'cd ~/kbuild && chmod +x build-fuzz-initramfs.sh && ./build-fuzz-initramfs.sh libpng'
scp artemis2:'~/kbuild/out/fuzz-initramfs-libpng.cpio' out/fuzz-initramfs-libpng.cpio 2>/dev/null \
  || scp artemis2:'~/kbuild/fuzz-initramfs-libpng.cpio' out/fuzz-initramfs-libpng.cpio
head -c 6 out/fuzz-initramfs-libpng.cpio   # expect 070701
```

The remote build log should end with `ldd /out/root/init` showing only
`ld-musl-aarch64.so.1` (libpng + zlib are static).

## Verify (must pass before committing)

| Artifact | Check | Expect |
|----------|-------|--------|
| `out/Image` | `xxd -s 56 -l 4 out/Image` | `4152 4d64` (ARMd) |
| `out/rootfs.ext4` | `dd ... skip=$((0x438)) count=2 \| xxd` | `53ef` |

## Hard rules

- **Never `strip`/`objcopy` the arm64 `Image`.** It is a valid PE/COFF binary;
  strip rewrites it and destroys the boot magic at `0x38`. Copy verbatim. Symptom
  of corruption: header `4d5a 9000 ...` and zeros at `0x38`.
- **Pull artifacts back after the build** — local `out/` keeps the old build
  until you `scp`. A stale `out/Image` date means the re-pull didn't run.
- `out/` is gitignored (large reproducible binaries) — artifacts are not
  committed, only the build scripts are.
- One-time host prereq on a fresh Docker host: register arm64 binfmt —
  `docker run --privileged --rm tonistiigi/binfmt --install arm64`.

## Common edits

- **Kernel config**: add `--enable`/`--disable` lines to the `scripts/config`
  block in `build-kernel.sh` (before `olddefconfig`). The script echoes the
  requested CONFIG lines after `olddefconfig` so you can confirm they survived.
- **Rootfs packages**: add `apk add` lines in the alpine provisioning block of
  `build-rootfs.sh`; bump the `96M` `mke2fs` size if it grows.
- **Kernel version**: change `KVER` and the config URL in `build-kernel.sh`
  (Firecracker ships 5.10 and 6.1 aarch64 configs).

See `kimage/README.md` for the artifact table, boot config JSON, SMP/PSCI
requirements, and the extra kernel features (virtio-balloon, vsock, devmem).
