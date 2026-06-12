# Phase 1 Milestone 2e: virtio-blk → shell prompt Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a synchronous virtio-mmio block device backed by `rootfs.ext4` so the guest kernel mounts root, runs init, and prints a shell/login prompt to host stdout.

**Architecture:** The guest's `QueueNotify` MMIO write (already dispatched to the `Bus` on the vCPU thread) drives the virtqueue inline: walk the available ring against a `GuestRam` view of the mmap, do file I/O, write the used ring, raise the device SPI via the GIC. No threads/eventfd. Ring-walk + block logic are pure and unit-tested; the boot run is the integration gate.

**Tech Stack:** Rust edition 2024, std `File`, the existing `arch`/`devices`/`hvf`/`vmm` crates, `tempfile` (dev-dep) for block tests.

**Commit convention for this project:** plain commit messages, NO `Co-Authored-By` / "Generated with Claude" trailer.

---

## File Structure

- `crates/arch/src/aarch64/layout.rs` — add `VIRTIO_*` consts; extend `default_cmdline`
- `crates/arch/src/aarch64/fdt.rs` — `virtio_mmio` node + `FdtConfig.virtio` field
- `spike/src/bin/gic-smoke.rs` — set `virtio: None` (FdtConfig grew a field)
- `spike/src/bin/boot.rs` — set `virtio` (None in Task A; the real device in Task E)
- `crates/devices/Cargo.toml` — add `tempfile` dev-dep
- `crates/devices/src/lib.rs` — `pub mod virtio;`
- `crates/devices/src/virtio/mod.rs` — `IrqLine` trait + submodule decls
- `crates/devices/src/virtio/guest_ram.rs` — `GuestRam`
- `crates/devices/src/virtio/queue.rs` — `Desc`/`DescChain`/`Virtqueue`
- `crates/devices/src/virtio/blk.rs` — `VirtioBlk`
- `crates/devices/src/virtio/mmio.rs` — `VirtioMmio` (`BusDevice`)

Tasks B/C/D are pure `cargo test`. Tasks A is `cargo test` + build. Task E is build + a live boot run (needs HVF + the kernel; run in the main session).

---

## Task A: layout consts, cmdline, FDT virtio node

**Files:**
- Modify: `crates/arch/src/aarch64/layout.rs`
- Modify: `crates/arch/src/aarch64/fdt.rs`
- Modify: `spike/src/bin/gic-smoke.rs`, `spike/src/bin/boot.rs`

- [ ] **Step 1: layout consts + cmdline**

In `crates/arch/src/aarch64/layout.rs`, after the `SERIAL_SPI` const add:

```rust
/// virtio-mmio device window (one block device). Above the serial, below GIC/RAM.
pub const VIRTIO_BASE: u64 = 0x0a00_0000;
pub const VIRTIO_SIZE: u64 = 0x200;
/// virtio block IRQ as the bare GIC SPI index (absolute INTID = 32 + this = 33).
pub const VIRTIO_SPI: u32 = 1;
```

Change `default_cmdline` to mount the virtio disk:

```rust
pub fn default_cmdline() -> String {
    format!("console=ttyS0 earlycon=uart8250,mmio,{SERIAL_BASE:#x} root=/dev/vda rw rootwait reboot=k panic=1")
}
```

- [ ] **Step 2: FdtConfig field + virtio node + generate wiring**

In `crates/arch/src/aarch64/fdt.rs`, add the field to `FdtConfig` (after `initrd`):

```rust
    /// (guest addr, size) when an initramfs is loaded.
    pub initrd: Option<(u64, u64)>,
    /// The virtio-mmio block device, when attached.
    pub virtio: Option<MmioDev>,
```

Add the node helper (after `create_serial_node`):

```rust
fn create_virtio_node(fdt: &mut FdtWriter, dev: &MmioDev) -> Result<(), vm_fdt::Error> {
    let node = fdt.begin_node(&format!("virtio_mmio@{:x}", dev.addr))?;
    fdt.property_string("compatible", "virtio,mmio")?;
    fdt.property_array_u64("reg", &[dev.addr, dev.size])?;
    fdt.property_null("dma-coherent")?;
    fdt.property_array_u32("interrupts", &[IRQ_TYPE_SPI, dev.irq, IRQ_TYPE_EDGE_RISING])?;
    fdt.property_u32("interrupt-parent", GIC_PHANDLE)?;
    fdt.end_node(node)?;
    Ok(())
}
```

In `generate`, after the `create_serial_node(&mut fdt, &cfg.serial)?;` line add:

```rust
    if let Some(virtio) = &cfg.virtio {
        create_virtio_node(&mut fdt, virtio)?;
    }
```

- [ ] **Step 3: update the FdtConfig literals so everything compiles**

In `crates/arch/src/aarch64/fdt.rs` `sample()` (the test helper), add `virtio: None,` after its `initrd: None,`.
In `spike/src/bin/gic-smoke.rs`, in its `FdtConfig { ... }`, add `virtio: None,` after `initrd: None,`.
In `spike/src/bin/boot.rs`, in its `FdtConfig { ... }`, add `virtio: None,` after `initrd,` (the real device is wired in Task E).

- [ ] **Step 4: add the virtio node test**

In `crates/arch/src/aarch64/fdt.rs` `mod tests`, after `serial_node_is_ns16550a`, add:

```rust
    #[test]
    fn virtio_node_present_only_when_set() {
        // Absent by default.
        let blob = generate(&sample()).unwrap();
        let dt = Fdt::new(&blob).unwrap();
        assert!(dt.find_node("/virtio_mmio@a000000").is_none());

        // Present when configured.
        let mut cfg = sample();
        cfg.virtio = Some(MmioDev { addr: 0x0a00_0000, size: 0x200, irq: 1 });
        let blob = generate(&cfg).unwrap();
        let dt = Fdt::new(&blob).unwrap();
        let node = dt.find_node("/virtio_mmio@a000000").unwrap();
        assert_eq!(dt_str(node.property("compatible").unwrap().value), "virtio,mmio");
        assert_eq!(be_u64s(node.property("reg").unwrap().value), vec![0x0a00_0000, 0x200]);
        assert_eq!(be_u32s(node.property("interrupts").unwrap().value), vec![0, 1, 1]);
    }
```

