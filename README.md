# ignition

A research fork porting AWS Firecracker's microVM VMM to **macOS on Apple
Silicon**, replacing KVM with Apple's **Hypervisor.framework (HVF)**. Permanently
diverged from upstream Firecracker (which is KVM-only by design tenet).

Reference implementation for the HVF layer is [libkrun](https://github.com/containers/libkrun)
(Apache-2.0, itself Firecracker-derived). See `HANDOFF.md` and
`firecracker-hvf-porting-map.md` for the full plan and source analysis, and
`SPIKE_RESULTS.md` for the validation spike that de-risked this structure.

## Status: Phase 0 — skeleton

The `hvf` crate (lifted verbatim from libkrun) is validated end-to-end on
macOS 26.5 / Apple Silicon: it creates a VM, runs a vCPU, and decodes MMIO + WFI
exits correctly (`cargo run -p hvf-spike` after signing). Everything above it is
stubs awaiting Phase 1 (boot-to-shell).

## Layout

```
crates/
  arch/      ignition-arch  (lib `arch`)  — aarch64 sysreg tables; FDT/boot regs later
  hvf/       ignition-hvf   (lib `hvf`)   — Hypervisor.framework backend, verbatim from libkrun
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
