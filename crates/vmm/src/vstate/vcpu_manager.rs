// Multi-vCPU orchestration: owns the device bus, the configured MPIDR set, and a
// registry of running vCPU threads. The primary boots alone; secondaries are
// spawned lazily on PSCI CPU_ON. In-kernel hv_gic handles SGIs/IPIs and per-cpu
// vtimers, so no userspace IRQ routing is needed here.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use ignition_devices::bus::Bus;
use ignition_hvf::{HvfVcpu, NoIrqVcpus, VcpuExit, Vcpus};
use crate::dirty::{DirtyTracker, PAGE};
use crate::snapshot::VcpuCheckpoint;

/// Dirty-page tracking config, armed by the boot harness before `run` when
/// `--track-dirty` is set. The window (`base`, `size`) is the write-protected
/// guest-RAM range; `tracker` is the shared atomic bitmap each vCPU thread
/// marks on a write-protect fault. Cloning shares the underlying bitmap.
#[derive(Clone)]
pub struct DirtyConfig {
    pub base: u64,
    pub size: u64,
    pub tracker: DirtyTracker,
}

/// Upper bound on an idle WFI/timer park (per-vCPU run loop).
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
    /// A snapshot is in progress — CPU_ON is frozen, reject.
    Frozen,
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
    threads: Mutex<Vec<JoinHandle<Result<(), ignition_hvf::Error>>>>,
    shutdown: AtomicBool,
    /// Set by `request_snapshot`; cleared by the leader inside `run_loop`.
    snapshot_req: AtomicBool,
    /// True for the duration of a snapshot rendezvous; freezes CPU_ON. Read and
    /// written only while holding the `running` lock, so it cannot race a claim.
    snapshot_active: AtomicBool,
    /// Per-snapshot barrier sized to the participant count, published by
    /// `request_snapshot` and read by each vCPU thread at the rendezvous.
    snap_barrier: Mutex<Option<Arc<Barrier>>>,
    /// Each participating vCPU thread pushes `(mpidr, save_state())` here; the
    /// leader drains it after the barrier.
    collected: Mutex<Vec<(u64, Result<ignition_hvf::VcpuState, ignition_hvf::Error>)>>,
    /// Installed by the boot harness before `run`; invoked on the leader thread.
    snapshot_handler: Option<SnapshotHandler>,
    /// Installed by the boot harness before `run` when `--track-dirty` is set.
    /// When present, each vCPU thread arms `set_dirty_window` before its loop,
    /// and the `DirtyFault` arm marks the page + re-grants WRITE.
    dirty: Option<DirtyConfig>,
}

