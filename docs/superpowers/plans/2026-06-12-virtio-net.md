# virtio-net (guest networking) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the guest a NAT'd network via a virtio-net device backed by vmnet.framework, so `sudo boot --net <Image> <rootfs>` brings up `eth0`, DHCPs an IP, and reaches the internet (ping gateway, ping 8.8.8.8, resolve DNS).

**Architecture:** Generalize the virtio-mmio transport over a `VirtioDevice` trait (blk + net implement it). The net device is generic over a `NetBackend` trait (testable with a fake). The real backend is a `crates/vmnet` FFI crate that bridges vmnet via a small C shim (sidestepping the Objective-C block ABI). RX is async: the vmnet callback pushes frames over an `mpsc` channel; a host RX thread injects them into the device and raises the IRQ.

**Tech Stack:** Rust, Apple vmnet.framework + libdispatch + libxpc (via a C shim built with `cc`), Apple Hypervisor.framework.

---

## Background the engineer needs

- **Transport** (`crates/devices/src/virtio/mmio.rs`): `VirtioMmio` is blk-specific —
  holds `blk: VirtioBlk`, one `vq: Option<Virtqueue>`, the mmio registers, and an
  `irq: Arc<dyn IrqLine>`. `read_reg`/`write_reg` handle the virtio-mmio register
  map; `notify()` pops the avail ring, calls `blk.process(&chain, mem)`, pushes used,
  sets `interrupt_status |= INT_STATUS_USED`, pulses the IRQ; ACK (0x064) clears the
  status bit and deasserts. The queue-config registers (`0x030` QueueSel … `0x0a4`)
  currently assume `queue_sel == 0`.
- **Queue** (`queue.rs`): `Virtqueue::{new(size, desc, driver, device), pop_avail(mem)
  -> Option<DescChain>, push_used(mem, head, len)}`. `DescChain { head, descriptors:
  Vec<Desc> }`, `Desc { addr, len, writable }`. The guest pre-fills the RX queue with
  *writable* buffers; `pop_avail` yields one free chain per call.
- **GuestRam** (`guest_ram.rs`): `read_slice/write_slice`, `read_u16/u32/u64`,
  `write_u16/u32`. All bounds-checked (return `Option`/`bool`).
- **blk** (`blk.rs`): `VirtioBlk::{new(File), capacity_sectors() -> u64, process(&DescChain,
  &GuestRam) -> u32}`.
- **IrqLine** (`virtio/mod.rs`): `trait IrqLine: Send + Sync { fn set_spi(&self, level: bool); }`.
- **Layout** (`crates/arch/src/aarch64/layout.rs`): `VIRTIO_BASE = 0x0a00_0000`,
  `VIRTIO_SIZE = 0x200`, `VIRTIO_SPI = 1`. The net device needs a second window +
  SPI.
- **Async injection precedent:** serial RX (`spike/src/bin/boot.rs`
  `spawn_stdin_reader`) — a host thread feeding a device + the device raising the GIC
  line. virtio-net RX mirrors it.
- **The boot harness** wires devices, the FDT (`FdtConfig.devices: Vec<FdtDevice>`),
  GIC SPIs (`GicIrq { gic, intid: SPI + 32 }`), and runs under `VcpuManager`.
- **Build/test:** `cargo test -p ignition-devices`, `cargo build --workspace`,
  `cargo clippy --workspace`. The integration boot needs **sudo** (vmnet shared
  mode). Re-sign after a build: `./scripts/sign.sh target/debug/boot`.
- **Commit policy:** plain messages, NO `Co-Authored-By` / "Generated with Claude".

## File structure

- `crates/devices/src/virtio/mmio.rs` — `VirtioDevice` trait; `VirtioMmio` over
  `Box<dyn VirtioDevice>` + per-queue state; `inject_rx` (Task 1).
- `crates/devices/src/virtio/blk.rs` — `impl VirtioDevice for VirtioBlk` (Task 1).
- `crates/devices/src/virtio/net.rs` (new) — `NetBackend` trait, `VirtioNet`, tests
  (Task 2).
- `crates/vmnet/` (new crate) — `vmnet_shim.c` + `build.rs` + `lib.rs` wrapping vmnet
  (Task 3).
- `crates/arch/src/aarch64/layout.rs` — `NET_BASE`/`NET_SIZE`/`NET_SPI` (Task 4).
- `spike/src/bin/boot.rs` — `--net`, net device + RX thread + FDT node (Task 4).

---

## Task 1: Generalize the transport over `VirtioDevice`; migrate blk

**Files:**
- Modify: `crates/devices/src/virtio/mmio.rs`
- Modify: `crates/devices/src/virtio/blk.rs`

- [ ] **Step 1: Define the `VirtioDevice` trait** (top of `mmio.rs`, after the imports)

```rust
/// A virtio device plugged into the mmio transport. The transport owns the
/// virtqueues and the interrupt line; the device supplies identity, features,
/// config space, and per-queue servicing.
pub trait VirtioDevice: Send {
    fn device_id(&self) -> u32;
    /// The device-feature word for `sel` (0 = bits 0..31, 1 = bits 32..63). The
    /// transport adds VIRTIO_F_VERSION_1 (bit 32) itself, so return only this
    /// device's own bits.
    fn device_features(&self, sel: u32) -> u32;
    /// Read a 32-bit word of device config space at `offset` (relative to 0x100).
    fn config_read(&self, offset: u64) -> u32;
    fn queue_count(&self) -> usize;
    /// Service a QueueNotify on `queue_idx`. Returns true if any buffer was used.
    fn handle_notify(&mut self, queue_idx: usize, vq: &mut Virtqueue, mem: &GuestRam) -> bool;
    /// Inject a received frame into the RX queue. Default: not an RX device.
    fn inject_rx(&mut self, _vq: &mut Virtqueue, _mem: &GuestRam, _frame: &[u8]) -> bool {
        false
    }
}
```

