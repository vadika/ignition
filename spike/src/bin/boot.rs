// Boot harness: load a real aarch64 Linux kernel + device tree into guest RAM,
// create the in-kernel GIC, and run a vCPU so the kernel's earlycon output
// reaches our 16550 on host stdout.
//
// MUST be codesigned with the hypervisor entitlement before running:
//   cargo build -p ignition-spike --bin boot
//   scripts/sign.sh target/debug/boot
//   target/debug/boot <kernel-Image> [rootfs-disk]
//
// Our diagnostics go to stderr; the guest console goes to stdout, so the kernel's
// earlycon lines are cleanly separable.

use std::io::{self, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::{env, fs, process};

use ignition_arch::aarch64::fdt::{self, FdtConfig};
use ignition_arch::aarch64::{kernel, layout};
use ignition_arch::aarch64::fdt::{FdtDevice, FuzzDev};
use ignition_devices::boot_timer::BootTimer;
use ignition_devices::fuzz::FuzzDevice;
use ignition_devices::fuzz::protocol;
use ignition_devices::rtc::Pl031;
use ignition_devices::serial::Serial;
use ignition_devices::virtio::balloon::Balloon;
use ignition_devices::virtio::blk::VirtioBlk;
use ignition_devices::virtio::guest_ram::{DirtySink, GuestRam};
use ignition_devices::virtio::mmio::VirtioMmio;
use ignition_devices::virtio::net::VirtioNet;
use ignition_devices::virtio::rng::VirtioRng;
use ignition_devices::virtio::vsock::VsockDevice;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use ignition_hvf::gic::HvfGicV3;
use ignition_vmm::device_manager::{DeviceManager, DeviceRecord, FrozenDevices};
use ignition_vmm::dirty::DirtyTracker;
use ignition_vmm::fuzz::controller::FuzzController;
use ignition_vmm::fuzz::controller::ResetMode;
use ignition_vmm::names;
use ignition_vmm::snapshot::{self, SnapshotManifest, VcpuCheckpoint, VmConfig, VmSnapshot};
use ignition_vmm::vstate::vcpu_manager::{mpidr_for, DirtyConfig, VcpuManager};
use ignition_vmm::vstate::hvf_vm::Vm;
use ignition_hvf::bindings::{HV_MEMORY_EXEC, HV_MEMORY_READ};

mod display_sink;

/// Host console escape key: Ctrl-A (0x01).
const CTRL_A: u8 = 0x01;

/// State of the host-side escape sequence. Ctrl-A then `x` quits the harness;
/// Ctrl-A then anything else forwards a literal Ctrl-A plus that byte.
enum EscState {
    Normal,
    SawCtrlA,
}

/// What the reader thread should do with one input byte.
enum Action {
    /// Forward `Action::Forward(buf, len)` — `buf[..len]` — to the guest RX
    /// FIFO. `buf[1]` is unused (zero) when `len == 1`.
    Forward([u8; 2], usize),
    /// Ctrl-A consumed; awaiting the next byte. Forward nothing yet.
    Pending,
    /// Quit the harness.
    Quit,
    /// Ctrl-A s: request a snapshot.
    Snapshot,
    /// Ctrl-A b: toggle the memory-balloon target.
    Balloon,
    /// Ctrl-A c: mark a reset point (checkpoint current state).
    Checkpoint,
    /// Ctrl-A r: roll back to the current reset point.
    Reset,
}

/// Advance the escape state machine by one input byte. The caller forwards
/// EXACTLY what the returned `Action` says and nothing else:
/// `Forward(buf, len)` -> write `buf[..len]` to the guest; `Pending` -> write
/// nothing (the Ctrl-A was consumed and may be re-emitted by a later call);
/// `Quit` -> stop. The input byte is never forwarded except via the returned
/// `Action`, so a Ctrl-A held in `SawCtrlA` is emitted only if the next byte
/// is not an escape.
fn step(state: &mut EscState, byte: u8) -> Action {
    match state {
        EscState::Normal if byte == CTRL_A => {
            *state = EscState::SawCtrlA;
            Action::Pending
        }
        EscState::Normal => Action::Forward([byte, 0], 1),
        EscState::SawCtrlA => {
            *state = EscState::Normal;
            match byte {
                b'x' => Action::Quit,
                b's' => Action::Snapshot,
                b'b' => Action::Balloon,
                b'c' => Action::Checkpoint,
                b'r' => Action::Reset,
                // Ctrl-A Ctrl-A sends a single literal Ctrl-A to the guest.
                CTRL_A => Action::Forward([CTRL_A, 0], 1),
                // Ctrl-A was not an escape: send it literally, then this byte.
                _ => Action::Forward([CTRL_A, byte], 2),
            }
        }
    }
}

/// Restore previously-saved terminal settings on stdin. No-op if `saved` is
/// `None` (stdin was not a TTY).
fn restore_termios(saved: &Option<libc::termios>) {
    if let Some(t) = saved {
        // SAFETY: `t` is a termios we read from this same fd; tcsetattr on
        // STDIN_FILENO with a valid pointer is sound.
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, t);
        }
    }
}

/// Puts stdin into raw mode for the lifetime of the guard and restores the
/// original settings on drop. No-op for non-TTY stdin (pipes / CI), so
/// output-only runs are unaffected.
struct TermiosGuard {
    original: Option<libc::termios>,
}

impl TermiosGuard {
    fn new() -> Self {
        // SAFETY: STDIN_FILENO is a valid fd; termios is plain-old-data; all
        // libc calls below receive valid pointers.
        unsafe {
            if libc::isatty(libc::STDIN_FILENO) != 1 {
                return Self { original: None };
            }
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut t) != 0 {
                return Self { original: None };
            }
            let original = t;
            // Raw: drop canonical mode, echo, signal chars (so Ctrl-C reaches the
            // guest), and extended input; drop XON/XOFF and CR->NL translation so
            // every keystroke passes through verbatim.
            t.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN);
            t.c_iflag &= !(libc::IXON | libc::ICRNL);
            t.c_cc[libc::VMIN] = 1;
            t.c_cc[libc::VTIME] = 0;
            // TCSAFLUSH: apply on entry after draining any buffered type-ahead.
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &t) != 0 {
                return Self { original: None };
            }
            Self { original: Some(original) }
        }
    }

    /// A copy of the saved termios for the reader thread to restore before
    /// `process::exit` (which skips `Drop`).
    fn saved(&self) -> Option<libc::termios> {
        self.original
    }
}

impl Drop for TermiosGuard {
    fn drop(&mut self) {
        restore_termios(&self.original);
    }
}

/// Resources the checkpoint/reset handlers capture. `mem_file` is `Some` in
/// restore mode (instance memory.bin -> clonefile pristine) and `None` for a
/// fresh boot (MAP_ANON -> owned copy).
struct ResetWiring {
    host_usize: usize,
    ram_size: u64,
    mem_file: Option<PathBuf>,
    inst_dir: PathBuf,
    gic: Arc<HvfGicV3>,
    frozen: Arc<FrozenDevices>,
    dirty: Option<DirtyTracker>,
    rx_stop: Option<Arc<AtomicBool>>,
    net_mmio: Option<Arc<Mutex<VirtioMmio>>>,
    gpu: Option<Arc<Mutex<VirtioMmio>>>,
}

fn install_reset_handlers(manager: &mut Arc<VcpuManager>, w: ResetWiring) {
    let point = manager.reset_point();

    // --- checkpoint: capture current RAM + gic + devices into a new ResetPoint ---
    {
        let point = point.clone();
        let host_usize = w.host_usize;
        let ram_size = w.ram_size;
        let mem_file = w.mem_file.clone();
        let inst_dir = w.inst_dir.clone();
        let gic = w.gic.clone();
        let frozen = w.frozen.clone();
        let dirty = w.dirty.clone();
        let rx_stop = w.rx_stop.clone();
        manager.set_checkpoint_handler(Box::new(move |checkpoints| {
            // vCPUs parked. Quiesce the vmnet RX feeder during the RAM clone.
            if let Some(stop) = &rx_stop { stop.store(true, Ordering::Release); }
            let live: &[u8] = unsafe {
                std::slice::from_raw_parts(host_usize as *const u8, ram_size as usize)
            };
            let pristine = match &mem_file {
                Some(src) => {
                    // MAP_SHARED -> flush so the clonefile sees current RAM.
                    let rc = unsafe { libc::msync(host_usize as *mut libc::c_void, ram_size as usize, libc::MS_SYNC) };
                    if rc != 0 {
                        eprintln!("[checkpoint] msync failed ({}); pristine may be slightly stale", std::io::Error::last_os_error());
                    }
                    let dst = inst_dir.join("pristine.bin");
                    let _ = std::fs::remove_file(&dst);
                    match ignition_vmm::reset::PristineRam::from_clone(src, &dst, ram_size as usize) {
                        Ok(p) => p,
                        Err(e) => {
                            eprintln!("[checkpoint] clonefile pristine failed ({e}); falling back to copy");
                            ignition_vmm::reset::PristineRam::from_copy(live)
                        }
                    }
                }
                None => ignition_vmm::reset::PristineRam::from_copy(live),
            };
            let gic_blob = match gic.save_state() {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("[checkpoint] gic save_state failed: {e}; reset point not updated");
                    if let Some(stop) = &rx_stop { stop.store(false, Ordering::Release); }
                    return;
                }
            };
            let devices = frozen.save();
            // Discard dirty pages accumulated up to now and re-arm, so the next
            // reset rolls back only changes AFTER this checkpoint. (Interleaving a
            // Ctrl-A s diff-snapshot between checkpoint and reset is out of scope.)
            if let Some(t) = &dirty {
                let _ = t.drain();
                let _ = ignition_hvf::vm_protect_memory(
                    layout::RAM_BASE, ram_size,
                    (HV_MEMORY_READ | HV_MEMORY_EXEC) as u64,
                );
            }
            *point.lock().unwrap() = Some(ignition_vmm::reset::ResetPoint {
                pristine, vcpus: checkpoints, gic_blob, devices,
            });
            if let Some(stop) = &rx_stop { stop.store(false, Ordering::Release); }
        }));
    }

    // --- reset: roll live RAM/GIC/devices back to the current ResetPoint ---
    {
        let point = point.clone();
        let host_usize = w.host_usize;
        let ram_size = w.ram_size;
        let frozen = w.frozen.clone();
        let dirty = w.dirty.clone();
        let rx_stop = w.rx_stop.clone();
        let net_mmio = w.net_mmio.clone();
        let gpu = w.gpu.clone();
        manager.set_reset_handler(Box::new(move || {
            let guard = point.lock().unwrap();
            let Some(rp) = guard.as_ref() else { return; };
            let t_reset = std::time::Instant::now();
            // Quiesce the vmnet RX feeder, then DRAIN it: the feeder holds the net
            // device lock across its stop-check + inject, so acquiring that lock
            // once is a barrier guaranteeing no inject is in-flight or can begin
            // before we roll back RAM. Without this the feeder advances the device
            // avail cursor past the rolled-back ring -> "virtio_net ... not a head".
            if let Some(stop) = &rx_stop { stop.store(true, Ordering::Release); }
            if let Some(net) = &net_mmio {
                drop(net.lock().unwrap_or_else(|p| p.into_inner()));
            }
            let live: &mut [u8] = unsafe {
                std::slice::from_raw_parts_mut(host_usize as *mut u8, ram_size as usize)
            };
            // Dirty-only rollback when a tracker is armed: device DMA writes are now
            // marked too (GuestRam DirtySink), so the drained set is complete. Full
            // copy only when there is no tracker.
            match &dirty {
                Some(t) => {
                    let pages = t.drain();
                    ignition_vmm::reset::rollback_pages(rp.pristine.as_slice(), live, &pages, ignition_vmm::dirty::PAGE);
                    let _ = ignition_hvf::vm_protect_memory(
                        layout::RAM_BASE, ram_size,
                        (HV_MEMORY_READ | HV_MEMORY_EXEC) as u64,
                    );
                }
                None => ignition_vmm::reset::rollback_full(rp.pristine.as_slice(), live),
            }
            // Deliberately do NOT re-restore the GIC distributor/redistributor here.
            // hv_gic_set_state mid-run (after the GIC has been delivering interrupts)
            // does not re-arm delivery: the vtimer PPI stops firing (RCU stalls) and
            // device used-ring IRQs never arrive (the GPU fence wait hangs -> black
            // screen). The guest does not reprogram the GIC after boot, so the live
            // distributor/redistributor already match the checkpoint; leaving them
            // untouched keeps interrupt delivery alive. Each vCPU still restores its
            // own ICC (CPU-interface) state via restore_state at the reset barrier.
            // rp.gic_blob stays captured for the disk-snapshot path; unused here.
            frozen.restore(&rp.devices);
            if let Some(gpu) = &gpu { gpu.lock().unwrap().present_scanout(); }
            // The synchronous in-place reset (RAM rollback + device restore + repaint)
            // is the "hot restore" snap-back; the net reconnect below is async (~2s).
            eprintln!("[reset] Reset-time = {} us (snap-back; net reconnect async)", t_reset.elapsed().as_micros());
            // Net: restoring the virtio-net device cursor under a live, actively-
            // receiving NIC does not cleanly resync (mergeable-buffer/feature layout,
            // the vmnet feeder's buffered frames, and device-side RX state are not
            // captured) -> "bad gso" / "not a head" after reset. Instead reuse the
            // proven --restore path: bounce the carrier so the guest's netwatch
            // poller rebinds virtio_net (a full guest-side queue tear-down + re-init
            // + re-DHCP), which discards whatever did not round-trip. Keep the RX
            // feeder STOPPED until the link is back up so no frame is injected into
            // torn-down queues; re-enable it shortly after link-up.
            match &net_mmio {
                Some(net) => {
                    net.lock().unwrap_or_else(|p| p.into_inner()).net_set_link(false);
                    let net = net.clone();
                    let rx_stop = rx_stop.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(1500));
                        net.lock().unwrap_or_else(|p| p.into_inner()).net_set_link(true);
                        std::thread::sleep(std::time::Duration::from_millis(500));
                        if let Some(stop) = &rx_stop { stop.store(false, Ordering::Release); }
                    });
                }
                None => {
                    if let Some(stop) = &rx_stop { stop.store(false, Ordering::Release); }
                }
            }
        }));
    }
}

