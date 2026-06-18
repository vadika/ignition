// Multi-vCPU orchestration: owns the device bus, the configured MPIDR set, and a
// registry of running vCPU threads. The primary boots alone; secondaries are
// spawned lazily on PSCI CPU_ON. In-kernel hv_gic handles SGIs/IPIs and per-cpu
// vtimers, so no userspace IRQ routing is needed here.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use ignition_devices::bus::{Bus, BusDevice};
use ignition_hvf::{HvfVcpu, NoIrqVcpus, VcpuExit, Vcpus};
use crate::dirty::{DirtyTracker, PAGE};
use crate::fuzz::controller::FuzzController;
use crate::reset::ResetPoint;
use crate::snapshot::VcpuCheckpoint;

/// Host SIGINT/SIGTERM stop flag for fuzz mode. The fuzz path has no guest
/// console to take a Ctrl-C, so `boot --fuzz` installs a signal handler that
/// sets this; the fuzz loop checks it at the top of each iteration and exits
/// cleanly (flushing benchmark metrics). A plain process-global atomic keeps the
/// signal handler async-signal-safe (it only does an atomic store).
pub static FUZZ_STOP: AtomicBool = AtomicBool::new(false);

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
    /// Set by `request_checkpoint`; cleared by the checkpoint leader.
    checkpoint_req: AtomicBool,
    /// Set by `request_reset`; cleared by the reset leader.
    reset_req: AtomicBool,
    /// Shared vtimer offset for an interactive reset (Ctrl+Alt+R). Computed once
    /// by the reset leader from the reset point's primary host_counter and read
    /// by every vCPU after barrier 2 before its `restore_state`, so CNTVCT stays
    /// synchronized across cores (see `run_restored`).
    reset_vtimer_offset: AtomicU64,
    /// True for the duration of any rendezvous (snapshot, checkpoint, or reset);
    /// freezes CPU_ON and rejects a re-entrant request. Read and written only
    /// while holding the `running` lock, so it cannot race a claim.
    rendezvous_active: AtomicBool,
    /// Per-rendezvous barrier sized to the participant count, published by
    /// `request_*` and read by each vCPU thread at the rendezvous.
    snap_barrier: Mutex<Option<Arc<Barrier>>>,
    /// Each participating vCPU thread pushes `(mpidr, save_state())` here; the
    /// leader drains it after the barrier.
    collected: Mutex<Vec<(u64, Result<ignition_hvf::VcpuState, ignition_hvf::Error>)>>,
    /// Installed by the boot harness before `run`; invoked on the leader thread.
    snapshot_handler: Option<SnapshotHandler>,
    /// Installed by the boot harness before `run`; invoked on the leader thread
    /// to build and store a `ResetPoint` from the collected checkpoints.
    checkpoint_handler: Option<CheckpointHandler>,
    /// Installed by the boot harness before `run`; invoked on the leader thread
    /// to roll live RAM/GIC/device state back to the current `ResetPoint`.
    reset_handler: Option<ResetHandler>,
    /// Shared with the handler closures and read by each vCPU at a reset barrier
    /// to find its own checkpoint. `None` until seeded (on `--restore`) or set by
    /// `Ctrl-A c`.
    reset_point: Arc<Mutex<Option<ResetPoint>>>,
    /// Installed by the boot harness before `run` when `--track-dirty` is set.
    /// When present, each vCPU thread arms `set_dirty_window` before its loop,
    /// and the `DirtyFault` arm marks the page + re-grants WRITE.
    dirty: Option<DirtyConfig>,
}

/// A snapshot handler: invoked on the elected leader vCPU thread once every
/// vCPU has saved its register state. Receives the per-vCPU checkpoints and
/// performs the global capture (RAM + GIC + device records) and file write.
type SnapshotHandler = Box<dyn Fn(Vec<VcpuCheckpoint>) + Send + Sync>;

/// Builds and stores a `ResetPoint` from the vCPU checkpoints collected at the
/// barrier (clonefile pristine + capture gic/devices). Runs on the leader vCPU
/// thread with all vCPUs parked.
pub type CheckpointHandler = Box<dyn Fn(Vec<VcpuCheckpoint>) + Send + Sync>;

