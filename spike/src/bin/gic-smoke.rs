// End-to-end GIC smoke test.
//
// MUST be codesigned with the hypervisor entitlement before running:
//   cargo build -p hvf-spike --bin gic-smoke
//   scripts/sign.sh target/debug/gic-smoke
//   target/debug/gic-smoke
//
// Creates a VM, then the in-kernel GICv3, asserts its placement, exercises
// set_spi, and confirms the GIC placement composes with FDT generation.

use arch::aarch64::fdt::{FdtConfig, FdtDevice, MmioDev};
use hvf::gic::HvfGicV3;
use vmm::vstate::hvf_vm::Vm;

// Throwaway; the real layout module lands with the kernel loader in 2c.
const GIC_TOP: u64 = 0x4000_0000; // guest RAM base; GIC sits just below

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // GIC must be created after the VM and before any vCPU. Hold the VM for the
    // whole run (`_vm_guard`, not `_`, keeps it alive to end of scope).
    let _vm_guard = Vm::new(false).expect("hv_vm_create failed (entitlement?)");
    let gic = HvfGicV3::new(1, GIC_TOP).expect("hv_gic_create failed");

    let info = gic.fdt_info();
    println!(
        "[ok] GIC created: dist=[{:#x}, {:#x}] redist=[{:#x}, {:#x}] maint_irq={}",
        info.dist_base, info.dist_size, info.redist_base, info.redist_size, info.maint_irq
    );

    // dist_size/redist_size come from HVF (hv_gic_get_*_size) — independent.
    // The base equalities below re-derive from new()'s own arithmetic, so they
    // check struct round-trip fidelity, not independent hardware placement.
    assert!(info.dist_size > 0 && info.redist_size > 0, "zero GIC region size");
    assert_eq!(info.redist_base, GIC_TOP - info.redist_size);
    assert_eq!(info.dist_base, GIC_TOP - info.dist_size - info.redist_size);
    assert!(info.dist_base < info.redist_base, "dist must be below redist");
    assert!(info.redist_base < GIC_TOP, "redist must be below gic_top");

    // Assert + deassert the first SPI (intid 32).
    gic.set_spi(32, true).expect("set_spi assert failed");
    gic.set_spi(32, false).expect("set_spi deassert failed");
    println!("[ok] set_spi(32, true/false) succeeded");

    // GIC placement composes with FDT generation.
    let cfg = FdtConfig {
        mem_base: 0x4000_0000,
        mem_size: 0x2000_0000,
        cpu_mpidrs: vec![0],
        cmdline: "console=ttyS0".to_string(),
        devices: vec![FdtDevice::Serial(MmioDev { addr: 0x0900_0000, size: 0x1000, irq: 33 })],
        gic: info,
        initrd: None,
    };
    let blob = arch::aarch64::fdt::generate(&cfg).expect("fdt generate failed");
    assert!(!blob.is_empty(), "empty DTB");
    println!("[ok] FDT generated from GIC info ({} bytes)", blob.len());

    println!("== GIC-SMOKE PASSED ==");
}
