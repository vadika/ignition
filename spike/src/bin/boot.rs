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

use arch::aarch64::fdt::{self, FdtConfig, MmioDev};
use arch::aarch64::{kernel, layout};
use devices::bus::{Bus, BusDevice};
use devices::serial::Serial;
use devices::virtio::IrqLine;
use devices::virtio::blk::VirtioBlk;
use devices::virtio::guest_ram::GuestRam;
use devices::virtio::mmio::VirtioMmio;
use devices::virtio::net::VirtioNet;
use hvf::gic::HvfGicV3;
use hvf::HvfVcpu;
use vmm::snapshot::{self, MmioWindow, VmConfig, VmSnapshot};
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
            }
        }
    });
}

/// Adapts the in-kernel GIC to the device `IrqLine`. The virtio SPI index is the
/// bare FDT index; hv_gic_set_spi wants the absolute INTID (32 + index).
struct GicIrq {
    gic: Arc<HvfGicV3>,
    /// Absolute GIC INTID (SPI index + 32).
    intid: u32,
}
impl IrqLine for GicIrq {
    fn set_spi(&self, level: bool) {
        let _ = self.gic.set_spi(self.intid, level);
    }
}

/// Unbuffered stdout sink for the guest console: writes each byte straight
/// through and flushes, so a newline-less prompt is visible immediately.
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

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args: Vec<String> = env::args().collect();
    // Parse `--smp N` (default 1, capped at 8); kernel/rootfs stay positional.
    // Cap matches the FDT/GIC sizing we exercise; raise if a guest needs more.
    const MAX_VCPUS: u64 = 8;
    let mut smp: u64 = 1;
    let mut net = false;
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
        match run_restore(&dir) {
            Ok(()) => eprintln!("\n[restore exited cleanly]"),
            Err(e) => {
                eprintln!("\n[restore error: {e}]");
                process::exit(1);
            }
        }
        return;
    }

    if positionals.is_empty() {
        eprintln!("usage: {} [--smp N] [--net] [--snap-dir <dir>] <kernel-Image> [rootfs-disk]", args[0]);
        process::exit(2);
    }
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

    // Build and place the device tree.
    let mut fdt_devices = vec![fdt::FdtDevice::Serial(MmioDev {
        addr: layout::SERIAL_BASE,
        size: layout::SERIAL_SIZE,
        irq: layout::SERIAL_SPI,
    })];
    if disk_path.is_some() {
        fdt_devices.push(fdt::FdtDevice::VirtioBlk(MmioDev {
            addr: layout::VIRTIO_BASE,
            size: layout::VIRTIO_SIZE,
            irq: layout::VIRTIO_SPI,
        }));
    }
    if net {
        fdt_devices.push(fdt::FdtDevice::VirtioNet(MmioDev {
            addr: layout::NET_BASE,
            size: layout::NET_SIZE,
            irq: layout::NET_SPI,
        }));
    }
    let cfg = FdtConfig {
        mem_base: layout::RAM_BASE,
        mem_size: RAM_SIZE,
        cpu_mpidrs: (0..smp).map(mpidr_for).collect(),
        cmdline: layout::default_cmdline(),
        devices: fdt_devices,
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

    // Device bus: 16550 serial to stdout, plus an optional virtio-blk disk.
    // Flush each byte: a prompt like "login: " has no trailing newline and would
    // otherwise sit forever in stdout's line buffer, looking like a hang.
    let mut bus = Bus::new();
    let serial_irq = Arc::new(GicIrq { gic: gic.clone(), intid: layout::SERIAL_SPI + 32 });
    let serial: Arc<Mutex<Serial<FlushWriter>>> =
        Arc::new(Mutex::new(Serial::with_irq(FlushWriter, serial_irq)));
    let serial_bus: Arc<Mutex<dyn BusDevice>> = serial.clone();
    bus.register(layout::SERIAL_BASE, layout::SERIAL_SIZE, serial_bus)
        .expect("serial range overlap");

    // Keep a typed Arc for the snapshot handler (None when no disk).
    let virtio_blk_mmio: Option<Arc<Mutex<VirtioMmio>>>;

    if let Some(path) = &disk_path {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .expect("failed to open rootfs disk");
        let blk = VirtioBlk::new(file).expect("virtio-blk init failed");
        // SAFETY: the host mapping outlives the run; the device touches it only
        // during MMIO exits, when the guest is paused.
        let guest_ram = GuestRam::new(host as *mut u8, RAM_SIZE as usize, layout::RAM_BASE);
        let virtio: Arc<Mutex<VirtioMmio>> =
            Arc::new(Mutex::new(VirtioMmio::new(
                Box::new(blk),
                guest_ram,
                Arc::new(GicIrq { gic: gic.clone(), intid: layout::VIRTIO_SPI + 32 }),
            )));
        let virtio_bus: Arc<Mutex<dyn BusDevice>> = virtio.clone();
        bus.register(layout::VIRTIO_BASE, layout::VIRTIO_SIZE, virtio_bus)
            .expect("virtio range overlap");
        virtio_blk_mmio = Some(virtio);
        eprintln!("virtio : /dev/vda backed by {path}");
    } else {
        virtio_blk_mmio = None;
    }

    if net {
        let (backend, rx) = ignition_vmnet::VmnetBackend::start()
            .expect("vmnet start failed (run boot under sudo for --net)");
        let net_irq = Arc::new(GicIrq { gic: gic.clone(), intid: layout::NET_SPI + 32 });
        let guest_ram_net = GuestRam::new(host as *mut u8, RAM_SIZE as usize, layout::RAM_BASE);
        let net_dev = VirtioNet::new(backend);
        let net_mmio: Arc<Mutex<VirtioMmio>> =
            Arc::new(Mutex::new(VirtioMmio::new(Box::new(net_dev), guest_ram_net, net_irq)));
        let net_bus: Arc<Mutex<dyn BusDevice>> = net_mmio.clone();
        bus.register(layout::NET_BASE, layout::NET_SIZE, net_bus)
            .expect("net range overlap");
        let net_rx = net_mmio.clone();
        std::thread::spawn(move || {
            for frame in rx {
                net_rx.lock().unwrap().inject_rx(&frame);
            }
        });
        eprintln!("virtio-net: enabled (vmnet shared/NAT)");
    }

    let bus = Arc::new(bus);

    // Build the VcpuManager and (for single-vCPU + disk + no-net) install the
    // snapshot handler before run.
    let mut manager = VcpuManager::new(smp, bus);

    if smp == 1 && !net {
        // Build a closure capturing all state needed to write the snapshot.
        // Clone Arcs and scalars; raw pointer is captured as usize for Send+Sync.
        let gic_snap = gic.clone();
        let serial_snap = serial.clone();
        let blk_snap = virtio_blk_mmio.clone();
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
            let blk_state = blk_snap.as_ref().map(|v| v.lock().unwrap().save());
            let serial_state = serial_snap.lock().unwrap().save();

            // Build VmSnapshot.
            let blk_mmio_state = match blk_state {
                Some(s) => s,
                None => {
                    // No disk — snapshot of a diskless guest is degenerate but allowed.
                    devices::virtio::mmio::VirtioMmioState {
                        status: 0, queue_sel: 0, device_features_sel: 0,
                        interrupt_status: 0, queues: vec![],
                    }
                }
            };
            let config = VmConfig {
                mem_size: RAM_SIZE,
                vcpu_count: 1,
                serial: MmioWindow {
                    base: layout::SERIAL_BASE,
                    size: layout::SERIAL_SIZE,
                    spi: layout::SERIAL_SPI,
                },
                blk: MmioWindow {
                    base: layout::VIRTIO_BASE,
                    size: layout::VIRTIO_SIZE,
                    spi: layout::VIRTIO_SPI,
                },
            };
            let devices_state = vmm::snapshot::DeviceState {
                blk: blk_mmio_state,
                serial: serial_state,
            };
            let snap = VmSnapshot::new(config, vcpu_state, devices_state);

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
    spawn_stdin_reader(serial.clone(), termios.saved(), manager.clone());
    eprintln!("--- console attached (quit: Ctrl-A x, snapshot: Ctrl-A s), {smp} vCPU(s) ---");

    // Run. Earlycon + virtio MMIO exits are dispatched through the bus.
    match manager.run(entry, fdt_addr) {
        Ok(()) => eprintln!("\n[vcpus exited cleanly]"),
        Err(e) => eprintln!("\n[vcpu error: {e}]"),
    }
}

/// Restore a snapshot from `dir` and resume the guest.
/// Does NOT load a kernel, generate an FDT, or call set_initial_state.
/// The running kernel + DTB are already in memory.bin.
fn run_restore(dir: &Path) -> io::Result<()> {
    use devices::bus::BusDevice;

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

    // 6. Copy disk.img to a private instance so clones are independent.
    let instance_disk = std::env::temp_dir().join(format!("ignition-instance-{}.img", process::id()));
    fs::copy(&paths.disk, &instance_disk)?;
    let disk_file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&instance_disk)
        .map_err(|e| io::Error::other(format!("open instance disk: {e}")))?;

    // 7. Build the bus from the restored device state.
    let mut bus = Bus::new();

    // Serial.
    let serial_irq: Arc<dyn IrqLine> =
        Arc::new(GicIrq { gic: gic.clone(), intid: snap.config.serial.spi + 32 });
    let serial: Arc<Mutex<Serial<FlushWriter>>> = Arc::new(Mutex::new(
        Serial::from_snapshot(FlushWriter, serial_irq, &snap.devices.serial),
    ));
    let serial_bus: Arc<Mutex<dyn BusDevice>> = serial.clone();
    bus.register(snap.config.serial.base, snap.config.serial.size, serial_bus)
        .map_err(|e| io::Error::other(format!("serial bus register: {e:?}")))?;

    // Virtio-blk.
    let blk = VirtioBlk::new(disk_file)
        .map_err(|e| io::Error::other(format!("VirtioBlk::new: {e}")))?;
    let guest_ram = GuestRam::new(host as *mut u8, RAM_SIZE as usize, layout::RAM_BASE);
    let blk_irq: Arc<dyn IrqLine> =
        Arc::new(GicIrq { gic: gic.clone(), intid: snap.config.blk.spi + 32 });
    let mut blk_mmio = VirtioMmio::new(Box::new(blk), guest_ram, blk_irq);
    blk_mmio.restore(&snap.devices.blk);
    let blk_arc: Arc<Mutex<VirtioMmio>> = Arc::new(Mutex::new(blk_mmio));
    let blk_bus: Arc<Mutex<dyn BusDevice>> = blk_arc.clone();
    bus.register(snap.config.blk.base, snap.config.blk.size, blk_bus)
        .map_err(|e| io::Error::other(format!("blk bus register: {e:?}")))?;

    let bus = Arc::new(bus);

    // 8. Set up the interactive console (raw terminal + stdin reader).
    let termios = TermiosGuard::new();
    let manager = VcpuManager::new(1, bus);
    spawn_stdin_reader(serial.clone(), termios.saved(), manager.clone());
    eprintln!("--- restore console attached (quit: Ctrl-A x) ---");

    eprintln!("== ignition restore == (no kernel boot; resuming from saved PC)");
    eprintln!("--- guest console (stdout) ---");
    io::stderr().flush().ok();

    // 9. Run: VcpuManager creates + restores the vCPU on the vCPU thread (thread-affinity).
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
}
