// Multi-vCPU orchestration: owns the device bus, the configured MPIDR set, and a
// registry of running vCPU threads. The primary boots alone; secondaries are
// spawned lazily on PSCI CPU_ON. In-kernel hv_gic handles SGIs/IPIs and per-cpu
// vtimers, so no userspace IRQ routing is needed here.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use devices::bus::Bus;
use hvf::{HvfVcpu, NoIrqVcpus, VcpuExit, Vcpus};

/// Upper bound on an idle WFI/timer park, matching the single-vCPU runner.
const MAX_PARK: Duration = Duration::from_millis(10);

/// MPIDR for a given logical cpu index. Linear Aff0 = index — valid for the
/// microVM's small core counts (<= 256). The FDT cpu node `reg` uses the same
/// value, so PSCI CPU_ON targets match.
pub fn mpidr_for(index: u64) -> u64 {
    index
}

/// Outcome of trying to claim an MPIDR for a CPU_ON request.
#[derive(Debug, PartialEq, Eq)]
pub enum Claim {
    /// Newly claimed — caller should spawn the vCPU.
    Claimed,
    /// Not one of the configured MPIDRs — reject.
    Unknown,
    /// Already running — duplicate CPU_ON, reject.
    AlreadyRunning,
}

pub struct VcpuManager {
    bus: Arc<Bus>,
    mpidrs: HashSet<u64>,
    /// MPIDRs that have been brought up. Bring-up-only: entries are never
    /// cleared (PSCI bring-up is one-way here; CPU_OFF/hotplug is out of scope),
    /// so a second CPU_ON for a live MPIDR is rejected as AlreadyRunning.
    running: Mutex<HashSet<u64>>,
    /// Live hvf vCPU ids, registered before each vCPU's first run() for the shutdown broadcast.
    vcpuids: Mutex<Vec<u64>>,
    threads: Mutex<Vec<JoinHandle<Result<(), hvf::Error>>>>,
    shutdown: AtomicBool,
    /// Set by `request_snapshot`; cleared atomically inside `run_loop` when handled.
    snapshot_req: AtomicBool,
    /// Installed by the boot harness before `run`; called on the vCPU thread when
    /// `snapshot_req` fires. Single-vCPU only (asserted at install time).
    snapshot_handler: Option<Box<dyn Fn(&HvfVcpu) + Send + Sync>>,
}

impl VcpuManager {
    /// Create a manager for `vcpu_count` cpus (MPIDRs `mpidr_for(0..vcpu_count)`).
    pub fn new(vcpu_count: u64, bus: Arc<Bus>) -> Arc<Self> {
        let mpidrs = (0..vcpu_count).map(mpidr_for).collect();
        Arc::new(Self {
            bus,
            mpidrs,
            running: Mutex::new(HashSet::new()),
            vcpuids: Mutex::new(Vec::new()),
            threads: Mutex::new(Vec::new()),
            shutdown: AtomicBool::new(false),
            snapshot_req: AtomicBool::new(false),
            snapshot_handler: None,
        })
    }

    /// Install a snapshot handler. MUST be called before `run` and only with
    /// `vcpu_count == 1`. The handler is invoked on the vCPU thread (required by
    /// HVF thread-affinity) when the snapshot request fires.
    pub fn set_snapshot_handler(
        self: &mut Arc<Self>,
        handler: Box<dyn Fn(&HvfVcpu) + Send + Sync>,
    ) {
        let me = Arc::get_mut(self).expect("set_snapshot_handler must be called before run");
        assert_eq!(me.mpidrs.len(), 1, "snapshot is single-vCPU only");
        me.snapshot_handler = Some(handler);
    }

    /// Request a snapshot. Sets the flag and interrupts the (single) vCPU so the
    /// handler fires at the next `Canceled` exit. No-op if no handler is installed.
    pub fn request_snapshot(&self) {
        if self.snapshot_handler.is_none() {
            return;
        }
        self.snapshot_req.store(true, Ordering::Release);
        // Interrupt the primary vCPU so it exits to `Canceled` promptly.
        if let Some(&vcpuid) = self.vcpuids.lock().unwrap().first() {
            let _ = hvf::vcpu_request_exit(vcpuid);
        }
    }