- [ ] **Step 5: test + build**

Run: `cargo test -p ignition-arch 2>&1 | grep 'test result' && cargo build --workspace 2>&1 | tail -3`
Expected: `test result: ok. 22 passed` (21 + the new one) and `Finished`.

- [ ] **Step 6: commit**

```bash
git add crates/arch/src/aarch64/layout.rs crates/arch/src/aarch64/fdt.rs spike/src/bin/gic-smoke.rs spike/src/bin/boot.rs
git commit -m "feat(arch): virtio-mmio FDT node + layout consts + root= cmdline

VIRTIO_BASE/SIZE/SPI, default_cmdline gains root=/dev/vda rw rootwait, and
FdtConfig grows an optional virtio_mmio node (lifted from FC). Callers set
virtio: None for now."
```

---

## Task B: GuestRam + split virtqueue

**Files:**
- Modify: `crates/devices/src/lib.rs`
- Create: `crates/devices/src/virtio/mod.rs`, `crates/devices/src/virtio/guest_ram.rs`, `crates/devices/src/virtio/queue.rs`

- [ ] **Step 1: module declarations**

In `crates/devices/src/lib.rs`, add at the end:

```rust
pub mod virtio;
```

Create `crates/devices/src/virtio/mod.rs`:

```rust
//! Synchronous, exit-driven virtio-mmio block device.

pub mod blk;
pub mod guest_ram;
pub mod mmio;
pub mod queue;

/// A device interrupt line. Implemented by the boot harness over the GIC.
pub trait IrqLine: Send + Sync {
    /// Assert (`true`) or deassert (`false`) the device's interrupt.
    fn set_spi(&self, level: bool);
}
```

(Note: `blk` and `mmio` are declared here but created in Tasks C/D. To let this
task build, create empty-ish placeholders now: see Step 4.)

- [ ] **Step 2: create `guest_ram.rs`**

```rust
//! A bounds-checked view of guest RAM for synchronous virtio DMA.
//!
//! Wraps a raw pointer into the host mmap that backs guest RAM. It is touched
//! only on the vCPU thread during an MMIO exit, when the guest is paused — so the
//! access is exclusive and single-threaded, justifying the `unsafe impl Send`.

pub struct GuestRam {
    ptr: *mut u8,
    len: usize,
    base: u64,
}

// SAFETY: only accessed on the vCPU thread while the guest is paused inside an
// MMIO exit (the device is dispatched synchronously from the run loop). No
// concurrent access occurs.
unsafe impl Send for GuestRam {}

impl GuestRam {
    /// `ptr`/`len` describe the host mapping; `base` is the guest physical
    /// address it is mapped at.
    pub fn new(ptr: *mut u8, len: usize, base: u64) -> Self {
        Self { ptr, len, base }
    }

    fn offset(&self, gpa: u64, n: usize) -> Option<usize> {
        let off = usize::try_from(gpa.checked_sub(self.base)?).ok()?;
        if off.checked_add(n)? <= self.len {
            Some(off)
        } else {
            None
        }
    }

    pub fn read_slice(&self, gpa: u64, out: &mut [u8]) -> bool {
        match self.offset(gpa, out.len()) {
            Some(off) => {
                // SAFETY: bounds checked by `offset`; exclusive access (see above).
                unsafe { std::ptr::copy_nonoverlapping(self.ptr.add(off), out.as_mut_ptr(), out.len()) };
                true
            }
            None => false,
        }
    }

    pub fn write_slice(&self, gpa: u64, data: &[u8]) -> bool {
        match self.offset(gpa, data.len()) {
            Some(off) => {
                // SAFETY: bounds checked by `offset`; exclusive access (see above).
                unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), self.ptr.add(off), data.len()) };
                true
            }
            None => false,
        }
    }

    pub fn read_u16(&self, gpa: u64) -> Option<u16> {
        let mut b = [0u8; 2];
        self.read_slice(gpa, &mut b).then(|| u16::from_le_bytes(b))
    }
    pub fn read_u32(&self, gpa: u64) -> Option<u32> {
        let mut b = [0u8; 4];
        self.read_slice(gpa, &mut b).then(|| u32::from_le_bytes(b))
    }
    pub fn read_u64(&self, gpa: u64) -> Option<u64> {
        let mut b = [0u8; 8];
        self.read_slice(gpa, &mut b).then(|| u64::from_le_bytes(b))
    }
    pub fn write_u16(&self, gpa: u64, v: u16) -> bool {
        self.write_slice(gpa, &v.to_le_bytes())
    }
    pub fn write_u32(&self, gpa: u64, v: u32) -> bool {
        self.write_slice(gpa, &v.to_le_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ram(backing: &mut Vec<u8>, base: u64) -> GuestRam {
        GuestRam::new(backing.as_mut_ptr(), backing.len(), base)
    }

    #[test]
    fn round_trip_within_bounds() {
        let mut backing = vec![0u8; 0x1000];
        let m = ram(&mut backing, 0x4000_0000);
        assert!(m.write_u32(0x4000_0010, 0xdead_beef));
        assert_eq!(m.read_u32(0x4000_0010), Some(0xdead_beef));
        assert!(m.write_slice(0x4000_0020, &[1, 2, 3, 4]));
        let mut out = [0u8; 4];
        assert!(m.read_slice(0x4000_0020, &mut out));
        assert_eq!(out, [1, 2, 3, 4]);
    }

    #[test]
    fn out_of_bounds_rejected() {
        let mut backing = vec![0u8; 0x100];
        let m = ram(&mut backing, 0x4000_0000);
        assert!(!m.write_u32(0x4000_00fe, 0)); // crosses the end
        assert_eq!(m.read_u32(0x3fff_ffff), None); // below base
        assert_eq!(m.read_u64(0x5000_0000), None); // far above
    }
}
```

