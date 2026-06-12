// Boot harness: load a real aarch64 Linux kernel + device tree into guest RAM,
// create the in-kernel GIC, and run a vCPU so the kernel's earlycon output
// reaches our 16550 on host stdout.
//
// MUST be codesigned with the hypervisor entitlement before running:
//   cargo build -p hvf-spike --bin boot
//   scripts/sign.sh target/debug/boot
//   target/debug/boot <kernel-Image> [initrd]
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
use hvf::gic::HvfGicV3;
use vmm::vstate::hvf_vcpu::Vcpu;
use vmm::vstate::hvf_vm::Vm;

const RAM_SIZE: u64 = 0x2000_0000; // 512 MiB
const INITRD_OFFSET: u64 = 0x0800_0000; // 128 MiB into RAM (clear of kernel + FDT)

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: {} <kernel-Image> [initrd]", args[0]);
        process::exit(2);
    }
    let kernel_image = fs::read(&args[1]).expect("failed to read kernel image");
    let initrd_bytes = args.get(2).map(|p| fs::read(p).expect("failed to read initrd"));

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

    // The FDT occupies the top FDT_MAX_SIZE of RAM; the kernel and initrd must
    // stay below it. Computed early so the initrd copy can assert against it.
    let fdt_addr = layout::fdt_addr(RAM_SIZE);
    let fdt_off = (fdt_addr - layout::RAM_BASE) as usize;

    // Optional initrd, copied in after the kernel, below the FDT region.
    let initrd = if let Some(ref bytes) = initrd_bytes {
        let off = INITRD_OFFSET as usize;
        let end = off + bytes.len();
        assert!(end <= fdt_off, "initrd overlaps the FDT region");
        ram[off..end].copy_from_slice(bytes);
        Some((layout::RAM_BASE + INITRD_OFFSET, bytes.len() as u64))
    } else {
        None
    };

    // VM, then the in-kernel GIC (must be created before any vCPU).
    let vm = Vm::new(false).expect("hv_vm_create failed (entitlement?)");
    let gic = HvfGicV3::new(1, layout::RAM_BASE).expect("hv_gic_create failed");

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
        initrd,
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
    if let Some((a, s)) = initrd {
        eprintln!("initrd : {s} bytes @ {a:#x}");
    }
    eprintln!("dtb    : {} bytes @ {fdt_addr:#x}", dtb.len());
    eprintln!(
        "gic    : dist=[{:#x}, {:#x}] redist=[{:#x}, {:#x}]",
        g.dist_base, g.dist_size, g.redist_base, g.redist_size
    );
    eprintln!("cmdline: {}", layout::default_cmdline());
    eprintln!("--- guest console (stdout) ---");
    io::stderr().flush().ok();

    // Device bus: one 16550 serial writing the guest console to stdout.
    let serial: Arc<Mutex<dyn BusDevice>> = Arc::new(Mutex::new(Serial::new(io::stdout())));
    let mut bus = Bus::new();
    bus.register(layout::SERIAL_BASE, layout::SERIAL_SIZE, serial);
    let bus = Arc::new(bus);

    // Run. PC=entry, X0=fdt_addr (set by Vcpu/HvfVcpu). Earlycon STRs to the
    // 16550 THR are dispatched MMIO -> Serial -> stdout.
    let vcpu = Vcpu::new(0, entry, fdt_addr, bus);
    match vcpu.start().join().expect("vCPU thread panicked") {
        Ok(()) => eprintln!("\n[vcpu exited cleanly]"),
        Err(e) => eprintln!("\n[vcpu error: {e}]"),
    }
}
