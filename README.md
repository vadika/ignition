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
harness — is original. See `HANDOFF.md` and `firecracker-hvf-porting-map.md` for
the source analysis, and `SPIKE_RESULTS.md` for the validation spike.

## Status: boots Linux to a shell, with snapshot/restore

Validated end-to-end on macOS 26.5 / Apple Silicon. Working today (each with a
spec under `docs/superpowers/specs/` and a result writeup under `docs/`):

- **Boot to shell** — aarch64 kernel + FDT load, in-kernel GICv3, interactive
  16550 console (TX + RX).
- **virtio-blk** — rootfs from a disk image.
- **virtio-net** — `--net`, vmnet NAT backend (guest reaches the internet).
- **SMP** — multiple vCPUs via PSCI `CPU_ON` (`--smp N`).
- **Snapshot / restore** — single-vCPU, clone-capable (`--snap-dir` + `Ctrl-A s`,
  `--restore`); restored guest idles at ~0% CPU and stays responsive.

The `hvf` crate (the Hypervisor.framework backend, lifted from libkrun) is the
load-bearing layer; the `hvf-spike` smoke test still exercises it in isolation
(`cargo run -p hvf-spike` after signing).

## Layout

```
crates/
  arch/      ignition-arch  (lib `arch`)  — aarch64 sysreg tables; FDT/boot regs later
  hvf/       ignition-hvf   (lib `hvf`)   — Hypervisor.framework backend, lifted from libkrun then reworked
  devices/   ignition-devices             — serial/virtio/GIC (Phase 1)
  vmm/       ignition-vmm   (lib `vmm`)   — vstate seam (HVF replacement for FC kvm/vm/vcpu)
spike/       hvf-spike                     — smoke test for the hvf crate
refs/        libkrun + firecracker clones (gitignored, reference only)
scripts/     sign.sh                       — ad-hoc codesign with hypervisor entitlement
```

Crate lib names (`arch`, `hvf`, `vmm`) match libkrun's so lifted modules compile
with zero import edits.

## Build & run

```sh
cargo build
# binaries need the hypervisor entitlement before they can call hv_vm_create:
scripts/sign.sh target/debug/hvf-spike
target/debug/hvf-spike
```

Requires: Apple Silicon Mac, macOS 15+ (26 preferred), Rust 1.96+ (edition 2024).

## Boot a Linux guest

The `boot` binary loads an aarch64 kernel + rootfs, runs the vCPU(s), and gives an
interactive 16550 console. **Re-sign after every build** — relinking strips the
hypervisor entitlement.

```sh
cargo build -p hvf-spike --bin boot
scripts/sign.sh target/debug/boot

# boot to a shell (log in as root); console keys: Ctrl-A s = snapshot, Ctrl-A x = quit
target/debug/boot kimage/out/Image kimage/out/rootfs.ext4

target/debug/boot --smp 4 kimage/out/Image kimage/out/rootfs.ext4   # multi-vCPU (SMP)
target/debug/boot --net  kimage/out/Image kimage/out/rootfs.ext4    # vmnet NAT networking
```

## Snapshot & restore

Single-vCPU only (do not combine with `--smp`). A snapshot dir holds
`memory.bin` + `gic.bin` + `disk.img` + `vmstate.json`.

```sh
# 1. boot with an output dir, then press Ctrl-A s in the console to snapshot
#    (guest keeps running afterwards)
target/debug/boot --snap-dir mysnap kimage/out/Image kimage/out/rootfs.ext4
ls -la mysnap/

# 2. restore — resumes from the saved PC (no kernel re-boot); press Enter for a prompt
target/debug/boot --restore mysnap

# 3. confirm it idles (~0% CPU, not spinning)
target/debug/boot --restore mysnap & BP=$!; sleep 3; ps -o pid,%cpu,command -p $BP; kill $BP

# 4. clone — restore the same snapshot into N independent guests (private disk copy each)
target/debug/boot --restore mysnap   # run in separate terminals
```

Headless drivers that run the whole cycle:

```sh
python3 scripts/restore_test.py        # boot -> snapshot -> restore; prints CPU% + responsive
python3 scripts/restore_clone_test.py  # login + run a command + two clones
```

Restore expects the same `RAM_SIZE` the snapshot was taken with. `/snapshot/` and
`/snapshot2/` are gitignored; any other `--snap-dir` name is tracked unless you ignore it.