/// Spawn a thread that reads host stdin one byte at a time, runs the escape
/// state machine, and feeds forwarded bytes into the serial's RX FIFO. On
/// Ctrl-A x it restores the terminal and exits the process. On Ctrl-A s it
/// requests a snapshot via the manager. On EOF/error it stops reading and
/// leaves the guest running.
fn spawn_stdin_reader(
    serial: Arc<Mutex<Serial<FlushWriter>>>,
    saved_termios: Option<libc::termios>,
    manager: Arc<ignition_vmm::vstate::vcpu_manager::VcpuManager>,
    balloon_target: Arc<AtomicU32>,
    balloon: Arc<Mutex<ignition_devices::virtio::mmio::VirtioMmio>>,
) {
    // Detached: the reader lives for the process lifetime.
    std::thread::spawn(move || {
        let mut state = EscState::Normal;
        let mut byte = [0u8; 1];
        loop {
            // SAFETY: reading 1 byte from STDIN_FILENO into a stack buffer.
            let n = unsafe {
                libc::read(libc::STDIN_FILENO, byte.as_mut_ptr() as *mut libc::c_void, 1)
            };
            if n < 0 {
                let e = std::io::Error::last_os_error();
                // EINTR (e.g. SIGWINCH on resize): retry the read.
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return; // real error: stop reading; the guest keeps running.
            }
            if n == 0 {
                return; // EOF on stdin.
            }
            match step(&mut state, byte[0]) {
                Action::Pending => {}
                Action::Forward(buf, len) => {
                    if let Err(e) = serial.lock().unwrap().enqueue(&buf[..len]) {
                        log::warn!("serial RX dropped byte: {e}");
                    }
                }
                Action::Quit => {
                    // process::exit skips destructors: an in-flight virtio-blk
                    // write on the vCPU thread may be truncated. Acceptable for
                    // this spike harness.
                    restore_termios(&saved_termios);
                    eprintln!("\n[console detached]");
                    process::exit(0);
                }
                Action::Snapshot => {
                    eprintln!("\n[snapshot requested]");
                    manager.request_snapshot();
                }
                Action::Checkpoint => {
                    eprintln!("\n[reset point marked]");
                    manager.request_checkpoint();
                }
                Action::Reset => {
                    if manager.has_reset_point() {
                        eprintln!("\n[reset to checkpoint]");
                        manager.request_reset();
                    } else {
                        eprintln!("\nreset: no checkpoint - press Ctrl-A c first");
                    }
                }
                Action::Balloon => {
                    const BALLOON_PAGES: u32 = 64 * 256; // 64 MiB in 4 KiB pages
                    let next = if balloon_target.load(Ordering::Relaxed) == 0 { BALLOON_PAGES } else { 0 };
                    // Release so the vCPU thread's Acquire load in config_read sees
                    // the new target before it services the config-change interrupt.
                    balloon_target.store(next, Ordering::Release);
                    balloon.lock().unwrap().signal_config_change();
                    eprintln!("\n[balloon target -> {} MiB]", next / 256);
                }
            }
        }
    });
}

/// Apply the Seatbelt sandbox, or exit. Fail-closed: an apply error terminates
/// the process (a security gate must not silently degrade open). `--no-sandbox`
/// is the one explicit, loudly-logged way to run unconfined.
fn apply_or_exit(paths: &ignition_sandbox::SandboxPaths, no_sandbox: bool) {
    if no_sandbox {
        eprintln!("WARN: sandbox disabled (--no-sandbox) — VMM runs unconfined");
        return;
    }
    if let Err(e) = ignition_sandbox::apply(paths) {
        eprintln!("FATAL: failed to apply sandbox: {e}");
        process::exit(1);
    }
    eprintln!("sandbox: applied (targeted-deny v1)");
}

/// Poll the vsock device's host connection fds and drive RX. A 200 ms timeout also
/// re-checks for newly-connected fds (TX adds connections between ticks).
fn spawn_vsock_reactor(
    vsock: Arc<Mutex<ignition_devices::virtio::mmio::VirtioMmio>>,
    uds_base: Option<PathBuf>,
) {
    use std::os::unix::io::{AsRawFd, RawFd};
    use std::os::unix::net::UnixListener;

    // Bind the control listener ({uds} itself) for host->guest (E2). Per-port
    // paths {uds}_{port} remain the E1 guest->host listeners (host side).
    let listener: Option<UnixListener> = uds_base.and_then(|base| {
        let _ = std::fs::remove_file(&base); // clear a stale socket
        match UnixListener::bind(&base) {
            Ok(l) => {
                l.set_nonblocking(true).ok();
                Some(l)
            }
            Err(e) => {
                eprintln!("vsock: control listener bind {base:?} failed: {e}");
                None
            }
        }
    });
    let listener_fd: Option<RawFd> = listener.as_ref().map(|l| l.as_raw_fd());

    std::thread::spawn(move || loop {
        let mut fds: Vec<RawFd> = { vsock.lock().unwrap().vsock_poll_set() };
        if let Some(lfd) = listener_fd {
            fds.push(lfd);
        }
        if fds.is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(200));
        } else {
            // POLLIN only: idle sockets are ~always writable, so POLLOUT would
            // busy-loop. Buffered guest->host tx flushes each tick in service().
            let mut pfds: Vec<libc::pollfd> = fds
                .iter()
                .map(|&fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 })
                .collect();
            unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, 200) };
        }
        // Accept any new control clients (non-blocking) and hand them to the device.
        if let Some(l) = &listener {
            loop {
                match l.accept() {
                    Ok((stream, _)) => {
                        stream.set_nonblocking(true).ok();
                        vsock.lock().unwrap().vsock_accept_control(stream);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
        }
        vsock.lock().unwrap().poll_vsock_rx();
    });
}

/// Unbuffered stdout sink for the guest console: writes each byte straight
/// through and flushes, so a newline-less prompt is visible immediately.
#[derive(Default)]
struct FlushWriter;
impl Write for FlushWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = io::stdout().write(buf)?;
        io::stdout().flush()?;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        io::stdout().flush()
    }
}

/// The known set of snapshot-able device ids (boot_timer is add_fixed: no record).
const KNOWN_DEVICE_IDS: &[&str] = &[
    "serial", "virtio-blk", "virtio-rng", "rtc", "virtio-balloon", "vsock", "virtio-net",
    "virtio-gpu", "virtio-keyboard", "virtio-tablet",
];

/// Fail if a snapshot record names a device this binary cannot rebuild.
fn check_known_ids(records: &[DeviceRecord]) -> io::Result<()> {
    for rec in records {
        if !KNOWN_DEVICE_IDS.contains(&rec.id.as_str()) {
            return Err(io::Error::other(format!("unknown device id in snapshot: {}", rec.id)));
        }
    }
    Ok(())
}

/// Whether we are wiring a fresh boot or restoring from a record set.
enum Mode<'a> {
    Boot,
    Restore(&'a [DeviceRecord]),
}

/// Inputs the device builders read, plus the typed handles they stash for the
/// console reader / Ctrl-A b / vsock reactor.
struct DeviceContext {
    host: *mut u8,
    ram_size: u64,
    disk: Option<PathBuf>,      // boot: original rootfs; restore: private instance copy
    vsock_uds: Option<PathBuf>,
    net: bool,                  // boot-only; never set on restore
    // outputs
    serial: Option<Arc<Mutex<Serial<FlushWriter>>>>,
    balloon_target: Option<Arc<AtomicU32>>,
    balloon: Option<Arc<Mutex<VirtioMmio>>>,
    vsock_mmio: Option<Arc<Mutex<VirtioMmio>>>,
    net_mmio: Option<Arc<Mutex<VirtioMmio>>>,
    rx_stop: Option<std::sync::Arc<AtomicBool>>, // set when a net feeder is running
    /// Display sink for the virtio-gpu device (Some only in --gui boot). Taken by
    /// the gpu builder; None means no virtio-gpu device is added.
    display_sink: Option<Box<dyn ignition_devices::display::DisplaySink>>,
    /// virtio-input device handles (Some only in --gui boot), kept for the event loop.
    keyboard_mmio: Option<Arc<Mutex<VirtioMmio>>>,
    tablet_mmio: Option<Arc<Mutex<VirtioMmio>>>,
    /// virtio-gpu handle (Some when a GPU device was wired), used to repaint the
    /// scanout once after a GUI restore.
    gpu_mmio: Option<Arc<Mutex<VirtioMmio>>>,
    /// When set (under --track-dirty), every device GuestRam reports its writes
    /// here so device DMA is captured by dirty tracking. None disables marking.
    dirty: Option<std::sync::Arc<dyn DirtySink>>,
}

impl DeviceContext {
    fn guest_ram(&self) -> GuestRam {
        GuestRam::with_dirty(self.host, self.ram_size as usize, layout::RAM_BASE, self.dirty.clone())
    }
}

/// Place a device once for whichever mode we're in: fresh `add` (boot, new
/// window/SPI) or `add_restored` (restore, saved window/SPI + state applied).
/// In restore mode a device with no matching record is skipped (returns None).
fn place<D, F>(
    mgr: &mut DeviceManager,
    mode: &Mode,
    id: &str,
    window: u64,
    build: F,
) -> io::Result<Option<Arc<Mutex<D>>>>
where
    D: ignition_devices::device::MmioDevice + 'static,
    F: FnOnce(Arc<dyn ignition_devices::virtio::IrqLine>) -> D,
{
    match mode {
        Mode::Boot => Ok(Some(mgr.add(window, build).map_err(io::Error::other)?)),
        Mode::Restore(recs) => match recs.iter().find(|r| r.id == id) {
            Some(rec) => Ok(Some(mgr.add_restored(rec, build).map_err(io::Error::other)?)),
            None => Ok(None),
        },
    }
}

/// The vmnet RX feeder injects a frame only when not quiesced for a snapshot.
fn rx_should_inject(stop_rx: &std::sync::Arc<AtomicBool>) -> bool {
    !stop_rx.load(Ordering::Acquire)
}

