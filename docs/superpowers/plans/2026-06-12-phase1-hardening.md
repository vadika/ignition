# Phase 1 Hardening (3 follow-ups) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close three carried-forward correctness/layering items from `docs/phase1-followups.md`: the halfword-MMIO write panic, the no-op `Vm` memory-ownership wrapper, and `Bus::register` overlap validation.

**Architecture:** Three independent changes. (1) De-duplicate the hvf MMIO encode/decode so read and write support the same access sizes (1/2/4/8), killing the len-2 panic. (2) Give `Vm` real `map_memory` that records regions and make its `hvf` field private; migrate the two bins. (3) Make `Bus::register` validate non-overlap and return `Result`; migrate callers.

**Tech Stack:** Rust, Apple Hypervisor.framework via the `hvf` crate.

---

## Background the engineer needs

- **hvf MMIO** (`crates/hvf/src/lib.rs`, in `HvfVcpu::run`): on a data abort the
  access length is `len = 1 << sas` where `sas` is a 2-bit field, so `len` is
  always one of `{1, 2, 4, 8}`. The READ path (~line 562) already decodes 1/2/4/8;
  the WRITE path (~line 642) only encodes 1/4/8 and `panic!`s on 2. A guest doing a
  halfword (`strh`) MMIO store hits the panic. Fix the divergence at the root by
  sharing one encode and one decode helper.
- **Vm** (`crates/vmm/src/vstate/hvf_vm.rs`): currently `pub struct Vm { pub hvf:
  HvfVm }`; the only reach-through is `vm.hvf.map_memory(host, guest, size)` in
  `spike/src/bin/boot.rs:273` and `spike/src/bin/uart-echo.rs:74`. `HvfVm::map_memory(&self, host_start_addr: u64, guest_start_addr: u64, size: u64) -> Result<(), hvf::Error>`.
- **Bus** (`crates/devices/src/bus.rs`): `register(&mut self, base, len, dev)` pushes
  without validation. Callers: `spike/src/bin/boot.rs:298,316`,
  `spike/src/bin/uart-echo.rs:83`, and two bus unit tests.
- **Build/test commands:** `cargo test -p ignition-hvf`, `cargo test -p
  ignition-devices`, `cargo build --workspace`, `cargo clippy --workspace`. The
  crate package names are `ignition-hvf`, `ignition-devices`, `ignition-vmm`,
  `hvf-spike` (verify with `cargo metadata` if unsure; the spike bins are
  `--bin boot` / `--bin uart-echo`).
- **Commit policy:** plain messages only — NO `Co-Authored-By` / "Generated with
  Claude" trailer.
- `Vm` is not constructible under `cargo test` (its `new` calls `hv_vm_create`,
  which needs the hypervisor entitlement), so Task 2 has no unit test — it is
  verified by the workspace build and the existing boot harness. That is expected;
  do not invent a test that requires a live VM.

## File structure

- **Modify `crates/hvf/src/lib.rs`** — add `encode_mmio_le` / `decode_mmio_le`
  helpers + tests; use them in both MMIO paths (Task 1).
- **Modify `crates/vmm/src/vstate/hvf_vm.rs`** — `MappedRegion`, region tracking,
  `Vm::map_memory`, private `hvf` (Task 2).
- **Modify `spike/src/bin/boot.rs` and `spike/src/bin/uart-echo.rs`** — call
  `vm.map_memory(...)` instead of `vm.hvf.map_memory(...)`; handle the
  `Bus::register` `Result` (Tasks 2 + 3).
- **Modify `crates/devices/src/bus.rs`** — `BusError`, overlap-checking `register`
  returning `Result`, + an overlap test (Task 3).

---

## Task 1: Fix the halfword-MMIO write panic (share encode/decode)

**Files:**
- Modify: `crates/hvf/src/lib.rs`

- [ ] **Step 1: Write the failing tests**

Add a test module at the end of `crates/hvf/src/lib.rs` (or extend an existing
`#[cfg(test)] mod tests` if one is present — check first; if present, add these two
functions to it instead of a new module):

