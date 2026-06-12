# Phase 1 Milestone 2e: virtio-blk → shell prompt

Status: approved design, pre-implementation.
Date: 2026-06-12.
Project: ignition (Firecracker → macOS/HVF fork). Follows the 2d boot (a real
Linux 6.1 aarch64 kernel boots to the rootfs-mount panic; see
`docs/2d-boot-result.md`). The kernel + `kimage/out/rootfs.ext4` (alpine arm64,
busybox, ttyS0, passwordless root) are built and ready.

## Goal

Give the guest a root filesystem so the kernel mounts it, runs `/sbin/init`, and
prints a shell/login prompt to our serial on host stdout. The mechanism is a
**synchronous virtio-mmio block device** backed by `rootfs.ext4`.

Success bar: **the alpine/busybox boot messages and a shell or login prompt
appear on host stdout.** Output only — keyboard input (serial RX) and blocking
WFI parking are the next milestone.

## Approach: synchronous, exit-driven virtio

No worker threads, no eventfd, no event-manager. The guest's write to the
virtio-mmio `QueueNotify` register is an ordinary MMIO exit that the run loop
already dispatches to the `Bus` on the vCPU thread. The `VirtioMmio` device
processes the virtqueue **inline** during that exit: the guest vCPU is paused, so
the device has exclusive access to guest RAM. It walks the available ring, does
the block file I/O, writes the used ring, sets `InterruptStatus`, and pulses the
device's GIC SPI. The kernel's virtio driver, on the next vCPU entry, takes the
interrupt and reaps the used ring.

This is far smaller than libkrun's threaded model and fits our `Bus` exactly. The
ring-walk and block logic are pure functions over a guest-RAM byte view plus a
backing `File`, so they are unit-tested without HVF.

## Memory model

`VirtioMmio` lives inside the `Bus` (`Arc<Mutex<dyn BusDevice>>`), which the vCPU
thread dispatches. It needs to read/write guest RAM (descriptors + DMA buffers).
The harness owns the `mmap` of guest RAM (also mapped into HVF) and hands the
device a `GuestRam { ptr: *mut u8, len: usize, base: u64 }`. Guest RAM is touched
only inside MMIO exits, when the vCPU is stopped — single-threaded, exclusive —
so `GuestRam` carries an `unsafe impl Send` justified by that invariant. All
accesses go through bounds-checked helpers (`read_slice`, `write_slice`,
`read_u16/u32/u64` at a guest physical address, translated `pa - base`).

## Components

### `arch::aarch64::layout` (+ cmdline)
```rust
pub const VIRTIO_BASE: u64 = 0x0a00_0000; // MMIO window for the virtio device
pub const VIRTIO_SIZE: u64 = 0x200;       // one virtio-mmio device register block
pub const VIRTIO_SPI: u32  = 1;           // bare SPI index -> absolute INTID 33
```
`default_cmdline()` gains `root=/dev/vda rw rootwait` (the virtio-blk disk is
`/dev/vda`; `rootwait` tolerates async probe). The serial clause is unchanged.
(`VIRTIO_BASE = 0x0a00_0000` is above the serial at `0x0900_0000` and below the
GIC/RAM — non-overlapping.)