/// The single device-wiring site. Lists every snapshot-able device once; both
/// fresh boot and restore call it. Fills `ctx`'s output handles. boot_timer is
/// wired separately by the caller (add_fixed: no record/FDT/state).
fn setup_devices(mgr: &mut DeviceManager, ctx: &mut DeviceContext, mode: Mode) -> io::Result<()> {
    if let Mode::Restore(recs) = &mode {
        check_known_ids(recs)?;
    }

    // Serial — always first (its base matches the earlycon address in the cmdline).
    if let Some(s) = place(mgr, &mode, "serial", layout::MMIO_WINDOW,
        |irq| Serial::with_irq(FlushWriter, irq))? {
        ctx.serial = Some(s);
    }

    // PL031 RTC — always-on; ignores irq (time-only).
    place::<Pl031, _>(mgr, &mode, "rtc", layout::MMIO_WINDOW, |_irq| Pl031::new())?;

    // virtio-rng — always-on, stateless.
    {
        let mem = ctx.guest_ram();
        place::<VirtioMmio, _>(mgr, &mode, "virtio-rng", layout::MMIO_WINDOW,
            move |irq| VirtioMmio::new("virtio-rng", Box::new(VirtioRng::new()), mem, irq))?;
    }

    // virtio-balloon — always-on; the shared target survives restore via its state.
    {
        let mem = ctx.guest_ram();
        let (balloon_dev, target) = Balloon::new();
        if let Some(h) = place(mgr, &mode, "virtio-balloon", layout::MMIO_WINDOW,
            move |irq| VirtioMmio::new("virtio-balloon", Box::new(balloon_dev), mem, irq))? {
            ctx.balloon_target = Some(target);
            ctx.balloon = Some(h);
        }
    }

    // virtio-blk — present iff a disk source was provided (boot) or a record exists (restore).
    if let Some(disk) = ctx.disk.clone() {
        let file = fs::OpenOptions::new().read(true).write(true).open(&disk)
            .map_err(|e| io::Error::other(format!("open disk {}: {e}", disk.display())))?;
        let blk = VirtioBlk::new(file).map_err(|e| io::Error::other(format!("VirtioBlk::new: {e}")))?;
        let mem = ctx.guest_ram();
        place::<VirtioMmio, _>(mgr, &mode, "virtio-blk", layout::MMIO_WINDOW,
            move |irq| VirtioMmio::new("virtio-blk", Box::new(blk), mem, irq))?;
    }

    // virtio-net — boot iff --net; restore iff a record exists. A fresh vmnet
    // interface each time (new MAC), so clones get distinct MAC + DHCP lease.
    let want_net = match &mode {
        Mode::Boot => ctx.net,
        Mode::Restore(recs) => recs.iter().any(|r| r.id == "virtio-net"),
    };
    if want_net {
        let (backend, rx) = ignition_vmnet::VmnetBackend::start()
            .map_err(|e| io::Error::other(format!("vmnet start failed (need sudo for --net): {e}")))?;
        let mem = ctx.guest_ram();
        let net_dev = VirtioNet::new(backend);
        if let Some(h) = place(mgr, &mode, "virtio-net", layout::MMIO_WINDOW,
            move |irq| VirtioMmio::new("virtio-net", Box::new(net_dev), mem, irq))? {
            let stop_rx = std::sync::Arc::new(AtomicBool::new(false));
            let h2 = h.clone();
            let stop2 = stop_rx.clone();
            std::thread::spawn(move || {
                for frame in rx {
                    // Hold the device lock across the stop-check + inject so the
                    // snapshot handler's drain-lock is a true barrier: once it sets
                    // stop=true and acquires this lock once, no inject can be
                    // in-flight or begin afterward.
                    let mut dev = h2.lock().unwrap();
                    if rx_should_inject(&stop2) {
                        dev.inject_rx(&frame);
                    }
                }
            });
            ctx.rx_stop = Some(stop_rx);
            ctx.net_mmio = Some(h);
        }
    }

    // virtio-vsock — present iff a uds base was provided (boot) or a record exists (restore).
    let want_vsock = match &mode {
        Mode::Boot => ctx.vsock_uds.is_some(),
        Mode::Restore(recs) => recs.iter().any(|r| r.id == "vsock"),
    };
    if want_vsock {
        let uds = ctx.vsock_uds.clone()
            .unwrap_or_else(|| std::env::temp_dir().join("ignition-vsock"));
        let mem = ctx.guest_ram();
        let vsock_dev = VsockDevice::new(uds);
        if let Some(h) = place(mgr, &mode, "vsock", layout::MMIO_WINDOW,
            move |irq| VirtioMmio::new("vsock", Box::new(vsock_dev), mem, irq))? {
            ctx.vsock_mmio = Some(h);
        }
    }

    // virtio-gpu + virtio-input — wired under --gui boot (a sink was provided) or
    // whenever a restore record exists (a GUI snapshot). In a headless restore
    // (record present, no sink) the GPU gets a NoopSink so its state restores
    // consistently while presented frames are discarded.
    let want_gpu = match &mode {
        Mode::Boot => ctx.display_sink.is_some(),
        Mode::Restore(recs) => recs.iter().any(|r| r.id == "virtio-gpu"),
    };
    if want_gpu {
        let sink = ctx
            .display_sink
            .take()
            .unwrap_or_else(|| Box::new(ignition_devices::display::NoopSink));
        let mem = ctx.guest_ram();
        if let Some(h) = place::<VirtioMmio, _>(mgr, &mode, "virtio-gpu", layout::MMIO_WINDOW,
            move |irq| VirtioMmio::new(
                "virtio-gpu",
                Box::new(ignition_devices::virtio::gpu::VirtioGpu::new(1280, 800, sink)),
                mem,
                irq,
            ))? {
            ctx.gpu_mmio = Some(h);
        }
        let mem_kbd = ctx.guest_ram();
        if let Some(h) = place::<VirtioMmio, _>(mgr, &mode, "virtio-keyboard", layout::MMIO_WINDOW,
            move |irq| VirtioMmio::new(
                "virtio-keyboard",
                Box::new(ignition_devices::virtio::input::VirtioInput::keyboard()),
                mem_kbd, irq))? {
            ctx.keyboard_mmio = Some(h);
        }
        let mem_tab = ctx.guest_ram();
        if let Some(h) = place::<VirtioMmio, _>(mgr, &mode, "virtio-tablet", layout::MMIO_WINDOW,
            move |irq| VirtioMmio::new(
                "virtio-tablet",
                Box::new(ignition_devices::virtio::input::VirtioInput::tablet(1280, 800)),
                mem_tab, irq))? {
            ctx.tablet_mmio = Some(h);
        }
    }

    Ok(())
}

/// Write a named base snapshot into `<store>/snapshots/<write_name>/`, plus its
/// manifest, and print the resolved name. Shared by the boot and restore handlers.
#[allow(clippy::too_many_arguments)]
fn write_named_snapshot(
    store: &Path,
    write_name: &str,
    ram: &[u8],
    gic_blob: &[u8],
    disk_src: &Path,
    checkpoints: Vec<VcpuCheckpoint>,
    devices: Vec<DeviceRecord>,
    mem_size: u64,
) -> io::Result<()> {
    let base = snapshot::base_dir(store, write_name);
    let config = VmConfig { mem_size, vcpu_count: checkpoints.len() as u64 };
    let vcpu_count = config.vcpu_count;
    let snap = VmSnapshot::new(config, checkpoints, devices);
    let t0 = std::time::Instant::now();
    snapshot::write_snapshot(&base, &snap, ram, gic_blob, disk_src)?;
    let manifest = SnapshotManifest::new_full(write_name.to_string(), mem_size, vcpu_count);
    snapshot::write_manifest(&base, &manifest)?;
    eprintln!("Snapshot-write-time = {} ms", t0.elapsed().as_millis());
    eprintln!("[snapshot] full '{write_name}' written to {}", base.display());
    Ok(())
}

