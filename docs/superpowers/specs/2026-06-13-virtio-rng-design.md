# virtio-rng — Design

Date: 2026-06-13. Status: approved design, ready for an implementation plan.

## Context

Sub-project **B** of the "full device model" milestone. Sub-project A (the
`DeviceManager` framework + `MmioDevice` trait) is merged. This adds the first new
device on that framework — the smallest one — which both delivers an entropy source
to the guest and validates the framework end to end (allocation → bus → FDT →
snapshot all driven by one `add` call).

### Existing pieces this builds on

- `devices::virtio::mmio::VirtioDevice` trait: `device_id`, `device_features(sel)`,
  `config_read(offset, &mut [u8])`, `queue_count`, `handle_notify(queue_idx, &mut
  Virtqueue, &GuestRam) -> bool`, and a default `inject_rx` (not used here).
- `VirtioMmio` transport already implements `MmioDevice` (FDT kind `VirtioMmio`,
  snapshot via the transport's queue state). `VirtioMmio::new(id, dev, mem, irq)`.
- `Virtqueue`: `pop_avail(mem) -> Option<DescChain>`, `push_used(mem, head, len)`.
  `DescChain { head: u16, descriptors: Vec<Desc> }`. `Desc { addr: u64, len: u32,
  writable: bool }`.
- `GuestRam::write_slice(gpa, &[u8]) -> bool`.
- `DeviceManager::add` / `add_restored` (boot harness wiring).
- The project already links `libc`.

## Goal

A working virtio-rng device that fills guest-posted buffers with host entropy,
wired always-on through the `DeviceManager`, requiring no changes to `layout`, the
FDT generator, or the snapshot format. Adding it should be: one new device file +
one `add` call (fresh) + one restore arm.

Non-goals: a configurable/seeded RNG, leak-rate limiting, the
`VIRTIO_RNG_F_*` feature bits (none defined in the base spec), multi-queue.

## Architecture

### `crates/devices/src/virtio/rng.rs` (new) — `VirtioRng`

`VirtioRng` is a unit-like struct (no fields; stateless) implementing `VirtioDevice`:

- `device_id() -> u32 { 4 }` — VIRTIO_ID_RNG.
- `device_features(_sel: u32) -> u32 { 0 }` — rng defines no device feature bits;
  the transport adds VIRTIO_F_VERSION_1 itself.
- `config_read(_offset, _data)` — no-op; rng has no config space. (If the guest ever
  reads, it gets the transport's zero-fill default — acceptable, never happens.)
- `queue_count() -> usize { 1 }` — a single device-writable request queue.
- `handle_notify(_queue_idx, vq, mem) -> bool`:
  ```
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
  ```
  Non-writable descriptors are skipped (rng's queue carries only device-writable
  buffers). An empty/all-read-only chain yields `push_used(head, 0)`.

`VirtioRng::new() -> Self`.

### Entropy: `fill_random(buf: &mut [u8])`

Private free function in `rng.rs`. Fills `buf` from the OS CSPRNG via
`libc::getentropy`, which accepts at most 256 bytes per call:

```
for chunk in buf.chunks_mut(256) {
    let ret = unsafe { libc::getentropy(chunk.as_mut_ptr() as *mut c_void, chunk.len()) };
    assert_eq!(ret, 0, "getentropy failed: {}", std::io::Error::last_os_error());
}
```

`getentropy` with a valid pointer and `len <= 256` cannot fail in normal operation;
a nonzero return is a programming/environment error and panics rather than silently
producing a weak/zero "random" buffer.

## Wiring (`spike/src/bin/boot.rs`)

Always-on, added after the serial device on both paths:

- **Fresh boot:** after the serial `add`, add
  `mgr.add(layout::MMIO_WINDOW, |irq| VirtioMmio::new("virtio-rng", Box::new(VirtioRng::new()), guest_ram_rng, irq))`
  where `guest_ram_rng` is a fresh `GuestRam` view of the host RAM mapping (same
  pattern as blk/net). Order relative to blk/net does not matter (serial must stay
  first for the earlycon address); rng goes right after serial.
- **Restore (`run_restore`):** a `"virtio-rng"` match arm building a fresh
  `VirtioRng` via `add_restored` (mirrors the blk arm but with no disk).

The FDT `virtio,mmio` node, the MMIO window, and the SPI are all produced by the
`DeviceManager` — no `layout` constants, no `fdt.rs` changes.

## Snapshot

`VirtioRng` carries no device-specific state. The transport's queue state
(`VirtioMmioState`) is the only thing to capture, and `VirtioMmio`'s existing
`MmioDevice::save/restore` already covers it. The device's `snapshot_id()` is
`"virtio-rng"` (set at `VirtioMmio::new`).

**No snapshot-version bump.** The v2 snapshot is a self-describing device-record
list, so:
- a snapshot taken before rng existed has no `"virtio-rng"` record and restores
  serial+blk only — consistent with how that guest actually ran;
- a snapshot taken with rng carries the record and restores it.

This is precisely the drop-in property the framework was designed for; bumping the
version on every new device would defeat it.

## Error handling

- Malformed or all-read-only descriptor chain → `push_used(head, 0)`; the guest's
  driver treats a zero-length used buffer as "no entropy this round" and retries.
- A `write_slice` that fails (descriptor addr outside guest RAM) → that descriptor
  contributes 0 to `written`; the chain is still completed with whatever succeeded.
- `getentropy` nonzero return → panic (see Entropy).

## Testing

Unit tests in `rng.rs` (no hypervisor entitlement; build a `GuestRam` over a
`Vec<u8>` backing and a `Virtqueue` with hand-written descriptors, mirroring the
existing `blk.rs` / `queue.rs` test scaffolding):

1. **Fills a writable descriptor.** Backing pre-filled with a `0xAA` sentinel; an
   avail chain with one writable 64-byte descriptor. After `handle_notify`: returns
   `true`; the used ring records `len == 64`; the 64-byte region differs from the
   all-`0xAA` sentinel in at least one byte (false-failure probability ~2⁻⁶⁴).
2. **Read-only chain fills nothing.** A chain whose only descriptor is non-writable
   → `handle_notify` completes it with `push_used(head, 0)` and writes no bytes.
3. **Multi-descriptor chain.** Two writable descriptors (e.g. 16 + 32 bytes) → both
   regions filled; used `len == 48`.
4. **Identity.** `device_id() == 4`, `queue_count() == 1`, `device_features(0) == 0`.

Live regression (drivers, not `cargo test`): after wiring, `scripts/restore_test.py`
and `scripts/restore_clone_test.py` still pass, and the booted guest shows the
kernel bound the device — `cat /sys/class/misc/hw_random/rng_current` reads
`virtio_rng.0` (and/or `dmesg | grep hw_random` shows it registered). A quick
interactive confirmation that the device works end to end.

## File structure

- Create `crates/devices/src/virtio/rng.rs` (device + entropy helper + tests).
- Modify `crates/devices/src/virtio/mod.rs` — `pub mod rng;`.
- Modify `spike/src/bin/boot.rs` — add rng on the fresh-boot and restore paths.

End state: virtio-rng present on every boot, fed by `getentropy`, snapshot/restore
and clone unaffected; the framework's drop-in claim demonstrated by the small diff.