- [ ] **Step 2: Replace `VirtioMmio`'s fields with device + per-queue state**

Replace the struct and `new` with:

```rust
/// Per-queue driver-programmed state.
#[derive(Default)]
struct QueueState {
    num: u16,
    ready: u32,
    desc_lo: u32,
    desc_hi: u32,
    driver_lo: u32,
    driver_hi: u32,
    device_lo: u32,
    device_hi: u32,
    vq: Option<Virtqueue>,
}

/// A virtio-mmio transport hosting one `VirtioDevice`.
pub struct VirtioMmio {
    dev: Box<dyn VirtioDevice>,
    mem: GuestRam,
    irq: Arc<dyn IrqLine>,
    queues: Vec<QueueState>,
    status: u32,
    device_features_sel: u32,
    queue_sel: u32,
    interrupt_status: u32,
}

impl VirtioMmio {
    pub fn new(dev: Box<dyn VirtioDevice>, mem: GuestRam, irq: Arc<dyn IrqLine>) -> Self {
        let queues = (0..dev.queue_count()).map(|_| QueueState::default()).collect();
        Self {
            dev,
            mem,
            irq,
            queues,
            status: 0,
            device_features_sel: 0,
            queue_sel: 0,
            interrupt_status: 0,
        }
    }
}
```

- [ ] **Step 3: Generalize `read_reg`/`write_reg` for `queue_sel` + config space**

`read_reg`:
- `0x008` → `self.dev.device_id()`.
- `0x010` (DeviceFeatures) → `if sel == 1 { FEATURES_HI_VERSION_1 | self.dev.device_features(1) } else { self.dev.device_features(0) }`.
- `0x034` QueueNumMax → `QUEUE_SIZE_MAX`.
- `0x044` QueueReady → `self.queues[self.queue_sel as usize].ready` (guard the index).
- `0x060` → `self.interrupt_status`; `0x070` → `self.status`.
- `off >= 0x100` → `self.dev.config_read(off - 0x100)`.
- else `0`.

`write_reg`: keep `0x014` (sel), `0x020/0x024` (no-op), `0x064` (ACK), `0x070`
(status/reset), `0x050` (notify) as-is in shape, but the queue-config writes
(`0x030` QueueSel, `0x038` QueueNum, `0x044` QueueReady, `0x080..0x0a4` addr halves)
now index `self.queues[self.queue_sel]`:

```rust
    fn write_reg(&mut self, off: u64, val: u32) {
        let sel = self.queue_sel as usize;
        match off {
            0x014 => self.device_features_sel = val,
            0x020 | 0x024 => {}
            0x030 => self.queue_sel = val,
            0x038 => { if let Some(q) = self.queues.get_mut(sel) { q.num = val as u16; } }
            0x044 => self.set_queue_ready(sel, val),
            0x050 => self.notify(val), // QueueNotify carries the queue index in `val`
            0x064 => {
                self.interrupt_status &= !val;
                if self.interrupt_status == 0 {
                    self.irq.set_spi(false);
                }
            }
            0x070 => { self.status = val; if val == 0 { self.reset(); } }
            0x080 => self.set_addr(sel, |q| &mut q.desc_lo, val),
            0x084 => self.set_addr(sel, |q| &mut q.desc_hi, val),
            0x090 => self.set_addr(sel, |q| &mut q.driver_lo, val),
            0x094 => self.set_addr(sel, |q| &mut q.driver_hi, val),
            0x0a0 => self.set_addr(sel, |q| &mut q.device_lo, val),
            0x0a4 => self.set_addr(sel, |q| &mut q.device_hi, val),
            _ => {}
        }
    }

    fn set_addr(&mut self, sel: usize, field: impl Fn(&mut QueueState) -> &mut u32, val: u32) {
        if let Some(q) = self.queues.get_mut(sel) {
            *field(q) = val;
        }
    }

    fn set_queue_ready(&mut self, sel: usize, val: u32) {
        let Some(q) = self.queues.get_mut(sel) else { return };
        q.ready = val;
        if val == 1 {
            let desc = (u64::from(q.desc_hi) << 32) | u64::from(q.desc_lo);
            let driver = (u64::from(q.driver_hi) << 32) | u64::from(q.driver_lo);
            let device = (u64::from(q.device_hi) << 32) | u64::from(q.device_lo);
            q.vq = Some(Virtqueue::new(q.num, desc, driver, device));
        } else if val == 0 {
            *q = QueueState::default();
        }
    }
```

Note: virtio-mmio `QueueNotify` (0x050) writes the queue index as the value, so
`notify` takes it. (The old code ignored the value and assumed queue 0.)

- [ ] **Step 4: Generalize `notify` and add `inject_rx`; keep the IRQ/status logic**

