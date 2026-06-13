// VM lifecycle and guest-memory mapping over Hypervisor.framework.
//
// Replaces: firecracker/src/vmm/src/vstate/vm.rs (+ the kvm.rs bits).

use ignition_hvf::HvfVm;

/// Owns the HVF VM handle.
pub struct Vm {
    hvf: HvfVm,
}

impl Vm {
    pub fn new(nested_enabled: bool) -> Result<Self, ignition_hvf::Error> {
        Ok(Self {
            hvf: HvfVm::new(nested_enabled)?,
        })
    }

    /// Map a host range into the guest. Same argument order as
    /// `ignition_hvf::HvfVm::map_memory` (host, guest, size). No dedup/overlap check here:
    /// the caller is responsible for not requesting overlapping guest ranges
    /// (HVF rejects re-mapping the same IPA but does not guarantee rejecting
    /// every overlap).
    pub fn map_memory(&mut self, host_addr: u64, guest_addr: u64, size: u64) -> Result<(), ignition_hvf::Error> {
        self.hvf.map_memory(host_addr, guest_addr, size)
    }
}