### `arch::aarch64::fdt`
Re-add the `virtio_mmio` node (dropped in 2a; lifted from FC `create_virtio_node`):
`compatible = "virtio,mmio"`, `reg = [addr, size]`, `interrupts = [SPI, irq,
EDGE_RISING]`, `interrupt-parent = GIC_PHANDLE`, `dma-coherent`. Add
`virtio: Option<MmioDev>` to `FdtConfig`; emit the node only when `Some`. Existing
callers (gic-smoke, the fdt tests' `sample()`) set `virtio: None`; the boot
harness sets `Some(MmioDev { VIRTIO_BASE, VIRTIO_SIZE, VIRTIO_SPI })`.

### `devices::virtio::GuestRam`
The bounds-checked guest-RAM view described under Memory model.

### `devices::virtio::queue` — split virtqueue
Operates over a `GuestRam` plus the three ring addresses and the negotiated size
`N`. Layout (virtio 1.0 split ring):
- Descriptor table @ `desc_addr`: `N` × 16 bytes `{addr: u64, len: u32, flags: u16, next: u16}`. Flags: `NEXT = 1`, `WRITE = 2` (device-writable), `INDIRECT = 4` (unsupported — treated as a chain end; minimal).
- Available ring @ `driver_addr`: `{flags: u16, idx: u16, ring: [u16; N], used_event: u16}`.
- Used ring @ `device_addr`: `{flags: u16, idx: u16, ring: [{id: u32, len: u32}; N], avail_event: u16}`.

API:
```rust
pub struct DescChain { pub head: u16, pub descriptors: Vec<Desc> } // Desc { addr, len, writable }
pub struct Virtqueue { /* ring addrs, size, last_avail_idx */ }
impl Virtqueue {
    pub fn pop_avail(&mut self, mem: &GuestRam) -> Option<DescChain>; // next unseen head, walks .next chain
    pub fn push_used(&mut self, mem: &GuestRam, head: u16, len: u32); // append + bump used.idx
}
```
`pop_avail` reads `avail.idx`, and while `last_avail_idx != avail.idx` returns the
chain at `avail.ring[last_avail_idx % N]`, incrementing `last_avail_idx`. A chain
walk follows `next` while `NEXT` is set (bounded by `N` to avoid loops).

### `devices::virtio::blk` — virtio-blk
```rust
pub struct VirtioBlk { file: File, capacity_sectors: u64 } // capacity = file_len >> 9
impl VirtioBlk {
    pub fn capacity_sectors(&self) -> u64;
    /// Process one request described by `chain`: desc[0] (readable, 16 B) is the
    /// header {type: u32, _reserved: u32, sector: u64}; the middle descriptors
    /// are the data buffer; the last descriptor (writable, 1 B) is the status.
    /// Returns the number of bytes written into guest-writable buffers (for the
    /// used-ring `len`). Reads/writes `self.file` at `sector * 512`.
    pub fn process(&mut self, chain: &DescChain, mem: &GuestRam) -> u32;
}
```
Request types: `IN = 0` (disk→guest, copy `file[sector*512..]` into the writable
data descriptor), `OUT = 1` (guest→disk, write the readable data descriptor to the
file), `FLUSH = 4` (`file.flush`/`sync`, status OK), `GET_ID = 8` (write a short
device id string into the data buffer). Status byte: `OK = 0`, `IOERR = 1`,
`UNSUPP = 2`. `SECTOR_SIZE = 512`.

### `devices::virtio::mmio` — `VirtioMmio` transport (a `BusDevice`)
Holds the `VirtioBlk`, a `Virtqueue`, a `GuestRam`, an `Arc<dyn IrqLine>`, and the
register state (status, feature-select, queue-select, queue addrs/size/ready,
interrupt status). Register map (virtio-mmio v2, all 32-bit accesses; offsets
relative to `VIRTIO_BASE`):

| Off | Name | R/W | Behavior |
|---|---|---|---|
| 0x000 | MagicValue | R | `0x74726976` ("virt") |
| 0x004 | Version | R | `2` |
| 0x008 | DeviceID | R | `2` (block) |
| 0x00c | VendorID | R | `0x4b4e_4752` ("KRUN"-ish; any non-zero) |
| 0x010 | DeviceFeatures | R | sel 0 → blk features (we offer `0`); sel 1 → `VIRTIO_F_VERSION_1` (bit 32 → `1`) |
| 0x014 | DeviceFeaturesSel | W | latch |
| 0x020 | DriverFeatures | W | accept (store; must include VERSION_1) |
| 0x024 | DriverFeaturesSel | W | latch |
| 0x030 | QueueSel | W | only queue 0 supported |
| 0x034 | QueueNumMax | R | `QUEUE_SIZE = 256` |
| 0x038 | QueueNum | W | negotiated `N` |
| 0x044 | QueueReady | RW | 1 = queue live |
| 0x050 | QueueNotify | W | **drive the queue** (see flow) |
| 0x060 | InterruptStatus | R | bit0 = used-buffer notification |
| 0x064 | InterruptACK | W | clear acked bits; deassert SPI when zero |
| 0x070 | Status | RW | device-status handshake; write 0 = reset |
| 0x080/4 | QueueDescLow/High | W | descriptor table addr |
| 0x090/4 | QueueDriverLow/High | W | available ring addr |
| 0x0a0/4 | QueueDeviceLow/High | W | used ring addr |
| 0x0fc | ConfigGeneration | R | `0` |
| 0x100 | Config: capacity | R | `capacity_sectors` (u64, little-endian; read as two u32 halves) |

On `QueueNotify` (queue 0, ready): loop `while let Some(chain) = vq.pop_avail(mem)
{ let len = blk.process(&chain, mem); vq.push_used(mem, chain.head, len); }`, then
set `InterruptStatus |= 1` and `irq.set_spi(true)`. On `InterruptACK` clearing the
last bit, `irq.set_spi(false)`.

`BusDevice::read/write` decode the offset and access width (handle the 4-byte
register accesses Linux uses; the config-space `capacity` read may be two 4-byte
reads). Out-of-map offsets log and return 0 / ignore.

### `devices::IrqLine`
```rust
pub trait IrqLine: Send { fn set_spi(&self, level: bool); }
```
Lives in `devices`. The boot harness provides an adapter over `Arc<HvfGicV3>`:
`gic.set_spi(layout::VIRTIO_SPI + 32, level)` (absolute INTID; the `+32`
SPI→INTID offset is the documented gic-smoke finding). This keeps `devices`
decoupled from `hvf`.

### Boot harness (`spike/src/bin/boot.rs`)
After mapping RAM and creating the GIC: open `rootfs.ext4` (`OpenOptions` read +
write), build `VirtioMmio::new(VirtioBlk::new(file), GuestRam::new(host_ptr,
RAM_SIZE, RAM_BASE), Arc::new(GicIrq(gic.clone())))`, register it on the `Bus` at
`[VIRTIO_BASE, VIRTIO_BASE + VIRTIO_SIZE)`, and set `cfg.virtio = Some(...)`. The
GIC must be shared (`Arc<HvfGicV3>`) between the harness and the IRQ adapter; the
serial stays as in 2d. (No code change to the run loop — `QueueNotify` is just
another MMIO write.)

## Data flow (one block read)

guest sets up rings → writes `QueueNotify` → MMIO exit → `Bus` → `VirtioMmio`:
`pop_avail` reads desc chain (header + writable data buf + status) → `VirtioBlk`
reads `file[sector*512 .. +len]` into the data buffer via `GuestRam.write_slice`
→ writes status `OK` → `push_used` records `{head, len}` and bumps `used.idx` →
`InterruptStatus |= 1`, `irq.set_spi(true)` → vCPU re-entry → in-kernel GIC
delivers INTID 33 → kernel reaps used ring, reads ACK → `set_spi(false)`.

## Testing

Pure `cargo test` (no HVF):
- `GuestRam`: read/write round-trip at a guest PA over a backing `Vec`; OOB access
  returns an error / is rejected.
- `Virtqueue`: build a synthetic descriptor table + avail ring in a `Vec`-RAM;
  `pop_avail` returns the right chain (single + multi-descriptor with `NEXT`),
  advances `last_avail_idx`, returns `None` when drained; `push_used` writes the
  used entry and bumps `idx`.
- `VirtioBlk`: a temp file with known contents; an `IN` request copies the right
  sector into a writable buffer; an `OUT` request writes the buffer to the file;
  status byte set to `OK`; `GET_ID` writes a string; an unknown type → `UNSUPP`.
- `VirtioMmio`: register reads (`MagicValue`, `Version`, `DeviceID`,
  `QueueNumMax`, config `capacity`) return the spec values; the status handshake
  latches; a `QueueNotify` with a prepared ring drives one request and pulses a
  fake `IrqLine` (record calls).

Integration (the boot run, like 2d) is the acceptance gate: `target/debug/boot
kimage/out/Image` (no separate initrd; `rootfs.ext4` is the virtio disk) prints
the alpine boot + a shell/login prompt. Run + debugged live in the main session.

## Decomposition (plan tasks)

A. `layout` consts + cmdline + virtio FDT node (arch) — unit-tested.
B. `GuestRam` + `Virtqueue` (devices::virtio) — unit-tested.
C. `VirtioBlk` (devices::virtio) — unit-tested.
D. `VirtioMmio` transport + `IrqLine` (devices::virtio) — unit-tested.
E. boot-harness wiring + the boot run (spike) — build-checked; boot run is the gate.

## Out of scope (→ next milestone)

Serial RX / interactive input, channel-based blocking WFI parking, multiple
virtqueues, indirect descriptors, virtio feature bits beyond `VERSION_1`,
virtio-mmio legacy (v1), write barriers beyond ordering the used-ring `idx` store
last, and any virtio device other than block.

## Risks (live-debug)

- **Handshake exactness:** the device-status / features negotiation must be
  correct or the driver aborts before any queue setup. The `Status` reset (write
  0) must reset queue state.
- **Used-ring ordering:** the used-ring entries must be written before `used.idx`
  is bumped, or the guest may reap a stale entry.
- **`GET_ID` / `FLUSH`:** alpine init may issue these; unhandled types must return
  `UNSUPP` (status 2), not hang.
- **`rootwait` / probe order:** the cmdline uses `rootwait` so a late virtio probe
  doesn't panic; confirm `/dev/vda` appears.
- **ext4 read-write:** the kernel may replay the journal on mount; the device must
  honor `OUT` writes back to the file or mount read-only fails. (`ro` fallback:
  change the cmdline to `root=/dev/vda ro` if RW mount misbehaves.)

## References

- `libkrun/src/devices/src/virtio/{mmio.rs,queue.rs,block/}` — register map, ring,
  blk (we lift the layout, not the threaded model)
- virtio 1.0 spec — MMIO transport (§4.2.2), split virtqueue (§2.6), block (§5.2)
- `firecracker/src/vmm/src/arch/aarch64/fdt.rs::create_virtio_node` — the FDT node
- `docs/2d-boot-result.md` — the boot this milestone continues