- [ ] **Step 3: create `queue.rs`**

```rust
//! Minimal split virtqueue (virtio 1.0 §2.6), processed synchronously.

use super::guest_ram::GuestRam;

const VIRTQ_DESC_F_NEXT: u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2;
const DESC_SIZE: u64 = 16;

/// One resolved descriptor.
#[derive(Debug, PartialEq, Eq)]
pub struct Desc {
    pub addr: u64,
    pub len: u32,
    /// Device-writable (VIRTQ_DESC_F_WRITE set).
    pub writable: bool,
}

/// A resolved descriptor chain.
#[derive(Debug, PartialEq, Eq)]
pub struct DescChain {
    pub head: u16,
    pub descriptors: Vec<Desc>,
}

pub struct Virtqueue {
    size: u16,
    desc_addr: u64,
    driver_addr: u64, // available ring
    device_addr: u64, // used ring
    last_avail_idx: u16,
}

impl Virtqueue {
    pub fn new(size: u16, desc_addr: u64, driver_addr: u64, device_addr: u64) -> Self {
        Self { size, desc_addr, driver_addr, device_addr, last_avail_idx: 0 }
    }

    /// The next not-yet-seen available chain, or `None` if drained.
    ///
    /// avail ring layout: `{flags: u16, idx: u16, ring: [u16; size]}`.
    pub fn pop_avail(&mut self, mem: &GuestRam) -> Option<DescChain> {
        if self.size == 0 {
            return None;
        }
        let avail_idx = mem.read_u16(self.driver_addr + 2)?;
        if self.last_avail_idx == avail_idx {
            return None;
        }
        let slot = self.last_avail_idx % self.size;
        let head = mem.read_u16(self.driver_addr + 4 + u64::from(slot) * 2)?;
        self.last_avail_idx = self.last_avail_idx.wrapping_add(1);

        let mut descriptors = Vec::new();
        let mut idx = head;
        // Bounded by `size` to defend against a malformed cyclic chain.
        for _ in 0..self.size {
            let d = self.desc_addr + u64::from(idx) * DESC_SIZE;
            let addr = mem.read_u64(d)?;
            let len = mem.read_u32(d + 8)?;
            let flags = mem.read_u16(d + 12)?;
            let next = mem.read_u16(d + 14)?;
            descriptors.push(Desc { addr, len, writable: flags & VIRTQ_DESC_F_WRITE != 0 });
            if flags & VIRTQ_DESC_F_NEXT == 0 {
                break;
            }
            idx = next;
        }
        Some(DescChain { head, descriptors })
    }

    /// Append a used element and publish it (the `idx` store happens last).
    ///
    /// used ring layout: `{flags: u16, idx: u16, ring: [{id: u32, len: u32}; size]}`.
    pub fn push_used(&mut self, mem: &GuestRam, head: u16, len: u32) {
        let used_idx = mem.read_u16(self.device_addr + 2).unwrap_or(0);
        let slot = used_idx % self.size;
        let elem = self.device_addr + 4 + u64::from(slot) * 8;
        mem.write_u32(elem, u32::from(head));
        mem.write_u32(elem + 4, len);
        mem.write_u16(self.device_addr + 2, used_idx.wrapping_add(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Memory map for the tests: desc table @0x1000, avail @0x2000, used @0x3000.
    const BASE: u64 = 0x4000_0000;
    const DESC: u64 = BASE + 0x1000;
    const AVAIL: u64 = BASE + 0x2000;
    const USED: u64 = BASE + 0x3000;

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

    #[test]
    fn pop_single_descriptor_chain() {
        let mut backing = vec![0u8; 0x4000];
        let m = mem(&mut backing);
        write_desc(&m, 0, 0x4000_0500, 16, 0, 0); // no NEXT
        m.write_u16(AVAIL + 2, 1); // avail.idx = 1
        m.write_u16(AVAIL + 4, 0); // ring[0] = desc 0

        let mut vq = Virtqueue::new(8, DESC, AVAIL, USED);
        let chain = vq.pop_avail(&m).unwrap();
        assert_eq!(chain.head, 0);
        assert_eq!(chain.descriptors, vec![Desc { addr: 0x4000_0500, len: 16, writable: false }]);
        assert!(vq.pop_avail(&m).is_none()); // drained
    }

    #[test]
    fn pop_walks_next_and_marks_writable() {
        let mut backing = vec![0u8; 0x4000];
        let m = mem(&mut backing);
        write_desc(&m, 0, 0x4000_0500, 16, VIRTQ_DESC_F_NEXT, 1); // -> desc 1
        write_desc(&m, 1, 0x4000_0600, 512, VIRTQ_DESC_F_WRITE, 0); // writable, end
        m.write_u16(AVAIL + 2, 1);
        m.write_u16(AVAIL + 4, 0);

        let mut vq = Virtqueue::new(8, DESC, AVAIL, USED);
        let chain = vq.pop_avail(&m).unwrap();
        assert_eq!(chain.descriptors.len(), 2);
        assert!(!chain.descriptors[0].writable);
        assert!(chain.descriptors[1].writable);
        assert_eq!(chain.descriptors[1].addr, 0x4000_0600);
    }

    #[test]
    fn push_used_writes_element_and_bumps_idx() {
        let mut backing = vec![0u8; 0x4000];
        let m = mem(&mut backing);
        let mut vq = Virtqueue::new(8, DESC, AVAIL, USED);
        vq.push_used(&m, 3, 512);
        assert_eq!(m.read_u32(USED + 4), Some(3)); // ring[0].id
        assert_eq!(m.read_u32(USED + 8), Some(512)); // ring[0].len
        assert_eq!(m.read_u16(USED + 2), Some(1)); // used.idx
    }
}
```

- [ ] **Step 4: temporary placeholders so the module tree builds**

