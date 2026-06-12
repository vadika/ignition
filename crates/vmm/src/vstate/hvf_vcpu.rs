// Per-vCPU state and run loop over Hypervisor.framework.
//
// Phase 0: re-export the validated `hvf` crate's vCPU + exit types. The spike
// already drives `HvfVcpu::run` through a real MMIO + WFI exit sequence.
//
// Phase 1 work to lift from libkrun/src/vmm/src/macos/vstate.rs:
//   - one OS thread per vCPU; hv_vcpu_create MUST run on that thread (affinity)
//   - WFE/WFI idle: park on a crossbeam channel with a CNTV_CVAL-derived timeout
//   - PSCI firmware: handle CpuOn by waking parked secondary vCPU threads
//   - kick via hv::vcpu_request_exit (not signals)
//   - vtimer mask/unmask sync per exit
//
// Replaces: firecracker/src/vmm/src/vstate/vcpu.rs.

pub use hvf::{HvfVcpu, InterruptType, VcpuExit, Vcpus};