/// Rolls live RAM/GIC/device state back to the current `ResetPoint`. Runs on the
/// leader vCPU thread with all vCPUs parked. Per-vCPU register restore happens
/// afterward on each vCPU's own thread.
pub type ResetHandler = Box<dyn Fn() + Send + Sync>;

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
            checkpoint_req: AtomicBool::new(false),
            reset_req: AtomicBool::new(false),
            reset_vtimer_offset: AtomicU64::new(0),
            rendezvous_active: AtomicBool::new(false),
            snap_barrier: Mutex::new(None),
            collected: Mutex::new(Vec::new()),
            snapshot_handler: None,
            checkpoint_handler: None,
            reset_handler: None,
            reset_point: Arc::new(Mutex::new(None)),
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

    /// Install a checkpoint handler. MUST be called before `run`. Invoked on the
    /// leader vCPU thread once every vCPU has rendezvoused, to build and store a
    /// `ResetPoint` from the collected per-vCPU checkpoints.
    pub fn set_checkpoint_handler(self: &mut Arc<Self>, handler: CheckpointHandler) {
        let me = Arc::get_mut(self).expect("set_checkpoint_handler must be called before run");
        me.checkpoint_handler = Some(handler);
    }

    /// Install a reset handler. MUST be called before `run`. Invoked on the
    /// leader vCPU thread with all vCPUs parked to roll RAM/GIC/devices back to
    /// the current `ResetPoint`.
    pub fn set_reset_handler(self: &mut Arc<Self>, handler: ResetHandler) {
        let me = Arc::get_mut(self).expect("set_reset_handler must be called before run");
        me.reset_handler = Some(handler);
    }

    /// The shared reset point. Cloned by `boot.rs` so the handler closures and
    /// the seeding code can read/write the same `Option<ResetPoint>`.
    pub fn reset_point(&self) -> Arc<Mutex<Option<ResetPoint>>> {
        self.reset_point.clone()
    }

    /// True once a reset point exists (seeded on restore or set by `Ctrl-A c`).
    pub fn has_reset_point(&self) -> bool {
        self.reset_point.lock().unwrap().is_some()
    }

    /// Shared prologue for snapshot/checkpoint/reset. Freezes CPU_ON under the
    /// `running` lock so no claim races the latch, rejects a re-entrant request
    /// while one is still in flight, latches the participant set (the vCPUs already
    /// registered — a CPU_ON mid-spawn is the documented mid-boot exclusion), and
    /// sizes the rendezvous barrier. Returns the latched ids on success, or `None`
    /// (already unfrozen) if there is nothing to do. Caller sets its own `*_req`
    /// and broadcasts the exits via `broadcast_exit`.
    fn begin_rendezvous(&self) -> Option<Vec<u64>> {
        {
            let _running = self.running.lock().unwrap();
            if self.rendezvous_active.swap(true, Ordering::Relaxed) {
                return None;
            }
        }
        let ids: Vec<u64> = self.vcpuids.lock().unwrap().clone();
        if ids.is_empty() {
            // No vCPU has registered yet; unfreeze and bail so a later request works.
            self.rendezvous_active.store(false, Ordering::Relaxed);
            return None;
        }
        *self.snap_barrier.lock().unwrap() = Some(Arc::new(Barrier::new(ids.len())));
        Some(ids)
    }

    /// Interrupt every latched vCPU so each exits to `Canceled` and joins the
    /// rendezvous. Call after the relevant `*_req` flag is set with Release.
    fn broadcast_exit(ids: Vec<u64>) {
        for id in ids {
            let _ = ignition_hvf::vcpu_request_exit(id);
        }
    }

    /// Request a snapshot. Freezes CPU_ON, latches the participant set, sizes the
    /// rendezvous barrier, and interrupts every registered vCPU so each exits to
    /// `Canceled` and joins the rendezvous. No-op if no handler is installed.
    pub fn request_snapshot(&self) {
        if self.snapshot_handler.is_none() {
            return;
        }
        let Some(ids) = self.begin_rendezvous() else { return };
        self.collected.lock().unwrap().clear();
        self.snapshot_req.store(true, Ordering::Release);
        Self::broadcast_exit(ids);
    }

    /// Request a checkpoint. Mirrors `request_snapshot`: freezes CPU_ON, latches
    /// the participant set, sizes the rendezvous barrier, clears `collected`, and
    /// interrupts every registered vCPU so each saves its state at the barrier and
    /// the leader builds a `ResetPoint`. No-op if no checkpoint handler is installed.
    pub fn request_checkpoint(&self) {
        if self.checkpoint_handler.is_none() {
            return;
        }
        let Some(ids) = self.begin_rendezvous() else { return };
        self.collected.lock().unwrap().clear();
        self.checkpoint_req.store(true, Ordering::Release);
        Self::broadcast_exit(ids);
    }

    /// Request a reset. Freezes CPU_ON, latches the participant set, sizes the
    /// rendezvous barrier, and interrupts every registered vCPU so the leader
    /// rolls RAM/GIC/devices back and each vCPU then restores its own registers.
    /// Does NOT touch `collected`. No-op if no reset handler is installed or no
    /// reset point exists.
    pub fn request_reset(&self) {
        if self.reset_handler.is_none() || self.reset_point.lock().unwrap().is_none() {
            return;
        }
        let Some(ids) = self.begin_rendezvous() else { return };
        self.reset_req.store(true, Ordering::Release);
        Self::broadcast_exit(ids);
    }

    /// Try to claim `mpidr` for a bring-up. Idempotent guard against unknown or
    /// duplicate CPU_ON targets; inserts into `running` on success.
    fn claim(&self, mpidr: u64) -> Claim {
        if !self.mpidrs.contains(&mpidr) {
            return Claim::Unknown;
        }
        let mut running = self.running.lock().unwrap();
        if self.rendezvous_active.load(Ordering::Relaxed) {
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
        // CNTVCT is system-wide: compute ONE vtimer offset (single mach read,
        // primary's host_counter) and give every vCPU the identical value, or the
        // cores desync and the guest clock jumps to the far future (RCU stalls).
        let primary_hc = checkpoints
            .iter()
            .find(|c| c.mpidr == mpidr_for(0))
            .or_else(|| checkpoints.first())
            .map(|c| c.state.host_counter)
            .unwrap_or(0);
        let vtimer_offset = ignition_hvf::shared_vtimer_offset(primary_hc);
        for cp in checkpoints {
            let me = Arc::clone(self);
            let bar = Arc::clone(&barrier);
            let blob = Arc::clone(&gic_blob);
            let handle =
                thread::spawn(move || me.run_restored_one(cp, bar, blob, vtimer_offset));
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
        vtimer_offset: u64,
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
        vcpu.restore_state(&cp.state, vtimer_offset)?;
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

    /// Run the single-vCPU fuzz loop. Boots the primary normally; once the guest
    /// rings SNAPSHOT_ME the loop captures the snapshot and drives
    /// inject->resume->observe->reset inline on this thread (HVF thread-affine).
    // The fuzz entry points carry the boot params plus the device + controller
    // handles; splitting them into a struct would not aid clarity here.
    #[allow(clippy::too_many_arguments)]
    pub fn run_fuzz(
        self: &Arc<Self>,
        entry: u64,
        fdt_addr: u64,
        doorbell_gpa: u64,
        ctrl_base: u64,
        fuzz_dev: Arc<Mutex<ignition_devices::fuzz::FuzzDevice>>,
        controller: FuzzController,
    ) -> Result<(), ignition_hvf::Error> {
        let me = Arc::clone(self);
        let handle = thread::spawn(move || {
            let mut controller = controller;
            let mpidr = mpidr_for(0);
            me.running.lock().unwrap().insert(mpidr);
            let vcpu = HvfVcpu::new(mpidr, false)?;
            me.vcpuids.lock().unwrap().push(vcpu.id());
            vcpu.set_initial_state(entry, fdt_addr)?;
            me.fuzz_loop(vcpu, doorbell_gpa, ctrl_base, fuzz_dev, &mut controller)
        });
        self.threads.lock().unwrap().push(handle);
        self.join_all()
    }

    #[allow(clippy::too_many_arguments)]
    fn fuzz_loop(
        self: &Arc<Self>,
        mut vcpu: HvfVcpu,
        doorbell_gpa: u64,
        ctrl_base: u64,
        fuzz_dev: Arc<Mutex<ignition_devices::fuzz::FuzzDevice>>,
        controller: &mut FuzzController,
    ) -> Result<(), ignition_hvf::Error> {
        let vcpus: Arc<dyn Vcpus> = Arc::new(NoIrqVcpus);
        // Arm dirty-page tracking (ResetMode::Dirty). The window is set before the
        // guest runs; RAM is only write-protected later, at the snapshot point
        // (FuzzController::capture), so boot-time writes don't fault. With no
        // dirty config (full-copy reset) this is a no-op and DirtyFault never fires.
        let dirty = self.dirty.clone();
        if let Some(cfg) = &dirty {
            vcpu.set_dirty_window(cfg.base, cfg.size);
        }
        loop {
            if self.shutdown.load(Ordering::Acquire) || FUZZ_STOP.load(Ordering::Acquire) {
                controller.write_metrics();
                return Ok(());
            }
            match vcpu.run(vcpus.clone())? {
                VcpuExit::MmioWrite(addr, data) if addr == doorbell_gpa => {
                    let cmd = if data.len() >= 4 {
                        u32::from_le_bytes(data[..4].try_into().unwrap())
                    } else {
                        0
                    };
                    use ignition_devices::fuzz::protocol::cmd as C;
                    if cmd == C::SNAPSHOT_ME {
                        // First snapshot: advance PC past the store, capture RAM +
                        // regs, expose the first input length.
                        vcpu.advance_pc()?;
                        let len = controller.capture(&vcpu)?;
                        fuzz_dev.lock().unwrap().set_input_len(len);
                    } else if cmd == C::DONE {
                        let len = controller.on_done(&mut vcpu)?;
                        fuzz_dev.lock().unwrap().set_input_len(len);
                    } else if cmd == C::CRASH {
                        let (code, in_len) = {
                            let mut dev = fuzz_dev.lock().unwrap();
                            let mut b = [0u8; 4];
                            dev.read(ctrl_base, ignition_devices::fuzz::protocol::reg::INPUT_LEN, &mut b);
                            (dev.crash_code(), u32::from_le_bytes(b))
                        };
                        let len = controller.on_crash(&mut vcpu, code, in_len)?;
                        fuzz_dev.lock().unwrap().set_input_len(len);
                    } else {
                        log::warn!("fuzz: unknown doorbell command {cmd:#x}");
                    }
                }
                VcpuExit::MmioWrite(addr, data) => self.bus.write(addr, data),
                VcpuExit::MmioRead(addr, data) => self.bus.read(addr, data),
                VcpuExit::DirtyFault(pa) => {
                    if let Some(cfg) = &dirty {
                        cfg.tracker.mark(pa);
                        let page_base = pa & !((PAGE as u64) - 1);
                        ignition_hvf::vm_protect_memory(
                            page_base,
                            PAGE as u64,
                            (ignition_hvf::bindings::HV_MEMORY_READ
                                | ignition_hvf::bindings::HV_MEMORY_WRITE
                                | ignition_hvf::bindings::HV_MEMORY_EXEC) as u64,
                        )
                        .expect("dirty-tracking re-grant of guest page failed");
                    } else {
                        log::warn!("fuzz DirtyFault at {pa:#x} but dirty tracking is not armed");
                    }
                }
                VcpuExit::Shutdown => {
                    controller.write_metrics();
                    self.request_shutdown();
                    return Ok(());
                }
                VcpuExit::Canceled => {
                    controller.write_metrics();
                    return Ok(());
                }
                VcpuExit::WaitForEventTimeout(d) => thread::sleep(d.min(MAX_PARK)),
                VcpuExit::WaitForEvent => thread::sleep(MAX_PARK),
                VcpuExit::WaitForEventExpired | VcpuExit::VtimerActivated => {}
                other => log::debug!("fuzz: unhandled vCPU exit: {other:?}"),
            }
        }
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
                        // Re-granting WRITE on a page that is already mapped in
                        // the guest must never fail; if it does, the stage-2
                        // tables are inconsistent and resuming would loop-fault
                        // forever. Treat it as a fatal invariant violation and
                        // abort loudly rather than letting `?` silently kill this
                        // one vCPU thread while the others keep running.
                        ignition_hvf::vm_protect_memory(
                            page_base,
                            PAGE as u64,
                            (ignition_hvf::bindings::HV_MEMORY_READ
                                | ignition_hvf::bindings::HV_MEMORY_WRITE
                                | ignition_hvf::bindings::HV_MEMORY_EXEC)
                                as u64,
                        )
                        .expect("dirty-tracking re-grant of guest page failed");
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
                            self.run_collect_leader(&self.snapshot_handler, &self.snapshot_req, "snapshot");
                        }
                        // Barrier 2: peers wait here while the leader writes;
                        // release together and resume.
                        bar.wait();
                        continue;
                    }
                    if self.checkpoint_req.load(Ordering::Acquire) {
                        // Save our own registers (HVF thread-affinity) and meet
                        // every other vCPU at the barrier.
                        let st = vcpu.save_state();
                        self.collected.lock().unwrap().push((mpidr, st));
                        let bar = self
                            .snap_barrier
                            .lock()
                            .unwrap()
                            .clone()
                            .expect("snap_barrier set when checkpoint_req is set");
                        // Barrier 1: a full happens-before edge — every push is
                        // visible to the leader after this returns.
                        if bar.wait().is_leader() {
                            self.run_collect_leader(&self.checkpoint_handler, &self.checkpoint_req, "checkpoint");
                        }
                        // Barrier 2: peers wait here while the leader builds the
                        // reset point; release together and resume.
                        bar.wait();
                        continue;
                    }
                    if self.reset_req.load(Ordering::Acquire) {
                        let bar = self
                            .snap_barrier
                            .lock()
                            .unwrap()
                            .clone()
                            .expect("snap_barrier set when reset_req is set");
                        // Barrier 1: all parked before the leader touches
                        // RAM/GIC/devices.
                        if bar.wait().is_leader() {
                            self.run_reset_leader();
                        }
                        // Barrier 2: rollback complete; each vCPU now restores its
                        // own registers from its checkpoint in the reset point.
                        bar.wait();
                        let off = self.reset_vtimer_offset.load(Ordering::Acquire);
                        if let Some(rp) = self.reset_point.lock().unwrap().as_ref()
                            && let Some(cp) = rp.vcpus.iter().find(|c| c.mpidr == mpidr)
                            && let Err(e) = vcpu.restore_state(&cp.state, off)
                        {
                            log::error!("reset: vcpu {mpidr:#x} restore_state failed: {e}");
                        }
                        // Force the vtimer UNMASKED after reset, overriding the
                        // checkpoint's mask. A checkpoint marked while the guest was
                        // mid-timer-handling captures vtimer_mask=true; the rolled-back
                        // code never reaches its re-arm, so a restored mask leaves the
                        // vtimer dead on this core (RCU stall). Unmasking lets the timer
                        // fire so the guest re-arms normally; harmless when it was
                        // already unmasked (idle checkpoint).
                        let _ = ignition_hvf::vcpu_set_vtimer_mask(vcpu.id(), false);
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
    /// any save failure (no torn collection), else invokes `handler`. Always
    /// clears `req` (and `rendezvous_active`) before returning so the second
    /// barrier resumes a clean state. `what` names the operation in log lines
    /// ("snapshot" or "checkpoint").
    fn run_collect_leader(
        self: &Arc<Self>,
        handler: &Option<SnapshotHandler>,
        req: &AtomicBool,
        what: &str,
    ) {
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
                log::error!("{what} aborted: vcpu {mpidr:#x} save_state failed: {e}");
            }
            None => {
                if let Some(h) = handler {
                    // A panic in the handler must not unwind the vCPU thread.
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        h(checkpoints)
                    }));
                    if r.is_err() {
                        log::error!("{what} handler panicked; guest resumed");
                    }
                }
            }
        }

        req.store(false, Ordering::Release);
        self.rendezvous_active.store(false, Ordering::Relaxed);
    }

    /// Runs on the single leader thread at the first reset barrier, with every
    /// other vCPU parked. Invokes the reset handler to roll RAM/GIC/devices back
    /// to the current `ResetPoint`. Per-vCPU register restore happens afterward
    /// on each vCPU's own thread (past the second barrier). Always clears the
    /// reset flags before returning.
    fn run_reset_leader(self: &Arc<Self>) {
        if let Some(h) = &self.reset_handler {
            // A panic in the handler must not unwind the vCPU thread.
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(h));
            if r.is_err() {
                log::error!("reset handler panicked; guest resumed (state may be inconsistent)");
            }
        }

        // Shared vtimer offset for all vCPUs (see run_restored); read by each vCPU
        // after barrier 2 before its restore_state. The leader runs this between
        // barrier 1 and barrier 2, so the Release store is visible (via the barrier
        // happens-before edge and the Acquire load) to every vCPU when it restores.
        let primary_hc = self
            .reset_point
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|rp| {
                rp.vcpus
                    .iter()
                    .find(|c| c.mpidr == mpidr_for(0))
                    .or_else(|| rp.vcpus.first())
            })
            .map(|c| c.state.host_counter)
            .unwrap_or(0);
        self.reset_vtimer_offset
            .store(ignition_hvf::shared_vtimer_offset(primary_hc), Ordering::Release);

        self.reset_req.store(false, Ordering::Release);
        self.rendezvous_active.store(false, Ordering::Relaxed);
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
    fn claim_rejected_while_rendezvous_active() {
        let m = mgr(4);
        m.rendezvous_active.store(true, Ordering::Relaxed);
        assert_eq!(m.claim(1), Claim::Frozen);
    }

    #[test]
    fn request_checkpoint_without_handler_is_noop() {
        let m = mgr(4);
        m.request_checkpoint();
        assert!(!m.checkpoint_req.load(Ordering::Relaxed));
        assert!(!m.rendezvous_active.load(Ordering::Relaxed));
    }

    #[test]
    fn request_reset_without_handler_is_noop() {
        let m = mgr(4);
        m.request_reset();
        assert!(!m.reset_req.load(Ordering::Relaxed));
        assert!(!m.rendezvous_active.load(Ordering::Relaxed));
    }
}
