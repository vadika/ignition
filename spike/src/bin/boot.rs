// Boot harness: load a real aarch64 Linux kernel + device tree into guest RAM,
// create the in-kernel GIC, and run a vCPU so the kernel's earlycon output
// reaches our 16550 on host stdout.
//
// MUST be codesigned with the hypervisor entitlement before running:
//   cargo build -p hvf-spike --bin boot
//   scripts/sign.sh target/debug/boot
//   target/debug/boot <kernel-Image> [rootfs-disk]
//
// Our diagnostics go to stderr; the guest console goes to stdout, so the kernel's
// earlycon lines are cleanly separable.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::{env, fs, process};

use arch::aarch64::fdt::{self, FdtConfig};
use arch::aarch64::{kernel, layout};
use devices::boot_timer::BootTimer;
use devices::rtc::Pl031;
use devices::serial::Serial;
use devices::virtio::balloon::Balloon;
use devices::virtio::blk::VirtioBlk;
use devices::virtio::guest_ram::GuestRam;
use devices::virtio::mmio::VirtioMmio;
use devices::virtio::net::VirtioNet;
use devices::virtio::rng::VirtioRng;
use devices::virtio::vsock::VsockDevice;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use hvf::gic::HvfGicV3;
use hvf::HvfVcpu;
use vmm::device_manager::{DeviceManager, DeviceRecord};
use vmm::snapshot::{self, VmConfig, VmSnapshot};
use vmm::vstate::vcpu_manager::{mpidr_for, VcpuManager};
use vmm::vstate::hvf_vm::Vm;

const RAM_SIZE: u64 = 0x2000_0000; // 512 MiB

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