```rust
    fn notify(&mut self, queue_idx: u32) {
        let idx = queue_idx as usize;
        let Some(q) = self.queues.get_mut(idx) else { return };
        if q.ready == 0 {
            return;
        }
        let Some(vq) = q.vq.as_mut() else { return };
        let serviced = self.dev.handle_notify(idx, vq, &self.mem);
        if serviced {
            self.raise();
        }
    }

    /// Inject a received frame into RX queue 0 (called from the host RX thread).
    /// Returns false if there was no free RX buffer (frame dropped).
    pub fn inject_rx(&mut self, frame: &[u8]) -> bool {
        let Some(q) = self.queues.get_mut(0) else { return false };
        if q.ready == 0 {
            return false;
        }
        let Some(vq) = q.vq.as_mut() else { return false };
        let used = self.dev.inject_rx(vq, &self.mem, frame);
        if used {
            self.raise();
        }
        used
    }

    fn raise(&mut self) {
        self.interrupt_status |= INT_STATUS_USED;
        self.irq.set_spi(true);
    }

    fn reset(&mut self) {
        for q in &mut self.queues {
            *q = QueueState::default();
        }
        self.interrupt_status = 0;
        self.irq.set_spi(false);
    }
```

(Remove the old `DEVICE_ID_BLK` const and the blk-specific `0x100/0x104` read arms —
they move into blk's `config_read`/`device_id`.)

- [ ] **Step 5: Make blk a `VirtioDevice`** (`blk.rs`)

Add (keeping the existing `process`, `capacity_sectors`, `new`):

```rust
use super::guest_ram::GuestRam;
use super::mmio::VirtioDevice;
use super::queue::Virtqueue;

const DEVICE_ID_BLK: u32 = 2;

impl VirtioDevice for VirtioBlk {
    fn device_id(&self) -> u32 {
        DEVICE_ID_BLK
    }
    fn device_features(&self, _sel: u32) -> u32 {
        0 // only VIRTIO_F_VERSION_1, added by the transport
    }
    fn config_read(&self, offset: u64) -> u32 {
        match offset {
            0 => (self.capacity_sectors() & 0xffff_ffff) as u32,
            4 => (self.capacity_sectors() >> 32) as u32,
            _ => 0,
        }
    }
    fn queue_count(&self) -> usize {
        1
    }
    fn handle_notify(&mut self, _queue_idx: usize, vq: &mut Virtqueue, mem: &GuestRam) -> bool {
        let mut serviced = false;
        while let Some(chain) = vq.pop_avail(mem) {
            let len = self.process(&chain, mem);
            vq.push_used(mem, chain.head, len);
            serviced = true;
        }
        serviced
    }
}
```

Ensure `VirtioDevice` and `Virtqueue` are `pub` where needed (export `VirtioDevice`
from `mmio.rs`; it's already `pub`). `process` may need to become `pub(crate)` if it
isn't already callable here (it's in the same module tree).

- [ ] **Step 6: Fix the mmio tests for the new constructor**

The mmio test helper `dev(...)` builds `VirtioMmio::new(VirtioBlk::new(disk())…)`.
Change it to `VirtioMmio::new(Box::new(VirtioBlk::new(disk()).unwrap()), mem, irq)`.
The `identity_registers` and `notify_services_a_request_and_pulses_irq` tests should
otherwise pass unchanged (device_id 2, capacity at 0x100, notify services the
request). The `notify` test writes QueueNotify as `wr(d, 0x050, 0)` — value 0 = queue
0, which matches.

- [ ] **Step 7: Build + test + clippy**

```bash
cargo test -p ignition-devices 2>&1 | grep 'test result'
cargo build --workspace 2>&1 | tail -1
cargo clippy --workspace 2>&1 | grep -c 'warning:'
```
Expected: all device tests pass (blk + queue + mmio + serial), workspace builds
(boot.rs still constructs `VirtioMmio::new(Box::new(blk), …)` — update that one call
site in boot.rs if the build flags it; if so, wrap the existing `VirtioBlk` in
`Box::new`), 0 clippy warnings.

- [ ] **Step 8: Commit**

```bash
git add crates/devices/src/virtio/mmio.rs crates/devices/src/virtio/blk.rs spike/src/bin/boot.rs
git commit -m "refactor(devices): generalize virtio-mmio over a VirtioDevice trait"
```

---

## Task 2: The virtio-net device (`net.rs`) over a `NetBackend`

**Files:**
- Create: `crates/devices/src/virtio/net.rs`
- Modify: `crates/devices/src/virtio/mod.rs` (add `pub mod net;` + re-exports)

- [ ] **Step 1: Write the failing tests** (in `net.rs`, see Step 3 for the impl)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::virtio::guest_ram::GuestRam;
    use crate::virtio::mmio::VirtioDevice;
    use crate::virtio::queue::Virtqueue;
    use std::sync::{Arc, Mutex};

    const BASE: u64 = 0x4000_0000;

    /// Captures TX frames; yields a fixed MAC.
    #[derive(Default, Clone)]
    struct FakeBackend(Arc<Mutex<Vec<Vec<u8>>>>);
    impl NetBackend for FakeBackend {
        fn write_frame(&self, frame: &[u8]) -> std::io::Result<()> {
            self.0.lock().unwrap().push(frame.to_vec());
            Ok(())
        }
        fn mac(&self) -> [u8; 6] {
            [0x52, 0x54, 0x00, 0x12, 0x34, 0x56]
        }
    }

    fn ram(backing: &mut Vec<u8>) -> GuestRam {
        GuestRam::new(backing.as_mut_ptr(), backing.len(), BASE)
    }

    fn write_desc(m: &GuestRam, desc: u64, i: u64, addr: u64, len: u32, flags: u16, next: u16) {
        let d = desc + i * 16;
        m.write_slice(d, &addr.to_le_bytes());
        m.write_slice(d + 8, &len.to_le_bytes());
        m.write_slice(d + 12, &flags.to_le_bytes());
        m.write_slice(d + 14, &next.to_le_bytes());
    }

    #[test]
    fn features_and_config_expose_mac() {
        let net = VirtioNet::new(FakeBackend::default());
        assert_eq!(net.device_id(), 1);
        assert_eq!(net.queue_count(), 2);
        assert_eq!(net.device_features(0) & (1 << VIRTIO_NET_F_MAC), 1 << VIRTIO_NET_F_MAC);
        // config offset 0 = MAC[0..4] little-endian as the device exposes it.
        assert_eq!(net.config_read(0), u32::from_le_bytes([0x52, 0x54, 0x00, 0x12]));
    }

    #[test]
    fn tx_strips_header_and_writes_frame() {
        // TX chain: one descriptor holding [12-byte hdr | 4-byte frame].
        let mut backing = vec![0u8; 0x6000];
        let desc = BASE + 0x1000;
        let avail = BASE + 0x2000;
        let used = BASE + 0x3000;
        let buf = BASE + 0x0100;
        {
            let m = ram(&mut backing);
            let mut pkt = vec![0u8; 12];
            pkt.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
            m.write_slice(buf, &pkt);
            write_desc(&m, desc, 0, buf, pkt.len() as u32, 0, 0); // read-only, end
            m.write_u16(avail + 2, 1);
            m.write_u16(avail + 4, 0);
        }
        let m = ram(&mut backing);
        let backend = FakeBackend::default();
        let mut net = VirtioNet::new(backend.clone());
        let mut vq = Virtqueue::new(8, desc, avail, used);
        assert!(net.handle_notify(1, &mut vq, &m)); // TX = queue 1
        assert_eq!(backend.0.lock().unwrap().as_slice(), &[vec![0xde, 0xad, 0xbe, 0xef]]);
        assert_eq!(m.read_u16(used + 2), Some(1));
    }

    #[test]
    fn rx_prepends_header_into_guest_buffer() {
        // RX queue pre-filled with one writable buffer big enough for hdr + frame.
        let mut backing = vec![0u8; 0x6000];
        let desc = BASE + 0x1000;
        let avail = BASE + 0x2000;
        let used = BASE + 0x3000;
        let buf = BASE + 0x0100;
        {
            let m = ram(&mut backing);
            write_desc(&m, desc, 0, buf, 2048, 2, 0); // WRITE, end
            m.write_u16(avail + 2, 1);
            m.write_u16(avail + 4, 0);
        }
        let m = ram(&mut backing);
        let mut net = VirtioNet::new(FakeBackend::default());
        let mut vq = Virtqueue::new(8, desc, avail, used);
        let frame = [0x11, 0x22, 0x33];
        assert!(net.inject_rx(&mut vq, &m, &frame));
        // Buffer holds [12 zero bytes | frame].
        let mut out = [0u8; 15];
        m.read_slice(buf, &mut out);
        assert_eq!(&out[..12], &[0u8; 12]);
        assert_eq!(&out[12..15], &frame);
        // used.len = hdr + frame = 15.
        assert_eq!(m.read_u32(used + 8), Some(15));
        assert_eq!(m.read_u16(used + 2), Some(1));
    }

    #[test]
    fn rx_with_no_buffer_drops_and_returns_false() {
        let mut backing = vec![0u8; 0x6000];
        let desc = BASE + 0x1000;
        let avail = BASE + 0x2000;
        let used = BASE + 0x3000;
        let m = ram(&mut backing); // avail.idx stays 0 -> no free buffer
        let mut net = VirtioNet::new(FakeBackend::default());
        let mut vq = Virtqueue::new(8, desc, avail, used);
        assert!(!net.inject_rx(&mut vq, &m, &[0x11, 0x22]));
        assert_eq!(net.dropped_rx(), 1);
    }
}
```

- [ ] **Step 2: Run tests, verify they fail**

Run: `cargo test -p ignition-devices net:: 2>&1 | tail -15`
Expected: FAIL — `VirtioNet`/`NetBackend` not found.

- [ ] **Step 3: Implement `net.rs`**

```rust
//! virtio-net (virtio 1.0 §5.1): exit-driven TX, async RX injection. No offloads.