    /// The HVF vcpuid of the primary vCPU (index 0), if it has been registered.
    pub fn primary_vcpuid(&self) -> Option<u64> {
        self.vcpuids.lock().unwrap().first().copied()
    }

    /// Try to claim `mpidr` for a bring-up. Idempotent guard against unknown or
    /// duplicate CPU_ON targets; inserts into `running` on success.
    fn claim(&self, mpidr: u64) -> Claim {
        if !self.mpidrs.contains(&mpidr) {
            return Claim::Unknown;
        }
        let mut running = self.running.lock().unwrap();
        if running.contains(&mpidr) {
            Claim::AlreadyRunning
        } else {
            running.insert(mpidr);
            Claim::Claimed
        }
    }

    /// Spawn the primary vCPU (MPIDR 0) and block until every vCPU thread exits
    /// (guest PSCI SYSTEM_OFF, or all threads cancelled). Returns the first
    /// vCPU error, if any.
    pub fn run(self: &Arc<Self>, entry: u64, fdt_addr: u64) -> Result<(), hvf::Error> {
        let me = Arc::clone(self);
        let primary = thread::spawn(move || me.run_primary(entry, fdt_addr));
        self.threads.lock().unwrap().push(primary);
        self.join_all()
    }

    /// Run the restore path: create a fresh primary vCPU, restore its state from
    /// `vcpu_state`, and run the loop without calling `set_initial_state`. The
    /// vCPU's PC/regs come exclusively from `vcpu_state`. Single-vCPU only.
    pub fn run_restored(
        self: &Arc<Self>,
        vcpu_state: hvf::VcpuState,
    ) -> Result<(), hvf::Error> {
        let me = Arc::clone(self);
        let handle = thread::spawn(move || me.run_restored_primary(vcpu_state));
        self.threads.lock().unwrap().push(handle);
        self.join_all()
    }

    fn run_restored_primary(self: &Arc<Self>, vcpu_state: hvf::VcpuState) -> Result<(), hvf::Error> {
        let mpidr = mpidr_for(0);
        self.running.lock().unwrap().insert(mpidr);
        let vcpu = HvfVcpu::new(mpidr, false)?;
        self.vcpuids.lock().unwrap().push(vcpu.id());
        // Restore state instead of set_initial_state.
        vcpu.restore_state(&vcpu_state)?;
        self.run_loop(vcpu)
    }

    fn run_primary(self: &Arc<Self>, entry: u64, fdt_addr: u64) -> Result<(), hvf::Error> {
        let mpidr = mpidr_for(0);
        self.running.lock().unwrap().insert(mpidr);
        let vcpu = HvfVcpu::new(mpidr, false)?;
        self.vcpuids.lock().unwrap().push(vcpu.id());
        vcpu.set_initial_state(entry, fdt_addr)?;
        self.run_loop(vcpu)
    }

    fn run_secondary(self: &Arc<Self>, mpidr: u64, entry: u64, ctx: u64) -> Result<(), hvf::Error> {
        let vcpu = HvfVcpu::new(mpidr, false)?;
        // Register before the first run() so a shutdown broadcast reaches us.
        self.vcpuids.lock().unwrap().push(vcpu.id());
        vcpu.set_secondary_state(entry, ctx)?;
        self.run_loop(vcpu)
    }

    /// Spawn a secondary for a PSCI CPU_ON, guarding against unknown/duplicate
    /// targets and against spawning after shutdown.
    fn spawn(self: &Arc<Self>, mpidr: u64, entry: u64, ctx: u64) {
        if self.shutdown.load(Ordering::Acquire) {
            return;
        }
        match self.claim(mpidr) {
            Claim::Claimed => {}
            Claim::Unknown => {
                log::warn!("CPU_ON for unconfigured mpidr {mpidr:#x} ignored");
                return;
            }
            Claim::AlreadyRunning => {
                log::warn!("CPU_ON for already-running mpidr {mpidr:#x} ignored");
                return;
            }
        }
        let me = Arc::clone(self);
        let handle = thread::spawn(move || me.run_secondary(mpidr, entry, ctx));
        self.threads.lock().unwrap().push(handle);
    }

