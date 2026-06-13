# virtio-balloon Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a virtio-balloon device so the host can reclaim ~64 MiB of guest RAM on demand via `Ctrl-A b` (`madvise(MADV_FREE_REUSABLE)` on inflated pages) and give it back.

**Architecture:** A `Balloon` `VirtioDevice` (id 5, inflate/deflate queues) whose `num_pages` target is a shared `Arc<AtomicU32>` the host trigger drives. The transport gains a config-change interrupt and a `config_write` path. The boot harness toggles the target on `Ctrl-A b` and signals the config change; the guest inflates and the device frees the host pages.

**Tech Stack:** Rust (edition 2024), `libc::madvise(MADV_FREE_REUSABLE)`, the existing `VirtioMmio`/`Virtqueue`/`GuestRam`/`DeviceManager` and the `Ctrl-A` escape machine.

**Spec:** `docs/superpowers/specs/2026-06-13-virtio-balloon-design.md`

---

## File structure

- `crates/devices/src/virtio/mmio.rs` *(modify)* — `VirtioDevice::config_write` (default no-op) + config-write routing; `INT_STATUS_CONFIG` + `signal_config_change`.
- `crates/devices/src/virtio/guest_ram.rs` *(modify)* — `madvise_free`.
- `crates/devices/src/virtio/balloon.rs` *(new)* — `Balloon` device + tests.
- `crates/devices/src/virtio/mod.rs` *(modify)* — `pub mod balloon;`.
- `spike/src/bin/boot.rs` *(modify)* — `Ctrl-A b` toggle, always-on wiring, restore arm.

---

## Task 1: Transport — `config_write` + config-change interrupt

**Files:**
- Modify: `crates/devices/src/virtio/mmio.rs`

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` in `crates/devices/src/virtio/mmio.rs`. These need a mock `VirtioDevice` that records config writes and a mock `IrqLine` that records assertions. If the test module already has a mock device, extend it with a `config_write` override and an `Arc<Mutex<Vec<(u64, Vec<u8>)>>>` recorder; otherwise add this self-contained pair:

```rust
#[test]
fn config_write_routes_to_device() {
    use std::sync::{Arc, Mutex};
    use crate::virtio::NoopIrq;

    #[derive(Clone, Default)]
    struct RecDev { writes: Arc<Mutex<Vec<(u64, Vec<u8>)>>> }
    impl VirtioDevice for RecDev {
        fn device_id(&self) -> u32 { 99 }
        fn device_features(&self, _: u32) -> u32 { 0 }
        fn config_read(&self, _: u64, _: &mut [u8]) {}
        fn queue_count(&self) -> usize { 1 }
        fn handle_notify(&mut self, _: usize, _: &mut Virtqueue, _: &GuestRam) -> bool { false }
        fn config_write(&mut self, offset: u64, data: &[u8]) {
            self.writes.lock().unwrap().push((offset, data.to_vec()));
        }
    }

    let backing = Box::leak(vec![0u8; 0x1000].into_boxed_slice());
    let mem = GuestRam::new(backing.as_mut_ptr(), backing.len(), 0x4000_0000);
    let dev = RecDev::default();
    let writes = dev.writes.clone();
    let mut t = VirtioMmio::new("rec", Box::new(dev), mem, Arc::new(NoopIrq));

    // MMIO write to config space (offset >= 0x100) routes to config_write(offset-0x100).
    t.write(0, 0x104, &[0xde, 0xad, 0xbe, 0xef]);
    assert_eq!(writes.lock().unwrap().as_slice(), &[(0x04, vec![0xde, 0xad, 0xbe, 0xef])]);
}

