# boot_timer Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Log `Guest-boot-time = N ms` once the guest signals boot-complete (a magic byte written to a fixed MMIO address at the end of boot).

**Architecture:** A plain `BootTimer` `BusDevice` (no FDT/SPI/snapshot) registered at a fixed top-of-MMIO-region address via a new `DeviceManager::add_fixed`. The guest writes the magic via a `devmem` line in the rootfs init.

**Tech Stack:** Rust (edition 2024), `std::time::Instant`, the existing `BusDevice`/`DeviceManager`, busybox `devmem`.

**Spec:** `docs/superpowers/specs/2026-06-13-boot-timer-design.md`

---

## File structure

- `crates/devices/src/boot_timer.rs` *(new)* — `BootTimer` device + tests.
- `crates/devices/src/lib.rs` *(modify)* — `pub mod boot_timer;`.
- `crates/arch/src/aarch64/layout.rs` *(modify)* — `BOOT_TIMER_ADDR`.
- `crates/vmm/src/device_manager.rs` *(modify)* — `add_fixed`.
- `spike/src/bin/boot.rs` *(modify)* — capture `boot_start`, register the timer.
- `kimage/build/build-rootfs.sh` *(modify)* — `boottime.start` guest hook.

---

## Task 1: `BootTimer` device

**Files:** Create `crates/devices/src/boot_timer.rs`; modify `crates/devices/src/lib.rs`.

- [ ] **Step 1: Write the failing test — create `crates/devices/src/boot_timer.rs`:**

```rust
//! Boot-timer pseudo device (Firecracker's pseudo/boot_timer.rs). The guest writes
//! a magic byte to offset 0 once at the end of boot; we log the elapsed wall time
//! since VM start. Plain BusDevice: no FDT node, no interrupt, no snapshot state.

use std::time::{Duration, Instant};

use crate::bus::BusDevice;

/// Magic value the guest writes to signal "userspace reached" (matches FC).
const MAGIC_BOOT_COMPLETE: u8 = 123;

pub struct BootTimer {
    start: Instant,
    fired: Option<Duration>,
}

impl BootTimer {
    pub fn new(start: Instant) -> BootTimer {
        BootTimer { start, fired: None }
    }
    /// The recorded boot time, once the guest has signalled (for tests/inspection).
    pub fn boot_time(&self) -> Option<Duration> {
        self.fired
    }
}

impl BusDevice for BootTimer {
    fn write(&mut self, _base: u64, offset: u64, data: &[u8]) {
        if offset != 0 || data.len() != 1 || data[0] != MAGIC_BOOT_COMPLETE {
            return;
        }
        if self.fired.is_none() {
            let elapsed = self.start.elapsed();
            self.fired = Some(elapsed);
            log::info!("Guest-boot-time = {} ms", elapsed.as_millis());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_fires_once() {
        let mut t = BootTimer::new(Instant::now());
        assert!(t.boot_time().is_none());
        t.write(0, 0, &[123]);
        let first = t.boot_time();
        assert!(first.is_some(), "magic write should record boot time");
        // a second magic write must not overwrite the recorded value
        t.write(0, 0, &[123]);
        assert_eq!(t.boot_time(), first);
    }

    #[test]
    fn non_magic_ignored() {
        let mut t = BootTimer::new(Instant::now());
        t.write(0, 0, &[1]); // wrong value
        t.write(0, 4, &[123]); // wrong offset
        t.write(0, 0, &[123, 0]); // wrong width (2 bytes)
        assert!(t.boot_time().is_none());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ignition-devices boot_timer`
Expected: FAIL to compile — `crate::boot_timer` not declared.

- [ ] **Step 3: Wire the module**

In `crates/devices/src/lib.rs`, add alongside the other `pub mod` lines:

```rust
pub mod boot_timer;
```

- [ ] **Step 4: Run tests + clippy**