/// Spawn a thread that reads host stdin one byte at a time, runs the escape
/// state machine, and feeds forwarded bytes into the serial's RX FIFO. On
/// Ctrl-A x it restores the terminal and exits the process. On Ctrl-A s it
/// requests a snapshot via the manager. On EOF/error it stops reading and
/// leaves the guest running.
fn spawn_stdin_reader(
    serial: Arc<Mutex<Serial<FlushWriter>>>,
    saved_termios: Option<libc::termios>,
    manager: Arc<vmm::vstate::vcpu_manager::VcpuManager>,
    balloon_target: Arc<AtomicU32>,
    balloon: Arc<Mutex<devices::virtio::mmio::VirtioMmio>>,
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

/// Poll the vsock device's host connection fds and drive RX. A 200 ms timeout also
/// re-checks for newly-connected fds (TX adds connections between ticks).
fn spawn_vsock_reactor(vsock: Arc<Mutex<devices::virtio::mmio::VirtioMmio>>) {
    use std::os::unix::io::RawFd;
    std::thread::spawn(move || loop {
        let fds: Vec<RawFd> = { vsock.lock().unwrap().vsock_poll_set() };
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
}

impl DeviceContext {
    fn guest_ram(&self) -> GuestRam {
        GuestRam::new(self.host, self.ram_size as usize, layout::RAM_BASE)
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
    D: devices::device::MmioDevice + 'static,
    F: FnOnce(Arc<dyn devices::virtio::IrqLine>) -> D,
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
#[expect(dead_code, reason = "wired up in a later task")]
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

    // virtio-net — boot-only (snapshots are blocked under --net, so no restore arm).
    if ctx.net {
        let (backend, rx) = ignition_vmnet::VmnetBackend::start()
            .expect("vmnet start failed (run boot under sudo for --net)");
        let mem = ctx.guest_ram();
        let net_dev = VirtioNet::new(backend);
        if let Some(h) = place(mgr, &mode, "virtio-net", layout::MMIO_WINDOW,
            move |irq| VirtioMmio::new("virtio-net", Box::new(net_dev), mem, irq))? {
            let h2 = h.clone();
            std::thread::spawn(move || {
                for frame in rx { h2.lock().unwrap().inject_rx(&frame); }
            });
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

    Ok(())
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args: Vec<String> = env::args().collect();
    // Parse `--smp N` (default 1, capped at 8); kernel/rootfs stay positional.
    // Cap matches the FDT/GIC sizing we exercise; raise if a guest needs more.
    const MAX_VCPUS: u64 = 8;
    let mut smp: u64 = 1;
    let mut net = false;
    let mut vsock_uds: Option<PathBuf> = None;
    let mut snap_dir: PathBuf = PathBuf::from("./snapshot");
    let mut restore_dir: Option<PathBuf> = None;
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
            "--net" => {
                net = true;
            }
            "--snap-dir" => {
                let v = it.next().expect("--snap-dir needs a path");
                snap_dir = PathBuf::from(v);
            }
            "--vsock-uds" => {
                let v = it.next().expect("--vsock-uds needs a path");
                vsock_uds = Some(PathBuf::from(v));
            }
            "--restore" => {
                let v = it.next().expect("--restore needs a directory path");
                restore_dir = Some(PathBuf::from(v));
            }
            other if other.starts_with('-') => {
                eprintln!("unknown flag: {other}");
                process::exit(2);
            }
            other => positionals.push(other.to_string()),
        }
    }

    // Restore path: skip normal boot entirely.
    if let Some(dir) = restore_dir {
        match run_restore(&dir, vsock_uds) {
            Ok(()) => eprintln!("\n[restore exited cleanly]"),
            Err(e) => {
                eprintln!("\n[restore error: {e}]");
                process::exit(1);
            }
        }
        return;
    }

    if positionals.is_empty() {
        eprintln!("usage: {} [--smp N] [--net] [--vsock-uds <path>] [--snap-dir <dir>] <kernel-Image> [rootfs-disk]", args[0]);
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
            RAM_SIZE as usize,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_ANON | libc::MAP_PRIVATE,
            -1,
            0,
        )
    };
    assert!(host != libc::MAP_FAILED, "mmap failed");
    let host_addr = host as u64;
    let ram: &mut [u8] = unsafe { std::slice::from_raw_parts_mut(host as *mut u8, RAM_SIZE as usize) };

    // Load the kernel; entry is where the vCPU's PC starts.
    let entry = kernel::load_kernel(ram, layout::RAM_BASE, &kernel_image).expect("load_kernel failed");

    // The FDT occupies the top FDT_MAX_SIZE of RAM; the kernel must stay below it.
    let fdt_addr = layout::fdt_addr(RAM_SIZE);
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

    let mut ctx = DeviceContext {
        host: host as *mut u8,
        ram_size: RAM_SIZE,
        disk: disk_path.as_ref().map(PathBuf::from),
        vsock_uds: vsock_uds.clone(),
        net,
        serial: None, balloon_target: None, balloon: None, vsock_mmio: None, net_mmio: None,
    };
    setup_devices(&mut mgr, &mut ctx, Mode::Boot).expect("device setup failed");
    let serial = ctx.serial.clone().expect("serial device");
    let balloon_target = ctx.balloon_target.clone().expect("balloon target");
    let balloon = ctx.balloon.clone().expect("balloon device");
    if let Some(vsock_mmio) = ctx.vsock_mmio.clone() {
        spawn_vsock_reactor(vsock_mmio);
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
        mem_size: RAM_SIZE,
        cpu_mpidrs: (0..smp).map(mpidr_for).collect(),
        cmdline: layout::default_cmdline(),
        devices: mgr.fdt_devices(),
        gic: gic.fdt_info(),
        initrd: None,
    };
    let dtb = fdt::generate(&cfg).expect("fdt generate failed");
    assert!(fdt_off + dtb.len() <= ram.len(), "DTB does not fit in RAM");
    ram[fdt_off..fdt_off + dtb.len()].copy_from_slice(&dtb);

    // Map the populated RAM into the guest.
    vm.map_memory(host_addr, layout::RAM_BASE, RAM_SIZE)
        .expect("hv_vm_map failed");

    // Diagnostics (stderr) so a silent boot is debuggable.
    let g = gic.fdt_info();
    eprintln!("== ignition boot ==");
    eprintln!("kernel : {} bytes, entry={entry:#x}", kernel_image.len());
    eprintln!("dtb    : {} bytes @ {fdt_addr:#x}", dtb.len());
    eprintln!(
        "gic    : dist=[{:#x}, {:#x}] redist=[{:#x}, {:#x}]",
        g.dist_base, g.dist_size, g.redist_base, g.redist_size
    );
    eprintln!("cmdline: {}", layout::default_cmdline());
    eprintln!("--- guest console (stdout) ---");
    io::stderr().flush().ok();

    // Freeze the device set: transfers bus ownership to the run loop.
    let frozen = Arc::new(mgr.freeze());
    let bus = frozen.bus();

    // Build the VcpuManager and (for single-vCPU + no-net) install the
    // snapshot handler before run.
    let mut manager = VcpuManager::new(smp, bus);

    if smp == 1 && !net {
        // Build a closure capturing all state needed to write the snapshot.
        // Clone Arcs and scalars; raw pointer is captured as usize for Send+Sync.
        let gic_snap = gic.clone();
        let snap_devices = frozen.clone();
        let disk_path_snap = disk_path.clone();
        let snap_dir_snap = snap_dir.clone();
        // The guest RAM base pointer captured as usize: raw *const u8 is neither
        // Send nor Sync, but usize is. Sound because the closure only reads the
        // slice at a Canceled exit, when the (single, non-net) vCPU is paused
        // and is the sole RAM writer. Rust 2021 partial-capture would see through
        // a newtype wrapper and capture the *const u8 field directly, defeating
        // the unsafe impl — so usize is the correct approach here.
        let host_usize = host as usize;

        manager.set_snapshot_handler(Box::new(move |vcpu: &HvfVcpu| {
            // All of this runs on the vCPU thread.
            let vcpu_state = match vcpu.save_state() {
                Ok(s) => s,
                Err(e) => { eprintln!("[snapshot] save_state failed: {e}"); return; }
            };
            let gic_blob = match gic_snap.save_state() {
                Ok(b) => b,
                Err(e) => { eprintln!("[snapshot] gic save_state failed: {e}"); return; }
            };

            let devices = snap_devices.save();
            let config = VmConfig { mem_size: RAM_SIZE, vcpu_count: 1 };
            let snap = VmSnapshot::new(config, vcpu_state, devices);

            // The RAM slice — host_usize round-trip avoids capturing *const u8.
            let ram_slice: &[u8] = unsafe {
                std::slice::from_raw_parts(host_usize as *const u8, RAM_SIZE as usize)
            };

            let disk_src = match &disk_path_snap {
                Some(p) => PathBuf::from(p),
                None => {
                    // Write an empty placeholder so the snapshot dir is complete.
                    let placeholder = snap_dir_snap.join("disk.img");
                    let _ = std::fs::write(&placeholder, b"");
                    placeholder
                }
            };

            match snapshot::write_snapshot(&snap_dir_snap, &snap, ram_slice, &gic_blob, &disk_src) {
                Ok(()) => eprintln!("[snapshot] written to {}", snap_dir_snap.display()),
                Err(e) => eprintln!("[snapshot] write failed: {e}"),
            }
        }));
    } else if !net {
        eprintln!("[snapshot] handler not installed: smp={smp} (snapshot is single-vCPU only)");
    } else {
        eprintln!("[snapshot] handler not installed: --net active (snapshot requires no net)");
    }

    // Raw terminal + host stdin reader for the interactive console. The guard
    // restores the terminal on drop (guest-initiated exit); the reader restores
    // it before process::exit on Ctrl-A x.
    let termios = TermiosGuard::new();
    spawn_stdin_reader(serial.clone(), termios.saved(), manager.clone(), balloon_target.clone(), balloon.clone());
    eprintln!("--- console attached (quit: Ctrl-A x, snapshot: Ctrl-A s, balloon: Ctrl-A b), {smp} vCPU(s) ---");

    // Run. Earlycon + virtio MMIO exits are dispatched through the bus.
    match manager.run(entry, fdt_addr) {
        Ok(()) => eprintln!("\n[vcpus exited cleanly]"),
        Err(e) => eprintln!("\n[vcpu error: {e}]"),
    }
}

/// Restore a snapshot from `dir` and resume the guest.
/// Does NOT load a kernel, generate an FDT, or call set_initial_state.
/// The running kernel + DTB are already in memory.bin.
fn run_restore(dir: &Path, vsock_uds: Option<PathBuf>) -> io::Result<()> {
    // Host-side restore clock: mmap + memory.bin load + GIC/device/vCPU state
    // restore, up to handing the guest to the run loop. The boot-timer device
    // can't measure restore (the guest's init does not re-run), so this is the
    // restore analog of `Guest-boot-time`.
    let restore_start = std::time::Instant::now();
    // 1. Read the snapshot metadata.
    let (snap, gic_blob, paths) = snapshot::read_snapshot(dir)?;
    assert_eq!(
        snap.config.vcpu_count, 1,
        "restore only supports single-vCPU snapshots"
    );
    assert_eq!(
        snap.config.mem_size, RAM_SIZE,
        "snapshot mem_size does not match this binary's RAM_SIZE"
    );

    // 2. Allocate guest RAM and load memory.bin into it.
    let host = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            RAM_SIZE as usize,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_ANON | libc::MAP_PRIVATE,
            -1,
            0,
        )
    };
    assert!(host != libc::MAP_FAILED, "mmap failed");
    let host_addr = host as u64;
    let ram: &mut [u8] =
        unsafe { std::slice::from_raw_parts_mut(host as *mut u8, RAM_SIZE as usize) };
    let mem_bytes = fs::read(&paths.memory)?;
    assert_eq!(
        mem_bytes.len(),
        snap.config.mem_size as usize,
        "memory.bin length != snap.config.mem_size"
    );
    ram.copy_from_slice(&mem_bytes);
    drop(mem_bytes);

    // 3. Create the HVF VM (must precede GIC and vCPU creation).
    let mut vm = Vm::new(false).map_err(|e| io::Error::other(format!("Vm::new: {e}")))?;

    // 4. Create the in-kernel GIC (same placement as a fresh boot). Its saved
    //    distributor/redistributor state is restored later via `gic_restore`, after
    //    the vCPU exists (see VcpuManager::run_restored / gic_restore).
    let gic = Arc::new(
        HvfGicV3::new(snap.config.vcpu_count, layout::RAM_BASE)
            .map_err(|e| io::Error::other(format!("GIC create: {e}")))?,
    );

    // 5. Map the populated RAM into the guest.
    vm.map_memory(host_addr, layout::RAM_BASE, RAM_SIZE)
        .map_err(|e| io::Error::other(format!("hv_vm_map: {e}")))?;

    // 6. Restore devices at their saved addresses via DeviceManager.
    let mut mgr = DeviceManager::new(
        gic.clone(),
        layout::MMIO_BASE,
        layout::MMIO_LEN,
        layout::SPI_BASE,
        layout::SPI_COUNT,
    );
    // Private disk instance so clones are independent (only if the snapshot has a disk).
    let disk = if snap.devices.iter().any(|r| r.id == "virtio-blk") {
        let instance_disk = std::env::temp_dir()
            .join(format!("ignition-instance-{}.img", process::id()));
        fs::copy(&paths.disk, &instance_disk)?;
        Some(instance_disk)
    } else {
        None
    };

    let mut ctx = DeviceContext {
        host: host as *mut u8,
        ram_size: RAM_SIZE,
        disk,
        vsock_uds: vsock_uds.clone(),
        net: false, // snapshots never contain net
        serial: None, balloon_target: None, balloon: None, vsock_mmio: None, net_mmio: None,
    };
    setup_devices(&mut mgr, &mut ctx, Mode::Restore(&snap.devices))?;

    let serial = ctx.serial.clone().ok_or_else(|| io::Error::other("snapshot had no serial device"))?;
    let balloon_target = ctx.balloon_target.clone()
        .ok_or_else(|| io::Error::other("snapshot had no balloon device"))?;
    let balloon = ctx.balloon.clone()
        .ok_or_else(|| io::Error::other("snapshot had no balloon device"))?;
    if let Some(vsock_mmio) = ctx.vsock_mmio.clone() {
        spawn_vsock_reactor(vsock_mmio);
    }
    let frozen = mgr.freeze();
    let bus = frozen.bus();

    // 7. Set up the interactive console (raw terminal + stdin reader).
    let termios = TermiosGuard::new();
    let manager = VcpuManager::new(1, bus);
    spawn_stdin_reader(serial.clone(), termios.saved(), manager.clone(), balloon_target.clone(), balloon.clone());
    eprintln!("--- restore console attached (quit: Ctrl-A x, balloon: Ctrl-A b) ---");

    eprintln!("== ignition restore == (no kernel boot; resuming from saved PC)");
    log::info!("Restore-time = {} ms", restore_start.elapsed().as_millis());
    eprintln!("--- guest console (stdout) ---");
    io::stderr().flush().ok();

    // 8. Run: VcpuManager creates + restores the vCPU on the vCPU thread (thread-affinity).
    match manager.run_restored(snap.vcpu, Some(gic_blob)) {
        Ok(()) => {}
        Err(e) => return Err(io::Error::other(format!("run_restored: {e}"))),
    }
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
    fn check_known_ids_accepts_known_and_rejects_unknown() {
        use vmm::device_manager::DeviceRecord;
        use devices::device::FdtKind;
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
}
