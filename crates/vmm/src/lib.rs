// ignition VMM core.
//
// This is the seam that, in upstream Firecracker, lives in
// `src/vmm/src/vstate/{kvm,vm,vcpu}.rs` and is hard-wired to KVM. Here it is
// being rebuilt on Apple's Hypervisor.framework. The `vstate` module is the
// HVF replacement for that seam.
//
// Status: Phase 0 (skeleton). The validated `hvf` crate is wired in; the
// threading model, WFE/WFI idle loop, PSCI secondary-vCPU bringup, and the
// device manager / MMIO bus still need to be lifted from libkrun's
// `macos/vstate.rs` and Firecracker's `device_manager` in Phase 1.

pub mod vstate;
