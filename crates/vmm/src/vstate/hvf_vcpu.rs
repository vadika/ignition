// Per-vCPU types over Hypervisor.framework.
//
// HVF vCPUs are thread-affine: hv_vcpu_create MUST run on the thread that runs
// the vCPU. The actual run loop lives in `VcpuManager` (vcpu_manager.rs); this
// module just re-exports the hvf vCPU primitives consumers build on.
//
// Replaces: firecracker/src/vmm/src/vstate/vcpu.rs (KVM-coupled there).

pub use ignition_hvf::{HvfVcpu, NoIrqVcpus, VcpuExit, Vcpus};