#[test]
fn signal_config_change_sets_bit_and_asserts_irq() {
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct RecIrq { level: Mutex<Option<bool>> }
    impl crate::virtio::IrqLine for RecIrq {
        fn set_spi(&self, level: bool) { *self.level.lock().unwrap() = Some(level); }
    }

    let backing = Box::leak(vec![0u8; 0x1000].into_boxed_slice());
    let mem = GuestRam::new(backing.as_mut_ptr(), backing.len(), 0x4000_0000);
    let irq = Arc::new(RecIrq::default());
    // minimal device
    #[derive(Default)]
    struct Z;
    impl VirtioDevice for Z {
        fn device_id(&self) -> u32 { 0 }
        fn device_features(&self, _: u32) -> u32 { 0 }
        fn config_read(&self, _: u64, _: &mut [u8]) {}
        fn queue_count(&self) -> usize { 0 }
        fn handle_notify(&mut self, _: usize, _: &mut Virtqueue, _: &GuestRam) -> bool { false }
    }
    let mut t = VirtioMmio::new("z", Box::new(Z), mem, irq.clone());

    t.signal_config_change();
    let mut b = [0u8; 4];
    t.read(0, 0x060, &mut b); // InterruptStatus
    assert_eq!(u32::from_le_bytes(b) & 0b10, 0b10, "config-change bit set");
    assert_eq!(*irq.level.lock().unwrap(), Some(true), "irq asserted");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ignition-devices virtio::mmio::tests::config_write_routes virtio::mmio::tests::signal_config_change`
Expected: FAIL to compile — `config_write` and `signal_config_change` don't exist.

- [ ] **Step 3: Add `config_write` to the trait (default no-op)**

In `crates/devices/src/virtio/mmio.rs`, add to the `VirtioDevice` trait (after `config_read`):

```rust
    /// Apply a guest write to device config space at `offset` (relative to 0x100).
    /// Default: ignore (most devices have read-only config).
    fn config_write(&mut self, _offset: u64, _data: &[u8]) {}
```

- [ ] **Step 4: Route config-space writes + add the config-change interrupt**

Add the constant next to `INT_STATUS_USED`:

```rust
const INT_STATUS_CONFIG: u32 = 2;
```

Change the transport's `BusDevice::write` to route config space (offset ≥ 0x100) to the device:

```rust
    fn write(&mut self, _base: u64, offset: u64, data: &[u8]) {
        if offset >= 0x100 {
            self.dev.config_write(offset - 0x100, data);
        } else if data.len() == 4 {
            self.write_reg(offset, u32::from_le_bytes(data.try_into().unwrap()));
        } else {
            log::warn!("virtio-mmio: non-32-bit write at {offset:#x} len {}", data.len());
        }
    }
```

Add the public signal method to the `impl VirtioMmio` block (near `save_state`):

```rust
    /// Raise a config-change interrupt: the guest will re-read config space. Used
    /// by the host to push a new balloon target (or any future config change).
    pub fn signal_config_change(&mut self) {
        self.interrupt_status |= INT_STATUS_CONFIG;
        self.irq.set_spi(true);
    }
```

(No InterruptACK change needed: the existing `0x064` handler clears whatever bits the
guest writes and deasserts the irq when `interrupt_status == 0`.)

- [ ] **Step 5: Run tests + clippy**

Run: `cargo test -p ignition-devices virtio::mmio && cargo clippy -p ignition-devices`
Expected: PASS, 0 warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/devices/src/virtio/mmio.rs
git commit -m "feat(devices): virtio-mmio config_write routing + config-change interrupt"
```

(Plain commit message — no Co-Authored-By / Generated-with trailers.)

---

## Task 2: `GuestRam::madvise_free`

**Files:**
- Modify: `crates/devices/src/virtio/guest_ram.rs`

- [ ] **Step 1: Write the failing test**

Add to `crates/devices/src/virtio/guest_ram.rs` (add a `#[cfg(test)] mod tests` if absent):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn madvise_free_bounds() {
        // Real mmap so madvise has a valid page-aligned region.
        let len = 0x4000usize;
        let ptr = unsafe {
            libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE,
                       libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0)
        };
        assert_ne!(ptr, libc::MAP_FAILED);
        let base = 0x4000_0000u64;
        let mem = GuestRam::new(ptr as *mut u8, len, base);

        // In-range page: succeeds.
        assert!(mem.madvise_free(base, 0x1000));
        // Below base: out of range.
        assert!(!mem.madvise_free(base - 0x1000, 0x1000));
        // Past the end: out of range.
        assert!(!mem.madvise_free(base + len as u64, 0x1000));

        unsafe { libc::munmap(ptr, len) };
    }
}
```

(If `guest_ram.rs` already has a `tests` module, add just the `madvise_free_bounds` fn to it.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ignition-devices virtio::guest_ram::tests::madvise_free_bounds`
Expected: FAIL to compile — `madvise_free` undefined (and possibly `libc` not imported in this file's tests; the crate already depends on `libc`).

- [ ] **Step 3: Implement**

Add to `impl GuestRam` in `crates/devices/src/virtio/guest_ram.rs`:

```rust
    /// Return the host physical pages backing `[gpa, gpa+len)` to the OS via
    /// MADV_FREE_REUSABLE (the macOS flag that actually frees anonymous pages;
    /// MADV_DONTNEED is a no-op for anon memory there). The HVF mapping stays
    /// valid — the guest re-faults to a zero page on next access. Returns false if
    /// the range is outside guest RAM.
    pub fn madvise_free(&self, gpa: u64, len: usize) -> bool {
        let off = match gpa.checked_sub(self.base) {
            Some(o) => o as usize,
            None => return false,
        };
        if off.checked_add(len).map_or(true, |end| end > self.len) {
            return false;
        }
        let ret = unsafe {
            libc::madvise(self.ptr.add(off) as *mut libc::c_void, len, libc::MADV_FREE_REUSABLE)
        };
        ret == 0
    }
```

- [ ] **Step 4: Run tests + clippy**

Run: `cargo test -p ignition-devices virtio::guest_ram && cargo clippy -p ignition-devices`
Expected: PASS, 0 warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/devices/src/virtio/guest_ram.rs
git commit -m "feat(devices): GuestRam::madvise_free (MADV_FREE_REUSABLE)"
```

---

## Task 3: `Balloon` device

**Files:**
- Create: `crates/devices/src/virtio/balloon.rs`
- Modify: `crates/devices/src/virtio/mod.rs`

- [ ] **Step 1: Write the failing test — create `crates/devices/src/virtio/balloon.rs`:**

```rust
//! virtio-balloon (VIRTIO_ID_BALLOON): the host raises a page target; the guest
//! inflates by posting page-frame numbers on the inflate queue, and this device
//! returns those host pages to the OS via GuestRam::madvise_free. Deflate is a
//! no-op (a freed page re-faults to zero on the guest's next touch).

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use super::guest_ram::GuestRam;
use super::mmio::VirtioDevice;
use super::queue::Virtqueue;

const VIRTIO_ID_BALLOON: u32 = 5;
/// Balloon PFNs are always in 4 KiB units (VIRTIO_BALLOON_PFN_SHIFT).
const PFN_SHIFT: u64 = 12;
const PAGE: usize = 4096;

const INFLATEQ: usize = 0;
const DEFLATEQ: usize = 1;

pub struct Balloon {
    /// Host target in 4 KiB pages (config.num_pages). Shared with the host trigger.
    num_pages: Arc<AtomicU32>,
    /// Guest-reported inflated page count (config.actual).
    actual: u32,
}

impl Balloon {
    /// Returns the device and a clone of the shared target the host trigger drives.
    pub fn new() -> (Self, Arc<AtomicU32>) {
        let num_pages = Arc::new(AtomicU32::new(0));
        (Balloon { num_pages: num_pages.clone(), actual: 0 }, num_pages)
    }

    /// 8-byte virtio_balloon_config: num_pages (0x00), actual (0x04).
    fn config_bytes(&self) -> [u8; 8] {
        let mut c = [0u8; 8];
        c[0..4].copy_from_slice(&self.num_pages.load(Ordering::Relaxed).to_le_bytes());
        c[4..8].copy_from_slice(&self.actual.to_le_bytes());
        c
    }

    /// Drain an inflate chain: read packed le32 PFNs from each readable descriptor
    /// and free the host pages.
    fn inflate(&self, vq: &mut Virtqueue, mem: &GuestRam) -> bool {
        let mut serviced = false;
        while let Some(chain) = vq.pop_avail(mem) {
            for d in &chain.descriptors {
                if !d.writable {
                    let count = (d.len / 4) as u64;
                    for i in 0..count {
                        let mut b = [0u8; 4];
                        if mem.read_slice(d.addr + i * 4, &mut b) {
                            let pfn = u32::from_le_bytes(b) as u64;
                            mem.madvise_free(pfn << PFN_SHIFT, PAGE);
                        }
                    }
                }
            }
            vq.push_used(mem, chain.head, 0);
            serviced = true;
        }
        serviced
    }

    /// Drain a deflate chain: no page action (re-fault restores the page).
    fn deflate(&self, vq: &mut Virtqueue, mem: &GuestRam) -> bool {
        let mut serviced = false;
        while let Some(chain) = vq.pop_avail(mem) {
            vq.push_used(mem, chain.head, 0);
            serviced = true;
        }
        serviced
    }
}

impl VirtioDevice for Balloon {
    fn device_id(&self) -> u32 {
        VIRTIO_ID_BALLOON
    }
    fn device_features(&self, _sel: u32) -> u32 {
        0
    }
    fn config_read(&self, offset: u64, data: &mut [u8]) {
        let cfg = self.config_bytes();
        for (i, b) in data.iter_mut().enumerate() {
            let o = offset as usize + i;
            *b = if o < cfg.len() { cfg[o] } else { 0 };
        }
    }
    fn config_write(&mut self, offset: u64, data: &[u8]) {
        // Guest reports config.actual at offset 0x04.
        if offset == 0x04 && data.len() == 4 {
            self.actual = u32::from_le_bytes(data.try_into().unwrap());
        }
    }
    fn queue_count(&self) -> usize {
        2
    }
    fn handle_notify(&mut self, queue_idx: usize, vq: &mut Virtqueue, mem: &GuestRam) -> bool {
        match queue_idx {
            INFLATEQ => self.inflate(vq, mem),
            DEFLATEQ => self.deflate(vq, mem),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: u64 = 0x4000_0000;
    const DESC: u64 = BASE + 0x1000;
    const AVAIL: u64 = BASE + 0x2000;
    const USED: u64 = BASE + 0x3000;
    const DATA: u64 = BASE + 0x500;

    fn mem(backing: &mut Vec<u8>) -> GuestRam {
        GuestRam::new(backing.as_mut_ptr(), backing.len(), BASE)
    }
    fn write_desc(m: &GuestRam, i: u64, addr: u64, len: u32, flags: u16, next: u16) {
        let d = DESC + i * 16;
        m.write_slice(d, &addr.to_le_bytes());
        m.write_slice(d + 8, &len.to_le_bytes());
        m.write_slice(d + 12, &flags.to_le_bytes());
        m.write_slice(d + 14, &next.to_le_bytes());
    }
    fn offer_head0(m: &GuestRam) {
        m.write_u16(AVAIL + 2, 1);
        m.write_u16(AVAIL + 4, 0);
    }

    #[test]
    fn inflate_services_queue() {
        let mut backing = vec![0u8; 0x4000];
        let m = mem(&mut backing);
        // readable descriptor holding two le32 PFNs at DATA
        m.write_slice(DATA, &0x4_0000u32.to_le_bytes()); // pfn (addr 0x4000_0000)
        m.write_slice(DATA + 4, &0x4_0001u32.to_le_bytes());
        write_desc(&m, 0, DATA, 8, 0 /* readable */, 0);
        offer_head0(&m);
        let mut vq = Virtqueue::new(8, DESC, AVAIL, USED);
        let (mut b, _t) = Balloon::new();
        assert!(b.handle_notify(0, &mut vq, &m));
        assert_eq!(m.read_u32(USED + 4), Some(0)); // head
        assert_eq!(m.read_u32(USED + 8), Some(0)); // balloon used len is 0
    }

    #[test]
    fn deflate_services_queue() {
        let mut backing = vec![0u8; 0x4000];
        let m = mem(&mut backing);
        write_desc(&m, 0, DATA, 8, 0, 0);
        offer_head0(&m);
        let mut vq = Virtqueue::new(8, DESC, AVAIL, USED);
        let (mut b, _t) = Balloon::new();
        assert!(b.handle_notify(1, &mut vq, &m));
        assert_eq!(m.read_u32(USED + 4), Some(0));
    }

    #[test]
    fn config_read_reports_target() {
        let (b, t) = Balloon::new();
        t.store(64 * 256, Ordering::Relaxed);
        let mut d = [0u8; 4];
        b.config_read(0x00, &mut d);
        assert_eq!(u32::from_le_bytes(d), 64 * 256);
    }

    #[test]
    fn config_write_stores_actual() {
        let (mut b, _t) = Balloon::new();
        b.config_write(0x04, &1234u32.to_le_bytes());
        let mut d = [0u8; 4];
        b.config_read(0x04, &mut d);
        assert_eq!(u32::from_le_bytes(d), 1234);
    }

    #[test]
    fn identity() {
        let (b, _t) = Balloon::new();
        assert_eq!(b.device_id(), 5);
        assert_eq!(b.queue_count(), 2);
        assert_eq!(b.device_features(0), 0);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ignition-devices virtio::balloon`
Expected: FAIL to compile — module not declared.

- [ ] **Step 3: Wire the module**

In `crates/devices/src/virtio/mod.rs`, add alongside the other `pub mod` lines:

```rust
pub mod balloon;
```

- [ ] **Step 4: Run tests + clippy**

Run: `cargo test -p ignition-devices virtio::balloon && cargo clippy -p ignition-devices`
Expected: PASS (5 tests), 0 warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/devices/src/virtio/balloon.rs crates/devices/src/virtio/mod.rs
git commit -m "feat(devices): virtio-balloon device (inflate frees host pages)"
```

---

## Task 4: Wire balloon into the boot harness (`Ctrl-A b`)

**Files:**
- Modify: `spike/src/bin/boot.rs`

- [ ] **Step 1: Read the current wiring**

Read `spike/src/bin/boot.rs`: the `enum Action`, `fn step`, `spawn_stdin_reader` (signature + the `match step(...)` arms), the fresh-boot device adds (serial, rng, rtc, blk/net), and the `run_restore` device loop. Note how `mgr.add` returns the typed `Arc<Mutex<VirtioMmio>>`.

- [ ] **Step 2: Add imports + the Balloon action**

Add imports:

```rust
use std::sync::atomic::{AtomicU32, Ordering};
use devices::virtio::balloon::Balloon;
```

Add a variant to `enum Action`:

```rust
    /// Ctrl-A b: toggle the memory-balloon target.
    Balloon,
```

In `fn step`, add to the `EscState::SawCtrlA` match (next to `b's' => Action::Snapshot`):

```rust
                b'b' => Action::Balloon,
```

- [ ] **Step 3: Extend `spawn_stdin_reader` to drive the balloon**

Change the signature to take the shared target + the balloon transport handle:

```rust
fn spawn_stdin_reader(
    serial: Arc<Mutex<Serial<FlushWriter>>>,
    saved_termios: Option<libc::termios>,
    manager: Arc<vmm::vstate::vcpu_manager::VcpuManager>,
    balloon_target: Arc<AtomicU32>,
    balloon: Arc<Mutex<devices::virtio::mmio::VirtioMmio>>,
) {
```

Add the `Action::Balloon` arm to the reader's `match` (next to `Action::Snapshot`):

```rust
                Action::Balloon => {
                    const BALLOON_PAGES: u32 = 64 * 256; // 64 MiB in 4 KiB pages
                    let next = if balloon_target.load(Ordering::Relaxed) == 0 { BALLOON_PAGES } else { 0 };
                    balloon_target.store(next, Ordering::Relaxed);
                    balloon.lock().unwrap().signal_config_change();
                    eprintln!("\n[balloon target -> {} MiB]", next / 256);
                }
```

- [ ] **Step 4: Fresh-boot wiring (always-on)**

After the rng/rtc adds, add the balloon and keep its target + handle:

```rust
let (balloon_dev, balloon_target) = Balloon::new();
let balloon = {
    let guest_ram_balloon = GuestRam::new(host as *mut u8, RAM_SIZE as usize, layout::RAM_BASE);
    mgr.add(layout::MMIO_WINDOW, move |irq| {
        VirtioMmio::new("virtio-balloon", Box::new(balloon_dev), guest_ram_balloon, irq)
    })
    .expect("add balloon")
};
```

Update the fresh-boot `spawn_stdin_reader(...)` call to pass `balloon_target.clone()` and `balloon.clone()`.

- [ ] **Step 5: Restore wiring**

In `run_restore`, before the run, build a fresh balloon (target 0) and thread it to the stdin reader. Add the restore match arm:

```rust
"virtio-balloon" => {
    let guest_ram_balloon = GuestRam::new(host as *mut u8, RAM_SIZE as usize, layout::RAM_BASE);
    let (balloon_dev, target) = Balloon::new();
    let handle = mgr.add_restored(rec, move |irq| {
        VirtioMmio::new("virtio-balloon", Box::new(balloon_dev), guest_ram_balloon, irq)
    })
    .map_err(io::Error::other)?;
    balloon_restore = Some((target, handle));
}
```

Declare `let mut balloon_restore: Option<(Arc<AtomicU32>, Arc<Mutex<VirtioMmio>>)> = None;` before the loop, and after the loop pass it to `spawn_stdin_reader` (use the restored balloon if present, else a throwaway: build a `Balloon::new()` whose handle is never added — simplest is to make the `spawn_stdin_reader` balloon args `Option`, OR ensure a balloon is always present). Since balloon is always-on, a restored snapshot will always contain a `"virtio-balloon"` record, so `balloon_restore` will be `Some`; `.expect("snapshot had no balloon")` is acceptable (mirrors the existing `serial` handling).

(Use `devices::virtio::mmio::VirtioMmio` in the type; it's already imported.)

- [ ] **Step 6: Build, sign, gate**

Run:
```bash
cargo build --workspace && cargo clippy --workspace && cargo test --workspace
scripts/sign.sh target/debug/boot
```
Expected: clean build, 0 clippy, all suites green.

- [ ] **Step 7: Live verification — reclaim + give-back + snapshot/restore**

Boot, then watch the host process RSS while toggling the balloon. Drive via a pty, typing the escape slowly (one byte at a time):
```bash
target/debug/boot kimage/out/Image kimage/out/rootfs.ext4 &
BP=$!
sleep 8                                   # let it boot to login
ps -o rss= -p $BP                         # baseline RSS (KiB)
# send Ctrl-A then b (0x01 0x62) to the process's stdin — easiest via a pty driver;
# or run interactively and press Ctrl-A b
sleep 5
ps -o rss= -p $BP                         # expect a drop of ~64 MiB (~65536 KiB) vs baseline
kill $BP
```
Report the before/after RSS numbers. (A pty-based Python driver like `scripts/restore_test.py` that writes `b"\x01b"` after boot is the reliable way; the guest needs the `virtio_balloon` driver — confirm with `dmesg | grep -i balloon` showing it registered.)

Snapshot/restore/clone regression (a `virtio-balloon` record now rides every snapshot):
```bash
rm -rf snapshot snapshot2
python3 scripts/restore_test.py
python3 scripts/restore_clone_test.py
```
Expected: `snapshot=True`, restore CPU ≈ 0%, responsive; both clones `marker=True`.

- [ ] **Step 8: Commit**

```bash
git add spike/src/bin/boot.rs
git commit -m "feat(boot): wire virtio-balloon with Ctrl-A b toggle"
```

---

## Notes for the implementer

- **`Arc<AtomicU32>` target** keeps the balloon-specific knob out of the `VirtioDevice` trait. The device reads it in `config_read`; the host writes it in the `Ctrl-A b` handler, then calls `signal_config_change()` so the guest re-reads config.
- **`signal_config_change` is generic** — it lives on `VirtioMmio`, sets `INT_STATUS_CONFIG`, asserts the irq. Any future config-change device reuses it.
- **`config_write` got a default no-op** on the trait, so blk/net/rng/rtc are unaffected; only the transport's write routing changed (config-space writes now reach the device instead of being dropped by `write_reg`'s `_ => {}`).
- **No `layout`/`fdt`/`device_manager`/`snapshot` changes** — balloon is a `VirtioMmio` device, so it reuses the existing FDT `virtio,mmio` kind and snapshot record. The target is NOT persisted across snapshot (documented TODO); a restored balloon starts deflated.
- **Device id string `"virtio-balloon"`** must match in the fresh `add`, the restore `add_restored` arm, and is what `VirtioMmio::snapshot_id()` returns.
- **`MADV_FREE_REUSABLE`** is the macOS flag that returns anon pages to the OS; the reclaim is real (host RSS drops). On the guest's next touch the page faults back to zero — exactly balloon deflate semantics.
