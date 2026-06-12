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
use hvf::gic::HvfGicV3;
use vmm::vstate::hvf_vcpu::Vcpu;
use vmm::vstate::hvf_vm::Vm;

const RAM_SIZE: u64 = 0x2000_0000; // 512 MiB

/// Adapts the in-kernel GIC to the device `IrqLine`. The virtio SPI index is the
/// bare FDT index; hv_gic_set_spi wants the absolute INTID (32 + index).
struct GicIrq(Arc<HvfGicV3>);
impl IrqLine for GicIrq {
    fn set_spi(&self, level: bool) {
        let _ = self.0.set_spi(layout::VIRTIO_SPI + 32, level);
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
    if args.len() < 2 {
        eprintln!("usage: {} <kernel-Image> [rootfs-disk]", args[0]);
        process::exit(2);
    }
    let kernel_image = fs::read(&args[1]).expect("failed to read kernel image");
    let disk_path = args.get(2).cloned();

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
    let vm = Vm::new(false).expect("hv_vm_create failed (entitlement?)");
    let gic = Arc::new(HvfGicV3::new(1, layout::RAM_BASE).expect("hv_gic_create failed"));

    // Build and place the device tree.
    let cfg = FdtConfig {
        mem_base: layout::RAM_BASE,
        mem_size: RAM_SIZE,
        cpu_mpidrs: vec![0],
        cmdline: layout::default_cmdline(),
        serial: MmioDev {
            addr: layout::SERIAL_BASE,
            size: layout::SERIAL_SIZE,
            irq: layout::SERIAL_SPI,
        },
        gic: gic.fdt_info(),
        initrd: None,
        virtio: disk_path
            .as_ref()
            .map(|_| MmioDev { addr: layout::VIRTIO_BASE, size: layout::VIRTIO_SIZE, irq: layout::VIRTIO_SPI }),
    };
    let dtb = fdt::generate(&cfg).expect("fdt generate failed");
    assert!(fdt_off + dtb.len() <= ram.len(), "DTB does not fit in RAM");
    ram[fdt_off..fdt_off + dtb.len()].copy_from_slice(&dtb);

    // Map the populated RAM into the guest.
    vm.hvf
        .map_memory(host_addr, layout::RAM_BASE, RAM_SIZE)
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
    let serial: Arc<Mutex<dyn BusDevice>> = Arc::new(Mutex::new(Serial::new(FlushWriter)));
    bus.register(layout::SERIAL_BASE, layout::SERIAL_SIZE, serial);

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
        let virtio: Arc<Mutex<dyn BusDevice>> =
            Arc::new(Mutex::new(VirtioMmio::new(blk, guest_ram, Arc::new(GicIrq(gic.clone())))));
        bus.register(layout::VIRTIO_BASE, layout::VIRTIO_SIZE, virtio);
        eprintln!("virtio : /dev/vda backed by {path}");
    }
    let bus = Arc::new(bus);

    // Run. Earlycon + virtio MMIO exits are dispatched through the bus.
    let vcpu = Vcpu::new(0, entry, fdt_addr, bus);
    match vcpu.start().join().expect("vCPU thread panicked") {
        Ok(()) => eprintln!("\n[vcpu exited cleanly]"),
        Err(e) => eprintln!("\n[vcpu error: {e}]"),
    }
}
