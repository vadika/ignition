// Validation spike for the Firecracker -> macOS/HVF port.
//
// Goal (per HANDOFF.md): confirm the *lifted* libkrun `hvf` crate compiles and
// links against the current macOS SDK, the `com.apple.security.hypervisor`
// entitlement works, and the checked-in HVF bindings still match Apple's ABI
// well enough to create a VM, create a vCPU, map guest RAM, and run code to a
// real exit.
//
// The guest is five hand-assembled aarch64 instructions: it stores a byte to an
// unmapped MMIO address (-> EC_DATAABORT, decoded as VcpuExit::MmioWrite) then
// spins on WFI (-> EC_WFX_TRAP, decoded as VcpuExit::WaitForEvent). Observing
// that exact exit sequence end-to-end validates the whole lifted pipeline.

use std::sync::Arc;

use hvf::{HvfVcpu, HvfVm, Vcpus, VcpuExit};

// Guest physical memory layout.
const GUEST_RAM_BASE: u64 = 0x4000_0000;
const GUEST_RAM_SIZE: u64 = 0x10_0000; // 1 MiB
const MMIO_ADDR: u64 = 0x0900_0000; // matches the movz in the payload below

// Hand-assembled guest payload (see /tmp/guest.s during development):
//   movz x1, #0x0900, lsl #16   ; x1 = 0x09000000
//   movz w0, #0x48              ; 'H'
//   str  w0, [x1]              ; -> EC_DATAABORT (MMIO write)
// 1:wfi                        ; -> EC_WFX_TRAP
//   b 1b
const GUEST_CODE: [u32; 5] = [0xd2a1_2001, 0x5280_0900, 0xb900_0020, 0xd503_207f, 0x17ff_ffff];

/// Minimal stand-in for libkrun's VcpuList. The spike has no interrupt
/// controller and no real sysreg emulation; it just satisfies the trait so
/// `HvfVcpu::run` can drive the vCPU.
struct DummyVcpus;

impl Vcpus for DummyVcpus {
    fn set_vtimer_irq(&self, _vcpuid: u64) {}
    fn should_wait(&self, _vcpuid: u64) -> bool {
        false
    }
    fn has_pending_irq(&self, _vcpuid: u64) -> bool {
        false
    }
    fn get_pending_irq(&self, _vcpuid: u64) -> u32 {
        0
    }
    fn handle_sysreg_read(&self, _vcpuid: u64, _reg: u32) -> Option<u64> {
        Some(0)
    }
    fn handle_sysreg_write(&self, _vcpuid: u64, _reg: u32, _val: u64) -> bool {
        true
    }
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug")).init();

    println!("== hvf-spike: validating lifted libkrun hvf crate on this macOS ==");

    // 1. Create the VM. This is the first call that requires both linking
    //    against Hypervisor.framework and the hypervisor entitlement.
    let vm = HvfVm::new(false).expect("hv_vm_create failed (entitlement? SDK?)");
    println!("[ok] HvfVm::new -> hv_vm_create succeeded");

    // 2. Allocate host-backed guest RAM and write the payload into it.
    let host_addr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            GUEST_RAM_SIZE as usize,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_ANON | libc::MAP_PRIVATE,
            -1,
            0,
        )
    };
    assert!(host_addr != libc::MAP_FAILED, "mmap failed");
    let host_addr = host_addr as u64;

    unsafe {
        let dst = host_addr as *mut u32;
        for (i, insn) in GUEST_CODE.iter().enumerate() {
            // Guest is little-endian; x86/arm host store of LE u32 matches.
            dst.add(i).write(insn.to_le());
        }
    }
    println!("[ok] guest RAM mmap'd at host {host_addr:#x}, payload written");

    // 3. Map host RAM into the guest physical address space.
    vm.map_memory(host_addr, GUEST_RAM_BASE, GUEST_RAM_SIZE)
        .expect("hv_vm_map failed");
    println!("[ok] hv_vm_map: host {host_addr:#x} -> guest {GUEST_RAM_BASE:#x} ({GUEST_RAM_SIZE:#x})");

    // 4. Create the vCPU on THIS thread (HVF vCPUs are thread-affine) and set
    //    initial boot registers: PC=entry, X0=fdt_addr.
    let mut vcpu = HvfVcpu::new(0, false).expect("hv_vcpu_create failed");
    println!("[ok] HvfVcpu::new -> hv_vcpu_create succeeded (id={})", vcpu.id());

    vcpu.set_initial_state(GUEST_RAM_BASE, 0)
        .expect("setting initial vCPU registers failed");
    println!("[ok] set_initial_state: PC={GUEST_RAM_BASE:#x}, X0=0");

    // 5. Run the vCPU and decode exits. Expect MmioWrite then WaitForEvent.
    let vcpus: Arc<dyn Vcpus> = Arc::new(DummyVcpus);
    let mut saw_mmio = false;
    let mut saw_wfe = false;

    for step in 0..16 {
        let exit = vcpu.run(vcpus.clone()).expect("hv_vcpu_run failed");
        println!("[run {step}] exit = {exit:?}");
        match exit {
            VcpuExit::MmioWrite(addr, data) => {
                assert_eq!(addr, MMIO_ADDR, "MMIO write to unexpected address");
                assert_eq!(data.first().copied(), Some(0x48), "expected 'H' (0x48)");
                saw_mmio = true;
            }
            VcpuExit::WaitForEvent
            | VcpuExit::WaitForEventExpired
            | VcpuExit::WaitForEventTimeout(_) => {
                saw_wfe = true;
                break;
            }
            VcpuExit::Canceled => break,
            _ => {}
        }
    }

    assert!(saw_mmio, "never saw the expected MMIO write exit");
    assert!(saw_wfe, "never saw the expected WFE exit");

    println!();
    println!("== SPIKE PASSED ==");
    println!("Lifted hvf crate compiles, links, is entitled, and the bindings");
    println!("match this macOS SDK well enough to create a VM, run a vCPU, and");
    println!("decode EC_DATAABORT (MMIO) + EC_WFX_TRAP (WFI) exits correctly.");
}