```rust
#[cfg(test)]
mod mmio_tests {
    use super::{decode_mmio_le, encode_mmio_le};

    #[test]
    fn encode_roundtrips_all_access_sizes() {
        for &len in &[1usize, 2, 4, 8] {
            let mut buf = [0u8; 8];
            let val: u64 = 0x1122_3344_5566_7788;
            encode_mmio_le(&mut buf, val, len);
            // Only the low `len` bytes are written, little-endian.
            let expected = &val.to_le_bytes()[..len];
            assert_eq!(&buf[..len], expected, "encode len={len}");
            // And decode recovers the zero-extended low `len` bytes.
            let mask = if len == 8 { u64::MAX } else { (1u64 << (len * 8)) - 1 };
            assert_eq!(decode_mmio_le(&buf, len), val & mask, "decode len={len}");
        }
    }

    #[test]
    fn halfword_write_does_not_panic() {
        let mut buf = [0u8; 8];
        encode_mmio_le(&mut buf, 0xBEEF, 2);
        assert_eq!(&buf[..2], &[0xEF, 0xBE]);
        assert_eq!(decode_mmio_le(&buf, 2), 0xBEEF);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ignition-hvf mmio_tests 2>&1 | tail -15`
Expected: FAIL — `cannot find function decode_mmio_le` / `encode_mmio_le`.

- [ ] **Step 3: Add the two helpers**

Add at module scope in `crates/hvf/src/lib.rs` (near the other free functions, e.g.
just above `impl HvfVcpu`):

```rust
/// Write the low `len` little-endian bytes of `val` into `buf` (the MMIO data
/// buffer). `len` is `1 << sas` from the data-abort syndrome, so it is always one
/// of 1, 2, 4, 8.
fn encode_mmio_le(buf: &mut [u8], val: u64, len: usize) {
    let bytes = val.to_le_bytes();
    buf[..len].copy_from_slice(&bytes[..len]);
}

/// Read `len` little-endian bytes from `buf` as a zero-extended `u64`. `len` is
/// `1 << sas`, always one of 1, 2, 4, 8.
fn decode_mmio_le(buf: &[u8], len: usize) -> u64 {
    let mut bytes = [0u8; 8];
    bytes[..len].copy_from_slice(&buf[..len]);
    u64::from_le_bytes(bytes)
}
```

- [ ] **Step 4: Use the helpers in both MMIO paths**

In `crates/hvf/src/lib.rs`, replace the READ-completion `match mmio_read.len { ... }`
block (the one assigning `let val = match mmio_read.len { 1 => ..., 8 => ..., _ =>
panic!(...) };`, ~line 562) with:

```rust
            let val = decode_mmio_le(&self.mmio_buf, mmio_read.len);
```

And replace the WRITE `match len { 1 => ..., 4 => ..., 8 => ..., _ => panic!(...) }`
block (~line 642) with:

```rust
                    encode_mmio_le(&mut self.mmio_buf, val, len);
```

(Both `panic!` arms are removed; `len`/`mmio_read.len` is always 1/2/4/8 so no case
is lost, and halfword now works on both paths.)

- [ ] **Step 5: Run tests to verify they pass + build**

Run:
```bash
cargo test -p ignition-hvf 2>&1 | grep 'test result'
cargo build -p ignition-hvf 2>&1 | tail -1
cargo clippy -p ignition-hvf 2>&1 | grep -c 'warning:'
```
Expected: tests pass (incl. the 2 new), builds, 0 clippy warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/hvf/src/lib.rs
git commit -m "fix(hvf): support halfword MMIO writes; share MMIO encode/decode"
```

---

## Task 2: `Vm` owns mapped regions; make `hvf` private

**Files:**
- Modify: `crates/vmm/src/vstate/hvf_vm.rs`
- Modify: `spike/src/bin/boot.rs`
- Modify: `spike/src/bin/uart-echo.rs`

- [ ] **Step 1: Rewrite `crates/vmm/src/vstate/hvf_vm.rs`**

Replace the file's `Vm` struct/impl with region tracking, a real `map_memory`, and a
private `hvf` field:

```rust
// VM lifecycle and guest-memory mapping over Hypervisor.framework.
//
// Phase 1: `Vm` owns the guest-memory regions it maps into HVF, so later
// milestones (snapshot/dirty-tracking) have a single place that knows the layout.
//
// Replaces: firecracker/src/vmm/src/vstate/vm.rs (+ the kvm.rs bits).

pub use hvf::HvfVm;

/// One host->guest mapping handed to HVF, retained so the VM owns its layout.
#[derive(Clone, Copy, Debug)]
pub struct MappedRegion {
    pub host_addr: u64,
    pub guest_addr: u64,
    pub size: u64,
}