use super::guest_ram::GuestRam;
use super::mmio::VirtioDevice;
use super::queue::Virtqueue;

/// Feature bit: the device exposes a MAC in config space.
pub const VIRTIO_NET_F_MAC: u32 = 5;

const DEVICE_ID_NET: u32 = 1;
/// `struct virtio_net_hdr` size with num_buffers (virtio 1.0 §5.1.6).
const NET_HDR_LEN: usize = 12;
/// Reject an absurd frame (defends a malformed TX descriptor `len`).
const MAX_FRAME: usize = 65_536;

/// Host side of the NIC: send frames out, supply the guest's MAC.
pub trait NetBackend: Send {
    fn write_frame(&self, frame: &[u8]) -> std::io::Result<()>;
    fn mac(&self) -> [u8; 6];
}

pub struct VirtioNet<B: NetBackend> {
    backend: B,
    mac: [u8; 6],
    dropped_rx: u64,
}

impl<B: NetBackend> VirtioNet<B> {
    pub fn new(backend: B) -> Self {
        let mac = backend.mac();
        Self { backend, mac, dropped_rx: 0 }
    }

    pub fn dropped_rx(&self) -> u64 {
        self.dropped_rx
    }

    /// Drain the TX queue: strip the 12-byte header, send each frame.
    fn drain_tx(&mut self, vq: &mut Virtqueue, mem: &GuestRam) -> bool {
        let mut serviced = false;
        while let Some(chain) = vq.pop_avail(mem) {
            // Gather the chain's readable bytes into one frame buffer.
            let mut buf = Vec::new();
            for d in &chain.descriptors {
                if d.writable {
                    continue; // TX buffers are device-readable
                }
                if buf.len() + d.len as usize > MAX_FRAME {
                    break;
                }
                let mut tmp = vec![0u8; d.len as usize];
                if mem.read_slice(d.addr, &mut tmp) {
                    buf.extend_from_slice(&tmp);
                }
            }
            if buf.len() > NET_HDR_LEN {
                let frame = &buf[NET_HDR_LEN..];
                if let Err(e) = self.backend.write_frame(frame) {
                    log::warn!("virtio-net TX write failed: {e}");
                }
            }
            vq.push_used(mem, chain.head, 0);
            serviced = true;
        }
        serviced
    }
}

