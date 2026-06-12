// aarch64 architecture support.
//
// `sysreg` is lifted verbatim from libkrun's
// `src/arch/src/aarch64/macos/sysreg.rs`. The hvf crate imports
// `arch::aarch64::sysreg::{SYSREG_MASK, sys_reg_name}` from here.

pub mod sysreg;
