// Architecture support crate for the ignition VMM.
//
// Layout mirrors libkrun's `krun-arch` (lib name `arch`) so modules lifted from
// libkrun import `arch::aarch64::...` unchanged. Today this holds only the
// aarch64 sysreg tables the hvf crate needs; boot-register setup and FDT
// generation get lifted from Firecracker's `arch/aarch64` in Phase 1.

#[cfg(target_arch = "aarch64")]
pub mod aarch64;
