# Phase 1, Milestone 1: UART-echo

Status: approved design, pre-implementation.
Date: 2026-06-12.
Project: ignition (Firecracker → macOS/HVF fork). See `docs/HANDOFF.md`, `docs/SPIKE_RESULTS.md`.

## Goal

Build the first reusable slice of the boot-to-shell device path — an MMIO device
bus, a 16550 serial device, and a threaded vCPU run loop — and prove it works
end to end **without** needing a guest kernel, GIC, or FDT.

A hand-assembled aarch64 guest writes the bytes `"IGNITION\n"` to the 16550
transmit-holding register, then issues a PSCI `SYSTEM_OFF`. Success:

- host stdout contains `IGNITION`
- the vCPU thread observes `VcpuExit::Shutdown` and exits
- the harness joins the thread and the process returns 0

This validates the device/run-loop infrastructure that the kernel-boot milestones
build on, and exercises HVF's thread-affinity requirement (the vCPU must be
created on the thread that runs it).

## Scope

**In:** `BusDevice` trait, `Bus` (address routing), `Serial` (wrap
`vm_superio::Serial`), threaded `Vcpu` runner with an exit-dispatch loop, a boot
harness binary that wires it together and asserts the output.

**Out (later milestones):** FDT generation, GIC (in-kernel `hv_gic` or userspace
gicv3), kernel `Image` loading + arm64 boot protocol, SMP / PSCI `CPU_ON`
secondary bringup, WFI idle-park timeout precision, virtio devices, rate
limiting, snapshot. The run loop will leave typed `TODO(phaseN)` seams for these.

## Why 16550 (not PL011)

Confirmed against Firecracker source: aarch64 uses a 16550, not PL011.
`arch/aarch64/fdt.rs` advertises the serial node as `compatible = "ns16550a"`,
and the device is `vm_superio::Serial` (rust-vmm 16550A) — the same crate and
device for x86 and aarch64. We use `vm_superio` directly so Firecracker's
`serial.rs` wrapper lifts cleanly in a later milestone.

## Components

All new code lands in the existing `crates/devices` and `crates/vmm`.

### `BusDevice` trait — `crates/devices`

Mirrors Firecracker's `vstate/bus.rs` signature for lift-compatibility:

```rust
pub trait BusDevice: Send {
    fn read(&mut self, base: u64, offset: u64, data: &mut [u8]) {}
    fn write(&mut self, base: u64, offset: u64, data: &[u8]) {}
}
```

(Firecracker's `write` returns `Option<Arc<Barrier>>` for vCPU-sync devices; the
milestone has no such device, so we omit the return now and add it when a device
needs it. Documented as an intentional, reversible simplification.)

### `Serial` — `crates/devices`

Thin wrapper over `vm_superio::Serial<NoopTrigger, _, W: Write>`, `W = io::Stdout`
for the milestone (injectable so tests pass a `Vec<u8>` sink).

- `write(_base, offset, data)` → `serial.write(offset as u8, data[0])`
- `read(_base, offset, data)` → `data[0] = serial.read(offset as u8)`
- `NoopTrigger`: `impl vm_superio::Trigger { type E = io::Error; fn trigger(&self) -> Result<(), io::Error> { Ok(()) } }` — no GIC yet, the TX-ready IRQ is a no-op.

The guest's `STR` to `THR` (offset 0) is all the milestone needs; `vm_superio`
handles the full register file (LSR THRE-ready on reads, etc.) for free.

### `Bus` — `crates/devices`

Address→device routing over a sorted map of `(base, len) → Arc<Mutex<dyn BusDevice>>`.

- `register(base, len, dev)`
- `read(addr, data)` / `write(addr, data)`: find the containing range, dispatch
  with `offset = addr - base`. Miss → log + ignore (a real guest would fault;
  the milestone tolerates it).

### `Vcpu` runner — `crates/vmm/src/vstate/hvf_vcpu.rs`

Replaces the Phase-0 re-export with a real threaded runner.

- `Vcpu::new(mpidr, bus: Arc<Mutex<Bus>>, entry, fdt_addr)` captures config; does
  **not** create the HVF vCPU yet (affinity).
