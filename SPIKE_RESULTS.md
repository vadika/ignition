# Validation Spike Results

Date: 2026-06-12. Machine: Apple Silicon, macOS 26.5.1 (build 25F80), arm64.
Toolchain: rustc/cargo 1.96.0 (Homebrew). SDK: MacOSX 26.5 (Xcode).

## What was validated

The HANDOFF's "concrete first task": confirm libkrun's `hvf` crate, lifted into
a standalone consumer, compiles and runs against the current macOS SDK before
committing to fork structure.

Spike lives in `spike/` (cargo bin `hvf-spike`). It lifts, **verbatim**:
- `bindings.rs` (4712 L) — libkrun's generated Hypervisor.framework bindings
- `lib.rs` (731 L) → `src/hvf/mod.rs` — only edits: dropped `#[macro_use] extern
  crate log` for `use log::{...}`, and repointed the one external dep
  `arch::aarch64::sysreg::{SYSREG_MASK, sys_reg_name}` to a local `crate::arch`.
- `sysreg.rs` (146 L) → `src/arch.rs` — copied unchanged.

Link: `cargo:rustc-link-lib=framework=Hypervisor` (same as libkrun's vmm/build.rs).
Entitlement: ad-hoc codesign with `com.apple.security.hypervisor`.

Guest = 5 hand-assembled aarch64 instructions: store byte to unmapped MMIO
`0x09000000` (→ EC_DATAABORT), then spin on WFI (→ EC_WFX_TRAP).

## Results — ALL PASS

1. **Compiles**: 0 errors, only dead-code warnings (unused enum variants/fields
   the spike doesn't exercise). Lifted code is clean against rustc 1.96 / edition
   2024 (let-chains, `unsafe extern`, etc. all fine).
2. **Links + entitlement**: `hv_vm_create` succeeds → framework linkage and the
   hypervisor entitlement both work with ad-hoc codesign.
3. **Runs**: VM + thread-affine vCPU created, 1 MiB guest RAM mapped, boot regs
   set (PC, X0), `hv_vcpu_run` drove the guest. Observed exits, in order:
   - `MmioWrite(0x09000000, [0x48, 0, 0, 0])`  — 'H', correct addr/data
   - `WaitForEvent`                            — WFI decoded correctly
4. **Bindings ABI matches macOS 26.5 SDK** (C probe vs checked-in asserts):
   `hv_vcpu_exit_t` size 32 / align 8, `reason`@0, `exception`@8;
   `hv_vcpu_exit_exception_t` syndrome@0 / virtual_address@8 / physical_address@16;
   `HV_EXIT_REASON` CANCELED=0 / EXCEPTION=1 / VTIMER=2. **Exact match.**

## Implications for the fork

- libkrun's checked-in `bindings.rs` is **reusable verbatim** on macOS 26.5 — no
  bindgen regeneration needed (de-risks the HANDOFF's "or regenerate" caveat).
- The ESR_EL2 syndrome decode in `lib.rs::run()` works as-is end to end.
- Green light to commit to fork structure and proceed to Phase 1 (boot-to-shell:
  add a serial/UART MMIO device + FDT + the vstate.rs threading/WFE-parking loop
  so a real kernel reaches a serial prompt).

## Repro

```
cd spike
cargo build
codesign --force --sign - --entitlements hvf-spike.entitlements target/debug/hvf-spike
target/debug/hvf-spike
```