Tasks C and D fill `blk.rs` and `mmio.rs`, but `mod.rs` declares them now. Create
minimal placeholders so this task compiles:

`crates/devices/src/virtio/blk.rs`:
```rust
//! virtio-blk (filled in Task C).
```
`crates/devices/src/virtio/mmio.rs`:
```rust
//! virtio-mmio transport (filled in Task D).
```

- [ ] **Step 5: test**

Run: `cargo test -p ignition-devices virtio 2>&1 | tail -20`
Expected: `test result: ok. 5 passed` (2 guest_ram + 3 queue).

- [ ] **Step 6: commit**

```bash
git add crates/devices/src/lib.rs crates/devices/src/virtio/
git commit -m "feat(devices): GuestRam view + split virtqueue

GuestRam: bounds-checked DMA over the host mmap (exclusive during MMIO exits).
Virtqueue: pop_avail walks the descriptor chain, push_used publishes the used
ring. IrqLine trait declared. Unit-tested."
```

---

## Task C: virtio-blk

**Files:**
- Modify: `crates/devices/Cargo.toml`
- Replace: `crates/devices/src/virtio/blk.rs`

- [ ] **Step 1: add the tempfile dev-dep**

In `crates/devices/Cargo.toml`, add a dev-dependencies section (after `[dependencies]`):

```toml
[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 2: replace `blk.rs`**

```rust
//! Synchronous virtio-blk request processing (virtio 1.0 §5.2).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};

use super::guest_ram::GuestRam;
use super::queue::{Desc, DescChain};

const SECTOR_SIZE: u64 = 512;

const VIRTIO_BLK_T_IN: u32 = 0;
const VIRTIO_BLK_T_OUT: u32 = 1;
const VIRTIO_BLK_T_FLUSH: u32 = 4;
const VIRTIO_BLK_T_GET_ID: u32 = 8;

const VIRTIO_BLK_S_OK: u8 = 0;
const VIRTIO_BLK_S_IOERR: u8 = 1;
const VIRTIO_BLK_S_UNSUPP: u8 = 2;

const DEVICE_ID: &[u8] = b"ignition-vda";

pub struct VirtioBlk {
    file: File,
    capacity_sectors: u64,
}

impl VirtioBlk {
    pub fn new(file: File) -> std::io::Result<Self> {
        let len = file.metadata()?.len();
        Ok(Self { file, capacity_sectors: len / SECTOR_SIZE })
    }

    pub fn capacity_sectors(&self) -> u64 {
        self.capacity_sectors
    }

    /// Process one request chain. Returns the number of bytes written into
    /// guest-writable buffers (used as the used-ring `len`). The chain is
    /// `[header(read, 16B), data..(read|write), status(write, 1B)]`.
    pub fn process(&mut self, chain: &DescChain, mem: &GuestRam) -> u32 {
        let descs = &chain.descriptors;
        if descs.len() < 2 {
            return 0;
        }
        let header = &descs[0];
        let status_desc = &descs[descs.len() - 1];
        let data = &descs[1..descs.len() - 1];

        let mut hdr = [0u8; 16];
        if header.len < 16 || !mem.read_slice(header.addr, &mut hdr) {
            self.set_status(mem, status_desc.addr, VIRTIO_BLK_S_IOERR);
            return 1;
        }
        let req_type = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
        let sector = u64::from_le_bytes(hdr[8..16].try_into().unwrap());

        let (status, written) = match req_type {
            VIRTIO_BLK_T_IN => self.read_to_guest(mem, data, sector),
            VIRTIO_BLK_T_OUT => (self.write_from_guest(mem, data, sector), 0),
            VIRTIO_BLK_T_FLUSH => {
                let s = if self.file.flush().is_ok() { VIRTIO_BLK_S_OK } else { VIRTIO_BLK_S_IOERR };
                (s, 0)
            }
            VIRTIO_BLK_T_GET_ID => self.get_id(mem, data),
            _ => (VIRTIO_BLK_S_UNSUPP, 0),
        };
        self.set_status(mem, status_desc.addr, status);
        written + 1 // include the status byte
    }

    fn read_to_guest(&mut self, mem: &GuestRam, data: &[Desc], sector: u64) -> (u8, u32) {
        let mut written = 0u32;
        let mut off = sector * SECTOR_SIZE;
        for d in data {
            let mut buf = vec![0u8; d.len as usize];
            if self.file.seek(SeekFrom::Start(off)).is_err() || self.file.read_exact(&mut buf).is_err() {
                return (VIRTIO_BLK_S_IOERR, written);
            }
            if !mem.write_slice(d.addr, &buf) {
                return (VIRTIO_BLK_S_IOERR, written);
            }
            written += d.len;
            off += u64::from(d.len);
        }
        (VIRTIO_BLK_S_OK, written)
    }

    fn write_from_guest(&mut self, mem: &GuestRam, data: &[Desc], sector: u64) -> u8 {
        let mut off = sector * SECTOR_SIZE;
        for d in data {
            let mut buf = vec![0u8; d.len as usize];
            if !mem.read_slice(d.addr, &mut buf) {
                return VIRTIO_BLK_S_IOERR;
            }
            if self.file.seek(SeekFrom::Start(off)).is_err() || self.file.write_all(&buf).is_err() {
                return VIRTIO_BLK_S_IOERR;
            }
            off += u64::from(d.len);
        }
        VIRTIO_BLK_S_OK
    }

    fn get_id(&self, mem: &GuestRam, data: &[Desc]) -> (u8, u32) {
        if let Some(d) = data.first() {
            let mut buf = vec![0u8; d.len as usize];
            let n = (d.len as usize).min(DEVICE_ID.len());
            buf[..n].copy_from_slice(&DEVICE_ID[..n]);
            if mem.write_slice(d.addr, &buf) {
                return (VIRTIO_BLK_S_OK, d.len);
            }
        }
        (VIRTIO_BLK_S_IOERR, 0)
    }

