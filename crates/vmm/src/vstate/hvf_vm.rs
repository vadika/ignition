// VM lifecycle and guest-memory mapping over Hypervisor.framework.
//
// Phase 1: `Vm` owns the guest-memory regions it maps into HVF, so later
// milestones (snapshot/dirty-tracking) have a single place that knows the layout.
//
// Replaces: firecracker/src/vmm/src/vstate/vm.rs (+ the kvm.rs bits).

pub use hvf::HvfVm;

/// One host->guest mapping handed to HVF, retained so the VM owns its layout.
#[derive(Clone, Copy, Debug)]
pub struct MappedRegion {
    pub host_addr: u64,
    pub guest_addr: u64,
    pub size: u64,
}

/// Owns the HVF VM handle and the guest-memory regions mapped into it.
/// TODO(phase1): add dirty-tracking hooks for snapshot on top of `regions`.
pub struct Vm {
    hvf: HvfVm,
    regions: Vec<MappedRegion>,
}

impl Vm {
    pub fn new(nested_enabled: bool) -> Result<Self, hvf::Error> {
        Ok(Self {
            hvf: HvfVm::new(nested_enabled)?,
            regions: Vec::new(),
        })
    }

    /// Map a host range into the guest and record it. Same argument order as
    /// `hvf::HvfVm::map_memory` (host, guest, size).
    pub fn map_memory(&mut self, host_addr: u64, guest_addr: u64, size: u64) -> Result<(), hvf::Error> {
        self.hvf.map_memory(host_addr, guest_addr, size)?;
        self.regions.push(MappedRegion { host_addr, guest_addr, size });
        Ok(())
    }

    /// The regions mapped into this VM, in insertion order.
    pub fn regions(&self) -> &[MappedRegion] {
        &self.regions
    }
}
