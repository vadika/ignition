# Phase 1 Milestone 1: UART-echo Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build an MMIO device bus, a 16550 serial device, and a threaded vCPU run loop, proven end-to-end by a hand-assembled guest that writes `"IGNITION\n"` to the UART then powers off via PSCI.

**Architecture:** A `Bus` routes guest MMIO accesses by address to `BusDevice`s. `Serial` wraps rust-vmm's `vm_superio::Serial` (the 16550 Firecracker uses on aarch64). A `Vcpu` runner spawns one OS thread, creates the HVF vCPU **on that thread** (HVF affinity requirement), and dispatches `VcpuExit::Mmio*` to the bus and `Shutdown` to thread exit. Verified by a codesigned demo binary, not `cargo test` (HVF calls need the hypervisor entitlement; plain test binaries would get `HV_DENIED`).

**Tech Stack:** Rust (edition 2024), `vm_superio` 0.8, Apple Hypervisor.framework via the lifted `hvf` crate, `libc` mmap.

---

## File Structure

- `crates/devices/Cargo.toml` — add `vm-superio` dependency
- `crates/devices/src/lib.rs` — declare `bus` and `serial` modules
- `crates/devices/src/bus.rs` — **create**: `BusDevice` trait + `Bus` router (+ unit tests)
- `crates/devices/src/serial.rs` — **create**: `NoopTrigger` + `Serial` 16550 wrapper (+ unit tests)
- `crates/vmm/src/vstate/hvf_vcpu.rs` — **replace** the Phase-0 re-export with the threaded `Vcpu` runner
- `spike/Cargo.toml` — add `uart-echo` bin + `vmm`/`devices` deps
- `spike/src/bin/uart-echo.rs` — **create**: the end-to-end harness (codesigned, run manually)

Pure-logic units (`Bus`, `Serial`) get `cargo test` unit tests. HVF-touching units (`Vcpu` runner, harness) are verified by the signed binary.

---

## Task 1: MMIO Bus + BusDevice trait

**Files:**
- Create: `crates/devices/src/bus.rs`
- Modify: `crates/devices/src/lib.rs`

- [ ] **Step 1: Declare the module**

In `crates/devices/src/lib.rs`, keep the existing doc comment and add at the end:

```rust
pub mod bus;
```

- [ ] **Step 2: Write `bus.rs` with the trait, router, and failing tests**

Create `crates/devices/src/bus.rs`:

```rust
// MMIO device bus: routes guest physical accesses to registered devices.

use std::sync::{Arc, Mutex};

/// One MMIO device. Signature mirrors Firecracker's `vstate::bus::BusDevice`
/// (minus the `Arc<Barrier>` return, unused this milestone) so FC device code
/// lifts later with minimal edits.
pub trait BusDevice: Send {
    fn read(&mut self, _base: u64, _offset: u64, _data: &mut [u8]) {}
    fn write(&mut self, _base: u64, _offset: u64, _data: &[u8]) {}
}

/// Address-routed collection of MMIO devices. Ranges are assumed
/// non-overlapping. `read`/`write` take `&self` (devices carry their own
/// `Mutex`), so a fully-built `Bus` can be shared as `Arc<Bus>` across threads.
#[derive(Default)]
pub struct Bus {
    devices: Vec<(u64, u64, Arc<Mutex<dyn BusDevice>>)>, // (base, len, device)
}

impl Bus {
    pub fn new() -> Self {
        Self { devices: Vec::new() }
    }

    pub fn register(&mut self, base: u64, len: u64, dev: Arc<Mutex<dyn BusDevice>>) {
        self.devices.push((base, len, dev));
    }

    fn find(&self, addr: u64) -> Option<(u64, &Arc<Mutex<dyn BusDevice>>)> {
        self.devices
            .iter()
            .find(|(base, len, _)| addr >= *base && addr < base + len)
            .map(|(base, _, dev)| (*base, dev))
    }

    pub fn read(&self, addr: u64, data: &mut [u8]) {
        match self.find(addr) {
            Some((base, dev)) => dev.lock().unwrap().read(base, addr - base, data),
            None => log::warn!("MMIO read miss at {addr:#x}"),
        }
    }

    pub fn write(&self, addr: u64, data: &[u8]) {
        match self.find(addr) {
            Some((base, dev)) => dev.lock().unwrap().write(base, addr - base, data),
            None => log::warn!("MMIO write miss at {addr:#x}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Recorder {
        last_write: Option<(u64, u64, Vec<u8>)>,
        read_val: u8,
    }
    impl BusDevice for Recorder {
        fn write(&mut self, base: u64, offset: u64, data: &[u8]) {
            self.last_write = Some((base, offset, data.to_vec()));
        }
        fn read(&mut self, _base: u64, _offset: u64, data: &mut [u8]) {
            data[0] = self.read_val;
        }
    }

    #[test]
    fn write_routes_with_base_and_offset() {
        let rec = Arc::new(Mutex::new(Recorder::default()));
        let mut bus = Bus::new();
        bus.register(0x1000, 0x100, rec.clone());
        bus.write(0x1004, &[0xab]);
        assert_eq!(rec.lock().unwrap().last_write, Some((0x1000, 0x4, vec![0xab])));
    }

    #[test]
    fn read_routes_with_offset() {
        let rec = Arc::new(Mutex::new(Recorder { read_val: 0x5a, ..Default::default() }));
        let mut bus = Bus::new();
        bus.register(0x2000, 0x10, rec.clone());
        let mut buf = [0u8; 1];
        bus.read(0x2008, &mut buf);
        assert_eq!(buf[0], 0x5a);
    }

    #[test]
    fn out_of_range_access_is_ignored() {
        let bus = Bus::new();
        bus.write(0xdead, &[1]); // must not panic
        let mut b = [0u8; 1];
        bus.read(0xbeef, &mut b); // must not panic
    }
}
```

- [ ] **Step 3: Run tests, verify they fail**

Run: `cargo test -p ignition-devices bus 2>&1 | tail -15`
Expected: compile error first run only if `lib.rs` not updated; once compiling, the three tests run. (They are written to pass against the implementation in the same file, so this step confirms they **compile and pass** — there is no separate red phase here because trait+impl+tests land together for a pure data-structure unit. If anything fails, fix before committing.)

- [ ] **Step 4: Run tests, verify pass**

Run: `cargo test -p ignition-devices bus 2>&1 | tail -15`
Expected: `test result: ok. 3 passed`

- [ ] **Step 5: Commit**

```bash
git add crates/devices/src/bus.rs crates/devices/src/lib.rs
git commit -m "feat(devices): MMIO bus + BusDevice trait

Address-routed device bus mirroring Firecracker's vstate::bus::BusDevice
signature. Unit-tested routing + offset math.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: 16550 Serial device

**Files:**
- Modify: `crates/devices/Cargo.toml`
- Create: `crates/devices/src/serial.rs`
- Modify: `crates/devices/src/lib.rs`

- [ ] **Step 1: Add the `vm-superio` dependency**

In `crates/devices/Cargo.toml`, under `[dependencies]` (which already has `log = "0.4"`), add:

```toml
vm-superio = "0.8"
```

- [ ] **Step 2: Declare the module**

In `crates/devices/src/lib.rs`, add at the end (after `pub mod bus;`):

```rust
pub mod serial;
```

- [ ] **Step 3: Write `serial.rs` with the wrapper and failing tests**

Create `crates/devices/src/serial.rs`:

```rust
// 16550 MMIO UART, backed by rust-vmm's `vm_superio::Serial` — the same device
// Firecracker uses on aarch64 (FDT `compatible = "ns16550a"`).

use std::io::{self, Write};