Run: `cargo test -p ignition-devices boot_timer && cargo clippy -p ignition-devices`
Expected: PASS (2 tests), 0 warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/devices/src/boot_timer.rs crates/devices/src/lib.rs
git commit -m "feat(devices): boot_timer pseudo device (logs guest boot time)"
```

(Plain commit messages — no Co-Authored-By / Generated-with trailers.)

---

## Task 2: Wire boot_timer into the boot harness + guest hook

**Files:** Modify `crates/arch/src/aarch64/layout.rs`, `crates/vmm/src/device_manager.rs`, `spike/src/bin/boot.rs`, `kimage/build/build-rootfs.sh`.

- [ ] **Step 1: Add the fixed address**

In `crates/arch/src/aarch64/layout.rs`, add (after the `MMIO_*` consts):

```rust
/// Fixed MMIO address of the boot-timer pseudo device. Placed at the TOP of the
/// device MMIO region so it never collides with the DeviceManager's bump allocator
/// (which grows up from MMIO_BASE). The guest writes the magic byte here via devmem;
/// there is no FDT node (the address is an out-of-band contract, like Firecracker).
pub const BOOT_TIMER_ADDR: u64 = MMIO_BASE + MMIO_LEN - 0x1000; // 0x091F_F000
```

- [ ] **Step 2: Add `DeviceManager::add_fixed`**

In `crates/vmm/src/device_manager.rs`, add to `impl DeviceManager` (near `add`):

```rust
/// Register a plain BusDevice at a fixed guest-physical address — for pseudo
/// devices (boot_timer) that need a stable address known to the guest out-of-band
/// and have no SPI / FDT node / snapshot state. Bypasses the window/SPI allocators
/// and the record list.
pub fn add_fixed(&mut self, base: u64, len: u64, dev: Arc<Mutex<dyn BusDevice>>) -> Result<(), DeviceMgrError> {
    self.bus.register(base, len, dev).map_err(DeviceMgrError::BusOverlap)
}
```

(`BusDevice`, `Arc`, `Mutex`, and `Bus::register` are already imported/used in this file. If `BusDevice` is not in scope, add `use devices::bus::BusDevice;`.)

- [ ] **Step 3: Build to verify the helper compiles**

Run: `cargo build -p ignition-vmm && cargo clippy -p ignition-vmm`
Expected: clean, 0 warnings.

- [ ] **Step 4: Wire into `boot.rs`**

In `spike/src/bin/boot.rs`:

1. Add the import: `use devices::boot_timer::BootTimer;`.
2. Capture the start instant at the very top of the fresh-boot path (before the kernel is read/loaded — find the earliest point in the non-restore boot flow, e.g. right after arg parsing succeeds):

```rust
let boot_start = std::time::Instant::now();
```

3. After the `DeviceManager` is built and the other devices are added (before `mgr.freeze()`), register the timer:

```rust
mgr.add_fixed(
    layout::BOOT_TIMER_ADDR,
    0x1000,
    Arc::new(Mutex::new(BootTimer::new(boot_start))),
)
.expect("add boot_timer");
```

Do NOT register it in `run_restore` (the guest init runs once at boot, so it never
re-fires on restore).

- [ ] **Step 5: Build + sign + unit gate**

Run:
```bash
cargo build --workspace && cargo clippy --workspace && cargo test --workspace
scripts/sign.sh target/debug/boot
```
Expected: clean build, 0 clippy, all suites green.

- [ ] **Step 6: Add the guest hook to the rootfs build**

In `kimage/build/build-rootfs.sh`, inside the alpine provisioning block (the
`docker run ... alpine:3.19 sh -euxc '...'` heredoc), next to where
`/etc/local.d/network.start` is created, add:

```sh
  # boot-timer: signal boot-complete to the VMM by writing the magic byte 123 to
  # the boot-timer MMIO address (out-of-band fixed address; see layout::BOOT_TIMER_ADDR).
  printf "#!/bin/sh\ndevmem 0x091FF000 8 123\n" > /etc/local.d/boottime.start
  chmod +x /etc/local.d/boottime.start
```

(The `local` openrc service is already enabled — `rc-update add local boot` exists
for `network.start` — so `boottime.start` runs at boot's end. `devmem` is a busybox
applet present in the alpine rootfs.)

- [ ] **Step 7: Live verification**

The rootfs must be rebuilt for the hook to take effect (the rebuild runs in Docker,
outside this session — the human runs `kimage/build/build-rootfs.sh` and copies the
result to `kimage/out/rootfs.ext4`). Once rebuilt:

```bash
target/debug/boot kimage/out/Image kimage/out/rootfs.ext4 2>&1 | grep -i "Guest-boot-time"
```
Expected: a `Guest-boot-time = N ms` line on stderr shortly after the guest reaches
the `local` service. (`Ctrl-A x` to quit.)

If the rootfs has not yet been rebuilt with the hook, the device + wiring still
compile and the unit tests + regression pass; the live line only appears after the
rebuild. Note this in the commit message / report.

Regression (boot_timer is fresh-boot-only, not in the snapshot path):
```bash
rm -rf snapshot snapshot2
python3 scripts/restore_test.py
python3 scripts/restore_clone_test.py
```
Expected: `snapshot=True`, restore CPU ≈ 0%, responsive; both clones `marker=True`.

- [ ] **Step 8: Commit**

```bash
git add crates/arch/src/aarch64/layout.rs crates/vmm/src/device_manager.rs spike/src/bin/boot.rs kimage/build/build-rootfs.sh
git commit -m "feat(boot): register boot_timer at fixed MMIO addr + rootfs devmem hook"
```

---

## Notes for the implementer

- **boot_timer is a plain `BusDevice`, not `MmioDevice`** — it has no FDT node, no
  SPI, and no snapshot state (matches FC's pseudo device). That's why it uses the new
  `add_fixed` (plain bus registration at a fixed address) instead of `mgr.add`.
- **Fixed address must match** between `layout::BOOT_TIMER_ADDR` (`0x091F_F000`) and
  the `devmem 0x091FF000` line in the rootfs hook. If you change one, change both.
- **Top-of-region placement** keeps it clear of the bump allocator (which grows up
  from `MMIO_BASE` and has only ~6 device windows so far) — no overlap, no FDT entry.
- **`boot_start` timing:** capture it as early as possible in the fresh-boot path so
  the measured interval includes VMM setup + guest boot (FC measures from VM-config
  request time). Don't capture it inside the device add — capture once at the top.
- **No restore registration:** the guest's `boottime.start` runs once at boot; a
  restored guest is already past it, so registering on restore would never fire and
  is omitted.
- **Live line needs the rootfs rebuild** (the `devmem` hook). The code lands and is
  unit-tested + regression-green regardless; flag in the report that the live
  `Guest-boot-time` line is pending the rootfs rebuild.
