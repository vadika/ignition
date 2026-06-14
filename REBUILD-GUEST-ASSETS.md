# Rebuilding guest assets (kernel Image + rootfs)

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
