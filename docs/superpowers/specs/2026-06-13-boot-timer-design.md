# boot_timer — Design

Date: 2026-06-13. Status: approved design, ready for an implementation plan.

## Context

Sub-project **F** of the device work. The "full device model" milestone (A–E1) is
merged. boot_timer is Firecracker's `pseudo/boot_timer.rs` ported: a tiny pseudo
MMIO device that records guest boot time. The guest writes a magic byte to a fixed
address once at the end of boot; the VMM logs the elapsed wall time since VM start.

This completes the last piece of Firecracker's aarch64 device complement (the only
other FC pseudo-device; i8042 is x86-only).

### Existing pieces this builds on

- `devices::bus::BusDevice` — `read(&mut self, base, offset, &mut [u8])`,
  `write(&mut self, base, offset, &[u8])`, default no-ops.
- `vmm::device_manager::DeviceManager` — owns the `Bus`; `add`/`add_restored` register
  SPI/FDT/snapshot devices. boot_timer needs none of that, so a new `add_fixed`
  registers a plain `BusDevice` at a caller-chosen fixed address.
- `arch::aarch64::layout` — `MMIO_BASE = 0x0900_0000`, `MMIO_LEN = 0x0020_0000`,
  `MMIO_WINDOW = 0x1000`. The `DeviceManager` bump-allocates device windows from
  `MMIO_BASE` upward (~6 devices so far).
- The rootfs build (`kimage/build/build-rootfs.sh`) runs `/etc/local.d/*.start`
  scripts at the openrc `local` service (near end of boot); busybox provides `devmem`.

## Goal

On a fresh boot, the VMM logs `Guest-boot-time = N ms` once the guest signals
boot-complete. Demonstrable on stderr.

Non-goals (TODOs): CPU-time accounting (FC logs `getrusage` CPU us too — wall time
only here); a boot_timer FDT node (FC has none — the address is out-of-band); firing
on the restore path (the guest init runs once at boot, so restore never re-signals).

## Architecture

### `crates/devices/src/boot_timer.rs` (new) — `BootTimer`

```rust
use std::sync::Arc;
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
    // read: default no-op.
}
```

- Plain `BusDevice` (not `MmioDevice`): no FDT kind, no SPI, no snapshot record —
  exactly the shape of FC's pseudo device.
- Only the first magic write fires; subsequent writes (and any non-magic / wrong
  offset / wrong width) are ignored.
- `fired` is stored (not just logged) so a unit test can assert behavior without
  capturing log output.

### `DeviceManager::add_fixed` (`crates/vmm/src/device_manager.rs`)

```rust
/// Register a plain BusDevice at a fixed guest-physical address — for pseudo
/// devices (boot_timer) that need a stable address known to the guest out-of-band
/// and have no SPI / FDT node / snapshot state. Bypasses the window/SPI allocators
/// and the record list.
pub fn add_fixed(&mut self, base: u64, len: u64, dev: Arc<Mutex<dyn BusDevice>>) -> Result<(), DeviceMgrError> {
    self.bus.register(base, len, dev).map_err(DeviceMgrError::BusOverlap)
}
```

(`DeviceManager` already imports `BusDevice` and `Bus::register`.)

### Fixed address (`arch::aarch64::layout`)

```rust
/// Fixed MMIO address of the boot-timer pseudo device. Placed at the TOP of the
/// device MMIO region so it never collides with the DeviceManager's bump allocator
/// (which grows up from MMIO_BASE). The guest writes the magic byte here via devmem;
/// there is no FDT node (the address is an out-of-band contract, like Firecracker).
pub const BOOT_TIMER_ADDR: u64 = MMIO_BASE + MMIO_LEN - 0x1000; // 0x091F_F000
```

### Boot wiring (`spike/src/bin/boot.rs`)

- Capture `let boot_start = std::time::Instant::now();` at the very top of the
  fresh-boot path (before kernel load), so the measured interval spans the whole VMM
  setup + guest boot.
- After building the `DeviceManager` (and the other `add`s), register the timer:
  ```rust
  mgr.add_fixed(
      layout::BOOT_TIMER_ADDR,
      0x1000,
      Arc::new(Mutex::new(BootTimer::new(boot_start))),
  ).expect("add boot_timer");
  ```
- Fresh-boot only. The restore path does not register it (the guest init does not
  re-run on restore, so it would never fire).

### Guest hook (`kimage/build/build-rootfs.sh`)

Add a `local.d` script that writes the magic byte at boot's end:
```sh
printf "#!/bin/sh\ndevmem 0x091FF000 8 123\n" > /etc/local.d/boottime.start
chmod +x /etc/local.d/boottime.start
```
(`devmem ADDR 8 123` writes the byte `123` as an 8-bit access to `0x091FF000`.
`busybox devmem` is present. The `local` openrc service is already enabled
(`rc-update add local boot`) for the existing `network.start`.)

## Data flow

1. VMM captures `boot_start` and registers `BootTimer` at `BOOT_TIMER_ADDR`.
2. Guest boots; the openrc `local` service runs `boottime.start` →
   `devmem 0x091FF000 8 123` → a 1-byte MMIO write of `123` at offset 0.
3. The MMIO exit routes through the bus to `BootTimer::write`, which logs
   `Guest-boot-time = N ms` and records `fired`.

## Error handling

- Wrong offset, width ≠ 1 byte, or value ≠ 123 → ignored.
- Repeated magic writes → only the first fires.
- A bus-overlap at registration (shouldn't happen — fixed top-of-region address) →
  `DeviceMgrError::BusOverlap`, fatal at boot (`expect`).

## Testing

Unit (`boot_timer.rs`, no entitlement):
1. **Magic fires once.** `BootTimer::new(Instant::now())`; `write(0, 0, &[123])` →
   `boot_time().is_some()`; a second `write(0, 0, &[123])` does not change the
   recorded value (capture it, assert equal after the second write).
2. **Non-magic ignored.** `write(0, 0, &[1])`, `write(0, 4, &[123])` (wrong offset),
   `write(0, 0, &[123, 0])` (wrong width) → `boot_time().is_none()`.

`add_fixed` is covered by the existing DeviceManager test style if a quick check is
cheap (register a mock BusDevice at a fixed addr; a bus read/write at that addr
reaches it) — optional; the live test exercises the real path.

Live (after the rootfs rebuild with `boottime.start`): boot
`target/debug/boot kimage/out/Image kimage/out/rootfs.ext4`; confirm a
`Guest-boot-time = N ms` line appears on stderr once the guest reaches the `local`
service. `cargo test --workspace` + snapshot/restore/clone drivers still pass
(boot_timer is fresh-boot-only and not in the snapshot path).

## File structure

- Create `crates/devices/src/boot_timer.rs` (device + tests).
- Modify `crates/devices/src/lib.rs` — `pub mod boot_timer;`.
- Modify `crates/vmm/src/device_manager.rs` — `add_fixed`.
- Modify `crates/arch/src/aarch64/layout.rs` — `BOOT_TIMER_ADDR`.
- Modify `spike/src/bin/boot.rs` — capture `boot_start`, register the timer.
- Modify `kimage/build/build-rootfs.sh` — `boottime.start` guest hook (needs a rootfs
  rebuild to take effect, run outside this repo's build).

End state: every fresh boot logs `Guest-boot-time = N ms`; ignition now has
Firecracker's boot-timer pseudo device. CPU-time accounting and a restore-path timer
are documented TODOs.