    fn set_status(&self, mem: &GuestRam, addr: u64, status: u8) {
        mem.write_slice(addr, &[status]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    use crate::virtio::guest_ram::GuestRam;
    use crate::virtio::queue::{Desc, DescChain};

    const BASE: u64 = 0x4000_0000;
    const HDR: u64 = BASE + 0x100;
    const DATA: u64 = BASE + 0x200;
    const STATUS: u64 = BASE + 0x800;

    fn mem(backing: &mut Vec<u8>) -> GuestRam {
        GuestRam::new(backing.as_mut_ptr(), backing.len(), BASE)
    }

    /// A two-sector file: sector 0 all 0xAA, sector 1 all 0xBB.
    fn disk() -> File {
        let mut f = tempfile::tempfile().unwrap();
        f.write_all(&[0xAAu8; 512]).unwrap();
        f.write_all(&[0xBBu8; 512]).unwrap();
        f
    }

    fn header(m: &GuestRam, req_type: u32, sector: u64) {
        let mut h = [0u8; 16];
        h[0..4].copy_from_slice(&req_type.to_le_bytes());
        h[8..16].copy_from_slice(&sector.to_le_bytes());
        m.write_slice(HDR, &h);
    }

    fn chain(data_len: u32, data_writable: bool) -> DescChain {
        DescChain {
            head: 0,
            descriptors: vec![
                Desc { addr: HDR, len: 16, writable: false },
                Desc { addr: DATA, len: data_len, writable: data_writable },
                Desc { addr: STATUS, len: 1, writable: true },
            ],
        }
    }

    #[test]
    fn read_copies_sector_into_guest() {
        let mut backing = vec![0u8; 0x1000];
        let m = mem(&mut backing);
        header(&m, VIRTIO_BLK_T_IN, 1); // sector 1 = 0xBB
        let mut blk = VirtioBlk::new(disk()).unwrap();
        let written = blk.process(&chain(512, true), &m);
        assert_eq!(written, 513); // 512 data + 1 status
        let mut out = [0u8; 512];
        m.read_slice(DATA, &mut out);
        assert!(out.iter().all(|&b| b == 0xBB));
        assert_eq!(m.read_u16(STATUS).unwrap() & 0xff, VIRTIO_BLK_S_OK as u16);
    }

    #[test]
    fn write_persists_guest_buffer_to_disk() {
        let mut backing = vec![0u8; 0x1000];
        let m = mem(&mut backing);
        header(&m, VIRTIO_BLK_T_OUT, 0);
        m.write_slice(DATA, &[0xCDu8; 512]);
        let mut blk = VirtioBlk::new(disk()).unwrap();
        blk.process(&chain(512, false), &m);
        // Read sector 0 back out of the file.
        let mut buf = [0u8; 512];
        blk.file.seek(SeekFrom::Start(0)).unwrap();
        blk.file.read_exact(&mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0xCD));
    }

    #[test]
    fn unknown_type_is_unsupported() {
        let mut backing = vec![0u8; 0x1000];
        let m = mem(&mut backing);
        header(&m, 0x99, 0);
        let mut blk = VirtioBlk::new(disk()).unwrap();
        blk.process(&chain(16, true), &m);
        let mut s = [0u8; 1];
        m.read_slice(STATUS, &mut s);
        assert_eq!(s[0], VIRTIO_BLK_S_UNSUPP);
    }

    #[test]
    fn capacity_is_file_len_over_512() {
        let blk = VirtioBlk::new(disk()).unwrap();
        assert_eq!(blk.capacity_sectors(), 2);
    }
}
```

- [ ] **Step 3: test**

Run: `cargo test -p ignition-devices blk 2>&1 | tail -20`
Expected: `test result: ok. 4 passed`.

- [ ] **Step 4: commit**

```bash
git add crates/devices/Cargo.toml crates/devices/src/virtio/blk.rs
git commit -m "feat(devices): virtio-blk request processing

Parses the request header, does file read/write at sector*512, writes the
status byte; IN/OUT/FLUSH/GET_ID handled, others UNSUPP. Unit-tested with a
tempfile disk."
```

---

## Task D: virtio-mmio transport

**Files:**
- Replace: `crates/devices/src/virtio/mmio.rs`

- [ ] **Step 1: replace `mmio.rs`**

```rust
//! virtio-mmio transport (virtio 1.0 §4.2), driven synchronously.

use std::sync::Arc;

use crate::bus::BusDevice;

use super::IrqLine;
use super::blk::VirtioBlk;
use super::guest_ram::GuestRam;
use super::queue::Virtqueue;

const MAGIC: u32 = 0x7472_6976; // "virt"
const VERSION: u32 = 2;
const DEVICE_ID_BLK: u32 = 2;
const VENDOR_ID: u32 = 0x4b4e_5246; // arbitrary non-zero
const QUEUE_SIZE_MAX: u32 = 256;
/// DeviceFeatures high word (sel 1): bit 0 == VIRTIO_F_VERSION_1 (feature bit 32).
const FEATURES_HI_VERSION_1: u32 = 1;
const INT_STATUS_USED: u32 = 1;

/// A single-queue virtio-mmio block device.
pub struct VirtioMmio {
    blk: VirtioBlk,
    mem: GuestRam,
    irq: Arc<dyn IrqLine>,
    vq: Option<Virtqueue>,

    status: u32,
    device_features_sel: u32,
    queue_sel: u32,
    queue_num: u16,
    queue_ready: u32,
    desc_lo: u32,
    desc_hi: u32,
    driver_lo: u32,
    driver_hi: u32,
    device_lo: u32,
    device_hi: u32,
    interrupt_status: u32,
}

impl VirtioMmio {
    pub fn new(blk: VirtioBlk, mem: GuestRam, irq: Arc<dyn IrqLine>) -> Self {
        Self {
            blk,
            mem,
            irq,
            vq: None,
            status: 0,
            device_features_sel: 0,
            queue_sel: 0,
            queue_num: 0,
            queue_ready: 0,
            desc_lo: 0,
            desc_hi: 0,
            driver_lo: 0,
            driver_hi: 0,
            device_lo: 0,
            device_hi: 0,
            interrupt_status: 0,
        }
    }

