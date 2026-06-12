# Phase 1 Milestone 2d: integration boot to earlycon Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a boot harness that loads a real aarch64 kernel + device tree into guest RAM, creates the in-kernel GIC, and runs a vCPU so the kernel's earlycon output reaches our 16550 on host stdout.

**Architecture:** Two changes. (1) The `Vcpu` run loop gains efficient handling of the idle/timer exits (`WaitForEvent*`, `VtimerActivated`) so it doesn't busy-spin. (2) A new `spike/src/bin/boot.rs` assembles kernel (2c `load_kernel`), DTB (2a `fdt::generate`), in-kernel GIC (2b `HvfGicV3`), 512 MiB of mmap'd RAM, and a `Serial`→stdout device, then runs the vCPU. The in-kernel GIC handles ICC/MMIO in-kernel, so the existing `NoIrqVcpus` is reused.

**Tech Stack:** Rust edition 2024, Apple Hypervisor.framework via the `hvf`/`vmm`/`devices`/`arch` crates, `libc` mmap.

**Commit convention for this project:** plain commit messages, NO `Co-Authored-By` / "Generated with Claude" trailer.

**Note on verification:** Neither task is `cargo test` — both touch HVF. The acceptance gate for BOTH tasks is a clean build. The actual boot RUN needs a real kernel Image (supplied by the operator at run time) and the hypervisor entitlement, so it is run + debugged in the main session AFTER this plan lands — not by the implementer subagents.

---

## File Structure

- `crates/vmm/src/vstate/hvf_vcpu.rs` — add idle/timer exit handling to the run loop
- `spike/Cargo.toml` — add the `boot` bin target
- `spike/src/bin/boot.rs` — **create**: the boot harness

---

## Task 1: run-loop idle/timer handling

**Files:**
- Modify: `crates/vmm/src/vstate/hvf_vcpu.rs`

No `cargo test` (the loop calls HVF). Build-checked.

- [ ] **Step 1: Add the Duration import and the park cap constant**

In `crates/vmm/src/vstate/hvf_vcpu.rs`, change the imports block. It currently is:

```rust
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use devices::bus::Bus;

pub use hvf::{HvfVcpu, InterruptType, VcpuExit, Vcpus};
```

Replace it with (adds `Duration` and a module-level cap constant):

```rust
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use devices::bus::Bus;

pub use hvf::{HvfVcpu, InterruptType, VcpuExit, Vcpus};

/// Upper bound on how long the run loop sleeps on an idle exit. Caps a large
/// timer deadline so the loop stays responsive, and bounds the busy-wait on a
/// no-deadline WFI on the earlycon path.
const MAX_PARK: Duration = Duration::from_millis(10);
```

- [ ] **Step 2: Replace the catch-all exit arm with explicit idle/timer arms**

In the same file, in `Vcpu::run`'s match, replace this tail (the `Canceled` arm and the `other =>` arm and their comment):

```rust
                VcpuExit::Canceled => return Ok(()),
                // No idle-park yet; the milestone guest does not WFI on the
                // success path. TODO(phase1-smp): WFE/WFI parking with a
                // CNTV_CVAL-derived timeout, lifted from libkrun macos/vstate.rs.
                other => log::debug!("unhandled vCPU exit: {other:?}"),
```

with:

```rust
                VcpuExit::Canceled => return Ok(()),
                // Idle/timer exits. Earlycon-grade parking: bounded sleeps keep
                // the CPU off the floor and let wall-clock advance toward the
                // next CNTV deadline. Proper channel parking that wakes on an
                // injected IRQ is a later milestone. The vtimer is already masked
                // by HvfVcpu::run; the in-kernel GIC redelivers it on re-entry.
                VcpuExit::WaitForEventTimeout(d) => thread::sleep(d.min(MAX_PARK)),
                VcpuExit::WaitForEvent => thread::sleep(MAX_PARK),
                VcpuExit::WaitForEventExpired | VcpuExit::VtimerActivated => {}
                other => log::debug!("unhandled vCPU exit: {other:?}"),
```

- [ ] **Step 3: Build-check**

Run: `cargo build -p ignition-vmm 2>&1 | tail -10`
Expected: `Finished`, no errors. (`WaitForEventTimeout`, `WaitForEvent`,
`WaitForEventExpired`, `VtimerActivated` are all variants of `hvf::VcpuExit`; if
any name mismatches, check the `pub enum VcpuExit` in `crates/hvf/src/lib.rs` and
use the exact names.)

- [ ] **Step 4: Confirm dependents still build and arch tests pass**

Run: `cargo build -p hvf-spike 2>&1 | tail -3 && cargo test -p ignition-arch 2>&1 | grep -E 'test result: ok'`
Expected: `Finished` and `test result: ok. 21 passed` (the existing bins + arch tests are unaffected).

- [ ] **Step 5: Commit (plain message, NO trailer)**

