# ignition

A research microVM for **macOS on Apple Silicon**, built on Apple's
**Hypervisor.framework (HVF)**. Architecturally modeled on AWS Firecracker — the
microVM model, the vstate seam, the device set — but **not a port of it**: it
shares ~0 lines of Firecracker source (the Firecracker repo isn't even a
dependency). The lineage is the *design*, plus the rust-vmm building blocks
Firecracker also uses (`vm-superio`, `vm-fdt`).

The one genuinely lifted piece is the HVF backend — the `hvf` crate, taken from
[libkrun](https://github.com/containers/libkrun) (Red Hat, Apache-2.0; itself
Firecracker-inspired) and then substantially reworked here (direct `hv_gic_*`,
SMP, snapshot/restore). Everything else — devices, FDT, the vstate layer, boot
harness — is original. See `docs/HANDOFF.md` and `docs/firecracker-hvf-porting-map.md`
for the source analysis, and `docs/SPIKE_RESULTS.md` for the validation spike.

## Status: boots Linux to a shell, with snapshot/restore

See `ROADMAP.md` for the full feature roadmap and progress tracking.

Validated end-to-end on macOS 26.5 / Apple Silicon. Working today (each with a
spec under `docs/superpowers/specs/` and a result writeup under `docs/`):

- **Boot to shell** — aarch64 kernel + FDT load, in-kernel GICv3, interactive
  16550 console (TX + RX).
