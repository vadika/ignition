// HVF-backed virtual machine state — the replacement for Firecracker's
// KVM-coupled `vstate` seam (`kvm.rs` / `vm.rs` / `vcpu.rs`).
//
// Split mirrors libkrun:
//   hvf_vm.rs    <- VM lifecycle + guest memory mapping  (FC vm.rs / kvm.rs)
//   hvf_vcpu.rs  <- per-vCPU thread, run loop, exit handling (FC vcpu.rs)
//
// Reference to lift from in Phase 1:
//   libkrun/src/vmm/src/macos/vstate.rs            (threading, WFE parking, run_emulation)
//   firecracker/src/vmm/src/vstate/{vm,vcpu}.rs    (the API shape to preserve)

pub mod hvf_vcpu;
pub mod hvf_vm;