```bash
git add crates/vmm/src/vstate/hvf_vcpu.rs
git commit -m "feat(vmm): handle idle/timer vCPU exits in the run loop

WaitForEventTimeout/WaitForEvent sleep (bounded by MAX_PARK) instead of
busy-spinning; WaitForEventExpired/VtimerActivated continue. Earlycon-grade
parking ahead of the boot integration; channel parking is a later milestone."
```

---

## Task 2: boot harness

**Files:**
- Modify: `spike/Cargo.toml`
- Create: `spike/src/bin/boot.rs`

The implementer BUILDS and COMMITS this; it is NOT run here (a real kernel + the
hypervisor entitlement are needed; the operator runs it afterward).

- [ ] **Step 1: Add the bin target**

In `spike/Cargo.toml`, after the existing `[[bin]] name = "gic-smoke"` block, add:

```toml
[[bin]]
name = "boot"
path = "src/bin/boot.rs"
```

(All needed deps — `hvf`, `vmm`, `devices`, `arch`, `libc`, `log`, `env_logger` —
are already in `spike/Cargo.toml` from prior milestones.)

- [ ] **Step 2: Create `spike/src/bin/boot.rs`**

```rust
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

    // Optional initrd, copied in after the kernel.
    let initrd = if let Some(ref bytes) = initrd_bytes {
        let off = INITRD_OFFSET as usize;
        let end = off + bytes.len();
        assert!(end <= ram.len(), "initrd does not fit in RAM");
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
    let fdt_addr = layout::fdt_addr(RAM_SIZE);
    let fdt_off = (fdt_addr - layout::RAM_BASE) as usize;
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
```

- [ ] **Step 3: Build (do NOT run — no kernel/entitlement here)**

Run: `cargo build -p hvf-spike --bin boot 2>&1 | tail -15`
Expected: `Finished`, no errors.

Likely compile points (adjust ONLY `boot.rs` if needed, report any change):
- The `serial` unsizing to `Arc<Mutex<dyn BusDevice>>` uses an explicit type annotation (same pattern as `gic-smoke.rs`/`uart-echo.rs`); keep it.
- `vm.hvf` is the public `HvfVm` field on `vmm::vstate::hvf_vm::Vm` with
  `map_memory(host_addr, guest_addr, size)`.
- `gic.fdt_info()` is called twice (once moved into `cfg.gic`, once into `g` for
  diagnostics) — fine, it borrows `&self` and returns a fresh `GicInfo`.
- `kernel_image.len()` after `load_kernel(ram, .., &kernel_image)` — fine,
  `load_kernel` borrows it immutably.

- [ ] **Step 4: Confirm the whole workspace builds**

Run: `cargo build --workspace 2>&1 | tail -3`
Expected: `Finished`, no errors.

- [ ] **Step 5: Commit (plain message, NO trailer)**

```bash
git add spike/Cargo.toml spike/src/bin/boot.rs Cargo.lock
git commit -m "feat(spike): boot harness for real-kernel earlycon

Loads an aarch64 kernel Image + generated DTB into 512 MiB of guest RAM,
creates the in-kernel GIC, maps RAM, and runs a vCPU with a 16550 serial to
stdout. Run with a real kernel after codesigning: target/debug/boot <Image>."
```

---

## Self-Review

**Spec coverage:**
- Run-loop: WaitForEventTimeout sleep(min cap), WaitForEvent bounded sleep, WaitForEventExpired/VtimerActivated continue, NoIrqVcpus kept → Task 1 ✓
- Boot harness: argv kernel[+initrd], mmap 512 MiB, load_kernel→entry, initrd at INITRD_OFFSET, Vm + HvfGicV3(before vCPU), FdtConfig from layout+gic.fdt_info()+serial+default_cmdline, generate DTB, write at fdt_addr-RAM_BASE, map_memory, Bus+Serial(stdout), Vcpu run, diagnostics banner → Task 2 ✓
- Verification = clean build for both; boot run deferred to operator with a real kernel → noted in header + Task 2 ✓
- Out-of-scope (initramfs→shell, channel parking, RX IRQ, SMP, Vm-owns-RAM) → not implemented ✓

**Placeholder scan:** No TBD/TODO-as-work. The one `TODO(phase1-smp)` comment is REMOVED by Task 1 (replaced with real handling). All code complete.

**Type consistency:** `layout::{RAM_BASE, SERIAL_BASE, SERIAL_SIZE, SERIAL_SPI, default_cmdline, fdt_addr}`, `kernel::load_kernel(&mut [u8], u64, &[u8]) -> Result<u64,_>`, `fdt::{FdtConfig, MmioDev, generate}`, `HvfGicV3::{new, fdt_info}`, `Vm::new`+`vm.hvf.map_memory`, `Bus::{new,register}`, `Serial::new`, `Vcpu::new(mpidr, entry, fdt_addr, bus).start().join()` — all match the signatures established in milestones 2a/2b/2c and the UART-echo/gic-smoke harnesses. `MAX_PARK: Duration` defined in Task 1 and used in the same file. `VcpuExit` variants match `hvf::VcpuExit`.

No issues found.