/// Owns the HVF VM handle and the guest-memory regions mapped into it.
/// TODO(phase1): add dirty-tracking hooks for snapshot on top of `regions`.
pub struct Vm {
    hvf: HvfVm,
    regions: Vec<MappedRegion>,
}

impl Vm {
    pub fn new(nested_enabled: bool) -> Result<Self, hvf::Error> {
        Ok(Self {
            hvf: HvfVm::new(nested_enabled)?,
            regions: Vec::new(),
        })
    }

    /// Map a host range into the guest and record it. Same argument order as
    /// `hvf::HvfVm::map_memory` (host, guest, size).
    pub fn map_memory(&mut self, host_addr: u64, guest_addr: u64, size: u64) -> Result<(), hvf::Error> {
        self.hvf.map_memory(host_addr, guest_addr, size)?;
        self.regions.push(MappedRegion { host_addr, guest_addr, size });
        Ok(())
    }

    /// The regions mapped into this VM, in insertion order.
    pub fn regions(&self) -> &[MappedRegion] {
        &self.regions
    }
}
```

- [ ] **Step 2: Migrate `spike/src/bin/boot.rs`**

Find (around line 271-275):

```rust
    vm.hvf
        .map_memory(host_addr, layout::RAM_BASE, RAM_SIZE)
        .expect("hv_vm_map failed");
```

The `vm` binding must be mutable now. Change its declaration `let vm = Vm::new(false)...`
to `let mut vm = Vm::new(false)...`, and replace the map call with:

```rust
    vm.map_memory(host_addr, layout::RAM_BASE, RAM_SIZE)
        .expect("hv_vm_map failed");
```

- [ ] **Step 3: Migrate `spike/src/bin/uart-echo.rs`**

Find (around line 74) the same `vm.hvf.map_memory(...)` call and the `let vm =
Vm::new(...)` binding. Change the binding to `let mut vm = ...` and replace:

```rust
    vm.hvf
        .map_memory(/* existing args */)
        .expect(/* existing message */);
```

with the equivalent `vm.map_memory(/* same args */).expect(/* same message */);`
(keep the exact host/guest/size arguments and expect message that are already there
— read the file to copy them verbatim; do not change the values).

- [ ] **Step 4: Build the workspace**

Run: `cargo build --workspace 2>&1 | tail -3`
Expected: builds. If anything else referenced `vm.hvf` it will fail to compile —
the grep showed only these two bins, but if a compile error names `hvf` being
private elsewhere, STOP and report it (do not re-expose the field).

- [ ] **Step 5: Re-sign and smoke-test the boot harness still boots**

Run:
```bash
cargo build -p hvf-spike --bin boot 2>&1 | tail -1
./scripts/sign.sh target/debug/boot
( target/debug/boot kimage/out/Image kimage/out/rootfs.ext4 >/tmp/h2.out 2>/dev/null & p=$!; sleep 35; kill -9 $p 2>/dev/null; wait $p 2>/dev/null )
grep -c 'login:' /tmp/h2.out
```
Expected: `1` (the guest still reaches the login prompt — memory mapping still
works through the new `Vm::map_memory`). If `0`, check `/tmp/h2.out` and report.

- [ ] **Step 6: Commit**

```bash
git add crates/vmm/src/vstate/hvf_vm.rs spike/src/bin/boot.rs spike/src/bin/uart-echo.rs
git commit -m "refactor(vmm): Vm owns mapped memory regions; make hvf field private"
```

---

## Task 3: `Bus::register` overlap validation

**Files:**
- Modify: `crates/devices/src/bus.rs`
- Modify: `spike/src/bin/boot.rs`
- Modify: `spike/src/bin/uart-echo.rs`

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `crates/devices/src/bus.rs`:

```rust
    #[test]
    fn overlapping_register_is_rejected() {
        let a = Arc::new(Mutex::new(Recorder::default()));
        let b = Arc::new(Mutex::new(Recorder::default()));
        let mut bus = Bus::new();
        bus.register(0x1000, 0x100, a).unwrap();
        // [0x1080, 0x10C0) overlaps [0x1000, 0x1100).
        let err = bus.register(0x1080, 0x40, b).unwrap_err();
        assert_eq!(err, BusError::Overlap { base: 0x1080, len: 0x40 });
    }

    #[test]
    fn adjacent_register_is_allowed() {
        let a = Arc::new(Mutex::new(Recorder::default()));
        let b = Arc::new(Mutex::new(Recorder::default()));
        let mut bus = Bus::new();
        bus.register(0x1000, 0x100, a).unwrap();
        // [0x1100, 0x1200) is adjacent, not overlapping.
        assert!(bus.register(0x1100, 0x100, b).is_ok());
    }