impl<B: NetBackend> VirtioDevice for VirtioNet<B> {
    fn device_id(&self) -> u32 {
        DEVICE_ID_NET
    }
    fn device_features(&self, sel: u32) -> u32 {
        if sel == 0 { 1 << VIRTIO_NET_F_MAC } else { 0 }
    }
    fn config_read(&self, offset: u64) -> u32 {
        // Config space: bytes 0..6 = MAC, 6..8 = status (link up). Word-addressed.
        let mut cfg = [0u8; 8];
        cfg[..6].copy_from_slice(&self.mac);
        // status = VIRTIO_NET_S_LINK_UP (1) — only meaningful if F_STATUS negotiated
        // (it isn't), but harmless to expose.
        cfg[6] = 1;
        let off = offset as usize;
        let mut word = [0u8; 4];
        for (i, b) in word.iter_mut().enumerate() {
            *b = *cfg.get(off + i).unwrap_or(&0);
        }
        u32::from_le_bytes(word)
    }
    fn queue_count(&self) -> usize {
        2 // RX = 0, TX = 1
    }
    fn handle_notify(&mut self, queue_idx: usize, vq: &mut Virtqueue, mem: &GuestRam) -> bool {
        match queue_idx {
            1 => self.drain_tx(vq, mem),
            // RX notifies (guest replenishing buffers) need no service here.
            _ => false,
        }
    }
    fn inject_rx(&mut self, vq: &mut Virtqueue, mem: &GuestRam, frame: &[u8]) -> bool {
        let Some(chain) = vq.pop_avail(mem) else {
            self.dropped_rx += 1;
            return false;
        };
        // Write [zeroed 12-byte hdr | frame] across the chain's writable buffers.
        let mut payload = vec![0u8; NET_HDR_LEN];
        payload.extend_from_slice(frame);
        let mut written = 0usize;
        let mut off = 0usize;
        for d in &chain.descriptors {
            if !d.writable || off >= payload.len() {
                continue;
            }
            let n = (d.len as usize).min(payload.len() - off);
            if mem.write_slice(d.addr, &payload[off..off + n]) {
                off += n;
                written += n;
            }
        }
        vq.push_used(mem, chain.head, written as u32);
        written > 0
    }
}
```

- [ ] **Step 4: Wire the module** (`crates/devices/src/virtio/mod.rs`)

Add `pub mod net;` next to the others.

- [ ] **Step 5: Run tests, verify pass + clippy**

```bash
cargo test -p ignition-devices net:: 2>&1 | grep 'test result'
cargo test -p ignition-devices 2>&1 | grep 'test result'
cargo clippy -p ignition-devices 2>&1 | grep -c 'warning:'
```
Expected: the 4 net tests pass, all device tests pass, 0 clippy.

- [ ] **Step 6: Commit**

```bash
git add crates/devices/src/virtio/net.rs crates/devices/src/virtio/mod.rs
git commit -m "feat(devices): virtio-net device (TX drain + async RX inject) over NetBackend"
```

---

## Task 3: `crates/vmnet` — vmnet.framework backend via a C shim

**Files:**
- Create: `crates/vmnet/Cargo.toml`, `crates/vmnet/build.rs`,
  `crates/vmnet/src/vmnet_shim.c`, `crates/vmnet/src/lib.rs`
- Modify: workspace `Cargo.toml` (add the member)

This task is macOS FFI that needs runtime iteration under sudo — it is NOT
unit-testable. The C shim sidesteps the Objective-C block ABI by creating the blocks
internally and exposing plain-C entry points. Build the shim, get a standalone
"start interface, print MAC" working under sudo, then expose `NetBackend`.

- [ ] **Step 1: The C shim** (`crates/vmnet/src/vmnet_shim.c`)

```c
// Plain-C bridge over vmnet.framework. Creates the dispatch queue + blocks
// internally so Rust only deals with function pointers.
#include <vmnet/vmnet.h>
#include <dispatch/dispatch.h>
#include <xpc/xpc.h>
#include <string.h>
#include <stdint.h>
#include <stdlib.h>

