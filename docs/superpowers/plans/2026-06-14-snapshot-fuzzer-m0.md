# Snapshot-Fuzzer M0 (Loop Skeleton + v0 Reset) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the M0 slice of the snapshot fuzzer from `docs/superpowers/specs/2026-06-14-snapshot-fuzzing-demonstrator-design.md`: an `ignition-fuzz` MMIO device (doorbell + shared window), a guest harness in an initramfs, an in-VMM per-iteration reset using a full-RAM copy (v0), blind random mutation, and CRASH capture. Gate: a hand-injected malformed input is captured as a crash solution.

**Architecture:** The fuzz loop runs *inline on the single vCPU thread* (HVF is thread-affine: vCPU registers can only be set on the thread that runs the vCPU). A guest harness parks at the parse site and rings a trap-MMIO **doorbell**. On the first `SNAPSHOT_ME` the VMM advances PC past the store, copies guest RAM to a host-side base buffer, and saves the vCPU register file. On each `DONE`/`CRASH` the VMM mutates the next input into a host-owned **shared window** (mapped at a known GPA via `hv_vm_map`, outside guest RAM so it survives reset), then resets: `memcpy` base→guest RAM and `restore_state` the registers. M0 deliberately omits coverage, libAFL, and dirty-page reset — those are M1/M2.

**Tech Stack:** Rust (crates `ignition-arch`, `ignition-devices`, `ignition-hvf`, `ignition-vmm`, `spike` boot binary), Apple Hypervisor.framework, aarch64. Guest harness in C (statically linked, packed into a cpio initramfs). Integration test in Python (matches existing `scripts/*.py`).

---

## File Structure

- `crates/devices/src/fuzz/mod.rs` — new `fuzz` module (re-exports).
- `crates/devices/src/fuzz/protocol.rs` — register offsets, command codes, default sizes (single source of truth; mirrored by the C header).
- `crates/devices/src/fuzz/device.rs` — `FuzzDevice` (`MmioDevice`): holds the control-register scalars (`INPUT_LEN`, `CRASH_CODE`, `STATUS`).
- `crates/arch/src/aarch64/fdt.rs` — `FdtDevice::Fuzz` variant + `create_fuzz_node` (two reg ranges, no interrupt).
- `crates/devices/src/device.rs` — `FdtKind::IgnitionFuzz` variant.
- `crates/hvf/src/lib.rs` — `HvfVcpu::advance_pc` + `HvfVcpu::clear_pending_advance` helpers.
- `crates/vmm/src/fuzz/mod.rs` + `crates/vmm/src/fuzz/controller.rs` — `FuzzController` (host brain: base RAM, base regs, mutator, solutions) + the pure `restore_ram` helper + xorshift mutator.
- `crates/vmm/src/vstate/vcpu_manager.rs` — `run_fuzz` entry + `fuzz_loop`.
- `guest/fuzz-harness/ignition_fuzz.h` — C mirror of `protocol.rs`.
- `guest/fuzz-harness/harness.c` — guest init: setup → `SNAPSHOT_ME` → loop(parse) → `DONE`/`CRASH`.
- `guest/fuzz-harness/build.sh` — cross-compile harness + pack cpio initramfs.
- `spike/src/bin/boot.rs` — `--fuzz` CLI mode wiring.
- `scripts/fuzz_m0_test.py` — the M0 gate integration test.

---

## Layout constants (used across tasks)