    fn read_reg(&self, off: u64) -> u32 {
        match off {
            0x000 => MAGIC,
            0x004 => VERSION,
            0x008 => DEVICE_ID_BLK,
            0x00c => VENDOR_ID,
            0x010 => {
                if self.device_features_sel == 1 {
                    FEATURES_HI_VERSION_1
                } else {
                    0
                }
            }
            0x034 => QUEUE_SIZE_MAX,
            0x044 => self.queue_ready,
            0x060 => self.interrupt_status,
            0x070 => self.status,
            0x0fc => 0,
            0x100 => (self.blk.capacity_sectors() & 0xffff_ffff) as u32,
            0x104 => (self.blk.capacity_sectors() >> 32) as u32,
            _ => 0,
        }
    }

    fn write_reg(&mut self, off: u64, val: u32) {
        match off {
            0x014 => self.device_features_sel = val,
            0x020 => {} // DriverFeatures: accepted.
            0x024 => {} // DriverFeaturesSel: ignored (we only key DeviceFeatures off sel).
            0x030 => self.queue_sel = val,
            0x038 => self.queue_num = val as u16,
            0x044 => {
                self.queue_ready = val;
                if val == 1 && self.queue_sel == 0 {
                    let desc = (u64::from(self.desc_hi) << 32) | u64::from(self.desc_lo);
                    let driver = (u64::from(self.driver_hi) << 32) | u64::from(self.driver_lo);
                    let device = (u64::from(self.device_hi) << 32) | u64::from(self.device_lo);
                    self.vq = Some(Virtqueue::new(self.queue_num, desc, driver, device));
                } else if val == 0 {
                    self.vq = None;
                }
            }
            0x050 => self.notify(),
            0x064 => {
                self.interrupt_status &= !val;
                if self.interrupt_status == 0 {
                    self.irq.set_spi(false);
                }
            }
            0x070 => {
                self.status = val;
                if val == 0 {
                    self.reset();
                }
            }
            0x080 => self.desc_lo = val,
            0x084 => self.desc_hi = val,
            0x090 => self.driver_lo = val,
            0x094 => self.driver_hi = val,
            0x0a0 => self.device_lo = val,
            0x0a4 => self.device_hi = val,
            _ => {}
        }
    }

    fn reset(&mut self) {
        self.vq = None;
        self.queue_ready = 0;
        self.interrupt_status = 0;
        self.irq.set_spi(false);
    }

    fn notify(&mut self) {
        if self.queue_ready == 0 || self.queue_sel != 0 {
            return;
        }
        let mut serviced = false;
        {
            let Some(vq) = self.vq.as_mut() else { return };
            let mem = &self.mem;
            let blk = &mut self.blk;
            while let Some(chain) = vq.pop_avail(mem) {
                let len = blk.process(&chain, mem);
                vq.push_used(mem, chain.head, len);
                serviced = true;
            }
        }
        if serviced {
            self.interrupt_status |= INT_STATUS_USED;
            self.irq.set_spi(true);
        }
    }
}

impl BusDevice for VirtioMmio {
    fn read(&mut self, _base: u64, offset: u64, data: &mut [u8]) {
        if data.len() == 4 {
            data.copy_from_slice(&self.read_reg(offset).to_le_bytes());
        } else {
            log::warn!("virtio-mmio: non-32-bit read at {offset:#x} len {}", data.len());
        }
    }

