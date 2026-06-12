// End-to-end UART-echo milestone check.
//
// MUST be codesigned with the hypervisor entitlement before running:
//   cargo build -p hvf-spike --bin uart-echo
//   scripts/sign.sh target/debug/uart-echo
//   target/debug/uart-echo
//
// A hand-assembled guest writes "IGNITION\n" to the 16550 THR then issues PSCI
// SYSTEM_OFF. Output is captured and asserted equal to "IGNITION\n".

use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use devices::bus::{Bus, BusDevice};
use devices::serial::Serial;
use vmm::vstate::hvf_vcpu::Vcpu;
use vmm::vstate::hvf_vm::Vm;

const GUEST_RAM_BASE: u64 = 0x4000_0000;
const GUEST_RAM_SIZE: u64 = 0x10_0000; // 1 MiB
const SERIAL_BASE: u64 = 0x0900_0000;
const SERIAL_LEN: u64 = 0x1000;

// Hand-assembled aarch64 (clang -target arm64-apple-macos): 11 instruction
// words + 3 u32 words encoding "IGNITION\n" (9 bytes + 3 null pad). Asm source
// is in docs/superpowers/specs/2026-06-12-phase1-uart-echo-design.md:
//   movz x1,#0x0900,lsl#16 ; adr x2,msg ; mov x3,#9
//   loop: ldrb w0,[x2],#1 ; strb w0,[x1] ; subs x3,#1 ; b.ne loop
//   movz x0,#0x0008 ; movk x0,#0x8400,lsl#16 ; hvc #0 ; b .
//   msg: "IGNITION\n"
const GUEST_CODE: [u32; 14] = [
    0xd2a1_2001, 0x1000_0142, 0xd280_0123, 0x3840_1440, 0x3900_0020, 0xf100_0463,
    0x54ff_ffa1, 0xd280_0100, 0xf2b0_8000, 0xd400_0002, 0x1400_0000,
    0x494e_4749, 0x4e4f_4954, 0x0000_000a,
];

/// `Write` sink capturing into a shared buffer.
#[derive(Clone)]
struct SharedSink(Arc<Mutex<Vec<u8>>>);
impl Write for SharedSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let mut vm = Vm::new(false).expect("hv_vm_create failed (entitlement?)");

    // Allocate + populate guest RAM. No munmap: the process exits right after
    // and the OS reclaims the mapping.
    let host = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            GUEST_RAM_SIZE as usize,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_ANON | libc::MAP_PRIVATE,
            -1,
            0,
        )
    };
    assert!(host != libc::MAP_FAILED, "mmap failed");
    unsafe {
        let dst = host as *mut u32;
        for (i, word) in GUEST_CODE.iter().enumerate() {
            dst.add(i).write(word.to_le());
        }
    }
    vm.map_memory(host as u64, GUEST_RAM_BASE, GUEST_RAM_SIZE)
        .expect("hv_vm_map failed");

    // Wire the device bus: one serial at SERIAL_BASE, output captured.
    let captured = Arc::new(Mutex::new(Vec::new()));
    let serial: Arc<Mutex<dyn BusDevice>> =
        Arc::new(Mutex::new(Serial::new(SharedSink(captured.clone()))));
    let mut bus = Bus::new();
    bus.register(SERIAL_BASE, SERIAL_LEN, serial)
        .expect("serial range overlap");
    let bus = Arc::new(bus);

    // Run the vCPU to shutdown.
    // Vcpu::new(mpidr, entry, fdt_addr, bus); this guest ignores X0/fdt_addr.
    let vcpu = Vcpu::new(0, GUEST_RAM_BASE, 0, bus);
    vcpu.start()
        .join()
        .expect("vCPU thread panicked")
        .expect("vCPU run failed");

    let out = captured.lock().unwrap().clone();
    print!("{}", String::from_utf8_lossy(&out));
    assert_eq!(out, b"IGNITION\n", "unexpected UART output: {out:?}");
    println!("== UART-ECHO MILESTONE PASSED ==");
}