    /// Broadcast a stop to every registered vCPU and set the shutdown flag.
    fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        for id in self.vcpuids.lock().unwrap().iter() {
            let _ = hvf::vcpu_request_exit(*id);
        }
    }

    /// The shared per-vCPU run loop (primary and secondary).
    fn run_loop(self: &Arc<Self>, mut vcpu: HvfVcpu) -> Result<(), hvf::Error> {
        let vcpus: Arc<dyn Vcpus> = Arc::new(NoIrqVcpus);
        // Termination relies on this top-of-loop shutdown check, not the
        // vcpu_request_exit broadcast: the broadcast only interrupts a vcpu
        // already blocked in run(); a vcpu that exits for any other reason
        // re-checks the (monotonic) flag here on the next iteration. Bounded by
        // one vcpu.run() (MAX_PARK on WFI).
        loop {
            if self.shutdown.load(Ordering::Acquire) {
                return Ok(());
            }
            match vcpu.run(vcpus.clone())? {
                VcpuExit::MmioWrite(addr, data) => self.bus.write(addr, data),
                VcpuExit::MmioRead(addr, data) => self.bus.read(addr, data),
                VcpuExit::CpuOn(mpidr, entry, ctx) => self.spawn(mpidr, entry, ctx),
                VcpuExit::Shutdown => {
                    log::info!("guest requested shutdown (PSCI SYSTEM_OFF)");
                    self.request_shutdown();
                    return Ok(());
                }
                VcpuExit::Canceled => {
                    if self.snapshot_req.swap(false, Ordering::AcqRel) {
                        if let Some(h) = &self.snapshot_handler {
                            // A panic in the handler must not unwind the vCPU thread —
                            // log and resume.
                            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| h(&vcpu)));
                            if r.is_err() {
                                log::error!("snapshot handler panicked; guest resumed");
                            }
                        }
                        continue; // resume the guest after snapshotting
                    }
                    return Ok(());
                }
                VcpuExit::WaitForEventTimeout(d) => thread::sleep(d.min(MAX_PARK)),
                VcpuExit::WaitForEvent => thread::sleep(MAX_PARK),
                VcpuExit::WaitForEventExpired | VcpuExit::VtimerActivated => {}
                other => log::debug!("unhandled vCPU exit: {other:?}"),
            }
        }
    }

    /// Join every spawned vCPU thread, draining the registry so threads spawned
    /// mid-run are still joined. Returns the first error.
    fn join_all(&self) -> Result<(), hvf::Error> {
        let mut result = Ok(());
        loop {
            let handle = self.threads.lock().unwrap().pop();
            match handle {
                Some(h) => {
                    if let Err(e) = h.join().expect("vCPU thread panicked")
                        && result.is_ok()
                    {
                        result = Err(e);
                    }
                }
                None => return result,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mgr(n: u64) -> Arc<VcpuManager> {
        VcpuManager::new(n, Arc::new(Bus::new()))
    }

    #[test]
    fn mpidr_is_linear() {
        assert_eq!(mpidr_for(0), 0);
        assert_eq!(mpidr_for(3), 3);
    }

    #[test]
    fn claim_accepts_configured_mpidr_once() {
        let m = mgr(4);
        assert_eq!(m.claim(1), Claim::Claimed);
        assert_eq!(m.claim(1), Claim::AlreadyRunning);
    }

    #[test]
    fn claim_rejects_unconfigured_mpidr() {
        let m = mgr(2); // mpidrs {0, 1}
        assert_eq!(m.claim(2), Claim::Unknown);
    }
}