typedef void (*ig_frame_cb)(void *ctx, const uint8_t *data, uintptr_t len);

struct ig_vmnet {
    interface_ref iface;
    dispatch_queue_t queue;
    ig_frame_cb cb;
    void *ctx;
    uint32_t max_packet;
};

// Start vmnet shared (NAT) mode. On success returns a handle and fills mac_out
// (6 bytes). Blocks until the async start handler fires. Returns NULL on failure.
struct ig_vmnet *ig_vmnet_start(uint8_t mac_out[6], ig_frame_cb cb, void *ctx) {
    struct ig_vmnet *h = calloc(1, sizeof(*h));
    h->cb = cb;
    h->ctx = ctx;
    h->queue = dispatch_queue_create("ignition.vmnet", DISPATCH_QUEUE_SERIAL);

    xpc_object_t desc = xpc_dictionary_create(NULL, NULL, 0);
    xpc_dictionary_set_uint64(desc, vmnet_operation_mode_key, VMNET_SHARED_MODE);

    dispatch_semaphore_t sem = dispatch_semaphore_create(0);
    __block vmnet_return_t start_status = VMNET_FAILURE;
    h->iface = vmnet_start_interface(desc, h->queue,
        ^(vmnet_return_t status, xpc_object_t params) {
            start_status = status;
            if (status == VMNET_SUCCESS) {
                const char *mac = xpc_dictionary_get_string(params, vmnet_mac_address_key);
                // mac is "xx:xx:xx:xx:xx:xx"
                unsigned m[6];
                if (mac && sscanf(mac, "%x:%x:%x:%x:%x:%x",
                        &m[0],&m[1],&m[2],&m[3],&m[4],&m[5]) == 6) {
                    for (int i = 0; i < 6; i++) mac_out[i] = (uint8_t)m[i];
                }
                h->max_packet = (uint32_t)xpc_dictionary_get_uint64(params,
                    vmnet_max_packet_size_key);
            }
            dispatch_semaphore_signal(sem);
        });
    dispatch_semaphore_wait(sem, DISPATCH_TIME_FOREVER);
    xpc_release(desc);
    if (h->iface == NULL || start_status != VMNET_SUCCESS) {
        free(h);
        return NULL;
    }

    // Deliver received frames via the callback.
    vmnet_interface_set_event_callback(h->iface, VMNET_INTERFACE_PACKETS_AVAILABLE,
        h->queue, ^(interface_event_t ev, xpc_object_t einfo) {
            int max = (int)xpc_dictionary_get_uint64(einfo,
                vmnet_estimated_packets_available_key);
            for (int i = 0; i < max; i++) {
                uint8_t buf[65536];
                struct iovec iov = { .iov_base = buf, .iov_len = sizeof(buf) };
                struct vmpktdesc pd = { .vm_pkt_size = sizeof(buf),
                    .vm_pkt_iov = &iov, .vm_pkt_iovcnt = 1, .vm_flags = 0 };
                int count = 1;
                if (vmnet_read(h->iface, &pd, &count) != VMNET_SUCCESS || count < 1) break;
                h->cb(h->ctx, buf, pd.vm_pkt_size);
            }
        });
    return h;
}

// Send one frame. Returns 0 on success.
int ig_vmnet_write(struct ig_vmnet *h, const uint8_t *data, uintptr_t len) {
    struct iovec iov = { .iov_base = (void *)data, .iov_len = len };
    struct vmpktdesc pd = { .vm_pkt_size = len, .vm_pkt_iov = &iov,
        .vm_pkt_iovcnt = 1, .vm_flags = 0 };
    int count = 1;
    return vmnet_write(h->iface, &pd, &count) == VMNET_SUCCESS ? 0 : -1;
}
```

(If a symbol name differs on this SDK — e.g. `vmnet_estimated_packets_available_key`
vs `vmnet_estimated_packets_available_key` — check `/Library/Developer/.../vmnet.h`
under the active SDK and adjust. `sscanf` needs `#include <stdio.h>`.)

- [ ] **Step 2: `build.rs`** (`crates/vmnet/build.rs`)

```rust
fn main() {
    cc::Build::new()
        .file("src/vmnet_shim.c")
        .compile("vmnet_shim");
    println!("cargo:rustc-link-lib=framework=vmnet");
    // libxpc / libdispatch are in libSystem; no extra link needed on macOS.
    println!("cargo:rerun-if-changed=src/vmnet_shim.c");
}
```

`Cargo.toml`: `[build-dependencies] cc = "1"`. Package name `ignition-vmnet`.

- [ ] **Step 3: `lib.rs`** — safe wrapper + `NetBackend` impl