/// A snapshot handler: invoked on the elected leader vCPU thread once every
/// vCPU has saved its register state. Receives the per-vCPU checkpoints and
/// performs the global capture (RAM + GIC + device records) and file write.
type SnapshotHandler = Box<dyn Fn(Vec<VcpuCheckpoint>) + Send + Sync>;

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
            snapshot_active: AtomicBool::new(false),
            snap_barrier: Mutex::new(None),
            collected: Mutex::new(Vec::new()),
            snapshot_handler: None,
            dirty: None,
        })
    }

    /// Arm dirty-page tracking. MUST be called before `run` (same `Arc::get_mut`
    /// sole-ownership constraint as `set_snapshot_handler`). Each vCPU thread
    /// then calls `set_dirty_window` before its run loop so in-RAM write faults
    /// surface as `DirtyFault`, and the run loop marks + re-grants those pages.
    pub fn set_dirty_config(self: &mut Arc<Self>, config: DirtyConfig) {
        let me = Arc::get_mut(self).expect("set_dirty_config must be called before run");
        me.dirty = Some(config);
    }

    /// Install a snapshot handler. MUST be called before `run`. The handler is
    /// invoked on the leader vCPU thread (HVF thread-affinity) once every vCPU
    /// has rendezvoused and saved its state.
    pub fn set_snapshot_handler(
        self: &mut Arc<Self>,
        handler: Box<dyn Fn(Vec<VcpuCheckpoint>) + Send + Sync>,
    ) {
        let me = Arc::get_mut(self).expect("set_snapshot_handler must be called before run");
        me.snapshot_handler = Some(handler);
    }

    /// Request a snapshot. Freezes CPU_ON, latches the participant set, sizes the
    /// rendezvous barrier, and interrupts every registered vCPU so each exits to
    /// `Canceled` and joins the rendezvous. No-op if no handler is installed.
    pub fn request_snapshot(&self) {
        if self.snapshot_handler.is_none() {
            return;
        }
        // Freeze CPU_ON under the `running` lock so no claim races the latch.
        // Reject a re-entrant request while a snapshot is still in flight — it
        // would clear `collected` before the in-flight leader drains it.
        {
            let _running = self.running.lock().unwrap();
            if self.snapshot_active.swap(true, Ordering::Relaxed) {
                return;
            }
        }
        // Participants = the vCPUs already registered (running their loop). A
        // CPU_ON mid-spawn (claimed but not yet registered) is the documented
        // mid-boot exclusion; snapshots are taken after boot.
        let ids: Vec<u64> = self.vcpuids.lock().unwrap().clone();
        if ids.is_empty() {
            // No vCPU has registered yet; nothing to snapshot. Unfreeze and bail
            // so a later snapshot still works.
            self.snapshot_active.store(false, Ordering::Relaxed);
            return;
        }
        *self.snap_barrier.lock().unwrap() = Some(Arc::new(Barrier::new(ids.len())));
        self.collected.lock().unwrap().clear();
        self.snapshot_req.store(true, Ordering::Release);
        for id in ids {
            let _ = ignition_hvf::vcpu_request_exit(id);
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
        if self.snapshot_active.load(Ordering::Relaxed) {
            return Claim::Frozen;
        }
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
    pub fn run(self: &Arc<Self>, entry: u64, fdt_addr: u64) -> Result<(), ignition_hvf::Error> {
        let me = Arc::clone(self);
        let primary = thread::spawn(move || me.run_primary(entry, fdt_addr));
        self.threads.lock().unwrap().push(primary);
        self.join_all()
    }

    /// Run the restore path for N cores. Spawns one thread per checkpoint, pre-
    /// seeds `running` with every restored MPIDR (so a later stray CPU_ON is
    /// rejected `AlreadyRunning`), restores the GIC once all redistributors
    /// exist, then resumes each core at its saved PC. Returns the first error.
    pub fn run_restored(
        self: &Arc<Self>,
        checkpoints: Vec<VcpuCheckpoint>,
        gic_blob: Option<Vec<u8>>,
    ) -> Result<(), ignition_hvf::Error> {
        let barrier = Arc::new(Barrier::new(checkpoints.len()));
        let gic_blob = Arc::new(gic_blob);
        {
            let mut running = self.running.lock().unwrap();
            for cp in &checkpoints {
                running.insert(cp.mpidr);
            }
        }
        for cp in checkpoints {
            let me = Arc::clone(self);
            let bar = Arc::clone(&barrier);
            let blob = Arc::clone(&gic_blob);
            let handle = thread::spawn(move || me.run_restored_one(cp, bar, blob));
            self.threads.lock().unwrap().push(handle);
        }
        self.join_all()
    }

    /// One restored vCPU thread. Two barriers bracket the GIC restore so it runs
    /// exactly once, after every redistributor exists and before any per-vCPU
    /// register restore (which writes per-cpu ICC state). A creation or GIC
    /// failure sets `shutdown`; every thread still reaches both barriers, so no
    /// peer deadlocks, and they all bail after the second barrier.
    fn run_restored_one(
        self: &Arc<Self>,
        cp: VcpuCheckpoint,
        barrier: Arc<Barrier>,
        gic_blob: Arc<Option<Vec<u8>>>,
    ) -> Result<(), ignition_hvf::Error> {
        let vcpu = HvfVcpu::new(cp.mpidr, false);
        match &vcpu {
            Ok(v) => self.vcpuids.lock().unwrap().push(v.id()),
            Err(_) => self.shutdown.store(true, Ordering::Release),
        }

        // Barrier 1: every redistributor now exists (or someone failed).
        let mut gic_err = None;
        if barrier.wait().is_leader()
            && !self.shutdown.load(Ordering::Acquire)
            && let Some(blob) = gic_blob.as_ref()
            && let Err(e) = ignition_hvf::gic::gic_restore(blob)
        {
            self.shutdown.store(true, Ordering::Release);
            gic_err = Some(e);
        }
        // Barrier 2: GIC restore (if any) is complete before any register restore.
        barrier.wait();

        if self.shutdown.load(Ordering::Acquire) {
            // Some thread failed creation or the GIC restore. Surface our own
            // error; otherwise bail cleanly so the failing thread's error wins.
            return match vcpu {
                Err(e) => Err(e),
                Ok(_) => gic_err.map_or(Ok(()), Err),
            };
        }

        let vcpu = vcpu.expect("not shutdown implies every vcpu was created");
        vcpu.restore_state(&cp.state)?;
        self.run_loop(cp.mpidr, vcpu)
    }

    fn run_primary(self: &Arc<Self>, entry: u64, fdt_addr: u64) -> Result<(), ignition_hvf::Error> {
        let mpidr = mpidr_for(0);
        self.running.lock().unwrap().insert(mpidr);
        let vcpu = HvfVcpu::new(mpidr, false)?;
        self.vcpuids.lock().unwrap().push(vcpu.id());
        vcpu.set_initial_state(entry, fdt_addr)?;
        self.run_loop(mpidr, vcpu)
    }

    fn run_secondary(self: &Arc<Self>, mpidr: u64, entry: u64, ctx: u64) -> Result<(), ignition_hvf::Error> {
        let vcpu = HvfVcpu::new(mpidr, false)?;
        // Register before the first run() so a shutdown broadcast reaches us.
        self.vcpuids.lock().unwrap().push(vcpu.id());
        vcpu.set_secondary_state(entry, ctx)?;
        self.run_loop(mpidr, vcpu)
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
            Claim::Frozen => {
                log::warn!("CPU_ON for mpidr {mpidr:#x} ignored: snapshot in progress");
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
            let _ = ignition_hvf::vcpu_request_exit(*id);
        }
    }

    /// The shared per-vCPU run loop (primary and secondary).
    fn run_loop(self: &Arc<Self>, mpidr: u64, mut vcpu: HvfVcpu) -> Result<(), ignition_hvf::Error> {
        let vcpus: Arc<dyn Vcpus> = Arc::new(NoIrqVcpus);
        // Arm dirty-page tracking on this vCPU before it runs (every vCPU must
        // have the window set, else its in-RAM write faults are misclassified).
        // The tracker handle is cloned per thread; its bitmap is shared (Arc).
        let dirty = self.dirty.clone();
        if let Some(cfg) = &dirty {
            vcpu.set_dirty_window(cfg.base, cfg.size);
        }
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
                VcpuExit::DirtyFault(pa) => {
                    // A guest write trapped on the write-protected RAM window.
                    // Record the page, re-grant WRITE on its granule, and resume
                    // WITHOUT advancing PC so the store re-executes. The tracker
                    // bitmap is atomic, so concurrent vCPU threads need no lock.
                    if let Some(cfg) = &dirty {
                        cfg.tracker.mark(pa);
                        let page_base = pa & !((PAGE as u64) - 1);
                        ignition_hvf::vm_protect_memory(
                            page_base,
                            PAGE as u64,
                            (ignition_hvf::bindings::HV_MEMORY_READ
                                | ignition_hvf::bindings::HV_MEMORY_WRITE
                                | ignition_hvf::bindings::HV_MEMORY_EXEC)
                                as u64,
                        )?;
                    } else {
                        log::warn!("DirtyFault at {pa:#x} but dirty tracking is not armed");
                    }
                }
                VcpuExit::Shutdown => {
                    log::info!("guest requested shutdown (PSCI SYSTEM_OFF)");
                    self.request_shutdown();
                    return Ok(());
                }
                VcpuExit::Canceled => {
                    if self.snapshot_req.load(Ordering::Acquire) {
                        // Save our own registers (HVF thread-affinity) and meet
                        // every other vCPU at the barrier.
                        let st = vcpu.save_state();
                        self.collected.lock().unwrap().push((mpidr, st));
                        let bar = self
                            .snap_barrier
                            .lock()
                            .unwrap()
                            .clone()
                            .expect("snap_barrier set when snapshot_req is set");
                        // Barrier 1: a full happens-before edge — every push is
                        // visible to the leader after this returns.
                        if bar.wait().is_leader() {
                            self.run_snapshot_leader();
                        }
                        // Barrier 2: peers wait here while the leader writes;
                        // release together and resume.
                        bar.wait();
                        continue;
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

    /// Runs on the single leader thread between the two rendezvous barriers, with
    /// every other vCPU parked. Drains the collected per-vCPU states, aborts on
    /// any save failure (no torn snapshot), else invokes the handler. Always
    /// clears the snapshot flags before returning so the second barrier resumes a
    /// clean state.
    fn run_snapshot_leader(self: &Arc<Self>) {
        let mut items = std::mem::take(&mut *self.collected.lock().unwrap());
        items.sort_by_key(|(mpidr, _)| *mpidr);

        let mut checkpoints = Vec::with_capacity(items.len());
        let mut failed = None;
        for (mpidr, res) in items {
            match res {
                Ok(state) => checkpoints.push(VcpuCheckpoint { mpidr, state }),
                Err(e) => {
                    failed = Some((mpidr, e));
                    break;
                }
            }
        }

        match failed {
            Some((mpidr, e)) => {
                log::error!("snapshot aborted: vcpu {mpidr:#x} save_state failed: {e}");
            }
            None => {
                if let Some(h) = &self.snapshot_handler {
                    // A panic in the handler must not unwind the vCPU thread.
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        h(checkpoints)
                    }));
                    if r.is_err() {
                        log::error!("snapshot handler panicked; guest resumed");
                    }
                }
            }
        }

        self.snapshot_req.store(false, Ordering::Release);
        self.snapshot_active.store(false, Ordering::Relaxed);
    }

    /// Join every spawned vCPU thread, draining the registry so threads spawned
    /// mid-run are still joined. Returns the first error.
    fn join_all(&self) -> Result<(), ignition_hvf::Error> {
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

    #[test]
    fn claim_rejected_while_snapshot_active() {
        let m = mgr(4);
        m.snapshot_active.store(true, Ordering::Relaxed);
        assert_eq!(m.claim(1), Claim::Frozen);
    }
}