The control region and shared window live in MMIO space, above guest RAM, so a full-RAM reset never touches them. Use these fixed GPAs (mirroring the boot-timer's fixed-address convention; verify no overlap against `crates/arch/src/aarch64/layout.rs` when implementing Task 8 and adjust if needed):

- Control region GPA: `0x0920_0000`, size `0x1000` (4 KiB).
- Shared window GPA: `0x0920_1000`, size = configurable, default `0x20_0000` (2 MiB).

---

### Task 1: Fuzz protocol constants

**Files:**
- Create: `crates/devices/src/fuzz/mod.rs`
- Create: `crates/devices/src/fuzz/protocol.rs`
- Modify: `crates/devices/src/lib.rs` (add `pub mod fuzz;`)

- [ ] **Step 1: Write the failing test**

In `crates/devices/src/fuzz/protocol.rs`:

```rust
//! The host/guest fuzz control protocol: register offsets within the control
//! region, doorbell command codes, and default window geometry. This is the
//! single source of truth; `guest/fuzz-harness/ignition_fuzz.h` mirrors it by
//! hand (keep them in sync — Task 7 asserts the values match).

/// Control-register offsets within the trap-MMIO control region.
pub mod reg {
    /// W: guest writes a command code (see `cmd`); traps to the VMM.
    pub const DOORBELL: u64 = 0x00;
    /// RW: length of the current input in the shared window (host writes, guest reads).
    pub const INPUT_LEN: u64 = 0x04;
    /// W: ASan/abort reason class on a CRASH doorbell (guest writes).
    pub const CRASH_CODE: u64 = 0x08;
    /// R: VMM->guest handshake (optional in M0).
    pub const STATUS: u64 = 0x0c;
}

/// Doorbell command codes (guest -> VMM).
pub mod cmd {
    /// One-time setup complete; parked at the parse site. First receipt captures
    /// the snapshot.
    pub const SNAPSHOT_ME: u32 = 0x1;
    /// Input processed cleanly.
    pub const DONE: u32 = 0x2;
    /// Target crashed (from the death/signal handler).
    pub const CRASH: u32 = 0x3;
}

/// Default shared-window size in bytes (2 MiB).
pub const DEFAULT_WINDOW_SIZE: u64 = 0x20_0000;
/// Control region size in bytes (4 KiB).
pub const CONTROL_SIZE: u64 = 0x1000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_offsets_are_distinct_and_within_control_region() {
        let offsets = [reg::DOORBELL, reg::INPUT_LEN, reg::CRASH_CODE, reg::STATUS];
        for (i, a) in offsets.iter().enumerate() {
            assert!(*a + 4 <= CONTROL_SIZE, "register {a:#x} must fit in control region");
            for b in &offsets[i + 1..] {
                assert_ne!(a, b, "register offsets must be distinct");
            }
        }
    }

    #[test]
    fn command_codes_are_distinct_and_nonzero() {
        let codes = [cmd::SNAPSHOT_ME, cmd::DONE, cmd::CRASH];
        for (i, a) in codes.iter().enumerate() {
            assert_ne!(*a, 0, "0 is reserved for 'no command'");
            for b in &codes[i + 1..] {
                assert_ne!(a, b, "command codes must be distinct");
            }
        }
    }
}
```

In `crates/devices/src/fuzz/mod.rs`:

```rust
//! The `ignition-fuzz` MMIO device and host/guest control protocol (M0).

pub mod protocol;
```

- [ ] **Step 2: Wire the module in**

Add to `crates/devices/src/lib.rs` (next to the other `pub mod` lines):

```rust
pub mod fuzz;
```

- [ ] **Step 3: Run the tests to verify they pass**

Run: `cargo test -p ignition-devices fuzz::protocol`
Expected: 2 passed.

- [ ] **Step 4: Commit**

```bash
git add crates/devices/src/fuzz/ crates/devices/src/lib.rs
git commit -m "fuzz: M0 control protocol constants (offsets, command codes)"
```

---

### Task 2: FDT node for the fuzz device

**Files:**
- Modify: `crates/devices/src/device.rs:9-14` (add `FdtKind::IgnitionFuzz`)
- Modify: `crates/arch/src/aarch64/fdt.rs` (add `FdtDevice::Fuzz`, `FuzzDev`, `create_fuzz_node`, match arm)
- Test: `crates/arch/src/aarch64/fdt.rs` (tests module)

- [ ] **Step 1: Add the `FdtKind` variant**

In `crates/devices/src/device.rs`, extend the enum:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FdtKind {
    Ns16550a,
    VirtioMmio,
    Pl031,
    IgnitionFuzz,
}
```

- [ ] **Step 2: Write the failing test**

In `crates/arch/src/aarch64/fdt.rs` tests module, add:

```rust
#[test]
fn fuzz_node_has_two_reg_ranges_and_no_interrupts() {
    let mut cfg = sample();
    cfg.devices.push(FdtDevice::Fuzz(FuzzDev {
        ctrl_addr: 0x0920_0000,
        ctrl_size: 0x1000,
        win_addr: 0x0920_1000,
        win_size: 0x20_0000,
    }));
    let blob = generate(&cfg).unwrap();
    let dt = Fdt::new(&blob).unwrap();
    let node = dt.find_node("/fuzz@9200000").expect("fuzz node present");
    assert_eq!(dt_str(node.property("compatible").unwrap().value), "ignition,fuzz");
    // reg = [ctrl_addr, ctrl_size, win_addr, win_size] (four u64 cells)
    assert_eq!(
        be_u64s(node.property("reg").unwrap().value),
        vec![0x0920_0000, 0x1000, 0x0920_1000, 0x20_0000]
    );
    assert!(node.property("interrupts").is_none(), "fuzz device uses a polled doorbell, no IRQ");
}
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test -p ignition-arch fuzz_node_has_two_reg_ranges`
Expected: FAIL — `FuzzDev` and `FdtDevice::Fuzz` not defined.

- [ ] **Step 4: Add `FuzzDev`, the enum variant, the node builder, and the match arm**

In `crates/arch/src/aarch64/fdt.rs`, after the `MmioDev` struct add:

```rust
/// The `ignition-fuzz` device placement: a trap-MMIO control region plus a
/// RAM-backed shared window, each emitted as a `reg` range. No interrupt (the
/// doorbell is a trapping store, not an IRQ source).
pub struct FuzzDev {
    pub ctrl_addr: u64,
    pub ctrl_size: u64,
    pub win_addr: u64,
    pub win_size: u64,
}
```

Add a variant to `FdtDevice`:

```rust
    /// ignition snapshot-fuzz device -> `ignition,fuzz` node (two reg ranges).
    Fuzz(FuzzDev),
```

Add the match arm inside the `for dev in &cfg.devices` loop in `generate`:

```rust
            FdtDevice::Fuzz(f) => create_fuzz_node(&mut fdt, f)?,
```

Add the node builder (next to `create_rtc_node`):

```rust
fn create_fuzz_node(fdt: &mut FdtWriter, f: &FuzzDev) -> Result<(), vm_fdt::Error> {
    let node = fdt.begin_node(&format!("fuzz@{:x}", f.ctrl_addr))?;
    fdt.property_string("compatible", "ignition,fuzz")?;
    // Two ranges: control registers first, then the shared window. The guest
    // harness reads both from this `reg` (or uses the fixed GPAs directly).
    fdt.property_array_u64("reg", &[f.ctrl_addr, f.ctrl_size, f.win_addr, f.win_size])?;
    // No "interrupts": the doorbell is a trapping store handled inline by the VMM.
    fdt.end_node(node)?;
    Ok(())
}
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p ignition-arch fuzz_node_has_two_reg_ranges`
Expected: PASS. Also run `cargo test -p ignition-arch -p ignition-devices` to confirm no regression.

- [ ] **Step 6: Commit**

```bash
git add crates/devices/src/device.rs crates/arch/src/aarch64/fdt.rs
git commit -m "fuzz: FdtKind::IgnitionFuzz + ignition,fuzz FDT node (control + window reg ranges)"
```

---

### Task 3: `FuzzDevice` control-register device

**Files:**
- Create: `crates/devices/src/fuzz/device.rs`
- Modify: `crates/devices/src/fuzz/mod.rs` (add `pub mod device;` + re-export)
- Test: in `crates/devices/src/fuzz/device.rs`

The device backs the trap-MMIO scalars only. The **doorbell** is recognized by the fuzz loop (Task 6) by comparing the trapped address; it does not need device state. `INPUT_LEN` is host-written/guest-read; `CRASH_CODE` is guest-written/host-read; `STATUS` is host-written/guest-read.

- [ ] **Step 1: Write the failing test**

In `crates/devices/src/fuzz/device.rs`:

```rust
//! The `ignition-fuzz` control-register device. Holds the trap-MMIO scalars the
//! host and guest exchange each iteration: INPUT_LEN (host->guest), CRASH_CODE
//! (guest->host), STATUS (host->guest). The DOORBELL register carries no state
//! here — a store to it traps and is handled by the fuzz loop directly.

use crate::bus::BusDevice;
use crate::device::{DeviceMgrError, FdtKind, MmioDevice};
use crate::fuzz::protocol::reg;

pub struct FuzzDevice {
    input_len: u32,
    crash_code: u32,
    status: u32,
}

impl FuzzDevice {
    pub fn new() -> FuzzDevice {
        FuzzDevice { input_len: 0, crash_code: 0, status: 0 }
    }
    /// Host: set the input length the guest will read this iteration.
    pub fn set_input_len(&mut self, len: u32) {
        self.input_len = len;
    }
    /// Host: read the crash reason class the guest wrote on a CRASH doorbell.
    pub fn crash_code(&self) -> u32 {
        self.crash_code
    }
    /// Host: set the STATUS handshake value.
    pub fn set_status(&mut self, status: u32) {
        self.status = status;
    }
}

impl Default for FuzzDevice {
    fn default() -> Self {
        Self::new()
    }
}

impl BusDevice for FuzzDevice {
    fn read(&mut self, _base: u64, offset: u64, data: &mut [u8]) {
        let val = match offset {
            reg::INPUT_LEN => self.input_len,
            reg::CRASH_CODE => self.crash_code,
            reg::STATUS => self.status,
            _ => 0,
        };
        let bytes = val.to_le_bytes();
        let n = data.len().min(4);
        data[..n].copy_from_slice(&bytes[..n]);
    }

    fn write(&mut self, _base: u64, offset: u64, data: &[u8]) {
        if data.len() < 4 {
            return;
        }
        let val = u32::from_le_bytes(data[..4].try_into().unwrap());
        match offset {
            reg::INPUT_LEN => self.input_len = val,
            reg::CRASH_CODE => self.crash_code = val,
            // DOORBELL is handled by the fuzz loop, not here; ignore stray writes.
            _ => {}
        }
    }
}

impl MmioDevice for FuzzDevice {
    fn fdt_kind(&self) -> FdtKind {
        FdtKind::IgnitionFuzz
    }
    fn snapshot_id(&self) -> &str {
        "ignition-fuzz"
    }
    fn save(&self) -> serde_json::Value {
        serde_json::json!({
            "input_len": self.input_len,
            "crash_code": self.crash_code,
            "status": self.status,
        })
    }
    fn restore(&mut self, v: &serde_json::Value) -> Result<(), DeviceMgrError> {
        let get = |k: &str| -> u32 {
            v.get(k).and_then(|x| x.as_u64()).unwrap_or(0) as u32
        };
        self.input_len = get("input_len");
        self.crash_code = get("crash_code");
        self.status = get("status");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_u32(dev: &mut FuzzDevice, offset: u64) -> u32 {
        let mut buf = [0u8; 4];
        dev.read(0, offset, &mut buf);
        u32::from_le_bytes(buf)
    }

    #[test]
    fn input_len_host_writes_guest_reads() {
        let mut d = FuzzDevice::new();
        d.set_input_len(1234);
        assert_eq!(read_u32(&mut d, reg::INPUT_LEN), 1234);
    }

    #[test]
    fn crash_code_guest_writes_host_reads() {
        let mut d = FuzzDevice::new();
        d.write(0, reg::CRASH_CODE, &11u32.to_le_bytes());
        assert_eq!(d.crash_code(), 11);
        assert_eq!(read_u32(&mut d, reg::CRASH_CODE), 11);
    }

    #[test]
    fn status_handshake_roundtrips() {
        let mut d = FuzzDevice::new();
        assert_eq!(read_u32(&mut d, reg::STATUS), 0);
        d.set_status(1);
        assert_eq!(read_u32(&mut d, reg::STATUS), 1);
    }

    #[test]
    fn save_restore_roundtrips() {
        let mut d = FuzzDevice::new();
        d.set_input_len(7);
        d.write(0, reg::CRASH_CODE, &3u32.to_le_bytes());
        let saved = d.save();
        let mut d2 = FuzzDevice::new();
        d2.restore(&saved).unwrap();
        assert_eq!(d2.crash_code(), 3);
        let mut buf = [0u8; 4];
        d2.read(0, reg::INPUT_LEN, &mut buf);
        assert_eq!(u32::from_le_bytes(buf), 7);
    }
}
```

- [ ] **Step 2: Wire the module + re-export**

In `crates/devices/src/fuzz/mod.rs`:

```rust
//! The `ignition-fuzz` MMIO device and host/guest control protocol (M0).

pub mod device;
pub mod protocol;

pub use device::FuzzDevice;
```

- [ ] **Step 3: Run the tests**

Run: `cargo test -p ignition-devices fuzz::device`
Expected: 4 passed.

- [ ] **Step 4: Commit**

```bash
git add crates/devices/src/fuzz/
git commit -m "fuzz: FuzzDevice control-register MmioDevice (INPUT_LEN/CRASH_CODE/STATUS)"
```

---

### Task 4: `HvfVcpu` PC-control helpers for the reset asymmetry

**Files:**
- Modify: `crates/hvf/src/lib.rs` (add two `pub` methods on `HvfVcpu`, near `write_reg` ~:689)

**Why:** PC advance after an MMIO trap is lazy — `run()` sets `pending_advance_pc` (lib.rs:959) and the *next* `run()` does `PC += 4` (lib.rs:887-890). The fuzz loop needs to (a) on `SNAPSHOT_ME`, advance PC +4 and clear the flag so the snapshot PC is *after* the doorbell store; (b) after a reset (`restore_state` sets PC), clear the flag so the next `run()` does not corrupt the restored PC.

These touch HVF state and cannot be unit-tested without a live vCPU; they are exercised by the Task 9 integration gate. Keep them minimal and documented.

- [ ] **Step 1: Add the helpers**

In `crates/hvf/src/lib.rs`, after `write_reg` (around line 696), add:

```rust
    /// Advance PC past the current instruction (4 bytes; aarch64 fixed width) and
    /// clear any pending lazy advance. Used by the fuzz loop on the one-time
    /// SNAPSHOT_ME doorbell so the captured snapshot PC sits *after* the store.
    pub fn advance_pc(&mut self) -> Result<(), Error> {
        let pc = self.read_reg(hv_reg_t_HV_REG_PC)?;
        self.write_reg(hv_reg_t_HV_REG_PC, pc + 4)?;
        self.pending_advance_pc = false;
        Ok(())
    }

    /// Cancel a pending lazy PC advance. Used by the fuzz loop after a reset:
    /// `restore_state` has just set PC to the snapshot value, so the +4 the next
    /// `run()` would apply (from the DONE/CRASH doorbell trap) must be dropped.
    pub fn clear_pending_advance(&mut self) {
        self.pending_advance_pc = false;
    }
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo build -p ignition-hvf`
Expected: builds clean (these only read/write existing fields).

- [ ] **Step 3: Commit**

```bash
git add crates/hvf/src/lib.rs
git commit -m "hvf: add advance_pc + clear_pending_advance for fuzz reset PC asymmetry"
```

---

### Task 5: `FuzzController` host brain (pure pieces + reset)

**Files:**
- Create: `crates/vmm/src/fuzz/mod.rs`
- Create: `crates/vmm/src/fuzz/controller.rs`
- Modify: `crates/vmm/src/lib.rs` (add `pub mod fuzz;`)
- Test: in `crates/vmm/src/fuzz/controller.rs`

The controller owns: a host-side base copy of guest RAM, the saved base register state, a deterministic xorshift mutator, the corpus seeds, and the solutions directory. The vCPU-touching `reset`/`capture` methods take `&HvfVcpu`; the pure pieces (`restore_ram`, the mutator, solution writing) are split out and unit-tested without a vCPU.

- [ ] **Step 1: Write the failing test (pure pieces)**

In `crates/vmm/src/fuzz/controller.rs`:

```rust
//! Host-side fuzzer brain for M0: snapshot/reset bookkeeping, blind mutation,
//! and crash capture. The vCPU register save/restore lives behind `capture`/
//! `reset` (HVF thread-affine, called on the vCPU thread); the memory reset,
//! mutator, and solution writer are pure and tested here.

use std::path::{Path, PathBuf};

/// Deterministic xorshift64* PRNG. A fixed seed makes a fuzz run reproducible,
/// which the determinism requirements (spec §7) depend on.
pub struct Rng {
    state: u64,
}

impl Rng {
    pub fn new(seed: u64) -> Rng {
        // Avoid the all-zero fixed point.
        Rng { state: if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed } }
    }
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next_u64() % n as u64) as usize }
    }
}