- `start() -> JoinHandle<Result<(), Error>>`: spawns a thread that
  1. creates `HvfVcpu::new(mpidr, false)` **on this thread**,
  2. `set_initial_state(entry, fdt_addr)`,
  3. loops `run(vcpus)` and dispatches:
     - `MmioWrite(addr,data)` → `bus.lock().write(addr,data)`
     - `MmioRead(addr,data)` → `bus.lock().read(addr,data)`
     - `Shutdown` → break, return `Ok(())`
     - `WaitForEvent*` → milestone: treat as spin/continue (idle-park is a later
       TODO); the guest reaches `SYSTEM_OFF` without WFI, so this path is not on
       the success route but must not panic
     - other exits → log; `Canceled` → break
- `Vcpus` trait impl: a `DummyVcpus` (no pending IRQ, sysreg stubs) as in the spike.

### Boot harness — new bin `spike/src/bin/uart-echo.rs`

Added as a second binary in the existing `spike` package, keeping the original
`hvf-spike` smoke test untouched.

- mmap guest RAM, write the payload at `GUEST_RAM_BASE`,
- `Vm::new(false)`, `vm.map_memory(...)`,
- build `Bus`, register `Serial` at `SERIAL_BASE` (len `0x1000`),
- `Vcpu::new(...).start()`, `join()`, assert success.

## Memory / MMIO layout (milestone-local constants)

| Region | Addr |
|---|---|
| Guest RAM base | `0x4000_0000` (1 MiB) |
| Serial base | `0x0900_0000`, len `0x1000` |

These are throwaway constants for the milestone. The real layout gets lifted from
Firecracker's `arch/aarch64/layout.rs` when FDT lands.

## Guest payload (hand-assembled aarch64)

Pseudocode; assembled with `clang -target arm64-apple-macos`, bytes embedded as a
`[u32; N]` (same method as the spike):

```
    movz x1, #0x0900, lsl #16     // x1 = SERIAL_BASE (THR at offset 0)
    adr  x2, msg                  // pointer to "IGNITION\n"
    mov  x3, #9                   // length
loop:
    ldrb w0, [x2], #1
    strb w0, [x1]                 // MMIO write -> Serial THR
    subs x3, x3, #1
    b.ne loop
    movz x0, #0x0008              // PSCI SYSTEM_OFF function id = 0x8400_0008
    movk x0, #0x8400, lsl #16
    hvc  #0                       // -> EC_AA64_HVC -> VcpuExit::Shutdown
msg: .ascii "IGNITION\n"
```

(Exact encoding/string placement finalized at implementation time; the assembler
produces the ground-truth bytes.)

## Testing

TDD, bottom-up:

1. `Bus`: unit tests — register, in-range dispatch, offset math, overlapping/miss
   behavior.
2. `Serial`: unit tests with a `Vec<u8>` sink — writing bytes to THR offset
   appears in the sink; LSR read reports TX-ready. NoopTrigger never errors.
3. `Vcpu` runner + harness: integration test — run the payload, capture the
   `Serial` sink, assert it equals `"IGNITION\n"` and the thread returned `Ok`.
   (Capture via injecting a shared `Arc<Mutex<Vec<u8>>>` sink instead of stdout.)

Run loop and HVF calls are validated by the integration test (they need a real
vCPU; already proven viable by the spike).

## Risks / open points

- **`vm_superio` Trigger associated-type signature** — confirm exact trait
  (`type E`, `fn trigger`) against 0.8.x at implementation; NoopTrigger adjusts.
- **`WaitForEvent` on the success path** — payload avoids WFI, so not exercised;
  loop must still handle it without panicking (continue).
- **Thread-affine create** — `HvfVcpu::new` MUST run inside the spawned thread,
  not before `spawn`. This is the whole point of building the threaded runner now.
- **stdout vs captured sink** — `Serial` output sink is generic so the
  integration test captures to a buffer while the demo bin uses real stdout.

## References to lift from

- `firecracker/src/vmm/src/vstate/bus.rs` — BusDevice trait shape
- `firecracker/src/vmm/src/devices/legacy/serial.rs` — vm_superio wrapper (full version later)
- `libkrun/src/vmm/src/macos/vstate.rs` — threading, WFE parking (next milestone)
- `libkrun/src/vmm/src/device_manager/hvf/mmio.rs` — MMIO bus without irqfd
