# kimage — aarch64 Firecracker guest kernel + minimal rootfs

Prebuilt **aarch64** guest assets for booting a Firecracker microVM: a Linux
kernel `Image` and a minimal ext4 root filesystem with a shell.

> These are **aarch64/arm64** artifacts. They run on an arm64 Firecracker host
> (or on an Apple-Silicon Mac via the surrounding `firecracker-mac` project).
> They do **not** run on an x86_64 host.

## Artifacts (`out/`)

| File | Size | What | Integrity check |
|------|------|------|-----------------|
| `out/Image` | 16.9 MB | Linux 6.1 aarch64 kernel, raw boot Image | arm64 magic `41 52 4d 64` ("ARMd") at offset `0x38` |
| `out/rootfs.ext4` | 96 MiB | Alpine 3.19 aarch64 rootfs, ext4 | ext4 magic `53 ef` at `0x438`, volume label `rootfs` |

Verify quickly:

```bash
xxd -s 56 -l 4 out/Image          # expect: 4152 4d64   (ARMd)
file out/Image                    # PE32+ ... Aarch64  (arm64 Image carries an EFI header — normal)
dd if=out/rootfs.ext4 bs=1 skip=$((0x438)) count=2 2>/dev/null | xxd  # expect: 53ef
```

## Rootfs details

- Base: `alpine:3.19` (aarch64), provisioned with `openrc` + `util-linux`.
- Init: OpenRC. Serial console **agetty on `ttyS0`** (Firecracker's default
  console), added to the `default` runlevel. `devfs`, `procfs`, `sysfs` added to
  the `boot` runlevel.
- Login: `root` with **no password** (passwordless console login).
- Shell: BusyBox `/bin/sh` (Alpine default).
- Runtime mountpoints present: `/dev /proc /run /sys /tmp /mnt`.
- Networking: **eth0 via DHCP**, brought up automatically at boot. Provided by
  `ifupdown-ng` + busybox `udhcpc`; `/etc/network/interfaces` marks `eth0` as
  `auto ... inet dhcp`, and the OpenRC `local` service runs `ifup -a` at boot
  (alpine ships no `networking` service). Assumes the host serves DHCP on the
  guest's link — true for vmnet shared mode and typical Firecracker TAP setups.
  For a static address instead, replace `/etc/network/interfaces` with an
  `inet static` stanza in `build/build-rootfs.sh`.
- `devmem`: alpine busybox ships no `devmem` applet, so `build/devmem.c` is
  compiled static (musl) in the build container and installed at
  `/usr/bin/devmem` (`devmem ADDR [WIDTH [VALUE]]`, busybox-compatible). The
  build toolchain is removed afterward (`apk del`). Used by the boot-timer hook
  `/etc/local.d/boottime.start` → `devmem 0x091FF000 8 123`, which signals
  boot-complete to the VMM's boot_timer MMIO device. Requires kernel
  `CONFIG_DEVMEM=y` (above).

## Booting in Firecracker

Minimal VM config:

```json
{
  "boot-source": {
    "kernel_image_path": "out/Image",
    "boot_args": "console=ttyS0 reboot=k panic=1 root=/dev/vda rw"
  },
  "drives": [
    {
      "drive_id": "rootfs",
      "path_on_host": "out/rootfs.ext4",
      "is_root_device": true,
      "is_read_only": false
    }
  ],
  "machine-config": { "vcpu_count": 1, "mem_size_mib": 128 }
}
```

The kernel mounts `/dev/vda` (the first virtio block device) as root and brings
up a login prompt on the serial console (`ttyS0`). Grow `rootfs.ext4` if you add
packages (`resize2fs`, after growing the file).

## SMP (multi-vCPU) boot requirements

The kernel is SMP-capable. On arm64 the guest brings up secondary CPUs via
**PSCI** (`CPU_ON` calls), so the VMM must advertise PSCI in the device tree —
there is no ACPI/MPParse path on this config.

Kernel side (already enabled in `microvm-kernel-ci-aarch64-6.1.config`, verify
with `grep -E 'CONFIG_SMP|PSCI' .config`):

- `CONFIG_SMP=y` — multi-processor support.
- `CONFIG_ARM_PSCI_FW=y` — PSCI firmware driver used to power on secondary CPUs.

VMM side — to boot N vCPUs the hypervisor must:

1. Create N vCPUs and set `vcpu_count` to N in machine-config.
2. Emit one `/cpus/cpu@<n>` node per vCPU in the FDT, each with
   `device_type = "cpu"`, the correct `reg` (MPIDR affinity), and
   `enable-method = "psci"`.
3. Emit a `/psci` node: `compatible = "arm,psci-0.2"` (or `"arm,psci-1.0"`),
   `method = "hvc"` (Firecracker/HVF convention), and the standard function-id
   properties.
4. Handle the PSCI `CPU_ON` HVC from the guest: start the target vCPU at the
   entry point + context-id passed in the call. Until the VMM services
   `CPU_ON`, secondaries stay parked and the guest logs
   `CPU%u: failed to boot: -110` (timeout).

```json
"machine-config": { "vcpu_count": 2, "mem_size_mib": 256 }
```