use vm_superio::Trigger;
use vm_superio::serial::NoEvents;

use crate::bus::BusDevice;

/// No-op IRQ trigger. With no interrupt controller yet, the 16550 TX-ready
/// interrupt has nowhere to go. Replaced when a GIC lands.
#[derive(Debug, Default)]
pub struct NoopTrigger;

impl Trigger for NoopTrigger {
    type E = io::Error;

    fn trigger(&self) -> io::Result<()> {
        Ok(())
    }
}

/// MMIO 16550 UART writing to sink `W` (e.g. `io::Stdout`, or a captured
/// buffer in tests).
pub struct Serial<W: Write + Send> {
    inner: vm_superio::Serial<NoopTrigger, NoEvents, W>,
}

impl<W: Write + Send> Serial<W> {
    pub fn new(out: W) -> Self {
        Self { inner: vm_superio::Serial::new(NoopTrigger, out) }
    }
}

impl<W: Write + Send> BusDevice for Serial<W> {
    fn read(&mut self, _base: u64, offset: u64, data: &mut [u8]) {
        if let (Ok(off), 1) = (u8::try_from(offset), data.len()) {
            data[0] = self.inner.read(off);
        }
    }

    fn write(&mut self, _base: u64, offset: u64, data: &[u8]) {
        if let (Ok(off), 1) = (u8::try_from(offset), data.len()) {
            let _ = self.inner.write(off, data[0]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// `Write` sink capturing into a shared buffer for assertions.
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

    #[test]
    fn thr_writes_reach_the_sink() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut serial = Serial::new(SharedSink(buf.clone()));
        for b in b"IGNITION\n" {
            // offset 0 == THR (transmit holding register)
            serial.write(0, 0, &[*b]);
        }
        assert_eq!(buf.lock().unwrap().as_slice(), b"IGNITION\n");
    }

    #[test]
    fn noop_trigger_never_errors() {
        assert!(NoopTrigger.trigger().is_ok());
    }
}
```

- [ ] **Step 4: Run tests, verify pass**

Run: `cargo test -p ignition-devices serial 2>&1 | tail -15`
Expected: `test result: ok. 2 passed` (cargo fetches `vm-superio` on first build).
If `vm_superio::Serial::new` arity or `NoEvents` path differs in the resolved 0.8.x, adjust per `cargo doc -p vm-superio --open` — the wrapper is the only thing that changes.

- [ ] **Step 5: Commit**

```bash
git add crates/devices/Cargo.toml crates/devices/src/serial.rs crates/devices/src/lib.rs Cargo.lock
git commit -m "feat(devices): 16550 serial via vm_superio

Wraps vm_superio::Serial (same 16550 Firecracker uses on aarch64) as a
BusDevice writing to an injectable sink. NoopTrigger stubs the IRQ until a
GIC exists. Unit-tested THR-write path.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: Threaded vCPU runner

**Files:**
- Replace: `crates/vmm/src/vstate/hvf_vcpu.rs`

No `cargo test` here — the runner calls into HVF, which needs the hypervisor entitlement. It is build-checked now and exercised end-to-end by Task 4's signed binary.

- [ ] **Step 1: Replace the Phase-0 re-export with the runner**

Overwrite `crates/vmm/src/vstate/hvf_vcpu.rs` entirely:

```rust
// Per-vCPU state and run loop over Hypervisor.framework.
//
// HVF vCPUs are thread-affine: hv_vcpu_create MUST run on the thread that runs
// the vCPU. So `Vcpu::new` only stores config; the vCPU is created inside the
// thread spawned by `start`.
//
// Replaces: firecracker/src/vmm/src/vstate/vcpu.rs (KVM-coupled there).

use std::sync::Arc;
use std::thread::{self, JoinHandle};

use devices::bus::Bus;

pub use hvf::{HvfVcpu, InterruptType, VcpuExit, Vcpus};

/// Interrupt source with no GIC yet: the guest receives no injected IRQs, and
/// trapped system-register accesses are acknowledged so the vCPU keeps running.
/// Replaced by a real GIC-backed `Vcpus` impl in a later milestone.
struct NoIrqVcpus;

impl Vcpus for NoIrqVcpus {
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

/// A single guest vCPU that runs on its own OS thread.
pub struct Vcpu {
    mpidr: u64,
    entry: u64,
    fdt_addr: u64,
    bus: Arc<Bus>,
}

impl Vcpu {
    pub fn new(mpidr: u64, entry: u64, fdt_addr: u64, bus: Arc<Bus>) -> Self {
        Self { mpidr, entry, fdt_addr, bus }
    }

    /// Spawn the vCPU thread. The join handle resolves to `Ok(())` on guest
    /// shutdown (PSCI SYSTEM_OFF) or vCPU cancel.
    pub fn start(self) -> JoinHandle<Result<(), hvf::Error>> {
        thread::spawn(move || self.run())
    }

    fn run(self) -> Result<(), hvf::Error> {
        let vcpus: Arc<dyn Vcpus> = Arc::new(NoIrqVcpus);

        // Thread-affine: create the vCPU here, not in `new`.
        let mut vcpu = HvfVcpu::new(self.mpidr, false)?;
        vcpu.set_initial_state(self.entry, self.fdt_addr)?;

        loop {
            let exit = vcpu.run(vcpus.clone())?;
            match exit {
                VcpuExit::MmioWrite(addr, data) => self.bus.write(addr, data),
                VcpuExit::MmioRead(addr, data) => self.bus.read(addr, data),
                VcpuExit::Shutdown => {
                    log::info!("guest requested shutdown (PSCI SYSTEM_OFF)");
                    return Ok(());
                }
                VcpuExit::Canceled => return Ok(()),
                // No idle-park yet; the milestone guest does not WFI on the
                // success path. TODO(phase1-smp): WFE/WFI parking with a
                // CNTV_CVAL-derived timeout, lifted from libkrun macos/vstate.rs.
                other => log::debug!("unhandled vCPU exit: {other:?}"),
            }
        }
    }
}
```

- [ ] **Step 2: Build-check the crate**

Run: `cargo build -p ignition-vmm 2>&1 | tail -10`
Expected: `Finished` with no errors. (`hvf_vm.rs` still provides `Vm`; this file now provides `Vcpu`.)

- [ ] **Step 3: Commit**

```bash
git add crates/vmm/src/vstate/hvf_vcpu.rs
git commit -m "feat(vmm): threaded vCPU runner over HVF

Vcpu::start spawns a thread that creates the HVF vCPU on that thread
(affinity) and dispatches MMIO exits to the device Bus and PSCI SYSTEM_OFF
to clean exit. NoIrqVcpus stubs interrupts until a GIC lands.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: End-to-end UART-echo harness

**Files:**
- Modify: `spike/Cargo.toml`
- Create: `spike/src/bin/uart-echo.rs`

- [ ] **Step 1: Add the bin target and deps**

In `spike/Cargo.toml`, after the existing `[[bin]] name = "hvf-spike"` block add:

```toml
[[bin]]
name = "uart-echo"
path = "src/bin/uart-echo.rs"
```

And under `[dependencies]` add (alongside the existing `hvf`, `log`, `env_logger`, `libc`):

```toml
vmm = { package = "ignition-vmm", path = "../crates/vmm" }
devices = { package = "ignition-devices", path = "../crates/devices" }
```

- [ ] **Step 2: Write the harness**

Create `spike/src/bin/uart-echo.rs`:

```rust
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

use devices::bus::Bus;
use devices::serial::Serial;
use vmm::vstate::hvf_vcpu::Vcpu;
use vmm::vstate::hvf_vm::Vm;

const GUEST_RAM_BASE: u64 = 0x4000_0000;
const GUEST_RAM_SIZE: u64 = 0x10_0000; // 1 MiB
const SERIAL_BASE: u64 = 0x0900_0000;
const SERIAL_LEN: u64 = 0x1000;

// Hand-assembled aarch64 (clang -target arm64-apple-macos). 11 instructions +
// the "IGNITION\n" bytes. See docs/superpowers/specs for the asm source:
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

    let vm = Vm::new(false).expect("hv_vm_create failed (entitlement?)");

    // Allocate + populate guest RAM.
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
    vm.hvf
        .map_memory(host as u64, GUEST_RAM_BASE, GUEST_RAM_SIZE)
        .expect("hv_vm_map failed");

    // Wire the device bus: one serial at SERIAL_BASE, output captured.
    let captured = Arc::new(Mutex::new(Vec::new()));
    let serial = Arc::new(Mutex::new(Serial::new(SharedSink(captured.clone()))));
    let mut bus = Bus::new();
    bus.register(SERIAL_BASE, SERIAL_LEN, serial);
    let bus = Arc::new(bus);

    // Run the vCPU to shutdown.
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
```

- [ ] **Step 3: Build**

Run: `cargo build -p hvf-spike --bin uart-echo 2>&1 | tail -10`
Expected: `Finished` with no errors.

- [ ] **Step 4: Codesign (required — unsigned binary gets HV_DENIED)**

Run: `scripts/sign.sh target/debug/uart-echo`
Expected: `signed: target/debug/uart-echo`

- [ ] **Step 5: Run and verify**

Run: `target/debug/uart-echo`
Expected output includes:
```
IGNITION
== UART-ECHO MILESTONE PASSED ==
```
If it panics on `hv_vm_create`, the binary is unsigned — re-run Step 4.

- [ ] **Step 6: Commit**

```bash
git add spike/Cargo.toml spike/src/bin/uart-echo.rs Cargo.lock
git commit -m "feat(spike): end-to-end UART-echo milestone

Hand-assembled guest writes IGNITION to a 16550 over the MMIO bus then PSCI
powers off; harness captures the output and asserts it. First boot-to-shell
slice working on HVF. Codesign with scripts/sign.sh before running.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Self-Review

**Spec coverage:**
- BusDevice trait + Bus → Task 1 ✓
- Serial (vm_superio 16550, NoopTrigger, injectable sink) → Task 2 ✓
- Threaded Vcpu runner, thread-affine create, exit dispatch, Shutdown path → Task 3 ✓
- Boot harness bin `spike/src/bin/uart-echo.rs`, mmap/map/register/join/assert → Task 4 ✓
- Memory/MMIO layout constants (RAM 0x4000_0000, serial 0x0900_0000) → Task 4 ✓
- Payload (write IGNITION + PSCI SYSTEM_OFF) → Task 4, ground-truth bytes ✓
- Testing: Bus/Serial unit tests + signed-binary e2e → Tasks 1,2,4 ✓
- Out-of-scope items (FDT, GIC, kernel, SMP, WFI park) → left as typed TODOs in Task 3 ✓

**Placeholder scan:** No TBD/TODO-as-work in steps. The two `TODO(phaseN)` comments in Task 3 are intentional future-seams, not plan gaps. All code blocks complete.

**Type consistency:** `BusDevice::{read,write}(&mut self, base, offset, data)` consistent across bus.rs, serial.rs, and the Recorder test. `Bus::{new,register,read,write}` signatures match call sites in Task 4. `Serial::new(out)` matches harness. `Vcpu::new(mpidr, entry, fdt_addr, bus)` + `start()` match harness usage. `Vm` exposes `pub hvf` (from Phase-0 hvf_vm.rs) used as `vm.hvf.map_memory` — consistent. `SharedSink` duplicated in serial.rs tests and harness intentionally (separate crates, no shared test util).

No issues requiring change.