    fn write(&mut self, _base: u64, offset: u64, data: &[u8]) {
        if data.len() == 4 {
            self.write_reg(offset, u32::from_le_bytes(data.try_into().unwrap()));
        } else {
            log::warn!("virtio-mmio: non-32-bit write at {offset:#x} len {}", data.len());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::virtio::guest_ram::GuestRam;

    const BASE: u64 = 0x4000_0000;

    #[derive(Default)]
    struct FakeIrq(Mutex<Vec<bool>>);
    impl IrqLine for FakeIrq {
        fn set_spi(&self, level: bool) {
            self.0.lock().unwrap().push(level);
        }
    }

    fn disk() -> std::fs::File {
        let mut f = tempfile::tempfile().unwrap();
        f.write_all(&[0xBBu8; 1024]).unwrap(); // 2 sectors
        f
    }

    fn dev(backing: &mut Vec<u8>, irq: Arc<dyn IrqLine>) -> VirtioMmio {
        let mem = GuestRam::new(backing.as_mut_ptr(), backing.len(), BASE);
        VirtioMmio::new(VirtioBlk::new(disk()).unwrap(), mem, irq)
    }

    fn rd(d: &mut VirtioMmio, off: u64) -> u32 {
        let mut b = [0u8; 4];
        d.read(BASE, off, &mut b);
        u32::from_le_bytes(b)
    }
    fn wr(d: &mut VirtioMmio, off: u64, v: u32) {
        d.write(BASE, off, &v.to_le_bytes());
    }

    #[test]
    fn identity_registers() {
        let mut backing = vec![0u8; 0x1000];
        let mut d = dev(&mut backing, Arc::new(FakeIrq::default()));
        assert_eq!(rd(&mut d, 0x000), 0x7472_6976);
        assert_eq!(rd(&mut d, 0x004), 2);
        assert_eq!(rd(&mut d, 0x008), 2);
        assert_eq!(rd(&mut d, 0x034), 256);
        assert_eq!(rd(&mut d, 0x100), 2); // capacity sectors low
        // DeviceFeatures high word advertises VERSION_1.
        wr(&mut d, 0x014, 1); // DeviceFeaturesSel = 1
        assert_eq!(rd(&mut d, 0x010), 1);
    }

    #[test]
    fn notify_services_a_request_and_pulses_irq() {
        // Lay out a one-entry queue in guest RAM and a single blk IN request.
        let mut backing = vec![0u8; 0x6000];
        let irq = Arc::new(FakeIrq::default());
        let mut d = dev(&mut backing, irq.clone());

        // Guest physical addresses (offsets from BASE).
        let desc = BASE + 0x1000;
        let avail = BASE + 0x2000;
        let used = BASE + 0x3000;
        let hdr = BASE + 0x0100;
        let data = BASE + 0x0200;
        let status = BASE + 0x0800;

        // Build the request header (type IN=0, sector 1) directly in RAM.
        {
            let m = GuestRam::new(backing.as_mut_ptr(), backing.len(), BASE);
            // header
            m.write_u32(hdr, 0); // type IN
            m.write_u32(hdr + 4, 0);
            m.write_u32(hdr + 8, 1); // sector 1 (low)
            m.write_u32(hdr + 12, 0);
            // descriptors: 0=header(r), 1=data(w,512), 2=status(w,1)
            let wd = |i: u64, a: u64, l: u32, fl: u16, nx: u16| {
                let dd = desc + i * 16;
                m.write_slice(dd, &a.to_le_bytes());
                m.write_slice(dd + 8, &l.to_le_bytes());
                m.write_slice(dd + 12, &fl.to_le_bytes());
                m.write_slice(dd + 14, &nx.to_le_bytes());
            };
            wd(0, hdr, 16, 1, 1); // NEXT -> 1
            wd(1, data, 512, 1 | 2, 2); // NEXT|WRITE -> 2
            wd(2, status, 1, 2, 0); // WRITE, end
            // avail: idx=1, ring[0]=0
            m.write_u16(avail + 2, 1);
            m.write_u16(avail + 4, 0);
        }

        // Program the queue registers and notify.
        wr(&mut d, 0x080, desc as u32);
        wr(&mut d, 0x084, (desc >> 32) as u32);
        wr(&mut d, 0x090, avail as u32);
        wr(&mut d, 0x094, (avail >> 32) as u32);
        wr(&mut d, 0x0a0, used as u32);
        wr(&mut d, 0x0a4, (used >> 32) as u32);
        wr(&mut d, 0x038, 8); // QueueNum
        wr(&mut d, 0x044, 1); // QueueReady
        wr(&mut d, 0x050, 0); // QueueNotify

        // The data buffer now holds sector 1 (0xBB), used ring advanced, IRQ pulsed.
        let m = GuestRam::new(backing.as_mut_ptr(), backing.len(), BASE);
        let mut out = [0u8; 512];
        m.read_slice(data, &mut out);
        assert!(out.iter().all(|&b| b == 0xBB));
        assert_eq!(m.read_u16(used + 2), Some(1)); // used.idx
        assert_eq!(rd(&mut d, 0x060), 1); // InterruptStatus = used
        assert_eq!(*irq.0.lock().unwrap().last().unwrap(), true);

        // ACK clears the interrupt and deasserts.
        wr(&mut d, 0x064, 1);
        assert_eq!(rd(&mut d, 0x060), 0);
        assert_eq!(*irq.0.lock().unwrap().last().unwrap(), false);
    }
}
```

- [ ] **Step 2: test + build the workspace**

Run: `cargo test -p ignition-devices virtio 2>&1 | tail -20 && cargo build --workspace 2>&1 | tail -3 && cargo clippy -p ignition-devices 2>&1 | tail -5`
Expected: `test result: ok. 11 passed` (2 guest_ram + 3 queue + 4 blk + 2 mmio), `Finished`, no clippy warnings.

- [ ] **Step 3: commit**

```bash
git add crates/devices/src/virtio/mmio.rs
git commit -m "feat(devices): virtio-mmio transport (single-queue block)

Register file (magic/version/id/features/queue setup/status/notify/interrupt)
+ QueueNotify drives the virtqueue through virtio-blk and pulses the IrqLine.
Unit-tested: identity registers + a full notify->service->irq->ack cycle."
```

---

## Task E: boot-harness wiring + boot run

**Files:**
- Modify: `spike/src/bin/boot.rs`

The implementer wires the device and BUILDS it. The boot RUN (with the real
kernel + rootfs) is done by the operator in the main session afterward.

- [ ] **Step 1: rewrite the relevant parts of `boot.rs`**

Replace the imports block at the top of `spike/src/bin/boot.rs`:

```rust
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::{env, fs, process};

use arch::aarch64::fdt::{self, FdtConfig, MmioDev};
use arch::aarch64::{kernel, layout};
use devices::bus::{Bus, BusDevice};
use devices::serial::Serial;
use devices::virtio::IrqLine;
use devices::virtio::blk::VirtioBlk;
use devices::virtio::guest_ram::GuestRam;
use devices::virtio::mmio::VirtioMmio;
use hvf::gic::HvfGicV3;
use vmm::vstate::hvf_vcpu::Vcpu;
use vmm::vstate::hvf_vm::Vm;
```

Add the GIC IRQ adapter just above `fn main()`:

```rust
/// Adapts the in-kernel GIC to the device `IrqLine`. The virtio SPI index is the
/// bare FDT index; hv_gic_set_spi wants the absolute INTID (32 + index).
struct GicIrq(Arc<HvfGicV3>);
impl IrqLine for GicIrq {
    fn set_spi(&self, level: bool) {
        let _ = self.0.set_spi(layout::VIRTIO_SPI + 32, level);
    }
}
```

Change the argv handling so `argv[2]` is the disk image (was an initrd):

```rust
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: {} <kernel-Image> [rootfs-disk]", args[0]);
        process::exit(2);
    }
    let kernel_image = fs::read(&args[1]).expect("failed to read kernel image");
    let disk_path = args.get(2).cloned();
```

Delete the old initrd block (the `let initrd = if let Some(ref bytes) =
initrd_bytes { ... }` block and its `initrd_bytes` read) and set `initrd: None`
in the `FdtConfig`. Keep the kernel load, the `fdt_addr`/`fdt_off` computation,
the DTB write, and the `map_memory` call as they are.

Make the GIC shared and attach virtio after `map_memory`. Replace the GIC
creation line:

```rust
    let gic = Arc::new(HvfGicV3::new(1, layout::RAM_BASE).expect("hv_gic_create failed"));
```

In the `FdtConfig`, set the virtio field based on whether a disk was given:

```rust
        gic: gic.fdt_info(),
        initrd: None,
        virtio: disk_path
            .as_ref()
            .map(|_| MmioDev { addr: layout::VIRTIO_BASE, size: layout::VIRTIO_SIZE, irq: layout::VIRTIO_SPI }),
```

After the `map_memory` call and the diagnostics block, build the bus with both
the serial and (if a disk was given) the virtio device. Replace the existing
bus-setup + run block:

```rust
    // Device bus: 16550 serial to stdout, plus an optional virtio-blk disk.
    let mut bus = Bus::new();
    let serial: Arc<Mutex<dyn BusDevice>> = Arc::new(Mutex::new(Serial::new(io::stdout())));
    bus.register(layout::SERIAL_BASE, layout::SERIAL_SIZE, serial);

