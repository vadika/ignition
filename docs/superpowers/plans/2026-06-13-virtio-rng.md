# virtio-rng Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an always-on virtio-rng device that fills guest-posted buffers with host entropy (`getentropy`), wired through the existing `DeviceManager`.

**Architecture:** A stateless `VirtioRng` implements the `VirtioDevice` trait (id 4, one device-writable queue); `handle_notify` fills each writable descriptor with `getentropy` bytes and marks it used. It is hosted by the existing `VirtioMmio` transport (which already implements `MmioDevice`), so the `DeviceManager` handles MMIO/SPI allocation, the FDT `virtio,mmio` node, and snapshot for free. No `layout`/`fdt`/`snapshot` changes.

**Tech Stack:** Rust (edition 2024), `libc::getentropy`, the existing `Virtqueue`/`GuestRam`/`VirtioMmio`/`DeviceManager`.

**Spec:** `docs/superpowers/specs/2026-06-13-virtio-rng-design.md`

---

## File structure

- `crates/devices/src/virtio/rng.rs` *(new)* — `VirtioRng` + `fill_random` + unit tests. One responsibility: the rng device.
- `crates/devices/src/virtio/mod.rs` *(modify)* — `pub mod rng;`.
- `crates/devices/Cargo.toml` *(modify)* — add `libc` dependency (used by `fill_random`).
- `spike/src/bin/boot.rs` *(modify)* — add rng on the fresh-boot path and the restore path.

---

## Task 1: `VirtioRng` device + entropy helper

**Files:**
- Create: `crates/devices/src/virtio/rng.rs`
- Modify: `crates/devices/src/virtio/mod.rs`
- Modify: `crates/devices/Cargo.toml`

- [ ] **Step 1: Add the `libc` dependency**

In `crates/devices/Cargo.toml`, under `[dependencies]`, add (match the version `libc` is pinned to elsewhere in the workspace — check `crates/hvf/Cargo.toml` or the root `Cargo.lock`; use the same major.minor, e.g. `libc = "0.2"`):

```toml
libc = "0.2"
```

- [ ] **Step 2: Write the failing test — create `crates/devices/src/virtio/rng.rs`:**

