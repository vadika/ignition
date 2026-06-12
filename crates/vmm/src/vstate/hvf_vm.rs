// VM lifecycle and guest-memory mapping over Hypervisor.framework.
//
// Phase 0: thin re-export of the validated `hvf` crate's VM type. Phase 1 wraps
// this with guest-memory region tracking, the FDT placement, and the
// machine-config plumbing so the Firecracker REST API can drive it.
//
// Replaces: firecracker/src/vmm/src/vstate/vm.rs (+ the kvm.rs bits).

pub use hvf::HvfVm;

/// Wrapper that will own the `GuestMemory` regions and hand them to HVF.
/// TODO(phase1): track mapped regions; add dirty-tracking hooks for snapshot.
pub struct Vm {
    pub hvf: HvfVm,
}

impl Vm {
    pub fn new(nested_enabled: bool) -> Result<Self, hvf::Error> {
        Ok(Self {
            hvf: HvfVm::new(nested_enabled)?,
        })
    }
}