```rust
//! vmnet.framework shared/NAT backend (via the C shim). Needs sudo.

use std::os::raw::c_void;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Mutex;

use devices::virtio::net::NetBackend;

#[repr(C)]
struct IgVmnet {
    _private: [u8; 0],
}

type FrameCb = extern "C" fn(*mut c_void, *const u8, usize);

unsafe extern "C" {
    fn ig_vmnet_start(mac_out: *mut u8, cb: FrameCb, ctx: *mut c_void) -> *mut IgVmnet;
    fn ig_vmnet_write(h: *mut IgVmnet, data: *const u8, len: usize) -> i32;
}

/// The RX callback context: a channel sender for received frames.
struct RxCtx {
    tx: Sender<Vec<u8>>,
}

extern "C" fn on_frame(ctx: *mut c_void, data: *const u8, len: usize) {
    // SAFETY: ctx is the leaked Box<RxCtx>; data/len describe one frame.
    let ctx = unsafe { &*(ctx as *const RxCtx) };
    let frame = unsafe { std::slice::from_raw_parts(data, len) }.to_vec();
    let _ = ctx.tx.send(frame);
}

pub struct VmnetBackend {
    handle: Mutex<*mut IgVmnet>,
    mac: [u8; 6],
}
// SAFETY: the shim's interface_ref is internally synchronized on its own serial
// dispatch queue; we serialize writes behind the Mutex.
unsafe impl Send for VmnetBackend {}
unsafe impl Sync for VmnetBackend {}

impl VmnetBackend {
    /// Start vmnet shared mode. Returns the backend + the RX frame receiver.
    pub fn start() -> std::io::Result<(VmnetBackend, Receiver<Vec<u8>>)> {
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = Box::into_raw(Box::new(RxCtx { tx })) as *mut c_void;
        let mut mac = [0u8; 6];
        // SAFETY: on_frame matches FrameCb; ctx outlives the interface (leaked).
        let handle = unsafe { ig_vmnet_start(mac.as_mut_ptr(), on_frame, ctx) };
        if handle.is_null() {
            return Err(std::io::Error::other(
                "vmnet_start_interface failed (run under sudo for shared mode)",
            ));
        }
        Ok((VmnetBackend { handle: Mutex::new(handle), mac }, rx))
    }
}

impl NetBackend for VmnetBackend {
    fn write_frame(&self, frame: &[u8]) -> std::io::Result<()> {
        let h = *self.handle.lock().unwrap();
        // SAFETY: h is a valid handle for the process lifetime.
        let rc = unsafe { ig_vmnet_write(h, frame.as_ptr(), frame.len()) };
        if rc == 0 { Ok(()) } else { Err(std::io::Error::other("vmnet_write failed")) }
    }
    fn mac(&self) -> [u8; 6] {
        self.mac
    }
}
```

(`devices::virtio::net::NetBackend` must be re-exported so `ignition-vmnet` can
implement it — add `pub use net::NetBackend;` etc. in `virtio/mod.rs` and ensure
`ignition-vmnet` depends on `ignition-devices`.)

- [ ] **Step 4: Standalone bring-up check** (a tiny bin or a `#[test] #[ignore]`)

Add `crates/vmnet/src/bin/vmnet-smoke.rs`:
```rust
fn main() {
    match ignition_vmnet::VmnetBackend::start() {
        Ok((b, _rx)) => {
            use devices::virtio::net::NetBackend;
            let m = b.mac();
            println!("vmnet up: mac {m:02x?}");
        }
        Err(e) => { eprintln!("vmnet start failed: {e}"); std::process::exit(1); }
    }
}
```
Run: `cargo build -p ignition-vmnet 2>&1 | tail -3` then
`sudo target/debug/vmnet-smoke`. Expected: `vmnet up: mac [..]` with a real MAC. If
it fails, iterate on the shim (symbol names, SDK header path) — this is the expected
debug loop for this task. Report the exact error if blocked after a couple of
attempts.

- [ ] **Step 5: Commit**

```bash
git add crates/vmnet Cargo.toml crates/devices/src/virtio/mod.rs
git commit -m "feat(vmnet): vmnet.framework shared-mode backend via a C shim"
```

---

## Task 4: Wire `--net` into the harness + integration (the bar)

**Files:**
- Modify: `crates/arch/src/aarch64/layout.rs`
- Modify: `spike/src/bin/boot.rs`

- [ ] **Step 1: Layout for the net device** (`layout.rs`)

Add (the second virtio window above the blk one, with its own SPI):
```rust
/// Second virtio-mmio window (the NIC). Above the block device, below GIC/RAM.
pub const NET_BASE: u64 = 0x0a00_0200;
pub const NET_SIZE: u64 = 0x200;
/// virtio-net IRQ as the bare GIC SPI index (absolute INTID = 32 + this = 34).
pub const NET_SPI: u32 = 2;
```
Add a layout test asserting `NET_BASE == VIRTIO_BASE + VIRTIO_SIZE` (adjacent, no
overlap) and `NET_BASE + NET_SIZE <= RAM_BASE`.

- [ ] **Step 2: `--net` flag + device + RX thread** (`boot.rs`)

- Add `--net` to the arg parser (a `bool net`, like `--smp` but no value).
- After building the bus, when `net` is set:
  ```rust
  let (backend, rx) = ignition_vmnet::VmnetBackend::start()
      .expect("vmnet start failed (run boot under sudo for --net)");
  let net_irq = Arc::new(GicIrq { gic: gic.clone(), intid: layout::NET_SPI + 32 });
  let guest_ram_net = GuestRam::new(host as *mut u8, RAM_SIZE as usize, layout::RAM_BASE);
  let net_dev = VirtioNet::new(backend);
  let net_mmio: Arc<Mutex<VirtioMmio>> =
      Arc::new(Mutex::new(VirtioMmio::new(Box::new(net_dev), guest_ram_net, net_irq)));
  let net_bus: Arc<Mutex<dyn BusDevice>> = net_mmio.clone();
  bus.register(layout::NET_BASE, layout::NET_SIZE, net_bus).expect("net range overlap");
  // RX thread: drain the vmnet receiver into the device.
  let net_rx = net_mmio.clone();
  std::thread::spawn(move || {
      for frame in rx {
          net_rx.lock().unwrap().inject_rx(&frame);
      }
  });
  ```