/// Write a Diff layer into `<store>/snapshots/<write_name>/`: full GIC / vmstate /
/// disk (clonefile of the live `disk_src`) plus only the drained dirty pages for
/// memory (packed `memory.bin` + `dirty.idx`), with a `new_diff` manifest pointing
/// at `parent`. Shares the GIC / vmstate / disk write with the Full path; only the
/// memory write and manifest constructor differ.
#[allow(clippy::too_many_arguments)]
fn write_named_diff(
    store: &Path,
    write_name: &str,
    parent: &str,
    ram: &[u8],
    dirty: &[u64],
    gic_blob: &[u8],
    disk_src: &Path,
    checkpoints: Vec<VcpuCheckpoint>,
    devices: Vec<DeviceRecord>,
    mem_size: u64,
) -> io::Result<()> {
    let base = snapshot::base_dir(store, write_name);
    let config = VmConfig { mem_size, vcpu_count: checkpoints.len() as u64 };
    let vcpu_count = config.vcpu_count;
    let snap = VmSnapshot::new(config, checkpoints, devices);
    let t0 = std::time::Instant::now();
    snapshot::write_diff_snapshot(&base, &snap, dirty, ram, gic_blob, disk_src)?;
    let manifest =
        SnapshotManifest::new_diff(write_name.to_string(), parent.to_string(), mem_size, vcpu_count);
    snapshot::write_manifest(&base, &manifest)?;
    eprintln!("Snapshot-write-time = {} ms", t0.elapsed().as_millis());
    eprintln!(
        "[snapshot] diff '{write_name}' (parent '{parent}', {} dirty pages) written to {}",
        dirty.len(),
        base.display()
    );
    Ok(())
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args: Vec<String> = env::args().collect();
    // Parse `--smp N` (default 1, capped at 8); kernel/rootfs stay positional.
    // Cap matches the FDT/GIC sizing we exercise; raise if a guest needs more.
    const MAX_VCPUS: u64 = 8;
    let mut smp: u64 = 1;
    let mut mem_mib: Option<u64> = None; // None -> per-mode default (512 MiB boot, 96 MiB fuzz)
    let mut net = false;
    let mut vsock_uds: Option<PathBuf> = None;
    let mut store: PathBuf = PathBuf::from("./vmstore");
    let mut name: Option<String> = None;
    let mut force = false;
    let mut no_sandbox = false;
    let mut gui = false;
    let mut track_dirty = false;
    let mut restore_name: Option<String> = None;
    // Fuzz mode (Task 8): boot a single-vCPU guest from an initramfs and run the
    // in-VMM fuzz loop against the ignition-fuzz device.
    let mut fuzz = false;
    let mut initramfs: Option<PathBuf> = None;
    let mut solutions: PathBuf = PathBuf::from("./fuzz-solutions");
    let mut seed_path: Option<PathBuf> = None;
    let mut replay_path: Option<PathBuf> = None;
    let mut window_mib: u64 = 2;
    let mut reset_mode = ignition_vmm::fuzz::controller::ResetMode::Dirty;
    let mut metrics_path: Option<PathBuf> = None;
    let mut append: Option<String> = None;
    let mut positionals: Vec<String> = Vec::new();
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--smp" => {
                let n = it
                    .next()
                    .expect("--smp needs a value")
                    .parse::<u64>()
                    .expect("--smp value must be a number");
                assert!((1..=MAX_VCPUS).contains(&n), "--smp must be 1..={MAX_VCPUS}");
                smp = n;
            }
            "--mem" => {
                let n = it
                    .next()
                    .expect("--mem needs a value")
                    .parse::<u64>()
                    .expect("--mem value must be a number (MiB)");
                assert!((1..=65536).contains(&n), "--mem must be 1..=65536 MiB");
                mem_mib = Some(n);
            }
            "--fuzz" => {
                fuzz = true;
            }
            "--initramfs" => {
                initramfs = Some(PathBuf::from(it.next().expect("--initramfs needs a path")));
            }
            "--solutions" => {
                solutions = PathBuf::from(it.next().expect("--solutions needs a dir"));
            }
            "--seed" => {
                seed_path = Some(PathBuf::from(it.next().expect("--seed needs a path")));
            }
            "--replay" => {
                replay_path = Some(PathBuf::from(it.next().expect("--replay needs a path")));
            }
            "--window-mib" => {
                let n = it
                    .next()
                    .expect("--window-mib needs a value")
                    .parse::<u64>()
                    .expect("--window-mib value must be a number (MiB)");
                assert!((1..=64).contains(&n), "--window-mib must be 1..=64 MiB");
                window_mib = n;
            }
            "--reset" => {
                let v = it.next().expect("--reset needs full|dirty");
                reset_mode = v.parse().expect("--reset must be full|dirty");
            }
            "--metrics" => {
                metrics_path = Some(PathBuf::from(it.next().expect("--metrics needs a path")));
            }
            "--net" => {
                net = true;
            }
            "--store" => {
                store = PathBuf::from(it.next().expect("--store needs a path"));
            }
            "--name" => {
                name = Some(it.next().expect("--name needs a value").to_string());
            }
            "--force" => {
                force = true;
            }
            "--no-sandbox" => {
                no_sandbox = true;
            }
            "--gui" => {
                gui = true;
            }
            "--track-dirty" => {
                track_dirty = true;
            }
            "--vsock-uds" => {
                let v = it.next().expect("--vsock-uds needs a path");
                vsock_uds = Some(PathBuf::from(v));
            }
            "--restore" => {
                restore_name = Some(it.next().expect("--restore needs a snapshot name").to_string());
            }
            "--append" => {
                append = Some(it.next().expect("--append needs a string").to_string());
            }
            other if other.starts_with('-') => {
                eprintln!("unknown flag: {other}");
                process::exit(2);
            }
            other => positionals.push(other.to_string()),
        }
    }

    // Per-mode RAM default: a fuzz guest needs only a small initramfs, so default
    // it to 96 MiB; the normal boot path keeps the historical 512 MiB.
    let mem_mib = mem_mib.unwrap_or(if fuzz { 96 } else { 512 });
    let ram_size: u64 = mem_mib << 20; // MiB -> bytes

    // Fuzz path (Task 8): boot a single-vCPU guest from an initramfs and run the
    // in-VMM fuzz loop. Skips the normal boot / restore paths entirely.
    if fuzz {
        let initramfs = initramfs.unwrap_or_else(|| {
            eprintln!("--fuzz requires --initramfs <path>");
            process::exit(2);
        });
        if positionals.is_empty() {
            eprintln!("usage: {} --fuzz --initramfs <cpio> [--solutions <dir>] [--seed <path>] [--replay <file>] [--window-mib N] [--reset full|dirty] [--metrics <path>] [--mem MiB] [--no-sandbox] <kernel-Image>", args[0]);
            process::exit(2);
        }
        let kernel_path = PathBuf::from(&positionals[0]);
        let window_size = window_mib << 20;
        // --replay feeds a saved crash input verbatim (no mutation) for the
        // determinism gate; it takes precedence over --seed.
        let replay = match replay_path {
            Some(p) => match fs::read(&p) {
                Ok(bytes) => Some(bytes),
                Err(e) => {
                    eprintln!("--replay read {}: {e}", p.display());
                    process::exit(2);
                }
            },
            None => None,
        };
        match run_fuzz_mode(&kernel_path, &initramfs, &solutions, seed_path.as_deref(), replay, window_size, ram_size, reset_mode, metrics_path, no_sandbox) {
            Ok(()) => eprintln!("\n[fuzz exited cleanly]"),
            Err(e) => {
                eprintln!("\n[fuzz error: {e}]");
                process::exit(1);
            }
        }
        return;
    }

    // Restore path: skip normal boot entirely.
    if let Some(rname) = restore_name {
        match run_restore(&store, &rname, name.clone(), force, track_dirty, vsock_uds, no_sandbox, gui) {
            Ok(()) => eprintln!("\n[restore exited cleanly]"),
            Err(e) => {
                eprintln!("\n[restore error: {e}]");
                process::exit(1);
            }
        }
        return;
    }

    if positionals.is_empty() {
        eprintln!("usage: {} [--smp N] [--mem MiB] [--net] [--vsock-uds <path>] [--store <dir>] [--name <name>] [--force] [--track-dirty] [--restore <name>] [--no-sandbox] [--gui] <kernel-Image> [rootfs-disk]", args[0]);
        eprintln!("   or: {} --fuzz --initramfs <cpio> [--solutions <dir>] [--seed <path>] [--replay <file>] [--window-mib N] [--reset full|dirty] [--mem MiB] [--no-sandbox] <kernel-Image>", args[0]);
        process::exit(2);
    }
    // Capture the start instant as early as possible in the fresh-boot path so
    // the boot-timer measures total VM startup time, not just kernel load.
    let boot_start = std::time::Instant::now();
    let kernel_image = fs::read(&positionals[0]).expect("failed to read kernel image");
    let disk_path = positionals.get(1).cloned();

    // Allocate guest RAM on the host.
    let host = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            ram_size as usize,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_ANON | libc::MAP_PRIVATE,
            -1,
            0,
        )
    };
    assert!(host != libc::MAP_FAILED, "mmap failed");
    let host_addr = host as u64;
    let ram: &mut [u8] = unsafe { std::slice::from_raw_parts_mut(host as *mut u8, ram_size as usize) };

    // Load the kernel; entry is where the vCPU's PC starts.
    let entry = kernel::load_kernel(ram, layout::RAM_BASE, &kernel_image).expect("load_kernel failed");

    // The FDT occupies the top FDT_MAX_SIZE of RAM; the kernel must stay below it.
    let fdt_addr = layout::fdt_addr(ram_size);
    let fdt_off = (fdt_addr - layout::RAM_BASE) as usize;

    // VM, then the in-kernel GIC (must be created before any vCPU).
    let mut vm = Vm::new(false).expect("hv_vm_create failed (entitlement?)");
    let gic = Arc::new(HvfGicV3::new(smp, layout::RAM_BASE).expect("hv_gic_create failed"));

    // Device manager: bump-allocates MMIO windows + SPIs, mints GIC IRQs.
    let mut mgr = DeviceManager::new(
        gic.clone(),
        layout::MMIO_BASE,
        layout::MMIO_LEN,
        layout::SPI_BASE,
        layout::SPI_COUNT,
    );

    // In --gui, create the display sink/receiver pair up front: the sink goes to the
    // virtio-gpu device (via DeviceContext), the receiver to the event loop.
    let mut gui_sink: Option<Box<dyn ignition_devices::display::DisplaySink>> = None;
    let gui_rx = if gui {
        let (sink, rx) = display_sink::WindowSink::new();
        gui_sink = Some(Box::new(sink));
        Some(rx)
    } else {
        None
    };

    // Create the dirty tracker (the shared Arc bitmap) BEFORE the DeviceContext so
    // every device GuestRam built in setup_devices reports its DMA writes into it
    // (GuestRam DirtySink). The actual write-protect (vm.protect_memory) is deferred
    // until AFTER vm.map_memory below: stage-2 protection requires the guest mapping
    // to exist, and the DTB is written into RAM after setup_devices. The tracker and
    // the protect step share one bitmap (DirtyTracker is Clone).
    let dirty_tracker: Option<DirtyTracker> = if track_dirty {
        let tracker = DirtyTracker::new(layout::RAM_BASE, ram_size);
        eprintln!(
            "dirty  : tracking armed ({} pages of {} bytes, RAM write-protected)",
            tracker.page_count(),
            ignition_vmm::dirty::PAGE
        );
        Some(tracker)
    } else {
        None
    };

    let mut ctx = DeviceContext {
        host: host as *mut u8,
        ram_size,
        disk: disk_path.as_ref().map(PathBuf::from),
        vsock_uds: vsock_uds.clone(),
        net,
        serial: None, balloon_target: None, balloon: None, vsock_mmio: None, net_mmio: None,
        rx_stop: None,
        display_sink: gui_sink,
        keyboard_mmio: None,
        tablet_mmio: None,
        gpu_mmio: None,
        dirty: dirty_tracker
            .as_ref()
            .map(|t| std::sync::Arc::new(t.clone()) as std::sync::Arc<dyn DirtySink>),
    };
    setup_devices(&mut mgr, &mut ctx, Mode::Boot).expect("device setup failed");
    let kbd_handle = ctx.keyboard_mmio.clone();
    let tab_handle = ctx.tablet_mmio.clone();
    let serial = ctx.serial.clone().expect("serial device");
    let balloon_target = ctx.balloon_target.clone().expect("balloon target");
    let balloon = ctx.balloon.clone().expect("balloon device");
    if let Some(vsock_mmio) = ctx.vsock_mmio.clone() {
        spawn_vsock_reactor(vsock_mmio, ctx.vsock_uds.clone());
        eprintln!("virtio-vsock: enabled (host uds base {})",
            ctx.vsock_uds.as_ref().unwrap().display());
    }
    if let Some(dp) = &disk_path {
        eprintln!("virtio : /dev/vda backed by {dp}");
    }
    if net { eprintln!("virtio-net: enabled (vmnet shared/NAT)"); }

    // Boot-timer: plain BusDevice at a fixed MMIO address (no FDT node, no SPI).
    // The guest writes magic byte 123 to signal userspace-reached; we log elapsed ms.
    mgr.add_fixed(
        layout::BOOT_TIMER_ADDR,
        0x1000,
        Arc::new(Mutex::new(BootTimer::new(boot_start))),
    )
    .expect("add boot_timer");

    // Build and place the device tree.
    let cfg = FdtConfig {
        mem_base: layout::RAM_BASE,
        mem_size: ram_size,
        cpu_mpidrs: (0..smp).map(mpidr_for).collect(),
        cmdline: build_cmdline(append.as_deref()),
        devices: mgr.fdt_devices(),
        gic: gic.fdt_info(),
        initrd: None,
    };
    let dtb = fdt::generate(&cfg).expect("fdt generate failed");
    assert!(fdt_off + dtb.len() <= ram.len(), "DTB does not fit in RAM");
    ram[fdt_off..fdt_off + dtb.len()].copy_from_slice(&dtb);

    // Map the populated RAM into the guest.
    vm.map_memory(host_addr, layout::RAM_BASE, ram_size)
        .expect("hv_vm_map failed");

    // Arm dirty-page tracking: write-protect all guest RAM (drop WRITE) so the
    // first guest write to each page traps as a DirtyFault. The shared tracker
    // bitmap (created above, before setup_devices, so device DMA marks it too) is
    // marked by every vCPU's run loop on fault and drained by the snapshot handler.
    // vCPU windows are armed inside the VcpuManager via set_dirty_config below.
    // This protect step MUST run after vm.map_memory above (stage-2 protection
    // needs the guest mapping to exist).
    if dirty_tracker.is_some() {
        vm.protect_memory(
            layout::RAM_BASE,
            ram_size,
            (HV_MEMORY_READ | HV_MEMORY_EXEC) as u64,
        )
        .expect("write-protect guest RAM for dirty tracking");
    }

    // Diagnostics (stderr) so a silent boot is debuggable.
    let g = gic.fdt_info();
    eprintln!("== ignition boot ==");
    eprintln!("kernel : {} bytes, entry={entry:#x}", kernel_image.len());
    eprintln!("dtb    : {} bytes @ {fdt_addr:#x}", dtb.len());
    eprintln!(
        "gic    : dist=[{:#x}, {:#x}] redist=[{:#x}, {:#x}]",
        g.dist_base, g.dist_size, g.redist_base, g.redist_size
    );
    eprintln!("cmdline: {}", build_cmdline(append.as_deref()));
    eprintln!("--- guest console (stdout) ---");
    io::stderr().flush().ok();

    // Freeze the device set: transfers bus ownership to the run loop.
    let frozen = Arc::new(mgr.freeze());
    let bus = frozen.bus();

    // Build the VcpuManager and install the snapshot handler before run.
    let mut manager = VcpuManager::new(smp, bus);

    let write_name = name.clone().unwrap_or_else(names::generate);

    let rx_stop_snap = ctx.rx_stop.clone();
    let net_mmio_snap = ctx.net_mmio.clone();
    // Reset-handler copies: rx_stop_snap/net_mmio_snap are moved into the snapshot
    // closure below, so capture independent clones here for install_reset_handlers.
    let rx_stop_reset = ctx.rx_stop.clone();
    let net_mmio_reset = ctx.net_mmio.clone();
    let gpu_handle = ctx.gpu_mmio.clone();

    // The "current parent" carried across Ctrl-A s invocations. None on a fresh
    // boot, so the first snapshot is a Full root even with tracking armed (nothing
    // to diff against yet). After any write, the handler stores the just-written
    // name here, so the NEXT Ctrl-A s is a Diff against it. The handler is a
    // `Fn` (Box<dyn Fn + Send + Sync>), so this mutable-across-calls state lives
    // behind an Arc<Mutex<_>>. Task 9 (restore) seeds it with the restored leaf by
    // handing the restore handler an equivalent Arc primed to Some(leaf).
    let current_parent: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    // Install the snapshot handler for any vCPU count. The manager rendezvouses
    // every vCPU and hands us their checkpoints; we capture the global state
    // (GIC + RAM + device records) and write the snapshot.
    {
        let gic_snap = gic.clone();
        let snap_devices = frozen.clone();
        let disk_path_snap = disk_path.clone();
        let store_snap = store.clone();
        let write_name_snap = write_name.clone();
        // The guest RAM base pointer captured as usize: raw *const u8 is neither
        // Send nor Sync, but usize is. Sound because the closure only reads the
        // slice at the rendezvous, when every vCPU is parked at the barrier. The
        // vmnet RX feeder is quiesced below before RAM is read. usize avoids the
        // 2021+ partial-capture seeing through a newtype to the *const u8 field.
        let host_usize = host as usize;
        let ram_size_snap = ram_size;
        // The dirty tracker (Some iff --track-dirty). A Diff requires it; the
        // handler `drain()`s it for the dirty page set and re-protects RAM after.
        let dirty_snap = dirty_tracker.clone();
        let parent_snap = current_parent.clone();
        // --force gates the same-name-as-parent guard below, mirroring the
        // restore-path guard. Captured by value (bool is Copy) so the closure owns it.
        let force_snap = force;

        manager.set_snapshot_handler(Box::new(move |checkpoints: Vec<VcpuCheckpoint>| {
            // Runs on the leader vCPU thread with all vCPUs parked.
            //
            // Layer type is decided by the carried current_parent:
            //   None    -> Full root (first snapshot; nothing to diff against).
            //   Some(p) -> Diff against p; requires the tracker to be armed.
            let parent = parent_snap.lock().unwrap().clone();

            // A Diff is only possible with a tracker. Refuse rather than silently
            // writing a Full under a name the user expects to chain off a parent.
            if parent.is_some() && dirty_snap.is_none() {
                eprintln!("dirty tracking not enabled; restart with --track-dirty for diffs");
                return;
            }

            // Same-name-as-parent guard (spec §4): a Diff whose name equals its
            // parent would atomically rename into base_dir(store, name) — the very
            // dir holding the Full root the chain depends on — clobbering it (and
            // forming a self-cycle). Refuse unless --force. Runs BEFORE drain so a
            // refused diff keeps its accumulated dirty set for the next attempt.
            if let Some(p) = &parent
                && *p == write_name_snap
                && !force_snap
            {
                eprintln!(
                    "[snapshot] refusing to overwrite parent snapshot '{p}'; \
                     pass --force or use a different --name"
                );
                return;
            }

            let gic_blob = match gic_snap.save_state() {
                Ok(b) => b,
                Err(e) => { eprintln!("[snapshot] gic save_state failed: {e}"); return; }
            };

            let devices = snap_devices.save();

            // Quiesce the vmnet RX feeder so it can't write guest RAM mid-read.
            if let Some(stop) = &rx_stop_snap {
                stop.store(true, Ordering::Release);
                if let Some(net) = &net_mmio_snap {
                    drop(net.lock().unwrap()); // drain any in-flight inject
                }
            }

            // The RAM slice — host_usize round-trip avoids capturing *const u8.
            let ram_slice: &[u8] = unsafe {
                std::slice::from_raw_parts(host_usize as *const u8, ram_size_snap as usize)
            };

            let disk_src = match &disk_path_snap {
                Some(p) => PathBuf::from(p),
                None => {
                    let placeholder = std::env::temp_dir()
                        .join(format!("ignition-empty-disk-{}", process::id()));
                    let _ = std::fs::write(&placeholder, b"");
                    placeholder
                }
            };

            let result = match &parent {
                // Full root: write exactly as before (whole RAM, new_full manifest).
                None => {
                    // Full captures whole RAM, so any pages dirtied since boot are
                    // already in it. Clear the bitmap (if armed) so the re-protect
                    // below starts the next interval clean and the next Diff carries
                    // only pages dirtied after THIS snapshot.
                    if let Some(t) = &dirty_snap {
                        let _ = t.drain();
                    }
                    write_named_snapshot(
                        &store_snap, &write_name_snap, ram_slice, &gic_blob, &disk_src,
                        checkpoints, devices, ram_size_snap,
                    )
                }
                // Diff: drain the dirty set (tracker presence checked above) and
                // write only those pages, with a new_diff manifest pointing at p.
                Some(p) => {
                    let dirty = dirty_snap.as_ref().expect("tracker checked above").drain();
                    write_named_diff(
                        &store_snap, &write_name_snap, p, ram_slice, &dirty, &gic_blob,
                        &disk_src, checkpoints, devices, ram_size_snap,
                    )
                }
            };

            match result {
                Ok(()) => {
                    // Carry the just-written layer forward: the next Ctrl-A s diffs
                    // against it.
                    *parent_snap.lock().unwrap() = Some(write_name_snap.clone());
                    // Re-protect ALL RAM (drop WRITE) so the next interval starts
                    // clean. drain() already cleared the bitmap; this rearms the
                    // write-protect faults via the same process-global path used at
                    // boot. No-op when tracking is off.
                    if dirty_snap.is_some()
                        && let Err(e) = ignition_hvf::vm_protect_memory(
                            layout::RAM_BASE,
                            ram_size_snap,
                            (HV_MEMORY_READ | HV_MEMORY_EXEC) as u64,
                        )
                    {
                        eprintln!("[snapshot] re-protect RAM failed: {e}");
                    }
                }
                Err(e) => eprintln!("[snapshot] write failed: {e}"),
            }

            if let Some(stop) = &rx_stop_snap {
                stop.store(false, Ordering::Release);
            }
        }));
    }

    // Arm dirty tracking on the manager BEFORE it is cloned (set_dirty_config,
    // like set_snapshot_handler, needs sole Arc ownership). Each vCPU thread
    // then sets its dirty window and the run loop handles DirtyFault.
    if let Some(tracker) = &dirty_tracker {
        manager.set_dirty_config(DirtyConfig {
            base: layout::RAM_BASE,
            size: ram_size,
            tracker: tracker.clone(),
        });
    }

    // Install the checkpoint/reset handlers (Ctrl-A c / Ctrl-A r) BEFORE `manager`
    // is cloned (set_*_handler requires sole Arc ownership, like the snapshot/dirty
    // setup above). Fresh boot seeds no reset point: the user marks one with Ctrl-A c.
    install_reset_handlers(&mut manager, ResetWiring {
        host_usize: host as usize,
        ram_size,
        mem_file: None,
        inst_dir: std::env::temp_dir(),
        gic: gic.clone(),
        frozen: frozen.clone(),
        dirty: dirty_tracker.clone(),
        rx_stop: rx_stop_reset,
        net_mmio: net_mmio_reset,
        gpu: gpu_handle,
    });

    // Raw terminal + host stdin reader for the interactive console. The guard
    // restores the terminal on drop (guest-initiated exit); the reader restores
    // it before process::exit on Ctrl-A x.
    let termios = TermiosGuard::new();
    spawn_stdin_reader(serial.clone(), termios.saved(), manager.clone(), balloon_target.clone(), balloon.clone());
    eprintln!("--- console attached (quit: Ctrl-A x, snapshot: Ctrl-A s, balloon: Ctrl-A b, checkpoint: Ctrl-A c, reset: Ctrl-A r), {smp} vCPU(s) ---");
    eprintln!("--- snapshots will be saved as '{write_name}' under {} ---", store.display());

    // Jail the VMM before running guest code. Reads of kernel/rootfs are already
    // done or held; writes must stay open for snapshot-on-demand to the store.
    // Note: on fresh boot the rootfs is opened read+write *before* this point
    // (virtio-blk holds the fd), so guest disk writes keep working even though the
    // rootfs path is not in `writable` — Seatbelt's file-write* check is at open()
    // time, not on writes through an already-open fd. Nothing reopens it after apply.
    // (Restore is unaffected: it writes a CoW instance copy under the store.)
    let sb_paths = ignition_sandbox::SandboxPaths {
        readable: [Some(PathBuf::from(&positionals[0])), positionals.get(1).map(PathBuf::from)]
            .into_iter().flatten().collect(),
        writable: [Some(store.clone()), Some(std::env::temp_dir()),
                   vsock_uds.as_ref().and_then(|u| u.parent().map(PathBuf::from))]
            .into_iter().flatten().collect(),
    };
    if gui {
        // GUI mode: the winit event loop must own the main thread on macOS, so the
        // VMM (sandbox apply + the vCPU join loop) moves to a spawned thread and the
        // event loop runs on main. `manager` is an Arc; cloning shares the VMM.
        // The virtio-gpu device (registered in setup_devices under --gui) presents
        // frames into `rx`; the event loop drains and blits them.
        // The `TermiosGuard` (`termios`) stays alive in this scope; when the event
        // loop returns and `main` returns, the guard's Drop restores the terminal and
        // the process exits (killing the VMM thread). Window close → loop exit; VMM
        // done → loop exit.
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let rx = gui_rx.expect("gui implies a receiver was created");
        let done_vmm = done.clone();
        let mgr = manager.clone();
        std::thread::spawn(move || {
            // Release the event loop however this thread leaves — clean return OR a
            // panic in the VMM — so a crashed guest still closes the window. Drop runs
            // on unwind; `apply_or_exit`'s process::exit on sandbox failure ends the
            // whole process, which tears the loop down too.
            struct SignalOnExit(std::sync::Arc<std::sync::atomic::AtomicBool>);
            impl Drop for SignalOnExit {
                fn drop(&mut self) {
                    self.0.store(true, std::sync::atomic::Ordering::Release);
                }
            }
            let _signal = SignalOnExit(done_vmm);
            apply_or_exit(&sb_paths, no_sandbox);
            match mgr.run(entry, fdt_addr) {
                Ok(()) => eprintln!("\n[vcpus exited cleanly]"),
                Err(e) => eprintln!("\n[vcpu error: {e}]"),
            }
        });
        display_sink::run_event_loop(
            rx,
            done,
            1280,
            800,
            kbd_handle,
            tab_handle,
            1280,
            800,
            Some(manager.clone()),
        );
    } else {
        apply_or_exit(&sb_paths, no_sandbox);

        // Run. Earlycon + virtio MMIO exits are dispatched through the bus.
        match manager.run(entry, fdt_addr) {
            Ok(()) => eprintln!("\n[vcpus exited cleanly]"),
            Err(e) => eprintln!("\n[vcpu error: {e}]"),
        }
    }
}