Boot proof: `nproc` (or `/proc/cpuinfo`) inside the guest reports N, and
`dmesg | grep -i smp` shows `SMP: Total of N processors activated`.

> The surrounding `firecracker-mac` HVF port handles the PSCI HVC and per-CPU
> FDT generation host-side; see its SMP design/implementation docs. This kernel
> imposes no extra requirement beyond the two CONFIG flags above.

## Extra kernel features

On top of the Firecracker CI config, `build/build-kernel.sh` force-enables
(built-in, `=y`) via `scripts/config` before `olddefconfig`:

- `CONFIG_VIRTIO_BALLOON` — guest memory balloon device.
- `CONFIG_VSOCKETS` + `CONFIG_VIRTIO_VSOCKETS` — vsock core + virtio transport;
  this is what a Firecracker guest needs for vsock (host side is userspace).
- `CONFIG_VHOST` + `CONFIG_VHOST_VSOCK` — host-side vhost-vsock. Inert in a guest
  image (Firecracker does not use vhost), included on request. Drop these two if
  you want a leaner guest.
- `CONFIG_DEVMEM=y`, `CONFIG_STRICT_DEVMEM=n` — `/dev/mem` access reaching MMIO,
  needed by the boot-timer poke (see below). Strict devmem is forced off so a
  userspace tool can write the device register at `BOOT_TIMER_ADDR`.

## How it was built

Built on host **`artemis2`** (Ubuntu 26.04, x86_64, 32 cores). That host has
**Docker but no compiler toolchain**, so everything runs in containers — nothing
is installed on the host. Because the host is x86_64, the kernel is
**cross-compiled** and the rootfs is built under **arm64 emulation** (binfmt +
QEMU).

Scripts live in `build/`:

| Script | Output | Notes |
|--------|--------|-------|
| `build/build-kernel.sh` | `~/kbuild/out/Image` | Linux 6.1 cross-compiled in `ubuntu:22.04` with `gcc-aarch64-linux-gnu`. Config = Firecracker's `microvm-kernel-ci-aarch64-6.1.config` (fetched from GitHub at build time), run through `make olddefconfig`, then `make ARCH=arm64 Image`. |
| `build/build-rootfs.sh` | `~/kbuild/out/rootfs.ext4` | Provisions `alpine:3.19` aarch64 in a container, exports the filesystem, then packs it into ext4 with `mke2fs -d` (no privileged `mount` needed). |

### One-time host prerequisite (arm64 emulation)

Register the arm64 binfmt handler so the host can run arm64 containers:

```bash
docker run --privileged --rm tonistiigi/binfmt --install arm64
# verify:
docker run --rm --platform linux/arm64 alpine uname -m   # -> aarch64
```

### Rebuild from scratch

```bash
# on artemis2 (or any Docker host; arm64 binfmt registered as above)
mkdir -p ~/kbuild
scp build/build-kernel.sh build/build-rootfs.sh artemis2:~/kbuild/
ssh artemis2 'cd ~/kbuild && chmod +x *.sh && ./build-kernel.sh && ./build-rootfs.sh'

# pull artifacts back
scp artemis2:'~/kbuild/out/Image' artemis2:'~/kbuild/out/rootfs.ext4' out/
```

The kernel build caches the unpacked Linux source and object tree under
`~/kbuild/linux-6.1`, so re-running `build-kernel.sh` is incremental.

## Tuning knobs

- **Kernel version**: change `KVER` in `build-kernel.sh` and the config URL.
  Firecracker ships configs for **5.10** and **6.1** (aarch64). 6.1 is used here.
- **Rootfs size**: the `96M` argument to `mke2fs` in `build-rootfs.sh`.
- **Extra packages**: add `apk add ...` lines in the alpine provisioning step of
  `build-rootfs.sh` (then likely bump the rootfs size).

## Gotchas (read before changing build scripts)

- **Never `strip` the arm64 `Image`.** An arm64 kernel `Image` is also a valid
  PE/COFF (EFI) binary. `aarch64-linux-gnu-strip` will happily parse and
  *rewrite* it, replacing the arm64 boot header with a generic DOS/PE stub and
  destroying the boot magic at offset `0x38`. Firecracker then rejects it. Copy
  the `Image` verbatim — never run `strip`/`objcopy` on it. (This bit us once;
  symptom was header `4d5a 9000 0300 ...` and zeros at `0x38` instead of
  `4d5a 40fa ...` / `ARMd`.)
- **`out/` ownership.** The kernel container runs as root and creates
  `~/kbuild/out` root-owned. The rootfs script therefore stages its tar in the
  user-owned `~/kbuild` (host-user `docker export` can't write into a root-owned
  dir), and writes the final ext4 from inside a root container. Artifacts end up
  root-owned but world-readable, so `scp` pulls them fine.
- **`mke2fs -d` vs mount.** The rootfs is populated with `mke2fs -d <dir>`, which
  needs no `sudo`/`mount`/loopback — works in an unprivileged container.

## References

- Firecracker rootfs & kernel setup: <https://github.com/firecracker-microvm/firecracker/blob/main/docs/rootfs-and-kernel-setup.md>
- Guest kernel configs: <https://github.com/firecracker-microvm/firecracker/tree/main/resources/guest_configs>
