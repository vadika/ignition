# ignition

A research microVM for **macOS on Apple Silicon**, built on Apple's
**Hypervisor.framework (HVF)**. Architecturally modeled on AWS Firecracker (the
microVM model, the vstate seam, the device set), but not a port: it shares ~0 lines
of Firecracker source. The lineage is the *design*, plus the rust-vmm building blocks
Firecracker also uses (`vm-superio`, `vm-fdt`). The HVF backend (the `ignition-hvf`
crate) originates from [libkrun](https://github.com/containers/libkrun) (Apache-2.0)
and was substantially reworked here; everything else is original.

> **📖 Documentation:** <https://vadika.github.io/ignition/> — build, concepts, features,
> fuzzing, benchmarks, internals. Build locally with `mdbook serve docs/`.

## Quickstart

```console
cargo build -p ignition-spike --bin boot
scripts/sign.sh target/debug/boot
target/debug/boot kimage/out/Image kimage/out/rootfs.ext4
```

Requires an Apple Silicon Mac, macOS 15+ (26 preferred), Rust 1.96+ (edition 2024). See
the documentation for everything else: guest assets, snapshot/restore, diff snapshots,
fuzzing, benchmarks.

## Status

Validated end-to-end on Apple Silicon. Working today:

- **Boot to shell** — aarch64 kernel + FDT load, in-kernel GICv3, interactive 16550 console.
- **Device model** — uniform `DeviceManager` + `MmioDevice` trait; the full Firecracker aarch64 set.
- **virtio** — blk, net (vmnet NAT, `--net`), rng, balloon, vsock (guest→host), gpu (2D), input.
- **PL031 RTC + boot-timer.**
- **SMP** — multiple vCPUs via PSCI `CPU_ON` (`--smp N`).
- **GUI display** — `--gui` opens a software-rendered macOS window over virtio-gpu (2D) +
  virtio-input (keyboard + tablet); a cage Wayland compositor runs a real desktop. The window
  is **resizable**: dragging it re-modesets the guest (virtio-gpu connector-cycle) and the
  desktop reflows to the new size. Snapshots, restores, and fans out like any guest.
- **Snapshot / restore** — clone-capable, lazy `clonefile` + `MAP_SHARED`, multi-vCPU + net + GUI.
- **Diff snapshots** — `--track-dirty` write-protect tracking (DMA-aware: device writes are tracked
  too); immutable delta chains.
- **Interactive reset** — serial `Ctrl-A r` rolls a live guest back in place to a checkpoint
  (RAM + vCPU + GIC + virtio-device state), fast dirty-only rollback. Under `--gui` reset is a
  **cold relaunch** instead (`Ctrl+Alt+R` exits, the launcher re-`--restore`s) — in-place rollback
  cannot reconcile the live GIC + virtio-gpu state on HVF.
- **Disposable browser** — a throwaway Firefox-kiosk microVM on an overlay root (immutable disk),
  cloned per session from a warm snapshot, reset by cold relaunch, fanned out N
  (`scripts/disposable-browser.sh`). Restore is **~130 ms vs ~7.8 s cold boot** (≈60×), flat
  regardless of working set (`clonefile` + lazy `MAP_SHARED`).
- **Sandbox / jailing** — the VMM self-applies a macOS Seatbelt profile at startup (on by default;
  `--no-sandbox` to disable, fail-closed).
- **In-VMM snapshot fuzzing** — `--fuzz` per-iteration dirty-page reset loop.

Full feature docs: the documentation site. Roadmap and progress: `ROADMAP.md`.

## Layout

```
crates/
  arch/      ignition-arch     (lib ignition_arch)     — aarch64 sysreg tables, FDT, boot regs
  hvf/       ignition-hvf      (lib ignition_hvf)      — Hypervisor.framework backend
  devices/   ignition-devices  (lib ignition_devices)  — serial, virtio, GIC, fuzz device
  vmm/       ignition-vmm      (lib ignition_vmm)       — vstate seam (HVF in place of FC kvm/vm/vcpu)
spike/       ignition-spike                            — the `boot` binary (interactive microVM)
docs/        mdBook documentation (src/) + agentic specs/plans (superpowers/)
examples/    runnable walkthroughs (diff-snapshot fan-out, fuzzing demo)
scripts/     sign.sh and the benchmark/gate drivers
refs/        reference VMM clones (gitignored, reference only)
```