/// The normal-boot kernel command line, optionally with extra args appended
/// (`--append`). Used to pass e.g. `init=/sbin/overlay-init` for the overlay-root
/// browser rootfs. Absent `--append` reproduces `layout::default_cmdline()`.
fn build_cmdline(append: Option<&str>) -> String {
    let base = layout::default_cmdline();
    match append {
        Some(extra) if !extra.is_empty() => format!("{base} {extra}"),
        _ => base,
    }
}

/// Fuzz-mode command line: run the initramfs `/init` (rdinit) and never try to
/// mount a root disk (no `root=`). `reboot=t` + `panic=-1` keep a wedged guest
/// from hanging the harness. Reuses the console token from `default_cmdline`.
fn fuzz_cmdline() -> String {
    format!(
        "console=ttyS0 earlycon=uart8250,mmio,{:#x} reboot=t panic=-1 rdinit=/init",
        layout::MMIO_BASE
    )
}

/// Fixed GPAs for the ignition-fuzz device (mirror of
/// `guest/fuzz-harness/ignition_fuzz.h`; 16 KiB-aligned). The control region sits
/// at the very top of the device-MMIO map (just past the bump allocator + the
/// boot-timer) and the shared window directly above it. Both are below `RAM_BASE`,
/// so neither collides with guest RAM, the GIC (below `RAM_BASE`), the serial /
/// virtio bump region ([`layout::MMIO_BASE`, `layout::MMIO_BASE + layout::MMIO_LEN`)),
/// or the boot-timer ([`layout::BOOT_TIMER_ADDR`]).
const FUZZ_CTRL_GPA: u64 = 0x0920_0000;
const FUZZ_WIN_GPA: u64 = 0x0920_4000; // CTRL_GPA + CONTROL_SIZE (0x4000)
// The coverage region: a host-readable RAM-backed map of 8-bit SanCov counters,
// mapped into the guest just above the input window. Like the window it sits
// below RAM_BASE, so it is outside the dirty-tracked guest-RAM range and never
// rolled back by the dirty reset (spec §6: host-managed pages are reset-exempt).
const FUZZ_COV_GPA: u64 = 0x0940_4000; // FUZZ_WIN_GPA + DEFAULT_WINDOW_SIZE (0x20_0000)

