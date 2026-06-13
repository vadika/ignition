// Architecture support crate for the ignition VMM (lib `ignition_arch`).
//
// Layout mirrors libkrun's `krun-arch`; modules lifted from libkrun were
// updated to import `ignition_arch::aarch64::...`. Today this holds the aarch64
// sysreg tables the hvf crate needs plus FDT generation and boot-register setup.

#[cfg(target_arch = "aarch64")]
pub mod aarch64;