- Add the net device to the FDT device list:
  `fdt_devices.push(fdt::FdtDevice::VirtioBlk(MmioDev { addr: NET_BASE, size: NET_SIZE, irq: NET_SPI }))`
  — **wait:** the FDT node for net must be a `virtio,mmio` node too (same
  `create_virtio_node`), so a `VirtioBlk`-variant node is structurally correct (it
  emits a generic `virtio_mmio@addr` node with reg+interrupts; the kernel probes the
  device id from the mmio registers, not the FDT). So pushing a second
  `FdtDevice::VirtioBlk(MmioDev{NET_BASE,NET_SIZE,NET_SPI})` produces the right node.
  (Optionally rename the enum variant to `VirtioMmio` in a follow-up; not required
  now.)

- [ ] **Step 3: Build + sign**

```bash
cargo build -p hvf-spike --bin boot 2>&1 | tail -3
cargo clippy --workspace 2>&1 | grep -c 'warning:'
./scripts/sign.sh target/debug/boot
```
Expected: builds, 0 clippy, signed. (`ignition_vmnet`, `VirtioNet`, `VirtioMmio`,
`GuestRam` must be imported in boot.rs; `spike` must depend on `ignition-vmnet`.)

- [ ] **Step 4: Single-vCPU non-net regression**

```bash
pkill -9 -f 'target/debug/boot' 2>/dev/null; sleep 1
( target/debug/boot kimage/out/Image kimage/out/rootfs.ext4 >/tmp/nonet.out 2>/dev/null & p=$!; sleep 35; kill -9 $p 2>/dev/null; wait $p 2>/dev/null )
echo "no-net login: $(grep -c 'login:' /tmp/nonet.out)"
```
Expected: `1` — without `--net` nothing changed, no sudo needed.

- [ ] **Step 5: Networking integration (the bar) — under sudo**

The rootfs must run a DHCP client on `eth0` (kimage side). Re-sign, then:
```bash
./scripts/sign.sh target/debug/boot
sudo bash -c '( sleep 35; printf "root\n"; sleep 3; \
  printf "ip link set eth0 up && udhcpc -i eth0 -n -q\n"; sleep 6; \
  printf "ip addr show eth0\n"; sleep 2; \
  printf "ping -c1 -W2 8.8.8.8\n"; sleep 4; \
  printf "nslookup example.com 2>/dev/null || wget -qO- -T5 http://example.com | head -c0 && echo DNS_OK\n"; sleep 5; \
  printf "poweroff\n"; sleep 5 ) \
  | target/debug/boot --net kimage/out/Image kimage/out/rootfs.ext4 >/tmp/net.out 2>/tmp/net.err'
echo "=== got an IP? ==="; grep -iE 'inet |bound|adding|lease' /tmp/net.out | head
echo "=== ping 8.8.8.8 ==="; grep -iE '1 packets transmitted|bytes from 8.8.8.8' /tmp/net.out
echo "=== DNS ==="; grep -iE 'DNS_OK|example.com|Address' /tmp/net.out | head
```
Expected: `eth0` gets an `inet` address; `ping 8.8.8.8` shows `1 received`; DNS
resolves. If RX never arrives (no DHCP lease), check `/tmp/net.err` for vmnet errors
and confirm sudo. If TX works but RX doesn't, the RX thread / `inject_rx` / IRQ path
is the suspect (the device unit tests isolate that logic). Capture findings.

- [ ] **Step 6: Commit**

```bash
git add crates/arch/src/aarch64/layout.rs spike/src/bin/boot.rs spike/Cargo.toml
git commit -m "feat(boot): --net virtio-net via vmnet (DHCP + outbound NAT)"
```

---

## Self-review notes (resolved)

- **Spec coverage:** transport generalization (Task 1); virtio-net TX/RX device +
  `NetBackend` with unit tests (Task 2); vmnet FFI backend (Task 3); `--net` wiring +
  RX thread + FDT + the integration bar (Task 4).
- **Queue ownership** (the spec's open point) is resolved: the **transport** owns the
  `Virtqueue`s; the device services a queue passed in by reference (`handle_notify` /
  `inject_rx`). The RX thread reaches the RX queue through `VirtioMmio::inject_rx`
  (held via `Arc<Mutex<VirtioMmio>>`), so all IRQ/InterruptStatus logic stays in the
  transport. `inject_rx` defaults to a no-op for blk.
- **Type consistency:** `VirtioDevice` (device_id/device_features/config_read/
  queue_count/handle_notify/inject_rx), `NetBackend` (write_frame/mac), `VirtioNet<B>`,
  `VmnetBackend`, `NET_BASE/NET_SIZE/NET_SPI` used consistently across tasks.
- **No unit tests for Tasks 3/4-runtime** is intentional (vmnet FFI + sudo); the
  device logic is fully unit-tested in Task 2, and the integration bar covers the
  rest.
- **InterruptStatus accumulation** is satisfied: `raise()` OR-sets `INT_STATUS_USED`
  from both `notify` (TX) and `inject_rx` (RX); ACK (0x064) clears it. Sticky across
  threads because both go through the `Arc<Mutex<VirtioMmio>>`.
- After all tasks, mark virtio-net done and write `docs/virtio-net-result.md`
  (controller, in finishing).
```