/// Boot a single-vCPU guest from an initramfs and run the in-VMM fuzz loop.
///
/// Mirrors the fresh-boot body (RAM mmap, kernel load, GIC + serial + bus, FDT
/// generate + write), then adds the fuzz wiring:
///   * the shared WINDOW is a host anon mmap mapped into the guest at `FUZZ_WIN_GPA`
///     (real RAM, no trap) — placed OUTSIDE guest RAM;
///   * the CONTROL region is registered on the bus via `add_fixed` at `FUZZ_CTRL_GPA`
///     but NOT mapped, so every guest access traps as a data abort and routes to
///     the bus / the doorbell arm in `fuzz_loop` (same pattern as the boot-timer);
///   * the FDT carries a `Fuzz` node + `initrd` pointing at the loaded cpio;
///   * `FuzzController` owns the host views of guest RAM and the window.
fn run_fuzz_mode(
    kernel_path: &Path,
    initramfs_path: &Path,
    solutions_dir: &Path,
    seed_path: Option<&Path>,
    replay: Option<Vec<u8>>,
    window_size: u64,
    ram_size: u64,
    reset_mode: ResetMode,
    metrics_path: Option<PathBuf>,
    no_sandbox: bool,
) -> io::Result<()> {
    // Fuzz mode has no guest console to absorb Ctrl-C, so install a SIGINT/SIGTERM
    // handler that flips the global stop flag; the fuzz loop polls it and exits
    // cleanly, flushing --metrics. The handler only does an atomic store (async-
    // signal-safe).
    extern "C" fn fuzz_stop_handler(_sig: libc::c_int) {
        ignition_vmm::vstate::vcpu_manager::FUZZ_STOP
            .store(true, std::sync::atomic::Ordering::Release);
    }
    // SAFETY: registering a signal handler that performs only an atomic store.
    unsafe {
        libc::signal(libc::SIGINT, fuzz_stop_handler as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, fuzz_stop_handler as *const () as libc::sighandler_t);
    }

    let kernel_image = fs::read(kernel_path)
        .map_err(|e| io::Error::other(format!("read kernel {}: {e}", kernel_path.display())))?;
    let initramfs = fs::read(initramfs_path)
        .map_err(|e| io::Error::other(format!("read initramfs {}: {e}", initramfs_path.display())))?;

    // The M0 guest harness is compiled with a fixed 2 MiB window clamp
    // (IGNITION_FUZZ_WIN_SIZE in ignition_fuzz.h). A host window of a different
    // size would diverge from what the guest mmaps/clamps, so warn unless the
    // harness is rebuilt to match.
    if window_size != ignition_devices::fuzz::protocol::DEFAULT_WINDOW_SIZE {
        log::warn!(
            "fuzz window {} MiB != harness-baked {} MiB; rebuild the harness or pass --window-mib 2",
            window_size >> 20,
            ignition_devices::fuzz::protocol::DEFAULT_WINDOW_SIZE >> 20
        );
    }

    // The window is RAM-backed (guest loads/stores hit it directly), so it must
    // live OUTSIDE guest RAM — otherwise it would shadow real RAM. Assert it.
    assert!(
        FUZZ_WIN_GPA + window_size <= layout::RAM_BASE,
        "fuzz window [{FUZZ_WIN_GPA:#x}, {:#x}) must sit below RAM_BASE {:#x}",
        FUZZ_WIN_GPA + window_size,
        layout::RAM_BASE
    );
    // Region layout is fixed at compile time: ctrl | window | coverage, ascending,
    // non-overlapping, all below RAM_BASE.
    const {
        assert!(
            FUZZ_CTRL_GPA + protocol::CONTROL_SIZE <= FUZZ_WIN_GPA,
            "fuzz control region overlaps the window"
        );
    }
    let cov_size = protocol::DEFAULT_COV_SIZE;
    assert!(
        FUZZ_WIN_GPA + window_size <= FUZZ_COV_GPA,
        "fuzz window [{FUZZ_WIN_GPA:#x}, {:#x}) overlaps the coverage region at {FUZZ_COV_GPA:#x}",
        FUZZ_WIN_GPA + window_size
    );
    assert!(
        FUZZ_COV_GPA + cov_size <= layout::RAM_BASE,
        "fuzz coverage region [{FUZZ_COV_GPA:#x}, {:#x}) must sit below RAM_BASE {:#x}",
        FUZZ_COV_GPA + cov_size,
        layout::RAM_BASE
    );
    assert_eq!(FUZZ_COV_GPA & 0x3FFF, 0, "coverage GPA must be 16 KiB-aligned");

    // Allocate guest RAM on the host (private anon, same as the fresh-boot path).
    let host = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            ram_size as usize,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_ANON | libc::MAP_PRIVATE,
            -1,
            0,
        )
    };
    if host == libc::MAP_FAILED {
        return Err(io::Error::other("mmap of guest RAM failed"));
    }
    let host_addr = host as u64;
    let ram: &mut [u8] = unsafe { std::slice::from_raw_parts_mut(host as *mut u8, ram_size as usize) };

    // Load the kernel; entry is where the vCPU's PC starts.
    let entry = kernel::load_kernel(ram, layout::RAM_BASE, &kernel_image)
        .map_err(|e| io::Error::other(format!("load_kernel: {e:?}")))?;

    // The FDT occupies the top FDT_MAX_SIZE of (capped) RAM.
    let fdt_addr = layout::fdt_addr(ram_size);
    let fdt_off = (fdt_addr - layout::RAM_BASE) as usize;

    // Place the initramfs at a 16 KiB-aligned offset above the kernel and below
    // the FDT. 64 MiB clears any reasonable kernel; assert it fits below the DTB.
    let initrd_off: usize = 0x0400_0000; // 64 MiB into RAM
    let initrd_gpa = layout::RAM_BASE + initrd_off as u64;
    assert_eq!(initrd_gpa & 0x3FFF, 0, "initrd GPA must be 16 KiB-aligned");
    if initrd_off + initramfs.len() > fdt_off {
        return Err(io::Error::other(format!(
            "initramfs ({} bytes at offset {:#x}) does not fit below the FDT at offset {:#x}; \
             increase --mem",
            initramfs.len(),
            initrd_off,
            fdt_off
        )));
    }
    ram[initrd_off..initrd_off + initramfs.len()].copy_from_slice(&initramfs);

    // VM, then the in-kernel GIC (must be created before any vCPU). Single vCPU.
    let mut vm = Vm::new(false).map_err(|e| io::Error::other(format!("Vm::new: {e}")))?;
    let gic = Arc::new(
        HvfGicV3::new(1, layout::RAM_BASE).map_err(|e| io::Error::other(format!("GIC create: {e}")))?,
    );

    // Device manager: serial first (its base matches the cmdline earlycon).
    let mut mgr = DeviceManager::new(
        gic.clone(),
        layout::MMIO_BASE,
        layout::MMIO_LEN,
        layout::SPI_BASE,
        layout::SPI_COUNT,
    );
    // Serial console: registered on the bus (the guest's earlycon output reaches
    // host stdout via FlushWriter). The fuzz loop drives no host stdin reader, so
    // we keep no handle — the frozen bus owns it.
    mgr.add(layout::MMIO_WINDOW, |irq| Serial::with_irq(FlushWriter, irq))
        .map_err(io::Error::other)?;

    // The ignition-fuzz CONTROL region: registered on the bus but NOT mapped into
    // the guest, so every guest access to it traps as a data abort (like the
    // boot-timer). The doorbell store routes to the fuzz_loop's doorbell arm.
    let fuzz_dev = Arc::new(Mutex::new(FuzzDevice::new()));
    mgr.add_fixed(FUZZ_CTRL_GPA, protocol::CONTROL_SIZE, fuzz_dev.clone())
        .map_err(io::Error::other)?;

    // Build and place the device tree: serial console + fuzz node, with initrd.
    let cfg = FdtConfig {
        mem_base: layout::RAM_BASE,
        mem_size: ram_size,
        cpu_mpidrs: vec![mpidr_for(0)],
        cmdline: fuzz_cmdline(),
        devices: {
            let mut devs = mgr.fdt_devices(); // serial (the fuzz device has no record)
            devs.push(FdtDevice::Fuzz(FuzzDev {
                ctrl_addr: FUZZ_CTRL_GPA,
                ctrl_size: protocol::CONTROL_SIZE,
                win_addr: FUZZ_WIN_GPA,
                win_size: window_size,
            }));
            devs
        },
        gic: gic.fdt_info(),
        initrd: Some((initrd_gpa, initramfs.len() as u64)),
    };
    let dtb = fdt::generate(&cfg).map_err(|e| io::Error::other(format!("fdt generate: {e:?}")))?;
    if fdt_off + dtb.len() > ram.len() {
        return Err(io::Error::other("DTB does not fit in RAM"));
    }
    ram[fdt_off..fdt_off + dtb.len()].copy_from_slice(&dtb);

    // Map the populated RAM into the guest.
    vm.map_memory(host_addr, layout::RAM_BASE, ram_size)
        .map_err(|e| io::Error::other(format!("hv_vm_map RAM: {e}")))?;

    // The shared WINDOW: a host anon mmap mapped into the guest at FUZZ_WIN_GPA, so
    // guest loads/stores hit real RAM (no trap). Mapped read/write/exec by HVF's
    // default map_memory grant.
    let win_host = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            window_size as usize,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_ANON | libc::MAP_PRIVATE,
            -1,
            0,
        )
    };
    if win_host == libc::MAP_FAILED {
        return Err(io::Error::other("mmap of fuzz window failed"));
    }
    let win_addr = win_host as u64;
    vm.map_memory(win_addr, FUZZ_WIN_GPA, window_size)
        .map_err(|e| io::Error::other(format!("hv_vm_map window: {e}")))?;

    // The shared COVERAGE region: host anon mmap mapped into the guest at
    // FUZZ_COV_GPA. The guest's trace-pc callback writes 8-bit edge counters here;
    // the host zeroes it before each input and reads it after DONE. Like the
    // window it lives below RAM_BASE, so the dirty reset never rolls it back.
    let cov_host = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            cov_size as usize,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_ANON | libc::MAP_PRIVATE,
            -1,
            0,
        )
    };
    if cov_host == libc::MAP_FAILED {
        return Err(io::Error::other("mmap of fuzz coverage region failed"));
    }
    vm.map_memory(cov_host as u64, FUZZ_COV_GPA, cov_size)
        .map_err(|e| io::Error::other(format!("hv_vm_map coverage: {e}")))?;

    // Diagnostics (stderr) so a silent boot is debuggable.
    let g = gic.fdt_info();
    eprintln!("== ignition fuzz ==");
    eprintln!("kernel : {} bytes, entry={entry:#x}", kernel_image.len());
    eprintln!("initrd : {} bytes @ {initrd_gpa:#x}", initramfs.len());
    eprintln!("dtb    : {} bytes @ {fdt_addr:#x}", dtb.len());
    eprintln!(
        "fuzz   : ctrl=[{FUZZ_CTRL_GPA:#x}, +{:#x}] (trap-mmio) window=[{FUZZ_WIN_GPA:#x}, +{window_size:#x}] (ram-backed)",
        protocol::CONTROL_SIZE
    );
    eprintln!(
        "gic    : dist=[{:#x}, {:#x}] redist=[{:#x}, {:#x}]",
        g.dist_base, g.dist_size, g.redist_base, g.redist_size
    );
    eprintln!("cmdline: {}", fuzz_cmdline());
    eprintln!("solutions: {}", solutions_dir.display());
    eprintln!("--- guest console (stdout) ---");
    io::stderr().flush().ok();

    // Freeze the device set: transfers bus ownership to the run loop.
    let frozen = Arc::new(mgr.freeze());
    let bus = frozen.bus();

    // Build the controller: host views of guest RAM + the window, plus the seed
    // corpus (a single file if --seed was given, else empty -> 1-byte default).
    let seeds: Vec<Vec<u8>> = match seed_path {
        Some(p) => vec![fs::read(p)
            .map_err(|e| io::Error::other(format!("read seed {}: {e}", p.display())))?],
        None => Vec::new(),
    };
    // Dirty tracker for ResetMode::Dirty: covers all guest RAM, base = RAM_BASE.
    let dirty_tracker: Option<DirtyTracker> = if reset_mode == ResetMode::Dirty {
        Some(DirtyTracker::new(layout::RAM_BASE, ram_size))
    } else {
        None
    };

    // Capture the metrics parent dir before `metrics_path` is moved into the
    // controller below; the sandbox is applied LATE (right before run_fuzz).
    let metrics_parent = metrics_path.as_ref().and_then(|m| m.parent().map(PathBuf::from));
    let controller = FuzzController::new(
        (host as *mut u8, ram_size as usize),
        (win_host as *mut u8, window_size as usize),
        (cov_host as *mut u8, cov_size as usize),
        layout::RAM_BASE,
        reset_mode,
        dirty_tracker.clone(),
        seeds,
        replay,
        0xF1FA_5EED,
        solutions_dir.to_path_buf(),
        metrics_path,
    );

    // Run the single-vCPU fuzz loop. The doorbell GPA is the DOORBELL register
    // within the (unmapped, trapping) control region.
    let mut manager = VcpuManager::new(1, bus);
    if let Some(tracker) = &dirty_tracker {
        manager.set_dirty_config(DirtyConfig {
            base: layout::RAM_BASE,
            size: ram_size,
            tracker: tracker.clone(),
        });
    }
    let sb_paths = ignition_sandbox::SandboxPaths {
        readable: vec![kernel_path.to_path_buf(), initramfs_path.to_path_buf()],
        writable: [Some(solutions_dir.to_path_buf()), Some(std::env::temp_dir()),
                   metrics_parent]
            .into_iter().flatten().collect(),
    };
    apply_or_exit(&sb_paths, no_sandbox);

    manager
        .run_fuzz(
            entry,
            fdt_addr,
            FUZZ_CTRL_GPA + protocol::reg::DOORBELL,
            FUZZ_CTRL_GPA,
            fuzz_dev,
            controller,
        )
        .map_err(|e| io::Error::other(format!("run_fuzz: {e}")))?;
    Ok(())
}