    if let Some(path) = &disk_path {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .expect("failed to open rootfs disk");
        let blk = VirtioBlk::new(file).expect("virtio-blk init failed");
        // SAFETY: the host mapping outlives the run; the device touches it only
        // during MMIO exits, when the guest is paused.
        let guest_ram = GuestRam::new(host as *mut u8, RAM_SIZE as usize, layout::RAM_BASE);
        let virtio: Arc<Mutex<dyn BusDevice>> =
            Arc::new(Mutex::new(VirtioMmio::new(blk, guest_ram, Arc::new(GicIrq(gic.clone())))));
        bus.register(layout::VIRTIO_BASE, layout::VIRTIO_SIZE, virtio);
        eprintln!("virtio : /dev/vda backed by {path}");
    }
    let bus = Arc::new(bus);

    // Run. Earlycon + virtio MMIO exits are dispatched through the bus.
    let vcpu = Vcpu::new(0, entry, fdt_addr, bus);
    match vcpu.start().join().expect("vCPU thread panicked") {
        Ok(()) => eprintln!("\n[vcpu exited cleanly]"),
        Err(e) => eprintln!("\n[vcpu error: {e}]"),
    }
```

(The `host` raw pointer from the `mmap` is still in scope; `gic.fdt_info()` works
through the `Arc<HvfGicV3>` deref. Ensure the diagnostics block's `let g =
gic.fdt_info();` still compiles — it does, via deref.)

- [ ] **Step 2: build (do NOT run)**

Run: `cargo build -p hvf-spike --bin boot 2>&1 | tail -15 && cargo build --workspace 2>&1 | tail -3`
Expected: `Finished`, no errors. If the borrow of `host` (used for both the
kernel/DTB `ram` slice earlier and `GuestRam` here) conflicts, note that the `ram`
slice's last use is the DTB write — it is not used after, so the `GuestRam` raw
pointer from `host` does not conflict. Report any change you make.

- [ ] **Step 3: commit**

```bash
git add spike/src/bin/boot.rs
git commit -m "feat(spike): attach virtio-blk disk in the boot harness

argv[2] is now a rootfs disk backing /dev/vda via a synchronous virtio-mmio
device on the bus; the GIC is shared (Arc) so the device can raise its SPI.
Run: target/debug/boot Image rootfs.ext4."
```

- [ ] **Step 4: (operator, main session) sign + boot run**

This is the milestone's integration gate, run by the controller after the plan
lands:

```bash
scripts/sign.sh target/debug/boot
target/debug/boot kimage/out/Image kimage/out/rootfs.ext4 2>/tmp/boot.diag
```

Expected: past the 2d panic point — `EXT4-fs (vda): mounted filesystem`, init
runs, and an alpine/busybox shell or login prompt appears on stdout. Debugged
live (handshake, ring, ext4 mount) if it stalls.

---

## Self-Review

**Spec coverage:**
- layout `VIRTIO_*` + cmdline `root=/dev/vda` → Task A ✓
- virtio FDT node + `FdtConfig.virtio` → Task A ✓
- `GuestRam` → Task B ✓
- split `Virtqueue` (pop_avail/push_used) → Task B ✓
- `VirtioBlk` (IN/OUT/FLUSH/GET_ID/status) → Task C ✓
- `VirtioMmio` transport (register map, notify→service→irq, ack) → Task D ✓
- `IrqLine` trait + GIC adapter → Task B (trait) + Task E (adapter) ✓
- harness wiring (open disk, GuestRam, register, root=) + boot run → Task E ✓
- Testing: GuestRam/queue/blk/mmio unit tests + boot run gate → Tasks B/C/D/E ✓
- Out-of-scope (serial RX, channel parking, multi-queue, indirect, legacy) → not implemented ✓

**Placeholder scan:** No TBD/TODO-as-work. The Task B blk.rs/mmio.rs placeholders
are explicitly replaced in Tasks C/D. All code is complete with real assertions.

**Type consistency:** `GuestRam::{new,read_slice,write_slice,read_u16/u32/u64,write_u16/u32}`
used identically across queue/blk/mmio/harness. `Desc{addr,len,writable}` /
`DescChain{head,descriptors}` consistent between queue, blk, and mmio tests.
`Virtqueue::{new,pop_avail,push_used}` signatures match call sites. `VirtioBlk::{new,
capacity_sectors,process}` and `VirtioMmio::new(blk, mem, irq)` match Task E.
`IrqLine::set_spi(&self, bool)` consistent (trait, FakeIrq, GicIrq). `FdtConfig.virtio:
Option<MmioDev>` added in Task A and set in every caller (sample/gic-smoke/boot).
`layout::{VIRTIO_BASE,VIRTIO_SIZE,VIRTIO_SPI}` consistent. Register offsets match
the spec's table.

Fixed inline: removed the accidental `mod_tests_placeholder!()` by flagging it for
deletion in the step text.
