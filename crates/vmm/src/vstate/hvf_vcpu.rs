// Per-vCPU state and run loop over Hypervisor.framework.
//
// HVF vCPUs are thread-affine: hv_vcpu_create MUST run on the thread that runs
// the vCPU. So `Vcpu::new` only stores config; the vCPU is created inside the
// thread spawned by `start`.
//
// Replaces: firecracker/src/vmm/src/vstate/vcpu.rs (KVM-coupled there).

use std::sync::Arc;
use std::thread::{self, JoinHandle};

use devices::bus::Bus;

pub use hvf::{HvfVcpu, InterruptType, VcpuExit, Vcpus};

/// Interrupt source with no GIC yet: the guest receives no injected IRQs, and
/// trapped system-register accesses are acknowledged so the vCPU keeps running.
/// Replaced by a real GIC-backed `Vcpus` impl in a later milestone.
struct NoIrqVcpus;

impl Vcpus for NoIrqVcpus {
    fn set_vtimer_irq(&self, _vcpuid: u64) {}
    fn should_wait(&self, _vcpuid: u64) -> bool {
        false
    }
    fn has_pending_irq(&self, _vcpuid: u64) -> bool {
        false
    }
    fn get_pending_irq(&self, _vcpuid: u64) -> u32 {
        0
    }
    fn handle_sysreg_read(&self, _vcpuid: u64, _reg: u32) -> Option<u64> {
        Some(0)
    }
    fn handle_sysreg_write(&self, _vcpuid: u64, _reg: u32, _val: u64) -> bool {
        true
    }
}

/// A single guest vCPU that runs on its own OS thread.
pub struct Vcpu {
    mpidr: u64,
    entry: u64,
    fdt_addr: u64,
    bus: Arc<Bus>,
}

impl Vcpu {
    pub fn new(mpidr: u64, entry: u64, fdt_addr: u64, bus: Arc<Bus>) -> Self {
        Self { mpidr, entry, fdt_addr, bus }
    }

    /// Spawn the vCPU thread. The join handle resolves to `Ok(())` on guest
    /// shutdown (PSCI SYSTEM_OFF) or vCPU cancel.
    pub fn start(self) -> JoinHandle<Result<(), hvf::Error>> {
        thread::spawn(move || self.run())
    }

    fn run(self) -> Result<(), hvf::Error> {
        let vcpus: Arc<dyn Vcpus> = Arc::new(NoIrqVcpus);

        // Thread-affine: create the vCPU here, not in `new`.
        let mut vcpu = HvfVcpu::new(self.mpidr, false)?;
        vcpu.set_initial_state(self.entry, self.fdt_addr)?;

        loop {
            // `VcpuExit` borrows from `vcpu` (mmio_buf lifetime), so we must
            // copy out any data we need before the borrow ends, then dispatch
            // to the bus outside the match arm that holds the borrow.
            enum Action {
                MmioWrite(u64, Vec<u8>),
                MmioRead(u64, usize),
                Shutdown,
                Canceled,
                Other,
            }

            let action = {
                let exit = vcpu.run(vcpus.clone())?;
                match exit {
                    VcpuExit::MmioWrite(addr, data) => {
                        Action::MmioWrite(addr, data.to_vec())
                    }
                    VcpuExit::MmioRead(addr, data) => {
                        Action::MmioRead(addr, data.len())
                    }
                    VcpuExit::Shutdown => Action::Shutdown,
                    VcpuExit::Canceled => Action::Canceled,
                    other => {
                        log::debug!("unhandled vCPU exit: {other:?}");
                        Action::Other
                    }
                }
            };

            match action {
                Action::MmioWrite(addr, data) => self.bus.write(addr, &data),
                Action::MmioRead(addr, len) => {
                    let mut buf = vec![0u8; len];
                    self.bus.read(addr, &mut buf);
                }
                Action::Shutdown => {
                    log::info!("guest requested shutdown (PSCI SYSTEM_OFF)");
                    return Ok(());
                }
                Action::Canceled => return Ok(()),
                // No idle-park yet; the milestone guest does not WFI on the
                // success path. TODO(phase1-smp): WFE/WFI parking with a
                // CNTV_CVAL-derived timeout, lifted from libkrun macos/vstate.rs.
                Action::Other => {}
            }
        }
    }
}