```rust
//! virtio-rng (VIRTIO_ID_RNG): fills guest-posted buffers with host entropy.
//! A single device-writable queue; the guest hands us writable descriptors and we
//! fill them from the OS CSPRNG via getentropy.

use std::os::raw::c_void;

use super::guest_ram::GuestRam;
use super::mmio::VirtioDevice;
use super::queue::Virtqueue;

/// VIRTIO_ID_RNG.
const VIRTIO_ID_RNG: u32 = 4;

/// Fill `buf` from the OS CSPRNG. `getentropy` accepts at most 256 bytes per call.
/// With a valid pointer and `len <= 256` it cannot fail in normal operation, so a
/// nonzero return is a programming/environment error and panics rather than
/// silently producing weak entropy.
fn fill_random(buf: &mut [u8]) {
    for chunk in buf.chunks_mut(256) {
        let ret = unsafe { libc::getentropy(chunk.as_mut_ptr() as *mut c_void, chunk.len()) };
        assert_eq!(ret, 0, "getentropy failed: {}", std::io::Error::last_os_error());
    }
}

/// Stateless virtio entropy source.
pub struct VirtioRng;

impl VirtioRng {
    pub fn new() -> Self {
        VirtioRng
    }
}

impl Default for VirtioRng {
    fn default() -> Self {
        Self::new()
    }
}

impl VirtioDevice for VirtioRng {
    fn device_id(&self) -> u32 {
        VIRTIO_ID_RNG
    }

    fn device_features(&self, _sel: u32) -> u32 {
        0
    }

    fn config_read(&self, _offset: u64, _data: &mut [u8]) {
        // rng has no device-specific config space.
    }

    fn queue_count(&self) -> usize {
        1
    }

    fn handle_notify(&mut self, _queue_idx: usize, vq: &mut Virtqueue, mem: &GuestRam) -> bool {
        let mut serviced = false;
        while let Some(chain) = vq.pop_avail(mem) {
            let mut written = 0u32;
            for d in &chain.descriptors {
                if d.writable {
                    let mut buf = vec![0u8; d.len as usize];
                    fill_random(&mut buf);
                    if mem.write_slice(d.addr, &buf) {
                        written += d.len;
                    }
                }
            }
            vq.push_used(mem, chain.head, written);
            serviced = true;
        }
        serviced
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mirror the memory map used by queue.rs tests.
    const BASE: u64 = 0x4000_0000;
    const DESC: u64 = BASE + 0x1000;
    const AVAIL: u64 = BASE + 0x2000;
    const USED: u64 = BASE + 0x3000;
    const DATA: u64 = BASE + 0x500;

    fn mem(backing: &mut Vec<u8>) -> GuestRam {
        GuestRam::new(backing.as_mut_ptr(), backing.len(), BASE)
    }

    // VIRTQ_DESC_F_NEXT = 1, VIRTQ_DESC_F_WRITE = 2.
    fn write_desc(m: &GuestRam, i: u64, addr: u64, len: u32, flags: u16, next: u16) {
        let d = DESC + i * 16;
        m.write_slice(d, &addr.to_le_bytes());
        m.write_slice(d + 8, &len.to_le_bytes());
        m.write_slice(d + 12, &flags.to_le_bytes());
        m.write_slice(d + 14, &next.to_le_bytes());
    }

    /// Make avail ring offer a single chain whose head is descriptor 0.
    fn offer_head0(m: &GuestRam) {
        m.write_u16(AVAIL + 2, 1); // avail.idx = 1
        m.write_u16(AVAIL + 4, 0); // ring[0] = desc 0
    }

    #[test]
    fn identity() {
        let rng = VirtioRng::new();
        assert_eq!(rng.device_id(), 4);
        assert_eq!(rng.queue_count(), 1);
        assert_eq!(rng.device_features(0), 0);
        assert_eq!(rng.device_features(1), 0);
    }

    #[test]
    fn fills_writable_descriptor() {
        let mut backing = vec![0xAAu8; 0x4000];
        let m = mem(&mut backing);
        write_desc(&m, 0, DATA, 64, 2 /* WRITE */, 0);
        offer_head0(&m);
        let mut vq = Virtqueue::new(8, DESC, AVAIL, USED);

        let mut rng = VirtioRng::new();
        assert!(rng.handle_notify(0, &mut vq, &m));

        // used ring records head 0 and len 64
        assert_eq!(m.read_u32(USED + 4), Some(0));
        assert_eq!(m.read_u32(USED + 8), Some(64));
        // the 64-byte region changed from the 0xAA sentinel (P(all unchanged) ~ 2^-64)
        let mut out = [0u8; 64];
        assert!(m.read_slice(DATA, &mut out));
        assert!(out.iter().any(|&b| b != 0xAA), "rng did not write entropy");
    }

    #[test]
    fn read_only_chain_fills_nothing() {
        let mut backing = vec![0xAAu8; 0x4000];
        let m = mem(&mut backing);
        write_desc(&m, 0, DATA, 64, 0 /* not writable */, 0);
        offer_head0(&m);
        let mut vq = Virtqueue::new(8, DESC, AVAIL, USED);

        let mut rng = VirtioRng::new();
        assert!(rng.handle_notify(0, &mut vq, &m));

        // completed with zero-length used buffer; data untouched
        assert_eq!(m.read_u32(USED + 4), Some(0)); // head
        assert_eq!(m.read_u32(USED + 8), Some(0)); // len 0
        let mut out = [0u8; 64];
        assert!(m.read_slice(DATA, &mut out));
        assert!(out.iter().all(|&b| b == 0xAA), "non-writable desc must not be filled");
    }

    #[test]
    fn multi_descriptor_chain_fills_all_writable() {
        let mut backing = vec![0xAAu8; 0x4000];
        let m = mem(&mut backing);
        // chain: desc0 (16B writable) -> desc1 (32B writable)
        write_desc(&m, 0, DATA, 16, 1 /* NEXT */ | 2 /* WRITE */, 1);
        write_desc(&m, 1, DATA + 0x100, 32, 2 /* WRITE */, 0);
        offer_head0(&m);
        let mut vq = Virtqueue::new(8, DESC, AVAIL, USED);

        let mut rng = VirtioRng::new();
        assert!(rng.handle_notify(0, &mut vq, &m));

        assert_eq!(m.read_u32(USED + 4), Some(0));      // head
        assert_eq!(m.read_u32(USED + 8), Some(16 + 32)); // total written
        let mut a = [0u8; 16];
        let mut b = [0u8; 32];
        assert!(m.read_slice(DATA, &mut a));
        assert!(m.read_slice(DATA + 0x100, &mut b));
        assert!(a.iter().any(|&x| x != 0xAA));
        assert!(b.iter().any(|&x| x != 0xAA));
    }
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p ignition-devices virtio::rng`
Expected: FAIL to compile — `crate::virtio::rng` not declared in `mod.rs`.

- [ ] **Step 4: Wire the module**

In `crates/devices/src/virtio/mod.rs`, add alongside the other `pub mod` lines:

```rust
pub mod rng;
```

- [ ] **Step 5: Run tests + clippy**

Run: `cargo test -p ignition-devices virtio::rng && cargo clippy -p ignition-devices`
Expected: PASS (4 tests), 0 warnings.

> If `cargo` reports `libc` unresolved, the dep add in Step 1 didn't take — re-check `crates/devices/Cargo.toml`. If `read_u32`/`read_slice`/`write_u16` have different names than used here, check `crates/devices/src/virtio/guest_ram.rs` and adapt the test calls (the queue.rs tests use exactly these names, so they should match).

