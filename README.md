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

> **📖 Full documentation:** build the book with `mdbook serve docs/` (or see the
> published site). Source under [`docs/src/`](docs/src/SUMMARY.md).

## Quickstart

```console
cargo build -p ignition-spike --bin boot
scripts/sign.sh target/debug/boot
target/debug/boot kimage/out/Image kimage/out/rootfs.ext4
```

See the book for everything else: build, guest assets, snapshot/restore, diff snapshots, fuzzing, benchmarks, internals.

## Status: boots Linux to a shell, snapshot/restore, in-VMM snapshot fuzzing

Validated end-to-end on macOS 26.5 / Apple Silicon. Working today:

- **Boot to shell** — aarch64 kernel + FDT load, in-kernel GICv3, interactive 16550 console.
- **Device model** — uniform `DeviceManager` + `MmioDevice` trait; full Firecracker aarch64 set.
- **virtio** — blk, net (vmnet NAT, `--net`), rng, balloon, vsock (guest→host E1).
- **PL031 RTC + boot-timer** — wall clock and a `Guest-boot-time` probe.
- **SMP** — multiple vCPUs via PSCI `CPU_ON` (`--smp N`).
- **Snapshot / restore** — clone-capable, lazy `clonefile` + `MAP_SHARED`, ~0% CPU idle, multi-vCPU + net.
- **Diff snapshots** — `--track-dirty` write-protect tracking; immutable delta chains.
- **In-VMM snapshot fuzzing** — `--fuzz` dirty-page reset loop; 1309 execs/sec on libpng 1.6.43.

Full feature docs: the book; roadmap: `ROADMAP.md`.

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

Requires: Apple Silicon Mac, macOS 15+ (26 preferred), Rust 1.96+ (edition 2024).
