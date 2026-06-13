// Per-vCPU state and run loop over Hypervisor.framework.
//
// HVF vCPUs are thread-affine: hv_vcpu_create MUST run on the thread that runs
// the vCPU. So `Vcpu::new` only stores config; the vCPU is created inside the
// thread spawned by `start`.
//
// Replaces: firecracker/src/vmm/src/vstate/vcpu.rs (KVM-coupled there).

use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use ignition_devices::bus::Bus;

pub use ignition_hvf::{HvfVcpu, InterruptType, NoIrqVcpus, VcpuExit, Vcpus};

/// Upper bound on how long the run loop sleeps on an idle exit. Caps a large
/// timer deadline so the loop stays responsive, and bounds the busy-wait on a
/// no-deadline WFI on the earlycon path.
const MAX_PARK: Duration = Duration::from_millis(10);

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
    /// shutdown (PSCI SYSTEM_OFF) or vCPU cancel, or `Err(ignition_hvf::Error)` if an
    /// HVF call failed — in which case the VM should be torn down.
    pub fn start(self) -> JoinHandle<Result<(), ignition_hvf::Error>> {
        thread::spawn(move || self.run())
    }

    fn run(self) -> Result<(), ignition_hvf::Error> {
        let vcpus: Arc<dyn Vcpus> = Arc::new(NoIrqVcpus);

        // Thread-affine: create the vCPU here, not in `new`.
        let mut vcpu = HvfVcpu::new(self.mpidr, false)?;
        vcpu.set_initial_state(self.entry, self.fdt_addr)?;

        loop {
            let exit = vcpu.run(vcpus.clone())?;
            match exit {
                VcpuExit::MmioWrite(addr, data) => self.bus.write(addr, data),
                // `data` aliases the vCPU's mmio_buf; `Bus::read` fills it in
                // place, and the hvf crate copies it into the guest register on
                // the next `run()`. On a bus miss the buffer is left unchanged
                // (zeroed), i.e. the guest reads zero — intentional this milestone.
                VcpuExit::MmioRead(addr, data) => self.bus.read(addr, data),
                VcpuExit::Shutdown => {
                    log::info!("guest requested shutdown (PSCI SYSTEM_OFF)");
                    return Ok(());
                }
                VcpuExit::Canceled => return Ok(()),
                // Idle/timer exits. Earlycon-grade parking: bounded sleeps keep
                // the CPU off the floor and let wall-clock advance toward the
                // next CNTV deadline. Proper channel parking that wakes on an
                // injected IRQ is a later milestone. On re-entry hvf_sync_vtimer
                // unmasks the vtimer and sets the IRQ; when a GIC is present (the
                // boot harness creates the in-kernel hv_gic) it redelivers it.
                VcpuExit::WaitForEventTimeout(d) => thread::sleep(d.min(MAX_PARK)),
                // TODO(phase1-smp): wake on a sibling vCPU's SEV instead of polling.
                VcpuExit::WaitForEvent => thread::sleep(MAX_PARK),
                VcpuExit::WaitForEventExpired | VcpuExit::VtimerActivated => {}
                other => log::debug!("unhandled vCPU exit: {other:?}"),
            }
        }
    }
}
