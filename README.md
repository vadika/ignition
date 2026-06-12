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