/// Restore a (possibly diff-chained) base snapshot from
/// `<store>/snapshots/<restore_name>/` and resume the guest. `restore_name` is the
/// LEAF of the chain; the chain is resolved root..leaf, the root's whole-RAM image
/// is cloned + mmap'd, every Diff layer is overlaid in order, and the vCPU/GIC/device
/// state of the LEAF is restored. Does NOT load a kernel, generate an FDT, or call
/// set_initial_state — the running kernel + DTB are already in the reassembled RAM.
#[allow(clippy::too_many_arguments)]
fn run_restore(
    store: &Path,
    restore_name: &str,
    name: Option<String>,
    force: bool,
    track_dirty: bool,
    vsock_uds: Option<PathBuf>,
    no_sandbox: bool,
    gui: bool,
) -> io::Result<()> {
    // Host-side restore clock: chain resolution + mmap + diff overlay + GIC/device/vCPU
    // state restore, up to handing the guest to the run loop. The boot-timer device
    // can't measure restore (the guest's init does not re-run), so this is the
    // restore analog of `Guest-boot-time`.
    let restore_start = std::time::Instant::now();

    // 1. Resolve the immutable diff chain root..leaf. resolve_chain rejects a missing
    //    parent layer and a cycle. Validate the shape: chain[0] must be the Full root,
    //    every later layer a Diff, and all layers must agree on mem_size.
    let chain = snapshot::resolve_chain(store, restore_name)?;
    let root = &chain[0];
    if root.snapshot_type != snapshot::SnapshotType::Full {
        return Err(io::Error::other(format!(
            "chain root '{}' is not a Full snapshot (got {:?})",
            root.name, root.snapshot_type
        )));
    }
    let mem_size = root.mem_size;
    for m in &chain[1..] {
        if m.snapshot_type != snapshot::SnapshotType::Diff {
            return Err(io::Error::other(format!(
                "non-root layer '{}' is not a Diff snapshot (got {:?})",
                m.name, m.snapshot_type
            )));
        }
        if m.mem_size != mem_size {
            return Err(io::Error::other(format!(
                "layer '{}' mem_size {} != root mem_size {mem_size}",
                m.name, m.mem_size
            )));
        }
    }
    let t_chain = restore_start.elapsed();

    // The LEAF carries the vCPU/GIC/device state to resume from. read_snapshot
    // version-guards (check_version) the leaf's vmstate.json, so we validate v3 here;
    // each overlaid Diff layer's own manifest was validated by resolve_chain.
    let leaf = chain.last().expect("resolve_chain returns >= 1 layer");
    let leaf_dir = snapshot::base_dir(store, &leaf.name);
    let (snap, gic_blob, leaf_paths) = snapshot::read_snapshot(&leaf_dir)?;
    if snap.config.mem_size != mem_size {
        return Err(io::Error::other(format!(
            "leaf vmstate mem_size {} != chain mem_size {mem_size}",
            snap.config.mem_size
        )));
    }

    // The ROOT's whole-RAM image is the base we clone + map. Validate its length.
    let root_dir = snapshot::base_dir(store, &root.name);
    let root_paths = snapshot::paths(&root_dir);
    let base_len = fs::metadata(&root_paths.memory)?.len();
    if base_len != mem_size {
        return Err(io::Error::other(format!(
            "root memory.bin length {base_len} != mem_size {mem_size}"
        )));
    }
    let t_read = restore_start.elapsed();

    // Per-restore instance dir: CoW clones of the immutable base live here, so the
    // running guest never writes back into the base. (A later task moves this under the store.)
    let inst_dir = snapshot::instance_dir(store, restore_name, process::id());
    let _ = fs::remove_dir_all(&inst_dir);
    fs::create_dir_all(&inst_dir)?;
    let inst_mem = inst_dir.join("memory.bin");
    // Clone the ROOT memory.bin (not the leaf — a Diff leaf's memory.bin is only its
    // packed dirty pages). Diff layers are overlaid onto this clone below.
    snapshot::clonefile_or_copy(&root_paths.memory, &inst_mem)?;
    let t_clone = restore_start.elapsed();

    // 2. Map the instance memory.bin as guest RAM. MAP_SHARED: pages fault in lazily
    //    from the clone, and guest writes land in the clone (APFS copy-on-writes the
    //    block off the base on first write) — the base is never touched.
    let memf = fs::OpenOptions::new().read(true).write(true).open(&inst_mem)?;
    let host = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            mem_size as usize,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            memf.as_raw_fd(),
            0,
        )
    };
    if host == libc::MAP_FAILED {
        return Err(io::Error::other("mmap of instance memory.bin failed"));
    }
    drop(memf); // the mapping keeps the underlying file alive after the fd closes
    let host_addr = host as u64;
    let t_mmap = restore_start.elapsed();

    // 2b. Overlay each Diff layer in order onto the MAP_SHARED clone. Writes land in
    //     the private instance file (APFS CoWs the block off the root on first write),
    //     so every stored layer — root and diffs — stays byte-for-byte immutable.
    //     Done BEFORE the vCPUs run so the guest sees the fully reassembled RAM.
    if chain.len() > 1 {
        let ram_overlay: &mut [u8] =
            unsafe { std::slice::from_raw_parts_mut(host as *mut u8, mem_size as usize) };
        for m in &chain[1..] {
            let d = snapshot::base_dir(store, &m.name);
            let (idx, packed) = snapshot::read_diff_pages(&d)?;
            snapshot::apply_diff(ram_overlay, &idx, &packed)?;
        }
        eprintln!(
            "[restore] reassembled chain: root '{}' + {} diff layer(s) -> leaf '{}'",
            root.name,
            chain.len() - 1,
            leaf.name
        );
    }
    let t_diff = restore_start.elapsed();

    // 3. Create the HVF VM (must precede GIC and vCPU creation).
    let mut vm = Vm::new(false).map_err(|e| io::Error::other(format!("Vm::new: {e}")))?;
    let t_vm = restore_start.elapsed();

    // 4. Create the in-kernel GIC (same placement as a fresh boot). Its saved
    //    distributor/redistributor state is restored later via `gic_restore`, after
    //    the vCPU exists (see VcpuManager::run_restored / gic_restore).
    let gic = Arc::new(
        HvfGicV3::new(snap.config.vcpu_count, layout::RAM_BASE)
            .map_err(|e| io::Error::other(format!("GIC create: {e}")))?,
    );
    let t_gic = restore_start.elapsed();

    // 5. Map the populated RAM into the guest.
    vm.map_memory(host_addr, layout::RAM_BASE, mem_size)
        .map_err(|e| io::Error::other(format!("hv_vm_map: {e}")))?;
    let t_map = restore_start.elapsed();

    // 5b. Arm dirty-page tracking on the restored guest if --track-dirty: write-protect
    //     all guest RAM (drop WRITE) so the first guest write to each page traps as a
    //     DirtyFault. The chain is already fully overlaid above, so the next interval
    //     starts clean and the first re-snapshot's Diff carries only pages dirtied AFTER
    //     this restore. Same mechanism as the boot path (Task 7).
    let dirty_tracker: Option<DirtyTracker> = if track_dirty {
        vm.protect_memory(
            layout::RAM_BASE,
            mem_size,
            (HV_MEMORY_READ | HV_MEMORY_EXEC) as u64,
        )
        .map_err(|e| io::Error::other(format!("write-protect restored RAM: {e}")))?;
        let tracker = DirtyTracker::new(layout::RAM_BASE, mem_size);
        eprintln!(
            "dirty  : tracking armed on restore ({} pages of {} bytes, RAM write-protected)",
            tracker.page_count(),
            ignition_vmm::dirty::PAGE
        );
        Some(tracker)
    } else {
        None
    };
    let t_protect = restore_start.elapsed();

    // 6. Restore devices at their saved addresses via DeviceManager.
    let mut mgr = DeviceManager::new(
        gic.clone(),
        layout::MMIO_BASE,
        layout::MMIO_LEN,
        layout::SPI_BASE,
        layout::SPI_COUNT,
    );
    // Private CoW disk instance so clones are independent and the base disk.img is
    // never mutated (only if the snapshot has a disk).
    let disk = if snap.devices.iter().any(|r| r.id == "virtio-blk") {
        let instance_disk = inst_dir.join("disk.img");
        // The leaf's disk.img is a full clonefile (Full and Diff layers both write the
        // whole disk), so it is the authoritative disk state for the resumed guest.
        snapshot::clonefile_or_copy(&leaf_paths.disk, &instance_disk)?;
        Some(instance_disk)
    } else {
        None
    };

    // Under --gui, wire a WindowSink so the restored virtio-gpu presents into the
    // event loop; without it the restore stays headless (NoopSink in setup_devices).
    let (gui_sink, gui_rx): (
        Option<Box<dyn ignition_devices::display::DisplaySink>>,
        Option<std::sync::mpsc::Receiver<ignition_devices::display::Frame>>,
    ) = if gui {
        let (sink, rx) = display_sink::WindowSink::new();
        (Some(Box::new(sink)), Some(rx))
    } else {
        (None, None)
    };

    let mut ctx = DeviceContext {
        host: host as *mut u8,
        ram_size: mem_size,
        disk: disk.clone(),
        vsock_uds: vsock_uds.clone(),
        net: false, // snapshots never contain net; setup_devices will re-wire if record exists
        serial: None, balloon_target: None, balloon: None, vsock_mmio: None, net_mmio: None,
        rx_stop: None,
        display_sink: gui_sink,
        keyboard_mmio: None,
        tablet_mmio: None,
        gpu_mmio: None,
        // Tracker created above (before this DeviceContext), so device DMA marks it.
        dirty: dirty_tracker
            .as_ref()
            .map(|t| std::sync::Arc::new(t.clone()) as std::sync::Arc<dyn DirtySink>),
    };
    setup_devices(&mut mgr, &mut ctx, Mode::Restore(&snap.devices))?;
    let kbd_handle = ctx.keyboard_mmio.clone();
    let tab_handle = ctx.tablet_mmio.clone();
    let gpu_handle = ctx.gpu_mmio.clone();
    let t_dev = restore_start.elapsed();

    let serial = ctx.serial.clone().ok_or_else(|| io::Error::other("snapshot had no serial device"))?;
    let balloon_target = ctx.balloon_target.clone()
        .ok_or_else(|| io::Error::other("snapshot had no balloon device"))?;
    let balloon = ctx.balloon.clone()
        .ok_or_else(|| io::Error::other("snapshot had no balloon device"))?;
    if let Some(vsock_mmio) = ctx.vsock_mmio.clone() {
        spawn_vsock_reactor(vsock_mmio, ctx.vsock_uds.clone());
    }
    let net_mmio_restore = ctx.net_mmio.clone();
    let rx_stop_snap = ctx.rx_stop.clone();
    let net_mmio_snap = ctx.net_mmio.clone();
    // Reset-handler copies: snap clones are moved into the re-snapshot closure below.
    let rx_stop_reset = ctx.rx_stop.clone();
    let net_mmio_reset = ctx.net_mmio.clone();
    let q_vsock = restore_start.elapsed();
    let frozen = Arc::new(mgr.freeze());
    let bus = frozen.bus();
    let q_freeze = restore_start.elapsed();

    // 7. Set up the interactive console (raw terminal + stdin reader).
    let termios = TermiosGuard::new();
    let mut manager = VcpuManager::new(snap.config.vcpu_count, bus);
    let q_console = restore_start.elapsed();

    // Re-snapshot: a restored guest can be snapshotted into a NEW base. An omitted
    // --name generates a fresh one (never collides with the source). The handler
    // mirrors the boot path's Full/Diff logic (Task 8): the carried current_parent
    // decides the layer type. We SEED it with the restored LEAF (restore_name) so the
    // first Ctrl-A s writes a Diff with parent=leaf — only possible when --track-dirty
    // armed the tracker above; without it the handler falls back to refusing the diff.
    // Must be installed before `manager` is cloned (spawn_stdin_reader / run_restored),
    // because set_snapshot_handler requires sole ownership of the Arc.
    let write_name = name.unwrap_or_else(names::generate);
    // Seed the parent with the leaf so the first re-snapshot diffs against it. None when
    // tracking is off, so the first re-snapshot is a self-contained Full (no parent to
    // diff against, and a Diff would be impossible without a tracker anyway).
    let current_parent: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(
        if track_dirty { Some(restore_name.to_string()) } else { None },
    ));
    {
        let store_snap = store.to_path_buf();
        let write_name_snap = write_name.clone();
        // The restored-from leaf, captured INDEPENDENTLY of dirty tracking. Guards
        // the immutable source layer below: a restored guest must never silently
        // clobber the snapshot it was restored from, whether or not --track-dirty
        // seeded current_parent. (Without tracking the seed is None, so the
        // same-name-as-parent guard alone would not catch it.)
        let restored_from = restore_name.to_string();
        let gic_snap = gic.clone();
        let snap_devices = frozen.clone();
        let disk_snap = disk.clone();
        let host_usize = host as usize;
        let mem_size_snap = mem_size;
        let dirty_snap = dirty_tracker.clone();
        let parent_snap = current_parent.clone();
        let force_snap = force;
        manager.set_snapshot_handler(Box::new(move |checkpoints: Vec<VcpuCheckpoint>| {
            // Runs on the leader vCPU thread with all vCPUs parked. Layer type is
            // decided by the carried current_parent (seeded to the leaf on restore):
            //   None    -> Full root (only when tracking is off).
            //   Some(p) -> Diff against p; requires the tracker to be armed.
            let parent = parent_snap.lock().unwrap().clone();

            // Restored-from guard (independent of dirty tracking): refuse to overwrite
            // the snapshot this guest was restored from. Applies to BOTH Full and Diff
            // branches — without --track-dirty current_parent is None, so the
            // same-name-as-parent guard below would let a `--name <source>` Full clobber
            // the immutable source layer. Runs BEFORE drain so a refused write keeps any
            // accumulated dirty set for the next attempt.
            if write_name_snap == restored_from && !force_snap {
                eprintln!(
                    "[snapshot] refusing to overwrite the base '{write_name_snap}' you are \
                     restored from; pass --force or --name <other>"
                );
                return;
            }

            if parent.is_some() && dirty_snap.is_none() {
                eprintln!("dirty tracking not enabled; restart with --track-dirty for diffs");
                return;
            }

            // Same-name-as-parent guard: a Diff whose name equals its parent would
            // rename over the dir holding that layer, clobbering it and forming a
            // self-cycle. Refuse unless --force. Runs BEFORE drain so a refused diff
            // keeps its accumulated dirty set for the next attempt.
            if let Some(p) = &parent
                && *p == write_name_snap
                && !force_snap
            {
                eprintln!(
                    "[snapshot] refusing to overwrite parent snapshot '{p}'; \
                     pass --force or use a different --name"
                );
                return;
            }

            let gic_blob = match gic_snap.save_state() {
                Ok(b) => b,
                Err(e) => { eprintln!("[snapshot] gic save_state failed: {e}"); return; }
            };
            let devices = snap_devices.save();

            // Quiesce the vmnet RX feeder so it can't write guest RAM mid-read.
            if let Some(stop) = &rx_stop_snap {
                stop.store(true, Ordering::Release);
                if let Some(net) = &net_mmio_snap {
                    drop(net.lock().unwrap()); // drain any in-flight inject
                }
            }

            let ram_slice: &[u8] = unsafe {
                std::slice::from_raw_parts(host_usize as *const u8, mem_size_snap as usize)
            };
            let disk_src = match &disk_snap {
                Some(p) => p.clone(),
                None => {
                    let placeholder = std::env::temp_dir()
                        .join(format!("ignition-empty-disk-{}", process::id()));
                    let _ = std::fs::write(&placeholder, b"");
                    placeholder
                }
            };

            let result = match &parent {
                None => {
                    // Full captures whole RAM; clear the bitmap (if armed) so the
                    // re-protect below starts the next interval clean.
                    if let Some(t) = &dirty_snap {
                        let _ = t.drain();
                    }
                    write_named_snapshot(
                        &store_snap, &write_name_snap, ram_slice, &gic_blob, &disk_src,
                        checkpoints, devices, mem_size_snap,
                    )
                }
                Some(p) => {
                    let dirty = dirty_snap.as_ref().expect("tracker checked above").drain();
                    write_named_diff(
                        &store_snap, &write_name_snap, p, ram_slice, &dirty, &gic_blob,
                        &disk_src, checkpoints, devices, mem_size_snap,
                    )
                }
            };

            match result {
                Ok(()) => {
                    *parent_snap.lock().unwrap() = Some(write_name_snap.clone());
                    if dirty_snap.is_some()
                        && let Err(e) = ignition_hvf::vm_protect_memory(
                            layout::RAM_BASE,
                            mem_size_snap,
                            (HV_MEMORY_READ | HV_MEMORY_EXEC) as u64,
                        )
                    {
                        eprintln!("[snapshot] re-protect RAM failed: {e}");
                    }
                }
                Err(e) => eprintln!("[snapshot] write failed: {e}"),
            }

            if let Some(stop) = &rx_stop_snap {
                stop.store(false, Ordering::Release);
            }
        }));
    }
    let q_handler = restore_start.elapsed();

    // Arm dirty tracking on the manager BEFORE it is cloned (set_dirty_config, like
    // set_snapshot_handler, needs sole Arc ownership). Each restored/secondary vCPU
    // thread then sets its dirty window and the run loop handles DirtyFault.
    if let Some(tracker) = &dirty_tracker {
        manager.set_dirty_config(DirtyConfig {
            base: layout::RAM_BASE,
            size: mem_size,
            tracker: tracker.clone(),
        });
    }
    let q_dirty = restore_start.elapsed();

    // Seed the reset point: the restored snapshot IS the default Ctrl-A r target.
    {
        let pristine_dst = inst_dir.join("pristine.bin");
        let _ = fs::remove_file(&pristine_dst);
        let pristine = ignition_vmm::reset::PristineRam::from_clone(&inst_mem, &pristine_dst, mem_size as usize)
            .map_err(|e| io::Error::other(format!("seed pristine clonefile: {e}")))?;
        *manager.reset_point().lock().unwrap() = Some(ignition_vmm::reset::ResetPoint {
            pristine,
            vcpus: snap.vcpus.clone(),
            gic_blob: gic_blob.clone(),
            devices: snap.devices.clone(),
        });
    }
    // Install checkpoint/reset handlers BEFORE `manager` is cloned (spawn_stdin_reader /
    // the GUI VMM thread), because set_*_handler requires sole Arc ownership.
    install_reset_handlers(&mut manager, ResetWiring {
        host_usize: host as usize,
        ram_size: mem_size,
        mem_file: Some(inst_mem.clone()),
        inst_dir: inst_dir.clone(),
        gic: gic.clone(),
        frozen: frozen.clone(),
        dirty: dirty_tracker.clone(),
        rx_stop: rx_stop_reset,
        net_mmio: net_mmio_reset,
        gpu: gpu_handle.clone(),
    });

    spawn_stdin_reader(serial.clone(), termios.saved(), manager.clone(), balloon_target.clone(), balloon.clone());
    let q_stdin = restore_start.elapsed();
    eprintln!("--- restore console attached (quit: Ctrl-A x, balloon: Ctrl-A b, checkpoint: Ctrl-A c, reset: Ctrl-A r) ---");

    // Net restore: present the link as DOWN, then raise it after resume so the
    // guest's carrier-watch sees a down->up edge and re-inits (new MAC + DHCP).
    if let Some(net) = net_mmio_restore {
        net.lock().unwrap().net_set_link(false);
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(1500));
            net.lock().unwrap().net_set_link(true);
        });
    }

    eprintln!("== ignition restore == (no kernel boot; resuming from saved PC)");
    let total = restore_start.elapsed();
    let us = |d: std::time::Duration| d.as_micros();
    log::info!(
        "Restore-breakdown = chain:{}us read:{}us clone:{}us mmap:{}us diff:{}us \
         vm:{}us gic:{}us map:{}us protect:{}us dev:{}us total:{}us",
        us(t_chain),
        us(t_read - t_chain),
        us(t_clone - t_read),
        us(t_mmap - t_clone),
        us(t_diff - t_mmap),
        us(t_vm - t_diff),
        us(t_gic - t_vm),
        us(t_map - t_gic),
        us(t_protect - t_map),
        us(t_dev - t_protect),
        us(total),
    );
    log::info!("Restore-time = {} ms", total.as_millis());
    log::info!(
        "Restore-tail = dev:{}us vsock:{}us freeze:{}us console:{}us handler:{}us dirty:{}us stdin:{}us net:{}us total:{}us",
        us(t_dev),
        us(q_vsock - t_dev),
        us(q_freeze - q_vsock),
        us(q_console - q_freeze),
        us(q_handler - q_console),
        us(q_dirty - q_handler),
        us(q_stdin - q_dirty),
        us(total - q_stdin),
        us(total),
    );
    eprintln!("--- guest console (stdout) ---");
    io::stderr().flush().ok();

    let sb_paths = ignition_sandbox::SandboxPaths {
        readable: vec![store.to_path_buf()],
        writable: [Some(store.to_path_buf()), Some(std::env::temp_dir()),
                   vsock_uds.as_ref().and_then(|u| u.parent().map(PathBuf::from))]
            .into_iter().flatten().collect(),
    };
    // 8. Run. Under --gui the winit event loop must own the main thread (macOS), so
    //    the VMM moves to a spawned thread and the event loop runs on main — mirroring
    //    the fresh-boot --gui split. The restored scanout is repainted once up front so
    //    the window shows the resumed desktop before the guest produces its next FLUSH.
    let run_result = if gui {
        let rx = gui_rx.expect("gui implies a receiver was created");
        // Repaint the resumed desktop from the restored backing into the event loop.
        if let Some(gpu) = &gpu_handle {
            gpu.lock().unwrap().present_scanout();
        }
        let done = std::sync::Arc::new(AtomicBool::new(false));
        let done_vmm = done.clone();
        let mgr_run = manager.clone();
        let vcpus = snap.vcpus;
        std::thread::spawn(move || {
            struct SignalOnExit(std::sync::Arc<AtomicBool>);
            impl Drop for SignalOnExit {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::Release);
                }
            }
            let _signal = SignalOnExit(done_vmm);
            apply_or_exit(&sb_paths, no_sandbox);
            let _ = mgr_run.run_restored(vcpus, Some(gic_blob));
        });
        display_sink::run_event_loop(
            rx,
            done,
            1280,
            800,
            kbd_handle,
            tab_handle,
            1280,
            800,
            Some(manager.clone()),
        );
        Ok(())
    } else {
        apply_or_exit(&sb_paths, no_sandbox);
        // VcpuManager creates + restores the vCPU on the vCPU thread (thread-affinity).
        manager.run_restored(snap.vcpus, Some(gic_blob))
    };

    // Best-effort cleanup of the CoW instance dir (memory.bin + disk.img clones).
    // A Ctrl-A x exit calls process::exit and intentionally skips this — a leftover
    // instance dir is harmless because the base is never mutated.
    let _ = fs::remove_dir_all(&inst_dir);

    run_result.map_err(|e| io::Error::other(format!("run_restored: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_byte_forwards_one() {
        let mut s = EscState::Normal;
        match step(&mut s, b'r') {
            Action::Forward(buf, 1) => assert_eq!(buf[0], b'r'),
            _ => panic!("expected Forward of one byte"),
        }
    }

    #[test]
    fn ctrl_a_then_x_quits() {
        let mut s = EscState::Normal;
        assert!(matches!(step(&mut s, CTRL_A), Action::Pending));
        assert!(matches!(step(&mut s, b'x'), Action::Quit));
    }

    #[test]
    fn ctrl_a_then_other_forwards_both() {
        let mut s = EscState::Normal;
        let _ = step(&mut s, CTRL_A);
        match step(&mut s, b'a') {
            Action::Forward(buf, 2) => assert_eq!(&buf, &[CTRL_A, b'a']),
            _ => panic!("expected Forward of [Ctrl-A, 'a']"),
        }
    }

    #[test]
    fn ctrl_a_twice_forwards_one_literal() {
        let mut s = EscState::Normal;
        let _ = step(&mut s, CTRL_A);
        match step(&mut s, CTRL_A) {
            Action::Forward(buf, 1) => assert_eq!(buf[0], CTRL_A),
            _ => panic!("expected one literal Ctrl-A"),
        }
    }

    #[test]
    fn ctrl_a_then_s_snapshots() {
        let mut s = EscState::Normal;
        assert!(matches!(step(&mut s, CTRL_A), Action::Pending));
        assert!(matches!(step(&mut s, b's'), Action::Snapshot));
    }

    #[test]
    fn ctrl_a_c_is_checkpoint() {
        let mut s = EscState::Normal;
        assert!(matches!(step(&mut s, CTRL_A), Action::Pending));
        assert!(matches!(step(&mut s, b'c'), Action::Checkpoint));
    }

    #[test]
    fn ctrl_a_r_is_reset() {
        let mut s = EscState::Normal;
        assert!(matches!(step(&mut s, CTRL_A), Action::Pending));
        assert!(matches!(step(&mut s, b'r'), Action::Reset));
    }

    #[test]
    fn check_known_ids_accepts_known_and_rejects_unknown() {
        use ignition_vmm::device_manager::DeviceRecord;
        use ignition_devices::device::FdtKind;
        let rec = |id: &str| DeviceRecord {
            id: id.into(), base: 0, size: 0x200, spi: 0,
            fdt_kind: FdtKind::VirtioMmio, state: serde_json::Value::Null,
        };
        let ok = vec![rec("serial"), rec("virtio-blk"), rec("virtio-balloon"), rec("vsock")];
        assert!(super::check_known_ids(&ok).is_ok());
        let bad = vec![rec("serial"), rec("mystery-device")];
        assert!(super::check_known_ids(&bad).is_err());
    }

    #[test]
    fn rx_gate_skips_when_stopped() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let stop = Arc::new(AtomicBool::new(false));
        assert!(super::rx_should_inject(&stop));
        stop.store(true, Ordering::Release);
        assert!(!super::rx_should_inject(&stop));
    }

    #[test]
    fn build_cmdline_without_append_is_default() {
        assert_eq!(build_cmdline(None), layout::default_cmdline());
    }

    #[test]
    fn build_cmdline_appends_extra_args() {
        let got = build_cmdline(Some("init=/sbin/overlay-init"));
        assert!(got.starts_with(&layout::default_cmdline()));
        assert!(got.ends_with(" init=/sbin/overlay-init"));
    }
}