- **Device model** — a uniform `DeviceManager` (MMIO/SPI allocation, bus, FDT,
  snapshot) behind one `MmioDevice` trait. The full Firecracker aarch64 device set:
  - **virtio-blk** — rootfs from a disk image.
  - **virtio-net** — `--net`, vmnet NAT backend (guest reaches the internet).
    Snapshot/restore supported (incl. `--smp N`, `sudo`): on restore a fresh vmnet
    interface is started (new MAC) and the VMM bounces the link; a guest
    carrier-watch service rebinds the driver + re-DHCPs, so clones get distinct
    MAC+IP. Active connections reset.
  - **virtio-rng** — entropy source (`getentropy`-backed), always-on.
  - **virtio-balloon** — on-demand memory reclaim (`Ctrl-A b`, `madvise(MADV_FREE_REUSABLE)`);
    the inflation target survives snapshot/restore.
  - **virtio-vsock** — guest→host streams over a host Unix socket (`--vsock-uds`); host→guest is
    a TODO (E2). On restore, live connections are reset (the guest is RST'd — host peers are gone).
  - **PL031 RTC** — wall clock; the kernel sets system time from it.
  - **boot-timer** — pseudo device; the guest pokes a magic byte at boot's end and
    the VMM logs `Guest-boot-time = N ms` (~200 ms here).
- **SMP** — multiple vCPUs via PSCI `CPU_ON` (`--smp N`).
- **Snapshot / restore** — clone-capable (`--store` + `Ctrl-A s`, `--restore <name>`);
  restore is lazy (clonefile + `mmap(MAP_SHARED)`) so the immutable base is never
  mutated and resume touches only used pages. The restored guest idles at ~0% CPU
  and stays responsive. Multi-vCPU (`--smp N`) is
  supported via a stop-the-world rendezvous: every vCPU saves its own registers
  and resumes at its saved PC (restored `--smp 4` guest reports `nproc == 4`). Both
  fresh boot and restore drive one device-wiring site; every device restores its
  full state (transport + queues + per-device: balloon target, vsock connection
  reset, virtio-net link-bounce re-init). `--net` and `--smp N` combine (`sudo`).

The `hvf` crate (the Hypervisor.framework backend, lifted from libkrun) is the
load-bearing layer, exercised end-to-end by the `boot` binary and the crate tests.

## Layout

```
crates/
  arch/      ignition-arch  (lib `ignition_arch`)  — aarch64 sysreg tables; FDT/boot regs later
  hvf/       ignition-hvf   (lib `ignition_hvf`)   — Hypervisor.framework backend, lifted from libkrun then reworked
  devices/   ignition-devices (lib `ignition_devices`) — serial/virtio/GIC (Phase 1)
  vmm/       ignition-vmm   (lib `ignition_vmm`)   — vstate seam (HVF replacement for FC kvm/vm/vcpu)
spike/       ignition-spike                         — the `boot` binary (interactive microVM)
refs/        libkrun + firecracker clones (gitignored, reference only)
scripts/     sign.sh                                — ad-hoc codesign with hypervisor entitlement
```

Crate lib names are `ignition_*`; the `hvf` crate was lifted from libkrun and then reworked, so imports were updated accordingly.

## Build & run

```sh
cargo build
# the runnable artifact is `boot`; it needs the hypervisor entitlement before it
# can call hv_vm_create — re-sign after every build (relinking strips it):
scripts/sign.sh target/debug/boot
# usage (kernel + rootfs) is in "Boot a Linux guest" below.
```

Requires: Apple Silicon Mac, macOS 15+ (26 preferred), Rust 1.96+ (edition 2024).

## Boot a Linux guest

The `boot` binary loads an aarch64 kernel + rootfs, runs the vCPU(s), and gives an
interactive 16550 console. **Re-sign after every build** — relinking strips the
hypervisor entitlement.

```sh
cargo build -p ignition-spike --bin boot
scripts/sign.sh target/debug/boot

# boot to a shell (log in as root); console keys: Ctrl-A s = snapshot, Ctrl-A x = quit, Ctrl-A b = balloon
target/debug/boot kimage/out/Image kimage/out/rootfs.ext4

target/debug/boot --smp 4 kimage/out/Image kimage/out/rootfs.ext4   # multi-vCPU (SMP)
target/debug/boot --net  kimage/out/Image kimage/out/rootfs.ext4    # vmnet NAT networking
```

## Snapshot & restore

Works with `--smp N` (snapshot after boot completes). Snapshots live in a **store**
(`--store <dir>`, default `./vmstore`), under a name (`--name <name>`; an
`adjective-surname` name is auto-generated when omitted). The layout is:

- `<store>/snapshots/<name>/` — the **immutable base**, holding `memory.bin` +
  `gic.bin` + `disk.img` + `vmstate.json` + `manifest.json`.
- `<store>/instances/<name>-<pid>/` — **ephemeral CoW clones**, one per restored
  guest, removed on a clean exit.

Restore is lazy: it `clonefile`s the base into an instance dir (copy-on-write) and
maps memory `MAP_SHARED`, so resume touches only the pages it actually uses and the
base is never mutated. `--mem <MiB>` sets guest RAM (default 512); restore reads the
size from the snapshot, so you don't pass `--mem` on the restore side.

```sh
# 1. boot into a store (name auto-generated, or pass --name), then press Ctrl-A s
#    in the console to snapshot (guest keeps running afterwards)
target/debug/boot --store vmstore --name mysnap kimage/out/Image kimage/out/rootfs.ext4
ls -la vmstore/snapshots/mysnap/

# 2. restore by name — resumes from the saved PC (no kernel re-boot); press Enter for a prompt
target/debug/boot --store vmstore --restore mysnap

# 3. confirm it idles (~0% CPU, not spinning)
target/debug/boot --store vmstore --restore mysnap & BP=$!; sleep 3; ps -o pid,%cpu,command -p $BP; kill $BP

# 4. clone — restore the same base into N independent guests (private CoW clone each)
target/debug/boot --store vmstore --restore mysnap   # run in separate terminals
```

A restored guest can re-snapshot: its `Ctrl-A s` writes a **new** named base.
Reusing the source name is refused unless you pass `--force`.

### Diff snapshots

`--track-dirty` arms write-protect dirty tracking: guest RAM is mapped read-only and
the first write to each 16 KiB page traps, faults the page back to writable, and marks
it dirty. A restored guest armed this way writes a **Diff** layer on `Ctrl-A s` — only
the changed pages, with `parent` set to the leaf it restored from — forming an
immutable delta chain. Restore reassembles the chain transparently: `clonefile` the
root base, then overlay each diff's pages in order. vmstate/GIC/devices are always
written full per layer (only RAM is deltified); the first write per page carries a
small vmexit cost. Snapshotting under the same name as the parent — or the base it was
restored from — is refused without `--force`.

```sh
# boot armed for diff tracking, snapshot a root, then restore + diff-snapshot
target/debug/boot --store vmstore --name base --track-dirty kimage/out/Image kimage/out/rootfs.ext4
target/debug/boot --store vmstore --restore base --track-dirty --name base-diff
python3 scripts/diff_snapshot_test.py  # full cycle: diff ~3% of RAM, mutation survives, bases immutable
```

Worked example — one warm golden base, many cheap divergent forks:
`docs/examples/diff-snapshot-fanout.md`.

Headless drivers that run the whole cycle:

```sh
python3 scripts/restore_test.py        # boot -> snapshot -> restore; prints CPU% + latency + immutability
python3 scripts/restore_clone_test.py  # login + run a command + two clones
```

Restore expects the same RAM size the snapshot was taken with (read from the
snapshot). The snapshot artifacts (`memory.bin`/`gic.bin`/`disk.img`/`vmstate.json`)
are gitignored by name wherever they land, so a store dir's bases aren't tracked.