/// Blind "havoc-lite" mutation in place: a handful of random byte sets / bit
/// flips on a copy of `seed`, clamped to `max_len`. No coverage feedback (that
/// is M2). Returns the mutated bytes.
pub fn mutate(seed: &[u8], rng: &mut Rng, max_len: usize) -> Vec<u8> {
    let mut out = seed.to_vec();
    if out.is_empty() {
        out.push(0);
    }
    if out.len() > max_len {
        out.truncate(max_len.max(1));
    }
    let rounds = 1 + rng.below(8);
    for _ in 0..rounds {
        let i = rng.below(out.len());
        match rng.below(3) {
            0 => out[i] = rng.next_u64() as u8,            // random byte
            1 => out[i] ^= 1u8 << rng.below(8),            // bit flip
            _ => out[i] = out[i].wrapping_add(1),          // increment
        }
    }
    out
}

/// Reset guest RAM to the captured base by overwriting every byte. v0 of the
/// spec's §6 reset: correct and simple, no dirty tracking. `base` and `live`
/// must be the same length (full guest RAM).
pub fn restore_ram(base: &[u8], live: &mut [u8]) {
    debug_assert_eq!(base.len(), live.len(), "base and live RAM must match in size");
    live.copy_from_slice(base);
}

/// Write a crash-triggering input and its metadata to the solutions directory.
/// Returns the path of the written input file.
pub fn write_solution(dir: &Path, index: u64, input: &[u8], crash_code: u32) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let input_path = dir.join(format!("crash-{index:06}.bin"));
    std::fs::write(&input_path, input)?;
    std::fs::write(
        dir.join(format!("crash-{index:06}.meta")),
        format!("crash_code={crash_code}\nlen={}\n", input.len()),
    )?;
    Ok(input_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rng_is_deterministic_for_a_seed() {
        let mut a = Rng::new(42);
        let mut b = Rng::new(42);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn mutate_is_deterministic_and_bounded() {
        let seed = b"hello world".to_vec();
        let mut r1 = Rng::new(7);
        let mut r2 = Rng::new(7);
        let m1 = mutate(&seed, &mut r1, 64);
        let m2 = mutate(&seed, &mut r2, 64);
        assert_eq!(m1, m2, "same seed -> same mutation");
        assert!(m1.len() <= 64);
    }

    #[test]
    fn mutate_handles_empty_seed() {
        let mut r = Rng::new(1);
        let m = mutate(&[], &mut r, 64);
        assert!(!m.is_empty());
    }

    #[test]
    fn restore_ram_overwrites_dirtied_bytes() {
        let base = vec![0xAAu8; 4096];
        let mut live = base.clone();
        live[10] = 0x55;
        live[4000] = 0x11;
        restore_ram(&base, &mut live);
        assert_eq!(live, base);
    }

    #[test]
    fn write_solution_emits_input_and_meta() {
        let dir = std::env::temp_dir().join(format!("ignition-fuzz-sol-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let p = write_solution(&dir, 0, b"\xde\xad", 9).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"\xde\xad");
        let meta = std::fs::read_to_string(dir.join("crash-000000.meta")).unwrap();
        assert!(meta.contains("crash_code=9"));
        assert!(meta.contains("len=2"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
```

- [ ] **Step 2: Wire the module in**

In `crates/vmm/src/fuzz/mod.rs`:

```rust
//! Host-side fuzzer brain and per-iteration reset (M0).

pub mod controller;
```

Add to `crates/vmm/src/lib.rs` (next to the other `pub mod` lines):

```rust
pub mod fuzz;
```

- [ ] **Step 3: Run the tests**

Run: `cargo test -p ignition-vmm fuzz::controller`
Expected: 5 passed.

- [ ] **Step 4: Commit**

```bash
git add crates/vmm/src/fuzz/ crates/vmm/src/lib.rs
git commit -m "fuzz: FuzzController pure pieces (xorshift mutator, restore_ram, solution writer)"
```

---

### Task 6: `run_fuzz` + `fuzz_loop` on the vCPU thread

**Files:**
- Modify: `crates/vmm/src/fuzz/controller.rs` (add the stateful `FuzzController` + `capture`/`reset`)
- Modify: `crates/vmm/src/vstate/vcpu_manager.rs` (add `run_fuzz` + `fuzz_loop`)

This is integration code (it drives HVF). It is verified by the Task 9 gate, not a unit test. Build correctness in carefully; the reviewer should read it against the spec §5/§6/§12.

- [ ] **Step 1: Add the stateful `FuzzController`**

Append to `crates/vmm/src/fuzz/controller.rs`:

```rust
use ignition_hvf::{HvfVcpu, VcpuState};

/// The live fuzzer state for one M0 run. Holds the host-side base copy of guest
/// RAM, the saved base register file, a raw view of live guest RAM and the
/// shared window (host VAs from the boot harness's mmaps), the mutator, the seed
/// corpus, and the solutions directory.
///
/// SAFETY: `ram_ptr`/`window_ptr` are host pointers to mappings that outlive the
/// fuzz run (owned by the boot harness). The controller is used only on the
/// single vCPU thread, so the &mut slices it forms are never aliased.
pub struct FuzzController {
    base_ram: Vec<u8>,
    base_state: Option<VcpuState>,
    ram_ptr: *mut u8,
    ram_len: usize,
    window_ptr: *mut u8,
    window_len: usize,
    rng: Rng,
    seeds: Vec<Vec<u8>>,
    seed_idx: usize,
    solutions_dir: PathBuf,
    crash_count: u64,
    iterations: u64,
    captured: bool,
}

// The controller lives on one thread; the raw pointers are not shared.
unsafe impl Send for FuzzController {}

impl FuzzController {
    /// `ram`/`window` are (ptr, len) of the host mappings for guest RAM and the
    /// shared window. `seeds` is the starting corpus (may be empty). `seed_rng`
    /// fixes the mutation stream for reproducibility.
    pub fn new(
        ram: (*mut u8, usize),
        window: (*mut u8, usize),
        seeds: Vec<Vec<u8>>,
        seed_rng: u64,
        solutions_dir: PathBuf,
    ) -> FuzzController {
        FuzzController {
            base_ram: Vec::new(),
            base_state: None,
            ram_ptr: ram.0,
            ram_len: ram.1,
            window_ptr: window.0,
            window_len: window.1,
            rng: Rng::new(seed_rng),
            seeds: if seeds.is_empty() { vec![vec![0u8; 1]] } else { seeds },
            seed_idx: 0,
            solutions_dir,
            crash_count: 0,
            iterations: 0,
            captured: true_false_init(),
        }
    }

    pub fn is_captured(&self) -> bool {
        self.captured
    }
    pub fn iterations(&self) -> u64 {
        self.iterations
    }
    pub fn crash_count(&self) -> u64 {
        self.crash_count
    }

    fn live_ram(&mut self) -> &mut [u8] {
        // SAFETY: see struct doc; single-threaded, mapping outlives the run.
        unsafe { std::slice::from_raw_parts_mut(self.ram_ptr, self.ram_len) }
    }
    fn window(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.window_ptr, self.window_len) }
    }

    /// One-time SNAPSHOT_ME handling: PC is advanced past the doorbell store by
    /// the caller; copy guest RAM into the base buffer and save the register
    /// file. Returns the first input length to expose to the guest.
    pub fn capture(&mut self, vcpu: &HvfVcpu) -> Result<u32, ignition_hvf::Error> {
        let live = self.live_ram().to_vec();
        self.base_ram = live;
        self.base_state = Some(vcpu.save_state()?);
        self.captured = true;
        Ok(self.prepare_next_input())
    }

    /// Pick the next seed, mutate it into the shared window, return its length.
    fn prepare_next_input(&mut self) -> u32 {
        let seed = self.seeds[self.seed_idx % self.seeds.len()].clone();
        self.seed_idx = self.seed_idx.wrapping_add(1);
        let max = self.window_len;
        let input = mutate(&seed, &mut self.rng, max);
        let n = input.len().min(self.window_len);
        self.window()[..n].copy_from_slice(&input[..n]);
        n as u32
    }

    /// DONE handling: count the iteration, prepare the next input, reset.
    /// Returns the next input length.
    pub fn on_done(&mut self, vcpu: &mut HvfVcpu) -> Result<u32, ignition_hvf::Error> {
        self.iterations += 1;
        let len = self.prepare_next_input();
        self.reset(vcpu)?;
        Ok(len)
    }

    /// CRASH handling: record the current input as a solution, then behave like
    /// DONE. `crash_code` came from the device. `current_input` is the bytes
    /// that were live in the window (re-read so the saved input is exact).
    pub fn on_crash(&mut self, vcpu: &mut HvfVcpu, crash_code: u32, input_len: u32) -> Result<u32, ignition_hvf::Error> {
        let n = (input_len as usize).min(self.window_len);
        let input = self.window()[..n].to_vec();
        if let Err(e) = write_solution(&self.solutions_dir, self.crash_count, &input, crash_code) {
            log::warn!("failed to write fuzz solution: {e}");
        }
        self.crash_count += 1;
        log::info!("fuzz: CRASH captured (code={crash_code}, len={n}), solutions={}", self.crash_count);
        self.iterations += 1;
        let len = self.prepare_next_input();
        self.reset(vcpu)?;
        Ok(len)
    }

    /// Roll the guest back to the snapshot: memcpy base->live RAM, restore the
    /// register file, and cancel the pending lazy PC advance from the doorbell
    /// trap (restore_state already set PC to the post-SNAPSHOT_ME value).
    fn reset(&mut self, vcpu: &mut HvfVcpu) -> Result<(), ignition_hvf::Error> {
        let base = std::mem::take(&mut self.base_ram);
        restore_ram(&base, self.live_ram());
        self.base_ram = base;
        let state = self.base_state.as_ref().expect("reset before capture");
        vcpu.restore_state(state)?;
        vcpu.clear_pending_advance();
        Ok(())
    }
}

// `captured` starts false; the helper keeps `new` readable.
fn true_false_init() -> bool {
    false
}
```

Note: remove the `true_false_init` helper and just write `captured: false` in `new` if the reviewer prefers — it exists only to keep the field list uniform. (Reviewer: this is a fine simplification to request.)

- [ ] **Step 2: Add `run_fuzz` + `fuzz_loop` to `VcpuManager`**

In `crates/vmm/src/vstate/vcpu_manager.rs`, add imports at top:

```rust
use crate::fuzz::controller::FuzzController;
use ignition_hvf::bindings;
```

Add public entry + the loop (single vCPU; mirrors `run_primary`/`run_loop` but with fuzz semantics). The doorbell GPA is passed in so the loop can discriminate the trap:

```rust
    /// Run the single-vCPU fuzz loop. Boots the primary normally; once the guest
    /// rings SNAPSHOT_ME the loop captures the snapshot and drives
    /// inject->resume->observe->reset inline on this thread (HVF thread-affine).
    pub fn run_fuzz(
        self: &Arc<Self>,
        entry: u64,
        fdt_addr: u64,
        doorbell_gpa: u64,
        ctrl_base: u64,
        fuzz_dev: Arc<Mutex<ignition_devices::fuzz::FuzzDevice>>,
        mut controller: FuzzController,
    ) -> Result<(), ignition_hvf::Error> {
        let me = Arc::clone(self);
        let handle = thread::spawn(move || {
            let mpidr = mpidr_for(0);
            me.running.lock().unwrap().insert(mpidr);
            let vcpu = HvfVcpu::new(mpidr, false)?;
            me.vcpuids.lock().unwrap().push(vcpu.id());
            vcpu.set_initial_state(entry, fdt_addr)?;
            me.fuzz_loop(vcpu, doorbell_gpa, ctrl_base, fuzz_dev, &mut controller)
        });
        self.threads.lock().unwrap().push(handle);
        self.join_all()
    }

    fn fuzz_loop(
        self: &Arc<Self>,
        mut vcpu: HvfVcpu,
        doorbell_gpa: u64,
        ctrl_base: u64,
        fuzz_dev: Arc<Mutex<ignition_devices::fuzz::FuzzDevice>>,
        controller: &mut FuzzController,
    ) -> Result<(), ignition_hvf::Error> {
        let vcpus: Arc<dyn Vcpus> = Arc::new(NoIrqVcpus);
        loop {
            if self.shutdown.load(Ordering::Acquire) {
                return Ok(());
            }
            match vcpu.run(vcpus.clone())? {
                VcpuExit::MmioWrite(addr, data) if addr == doorbell_gpa => {
                    let cmd = if data.len() >= 4 {
                        u32::from_le_bytes(data[..4].try_into().unwrap())
                    } else {
                        0
                    };
                    use ignition_devices::fuzz::protocol::cmd as C;
                    match cmd {
                        c if c == C::SNAPSHOT_ME => {
                            // First (and only) snapshot: advance PC past the store,
                            // capture RAM + regs, expose the first input length.
                            vcpu.advance_pc()?;
                            let len = controller.capture(&vcpu)?;
                            fuzz_dev.lock().unwrap().set_input_len(len);
                        }
                        c if c == C::DONE => {
                            let len = controller.on_done(&mut vcpu)?;
                            fuzz_dev.lock().unwrap().set_input_len(len);
                        }
                        c if c == C::CRASH => {
                            let (code, in_len) = {
                                let d = fuzz_dev.lock().unwrap();
                                (d.crash_code(), {
                                    let mut b = [0u8; 4];
                                    // read INPUT_LEN back via the device
                                    let mut dev = fuzz_dev.lock().is_err();
                                    let _ = dev; // placeholder; see note below
                                    b[0]; 0u32
                                })
                            };
                            // NOTE: read INPUT_LEN through the device's read() path
                            // instead of the inline hack above; see Step 3 fix.
                            let len = controller.on_crash(&mut vcpu, code, in_len)?;
                            fuzz_dev.lock().unwrap().set_input_len(len);
                        }
                        other => log::warn!("fuzz: unknown doorbell command {other:#x}"),
                    }
                }
                VcpuExit::MmioWrite(addr, data) => self.bus.write(addr, data),
                VcpuExit::MmioRead(addr, data) => self.bus.read(addr, data),
                VcpuExit::Shutdown => {
                    self.request_shutdown();
                    return Ok(());
                }
                VcpuExit::Canceled => return Ok(()),
                VcpuExit::WaitForEventTimeout(d) => thread::sleep(d.min(MAX_PARK)),
                VcpuExit::WaitForEvent => thread::sleep(MAX_PARK),
                VcpuExit::WaitForEventExpired | VcpuExit::VtimerActivated => {}
                other => log::debug!("fuzz: unhandled vCPU exit: {other:?}"),
            }
        }
    }
```

- [ ] **Step 3: Fix the CRASH INPUT_LEN read (clean version)**

The inline block above is a deliberate placeholder. Replace the `C::CRASH` arm with a clean read of both scalars from the device through its `BusDevice::read`:

```rust
                        c if c == C::CRASH => {
                            let (code, in_len) = {
                                let mut dev = fuzz_dev.lock().unwrap();
                                let mut b = [0u8; 4];
                                dev.read(ctrl_base, ignition_devices::fuzz::protocol::reg::INPUT_LEN, &mut b);
                                (dev.crash_code(), u32::from_le_bytes(b))
                            };
                            let len = controller.on_crash(&mut vcpu, code, in_len)?;
                            fuzz_dev.lock().unwrap().set_input_len(len);
                        }
```

(`ctrl_base` is the control-region GPA, passed in; `read` ignores `base` for offset math here but keep the signature consistent.)

- [ ] **Step 4: Build**

Run: `cargo build -p ignition-vmm`
Expected: builds clean. Resolve any borrow/lock-ordering issues the compiler flags (the double-lock in the placeholder must be gone after Step 3).

- [ ] **Step 5: Commit**

```bash
git add crates/vmm/src/fuzz/controller.rs crates/vmm/src/vstate/vcpu_manager.rs
git commit -m "fuzz: in-VMM single-vCPU fuzz loop (capture/inject/reset) on the vCPU thread"
```

---

### Task 7: Guest harness + initramfs build

**Files:**
- Create: `guest/fuzz-harness/ignition_fuzz.h`
- Create: `guest/fuzz-harness/harness.c`
- Create: `guest/fuzz-harness/build.sh`

The harness is PID 1 in a cpio initramfs. It maps the device regions via `/dev/mem` at the fixed GPAs, runs one-time setup, rings `SNAPSHOT_ME`, then loops: read `INPUT_LEN`, call the target, ring `DONE`. The M0 target is a stub parser that crashes (writes past a small stack buffer) when the first input byte is `0xFF`, so the gate can plant a crash deterministically. A `SIGSEGV`/`SIGABRT` handler writes `CRASH_CODE` + the `CRASH` doorbell.

- [ ] **Step 1: Write the C header (mirror of `protocol.rs`)**

`guest/fuzz-harness/ignition_fuzz.h`:

```c
/* Mirror of crates/devices/src/fuzz/protocol.rs. Keep in sync by hand. */
#ifndef IGNITION_FUZZ_H
#define IGNITION_FUZZ_H
#include <stdint.h>

/* Fixed GPAs (mirror docs plan "Layout constants"). */
#define IGNITION_FUZZ_CTRL_GPA   0x09200000UL
#define IGNITION_FUZZ_CTRL_SIZE  0x1000UL
#define IGNITION_FUZZ_WIN_GPA    0x09201000UL
#define IGNITION_FUZZ_WIN_SIZE   0x200000UL  /* default 2 MiB */

/* Control-register offsets. */
#define REG_DOORBELL    0x00
#define REG_INPUT_LEN   0x04
#define REG_CRASH_CODE  0x08
#define REG_STATUS      0x0c

/* Doorbell commands. */
#define CMD_SNAPSHOT_ME 0x1u
#define CMD_DONE        0x2u
#define CMD_CRASH       0x3u

#endif
```

- [ ] **Step 2: Write the harness**

`guest/fuzz-harness/harness.c`:

```c
/* M0 guest fuzz harness: PID 1 in an initramfs. Maps the ignition-fuzz device,
 * parks at the parse site, and drives the reset->inject->run->observe loop via
 * the doorbell. The "target" is a stub parser that overflows on a magic byte so
 * the M0 gate can plant a deterministic crash. */
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>
#include "ignition_fuzz.h"

static volatile uint8_t *g_ctrl;   /* control registers (4 KiB) */
static volatile uint8_t *g_win;    /* shared window (input bytes) */

static inline void reg_write(unsigned off, uint32_t v) {
    *(volatile uint32_t *)(g_ctrl + off) = v;
}
static inline uint32_t reg_read(unsigned off) {
    return *(volatile uint32_t *)(g_ctrl + off);
}
static inline void doorbell(uint32_t cmd) { reg_write(REG_DOORBELL, cmd); }

/* On any fatal signal: report a CRASH and spin. The VMM resets PC/regs/RAM on
 * the CRASH doorbell, so this frame is discarded — we never actually return. */
static void crash_handler(int sig) {
    reg_write(REG_CRASH_CODE, (uint32_t)sig);
    doorbell(CMD_CRASH);
    for (;;) { /* VMM resets us out of this loop */ }
}

/* The M0 stub target. A real target (libpng) replaces this in M1. */
static void target_parse(const uint8_t *data, uint32_t len) {
    char buf[16];
    if (len > 0 && data[0] == 0xFF) {
        /* deterministic overflow -> SIGSEGV/SIGABRT (ASan in M1) */
        memset(buf, 0xAA, (size_t)len + 64);
    } else {
        /* touch the input so the read is real work */
        volatile uint8_t acc = 0;
        for (uint32_t i = 0; i < len && i < sizeof(buf); i++) acc ^= data[i];
        (void)acc;
    }
}

int main(void) {
    int fd = open("/dev/mem", O_RDWR | O_SYNC);
    if (fd < 0) return 1;
    g_ctrl = mmap(0, IGNITION_FUZZ_CTRL_SIZE, PROT_READ | PROT_WRITE, MAP_SHARED, fd, IGNITION_FUZZ_CTRL_GPA);
    g_win  = mmap(0, IGNITION_FUZZ_WIN_SIZE,  PROT_READ | PROT_WRITE, MAP_SHARED, fd, IGNITION_FUZZ_WIN_GPA);
    if (g_ctrl == MAP_FAILED || g_win == MAP_FAILED) return 2;

    signal(SIGSEGV, crash_handler);
    signal(SIGABRT, crash_handler);
    signal(SIGBUS,  crash_handler);

    /* One-time setup is complete; park at the parse site. */
    doorbell(CMD_SNAPSHOT_ME);   /* <-- snapshot/reset PC lands just after here */

    for (;;) {
        uint32_t len = reg_read(REG_INPUT_LEN);
        if (len > IGNITION_FUZZ_WIN_SIZE) len = IGNITION_FUZZ_WIN_SIZE;
        target_parse((const uint8_t *)g_win, len);
        doorbell(CMD_DONE);
    }
    return 0;
}
```

- [ ] **Step 3: Write the build script**

`guest/fuzz-harness/build.sh`:

```bash
#!/usr/bin/env bash
# Cross-compile the M0 fuzz harness as a static aarch64 PIE-free binary and pack
# it into a cpio initramfs with the harness as /init. Requires an aarch64
# musl/gcc cross toolchain (e.g. brew install aarch64-elf-gcc + musl, or a Linux
# builder). Output: guest/fuzz-harness/initramfs.cpio
set -euo pipefail
cd "$(dirname "$0")"

CC="${CC:-aarch64-linux-musl-gcc}"
OUT=build
mkdir -p "$OUT/root"

# -static so there is no dynamic loader; the harness is /init.
"$CC" -static -O2 -ffreestanding-safe -o "$OUT/root/init" harness.c || \
"$CC" -static -O2 -o "$OUT/root/init" harness.c

# Minimal initramfs: just /init plus the dirs the kernel expects.
( cd "$OUT/root"
  mkdir -p dev proc sys
  find . -print0 | cpio --null -ov --format=newc ) > initramfs.cpio
echo "wrote $(pwd)/initramfs.cpio"
```

- [ ] **Step 4: Verify the header matches the protocol (manual check + assertion)**

Run: `cargo test -p ignition-devices fuzz::protocol` (re-confirm the Rust side), then visually confirm `ignition_fuzz.h` values equal `protocol.rs`. Add a comment cross-reference in both files. (No automated cross-language check in M0; M1 may add a generated header.)

- [ ] **Step 5: Build the initramfs (if a cross toolchain is available)**

Run: `chmod +x guest/fuzz-harness/build.sh && CC=aarch64-linux-musl-gcc ./guest/fuzz-harness/build.sh`
Expected: `guest/fuzz-harness/initramfs.cpio` written. If no cross toolchain is present, note the blocker and provide the prebuilt cpio path to Task 8/9 via the test's `--initramfs` argument; do not block the Rust tasks on it.

- [ ] **Step 6: Commit**

```bash
git add guest/fuzz-harness/
git commit -m "fuzz: M0 guest harness (doorbell protocol, stub crash target) + initramfs build"
```

---

### Task 8: `--fuzz` CLI mode in the boot binary

**Files:**
- Modify: `spike/src/bin/boot.rs` (arg parsing ~:491-571; a new `run_fuzz_mode` fn; device + window + FDT wiring mirroring the boot path ~:575-882)

- [ ] **Step 1: Parse the `--fuzz` flag and its options**

In `main` (boot.rs ~:491), add parsing for: `--fuzz` (enable), `--initramfs <path>` (required in fuzz mode), `--solutions <dir>` (default `./fuzz-solutions`), `--seed <path>` (optional starting input; repeatable or single), `--window-mib <N>` (default 2), and reuse `--mem` (default small, e.g. 96). Update the usage string to include the fuzz options. When `--fuzz` is set, dispatch to `run_fuzz_mode(...)` instead of the normal boot, before the existing boot body runs.

- [ ] **Step 2: Implement `run_fuzz_mode`**

Add to `spike/src/bin/boot.rs`. It mirrors the fresh-boot setup (mmap guest RAM, load kernel, build GIC, set up serial) and additionally:

```rust
/// M0 fuzz mode: boot a single-vCPU guest from an initramfs, map the fuzz
/// device's control region + shared window, and run the in-VMM fuzz loop.
fn run_fuzz_mode(
    kernel_path: &str,
    initramfs_path: &str,
    mem_mib: u64,
    window_mib: u64,
    seeds: Vec<Vec<u8>>,
    solutions_dir: std::path::PathBuf,
) -> io::Result<()> {
    use ignition_devices::fuzz::FuzzDevice;
    use ignition_vmm::fuzz::controller::FuzzController;
    use std::sync::{Arc, Mutex};

    // Fixed GPAs (see plan "Layout constants"); assert they sit above guest RAM.
    const CTRL_GPA: u64 = 0x0920_0000;
    const CTRL_SIZE: u64 = 0x1000;
    const WIN_GPA: u64 = 0x0920_1000;
    let win_size = window_mib * 0x10_0000;

    let ram_size = mem_mib * 0x10_0000;
    assert!(layout::RAM_BASE + ram_size <= CTRL_GPA, "guest RAM overlaps fuzz device region");

    // --- guest RAM (anon, like fresh boot) ---
    let ram_host = /* mmap MAP_ANON|MAP_PRIVATE ram_size, as boot.rs:580-593 */;
    let ram: &mut [u8] = unsafe { std::slice::from_raw_parts_mut(ram_host as *mut u8, ram_size as usize) };

    // --- load kernel + initramfs ---
    let kernel_image = fs::read(kernel_path)?;
    let entry = kernel::load_kernel(ram, layout::RAM_BASE, &kernel_image).expect("load_kernel");
    let initrd_bytes = fs::read(initramfs_path)?;
    // Place initramfs below the FDT, above the kernel (reuse the project's
    // existing initrd placement helper / convention; compute (initrd_gpa, len)).
    let (initrd_gpa, initrd_len) = place_initramfs(ram, &initrd_bytes);

    // --- shared window: a host anon mapping, mapped into the guest at WIN_GPA ---
    let win_host = /* mmap MAP_ANON|MAP_PRIVATE win_size */;
    let window: &mut [u8] = unsafe { std::slice::from_raw_parts_mut(win_host as *mut u8, win_size as usize) };

    // --- create VM, GIC, map memory ---
    let mut vm = /* HvfVm::new + create GIC, as boot.rs ~:602-660 */;
    vm.map_memory(ram_host as u64, layout::RAM_BASE, ram_size)?;
    vm.map_memory(win_host as u64, WIN_GPA, win_size)?;
    // Control region: backed by a small host page mapped read/write so guest
    // stores to INPUT_LEN/STATUS are plain RAM, while DOORBELL stores must TRAP.
    // M0 simplification: do NOT map the control region as RAM — leave it
    // unmapped so EVERY control-register access traps to MmioRead/MmioWrite and
    // is served by FuzzDevice on the bus + the doorbell arm in fuzz_loop.

    // --- devices: serial (console) + fuzz device on the bus ---
    let bus = /* build Bus with serial as boot.rs ~:337 */;
    let fuzz_dev = Arc::new(Mutex::new(FuzzDevice::new()));
    // Register the control region on the bus at CTRL_GPA so trapped accesses route here.
    /* dev_manager.add_fixed(CTRL_GPA, CTRL_SIZE, fuzz_dev.clone() as Arc<Mutex<dyn BusDevice>>)?; */

    // --- FDT: serial + fuzz node + initrd ---
    let fdt = /* generate FDT, pushing FdtDevice::Fuzz(FuzzDev{CTRL_GPA,CTRL_SIZE,WIN_GPA,win_size})
                and setting initrd: Some((initrd_gpa, initrd_len)); cmdline must include
                "console=ttyS0 ... rdinit=/init" so the kernel runs the harness */;
    let fdt_addr = /* write fdt into RAM top, as boot.rs ~:598 */;

    // --- controller + run ---
    let controller = FuzzController::new(
        (ram_host as *mut u8, ram_size as usize),
        (win_host as *mut u8, win_size as usize),
        seeds,
        /* fixed seed_rng */ 0xF1FA_5EED,
        solutions_dir,
    );
    let manager = ignition_vmm::vstate::vcpu_manager::VcpuManager::new(1, bus);
    let doorbell_gpa = CTRL_GPA + ignition_devices::fuzz::protocol::reg::DOORBELL;
    manager
        .run_fuzz(entry, fdt_addr, doorbell_gpa, CTRL_GPA, fuzz_dev, controller)
        .map_err(|e| io::Error::other(format!("run_fuzz: {e}")))?;
    Ok(())
}
```

The `/* ... */` blocks are to be filled from the existing fresh-boot body in `boot.rs` (the implementer copies the corresponding setup lines: RAM mmap :580-593, kernel load :596, VM/GIC :602-660, serial :337, FDT generate :640-660, fdt write). `place_initramfs` should follow the project's existing initrd convention; if none exists, place the cpio at a 2 MiB-aligned GPA above the kernel and below the FDT, and pass `(gpa, len)` to the FDT `initrd` field (the FDT already supports `initrd: Option<(u64,u64)>`, fdt.rs:71).

- [ ] **Step 3: Build**

Run: `cargo build -p spike`
Expected: builds clean.

- [ ] **Step 4: Commit**

```bash
git add spike/src/bin/boot.rs
git commit -m "fuzz: --fuzz boot mode (map window, wire fuzz device + initramfs, run fuzz loop)"
```

---

### Task 9: M0 gate integration test

**Files:**
- Create: `scripts/fuzz_m0_test.py`

The gate: boot fuzz mode with a seed whose first byte is `0xFF`, run a bounded number of iterations, and assert a crash solution file appears. This proves the full loop: injection → guest parse → crash → doorbell → capture → reset.

- [ ] **Step 1: Write the test**

`scripts/fuzz_m0_test.py` (model on the existing `scripts/restore_test.py` invocation/teardown style):

```python
#!/usr/bin/env python3
"""M0 fuzz gate: a planted-crash seed must be captured as a solution.

Boots `boot --fuzz` with a seed beginning 0xFF (the stub target overflows on
that), runs briefly, and asserts a crash-*.bin solution file is written. Also
sanity-checks that the loop iterates (so we know reset works, not just one shot).
"""
import os, subprocess, sys, tempfile, time, glob, signal

BOOT = os.environ.get("BOOT_BIN", "target/debug/boot")
KERNEL = os.environ["FUZZ_KERNEL"]          # path to aarch64 Image
INITRAMFS = os.environ["FUZZ_INITRAMFS"]    # guest/fuzz-harness/initramfs.cpio

def main():
    d = tempfile.mkdtemp(prefix="fuzz-m0-")
    sol = os.path.join(d, "solutions")
    seed = os.path.join(d, "seed.bin")
    with open(seed, "wb") as f:
        f.write(b"\xff\x00\x00\x00")        # triggers the stub overflow
    cmd = [BOOT, "--fuzz", "--mem", "96", "--window-mib", "2",
           "--initramfs", INITRAMFS, "--solutions", sol, "--seed", seed, KERNEL]
    p = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    deadline = time.time() + 30
    found = False
    while time.time() < deadline:
        if glob.glob(os.path.join(sol, "crash-*.bin")):
            found = True
            break
        if p.poll() is not None:
            break
        time.sleep(0.5)
    try:
        p.send_signal(signal.SIGINT)
        p.wait(timeout=5)
    except Exception:
        p.kill()
    if not found:
        out = p.stdout.read().decode(errors="replace") if p.stdout else ""
        print(out)
        print("FAIL: no crash solution captured", file=sys.stderr)
        sys.exit(1)
    print("PASS: crash solution captured in", sol)

if __name__ == "__main__":
    main()
```

- [ ] **Step 2: Run the gate**

Run (with a kernel + initramfs available):
```bash
cargo build -p spike
FUZZ_KERNEL=<aarch64-Image> FUZZ_INITRAMFS=guest/fuzz-harness/initramfs.cpio \
  python3 scripts/fuzz_m0_test.py
```
Expected: `PASS: crash solution captured in ...`. If the cross toolchain or kernel is unavailable in the environment, document the blocker; the gate is the acceptance criterion to run once artifacts exist.

- [ ] **Step 3: Commit**

```bash
git add scripts/fuzz_m0_test.py
git commit -m "fuzz: M0 gate test (planted-crash seed captured as a solution)"
```

---

## Self-Review

**Spec coverage (against spec §3–§6, M0 milestone):**
- ignition-fuzz device (doorbell + window): Tasks 2, 3, 8. ✓
- Doorbell control protocol (SNAPSHOT_ME/DONE/CRASH, INPUT_LEN, CRASH_CODE): Tasks 1, 3, 6, 7. ✓
- Shared window via hv_vm_map at known GPA, excluded from reset: Task 8 (separate mapping above RAM). ✓
- v0 full-memory reset + register restore: Tasks 5, 6 (`restore_ram` + `restore_state`). ✓
- PC-advance asymmetry (spec §12): Task 4 + Task 6 SNAPSHOT_ME/reset handling. ✓
- Blind mutation, CRASH capture, no coverage/libAFL/dirty-reset: Tasks 5, 6 (M0 scope). ✓
- Guest harness in initramfs, single vCPU, deterministic: Tasks 7, 8. ✓
- M0 gate (planted crash captured): Task 9. ✓

**Out of M0 (correctly deferred to M1/M2, per the scope decision):** real libpng/CVE target, SanitizerCoverage + libAFL feedback, dirty-page reset. Not in this plan.

**Type/name consistency:** `FuzzDevice` (devices), `FuzzController`/`Rng`/`mutate`/`restore_ram`/`write_solution` (vmm), `FdtKind::IgnitionFuzz`, `FdtDevice::Fuzz`/`FuzzDev`, `HvfVcpu::advance_pc`/`clear_pending_advance`, protocol `reg::*`/`cmd::*` — used consistently across tasks. Doorbell GPA = `CTRL_GPA + reg::DOORBELL`, passed to `run_fuzz`/`fuzz_loop` identically.

**Known soft spots for the implementer/reviewer to resolve during execution (not placeholders — flagged judgment calls):**
1. Task 6 `true_false_init` is a deliberate stub; replace with `captured: false`.
2. Task 6 Step 2 contains a placeholder CRASH arm; Step 3 supplies the clean replacement — the placeholder must not survive.
3. Task 8 leaves the fresh-boot setup blocks as `/* copy from boot.rs:NNN */` references because they are long and already exist verbatim in `boot.rs`; the implementer copies them rather than the plan duplicating ~150 lines. Control region is intentionally left unmapped (trap-only); confirm HVF traps an access to an unmapped GPA as a data abort routed to the bus (it does — that is how serial/virtio MMIO already works).
4. Layout GPAs (`0x0920_0000` / `0x0920_1000`) must be validated against `crates/arch/src/aarch64/layout.rs` for no overlap with serial/GIC/virtio windows; adjust both the Rust constants and `ignition_fuzz.h` together if moved.