```

The two existing tests (`write_routes_with_base_and_offset`,
`read_routes_with_offset`) call `bus.register(...)` without handling a `Result` —
append `.unwrap()` to those two `register` calls so they still compile.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ignition-devices bus 2>&1 | tail -15`
Expected: FAIL — `BusError` not found / `register` returns `()` not `Result`.

- [ ] **Step 3: Implement the error type + overlap check**

In `crates/devices/src/bus.rs`, add the error type above `Bus`:

```rust
/// Why a `Bus::register` was rejected.
#[derive(Debug, PartialEq, Eq)]
pub enum BusError {
    /// The requested range overlaps an already-registered device.
    Overlap { base: u64, len: u64 },
}

impl std::fmt::Display for BusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BusError::Overlap { base, len } => {
                write!(f, "MMIO range [{base:#x}, {:#x}) overlaps a registered device", base + len)
            }
        }
    }
}

impl std::error::Error for BusError {}
```

Replace `register` with an overlap-checking version:

```rust
    pub fn register(
        &mut self,
        base: u64,
        len: u64,
        dev: Arc<Mutex<dyn BusDevice>>,
    ) -> Result<(), BusError> {
        // Two half-open ranges [a, a+alen) and [b, b+blen) overlap iff
        // a < b+blen and b < a+alen.
        let overlaps = self.devices.iter().any(|(b, blen, _)| {
            base < b.saturating_add(*blen) && *b < base.saturating_add(len)
        });
        if overlaps {
            return Err(BusError::Overlap { base, len });
        }
        self.devices.push((base, len, dev));
        Ok(())
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ignition-devices 2>&1 | grep 'test result'`
Expected: PASS (the two new bus tests + all existing).

- [ ] **Step 5: Migrate the bin callers**

In `spike/src/bin/boot.rs`, the two `bus.register(...)` calls (serial ~line 298,
virtio ~line 316) now return `Result`. Append `.expect("device range overlap")` to
each:

```rust
    bus.register(layout::SERIAL_BASE, layout::SERIAL_SIZE, serial_bus)
        .expect("serial range overlap");
```
```rust
        bus.register(layout::VIRTIO_BASE, layout::VIRTIO_SIZE, virtio)
            .expect("virtio range overlap");
```

In `spike/src/bin/uart-echo.rs`, the single `bus.register(SERIAL_BASE, SERIAL_LEN,
serial)` (~line 83) — append `.expect("serial range overlap");`.

- [ ] **Step 6: Build + clippy the workspace**

Run:
```bash
cargo build --workspace 2>&1 | tail -1
cargo clippy --workspace 2>&1 | grep -c 'warning:'
```
Expected: builds, 0 clippy warnings.

- [ ] **Step 7: Commit**

```bash
git add crates/devices/src/bus.rs spike/src/bin/boot.rs spike/src/bin/uart-echo.rs
git commit -m "feat(devices): validate non-overlap in Bus::register"
```

---

## Self-review notes (resolved)

- **Spec coverage:** halfword panic (Task 1), `Vm` memory ownership + private `hvf`
  (Task 2), `Bus::register` overlap validation (Task 3) — all three followups
  covered.
- **Type consistency:** `encode_mmio_le(&mut [u8], u64, usize)` /
  `decode_mmio_le(&[u8], usize) -> u64`; `MappedRegion{host_addr,guest_addr,size}`,
  `Vm::map_memory(&mut self,u64,u64,u64)`, `Vm::regions()`; `BusError::Overlap{base,len}`,
  `register(...) -> Result<(),BusError>` used consistently across tasks and callers.
- **No unit test for Task 2** is intentional (`Vm::new` needs the hypervisor
  entitlement, unavailable under `cargo test`); verified by build + boot smoke test.
- **`vm` mutability:** Task 2 makes `map_memory` take `&mut self`, so both bins'
  `let vm` becomes `let mut vm` — called out in steps.
```