- [ ] **Step 6: Commit**

```bash
git add crates/devices/src/virtio/rng.rs crates/devices/src/virtio/mod.rs crates/devices/Cargo.toml
git commit -m "feat(devices): virtio-rng device (getentropy-backed)"
```

(Plain commit message — no Co-Authored-By / Generated-with trailers.)

---

## Task 2: Wire virtio-rng into the boot harness (always-on)

**Files:**
- Modify: `spike/src/bin/boot.rs`

- [ ] **Step 1: Read the current wiring**

Read `spike/src/bin/boot.rs`. Locate:
- the fresh-boot device adds (the serial `mgr.add(...)` and the blk/net `mgr.add(...)` calls);
- the `run_restore` device loop (`for rec in &snap.devices { match rec.id.as_str() { "serial" => ..., "virtio-blk" => ..., other => Err } }`);
- the `use devices::virtio::...` import block and how `GuestRam::new(host as *mut u8, RAM_SIZE as usize, layout::RAM_BASE)` is constructed per device.

- [ ] **Step 2: Add the import**

Add to the imports in `spike/src/bin/boot.rs`:

```rust
use devices::virtio::rng::VirtioRng;
```

- [ ] **Step 3: Add rng on the fresh-boot path (right after the serial `add`)**

Immediately after the serial `mgr.add(...).expect("add serial")` line, insert:

```rust
// virtio-rng: always-on entropy source. Stateless; the framework handles its
// MMIO window, SPI, FDT node, and snapshot record.
{
    let guest_ram_rng = GuestRam::new(host as *mut u8, RAM_SIZE as usize, layout::RAM_BASE);
    mgr.add(layout::MMIO_WINDOW, move |irq| {
        VirtioMmio::new("virtio-rng", Box::new(VirtioRng::new()), guest_ram_rng, irq)
    })
    .expect("add rng");
}
```

(If the surrounding code uses a different variable name than `host` for the RAM pointer, or constructs `GuestRam` differently for blk/net, mirror that exact pattern.)

- [ ] **Step 4: Add the restore arm in `run_restore`**

In the `match rec.id.as_str()` loop, add a `"virtio-rng"` arm before the `other =>` catch-all:

```rust
"virtio-rng" => {
    let guest_ram_rng = GuestRam::new(host as *mut u8, RAM_SIZE as usize, layout::RAM_BASE);
    mgr.add_restored(rec, move |irq| {
        VirtioMmio::new("virtio-rng", Box::new(VirtioRng::new()), guest_ram_rng, irq)
    })
    .map_err(io::Error::other)?;
}
```

- [ ] **Step 5: Build, sign, gate**

Run:
```bash
cargo build --workspace && cargo clippy --workspace && cargo test --workspace
scripts/sign.sh target/debug/boot
```
Expected: builds clean, 0 clippy warnings, all suites green.

- [ ] **Step 6: Live verification — device binds + snapshot/restore/clone still work**

Fresh-boot rng check:
```bash
target/debug/boot kimage/out/Image kimage/out/rootfs.ext4
```
After `login:` (log in as `root`), run in the guest:
```
cat /sys/class/misc/hw_random/rng_current
```
Expected: prints `virtio_rng.0` (and `dmesg | grep hw_random` shows it registered). Then `Ctrl-A x` to quit.

Snapshot/restore/clone regression (the rng record now rides every snapshot):
```bash
rm -rf snapshot snapshot2
python3 scripts/restore_test.py        # expect snapshot=True, restore_cpu≈0%, responsive=True
python3 scripts/restore_clone_test.py  # expect both clones marker=True, cpu≈0%
```

- [ ] **Step 7: Commit**

```bash
git add spike/src/bin/boot.rs
git commit -m "feat(boot): wire always-on virtio-rng via DeviceManager"
```

---

## Notes for the implementer

- **No `layout`/`fdt`/`snapshot` changes.** The whole point of sub-project A: rng drops in via `mgr.add` + a restore arm. If you find yourself editing `layout.rs`, `fdt.rs`, or `snapshot.rs`, stop — something is wrong.
- **Device id string consistency:** the `"virtio-rng"` literal must be identical in the fresh-boot `add` and the restore `add_restored` arm (it is what `VirtioMmio::snapshot_id()` returns and what the restore loop matches on).
- **`getentropy` is macOS-native** (and on Linux ≥ 3.17 via glibc); no `/dev/urandom` handle, no extra failure path. The `assert_eq!(ret, 0, …)` is intentional — a failing CSPRNG must not degrade to predictable bytes.
- **Stateless device:** do not add any `save`/`restore` logic to `VirtioRng`; the transport's queue state is the only snapshot state and `VirtioMmio` already handles it.
- **Old snapshots:** a pre-rng snapshot (no `"virtio-rng"` record) restores serial+blk only and is still valid — no version bump. Don't add a version check.
