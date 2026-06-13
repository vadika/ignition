# virtio-balloon — Design

Date: 2026-06-13. Status: approved design, ready for an implementation plan.

## Context

Sub-project **D** of the "full device model" milestone (A = DeviceManager framework,
B = virtio-rng, C = RTC PL031 — all merged). This adds a memory balloon: the host
can reclaim guest RAM on demand by raising a target; the guest inflates (hands pages
back), and the host releases that physical memory.

### Existing pieces this builds on

- `devices::virtio::mmio::VirtioDevice` trait (`device_id`, `device_features`,
  `config_read`, `queue_count`, `handle_notify`) and the `VirtioMmio` transport
  (implements `MmioDevice`; holds the `IrqLine`; `interrupt_status` with
  `INT_STATUS_USED = 1`).
- `Virtqueue` (`pop_avail`, `push_used`), `DescChain`/`Desc`, `GuestRam`
  (`read_slice`, holds the host `ptr` + guest `base`).
- `DeviceManager::add`/`add_restored` (return the typed `Arc<Mutex<D>>`), the
  always-on wiring pattern from rng/rtc, and the `Ctrl-A` escape state machine in
  `spike/src/bin/boot.rs` (`Ctrl-A s` snapshot, `Ctrl-A x` quit).
- `libc` is a `devices` dependency (added for rng).

## Goal

A working virtio-balloon: `Ctrl-A b` toggles a 64 MiB reclaim target; the guest
inflates; the host returns that physical RAM via `madvise(MADV_FREE_REUSABLE)`;
`Ctrl-A b` again deflates. Demonstrable by a drop in the host process RSS.

Non-goals (explicit TODOs): the stats queue (`VIRTIO_BALLOON_F_STATS_VQ`),
free-page-hint/OOM features, persisting the target across snapshot, `hv_vm_unmap`
as an alternative reclaim path, and a non-console control API (a REST/IPC handle is
future work — the console toggle is the v1 trigger).

## Architecture

### `crates/devices/src/virtio/balloon.rs` (new) — `Balloon`

```rust
pub struct Balloon {
    /// Host's reclaim target in 4 KiB pages. Shared so the host trigger can update
    /// it without going through the VirtioDevice trait. Guest reads it as
    /// config.num_pages.
    num_pages: Arc<AtomicU32>,
    /// Pages the guest reports it has inflated (config.actual); guest-written.
    actual: u32,
}
```

- `Balloon::new() -> (Self, Arc<AtomicU32>)` — returns the device and a clone of the
  shared `num_pages` target for the host trigger to drive. (Both share one
  `Arc<AtomicU32>`.)
- `device_id() -> 5` (VIRTIO_ID_BALLOON).
- `device_features(_) -> 0` (no feature bits; the transport adds
  VIRTIO_F_VERSION_1).
- `queue_count() -> 2` — queue 0 = inflateq, queue 1 = deflateq.
- `config_read(offset, data)` — serve `virtio_balloon_config`: `0x00` = `num_pages`
  (`self.num_pages.load(Relaxed)`), `0x04` = `actual`. Assemble the requested bytes
  little-endian for any width (mirror the byte-addressable pattern virtio-net uses);
  out-of-range config offsets read 0.
- A config *write* path: the transport currently has no `config_write`. The guest
  writes `config.actual`. **Add `fn config_write(&mut self, offset: u64, data:
  &[u8]) {}`** to the `VirtioDevice` trait (default no-op so blk/net/rng aren't
  touched) and have the transport route config-space writes (offset ≥ 0x100) to it.
  `Balloon::config_write` stores `actual` from a 4-byte write at offset 0x04.
- `handle_notify(queue_idx, vq, mem)`:
  - queue 0 (inflate): `while let Some(chain) = vq.pop_avail(mem)` → the chain's
    readable descriptors hold a packed array of little-endian `u32` PFNs. For each
    PFN: `addr = (pfn as u64) << 12`; `mem.madvise_free(addr, 4096)` (ignore a false
    return — a bad PFN is skipped). `push_used(chain.head, 0)` (balloon used buffers
    carry no payload length). Return `true` if any chain serviced.
  - queue 1 (deflate): drain chains, `push_used(head, 0)` each; no page action (a
    `MADV_FREE_REUSABLE` page re-faults to zero on the guest's next touch). Return
    `true` if serviced.

PFNs are read from guest memory at the descriptor addr/len: each descriptor covers
`len / 4` `u32` PFNs; read them with `mem.read_slice` into a `[u8; 4]` per PFN (or a
bulk read + chunk). Cap a single chain's PFN count defensively at `len / 4`.

### `crates/devices/src/virtio/guest_ram.rs` — `madvise_free`

```rust
/// Return the host physical pages backing `[gpa, gpa+len)` to the OS
/// (MADV_FREE_REUSABLE). The HVF mapping stays valid; the guest re-faults to a
/// zero page on next access. Returns false if the range is outside guest RAM.
pub fn madvise_free(&self, gpa: u64, len: usize) -> bool {
    let off = match gpa.checked_sub(self.base) { Some(o) => o as usize, None => return false };
    if off + len > self.len { return false; }
    let ret = unsafe { libc::madvise(self.ptr.add(off) as *mut libc::c_void, len, libc::MADV_FREE_REUSABLE) };
    ret == 0
}
```

(`MADV_FREE_REUSABLE` is the macOS flag that actually returns anonymous pages to the
OS; `MADV_DONTNEED` is a no-op for anon memory on macOS.)

### `crates/devices/src/virtio/mmio.rs` — config-change interrupt + config write

- Add `const INT_STATUS_CONFIG: u32 = 2;`.
- Add `pub fn signal_config_change(&mut self) { self.interrupt_status |=
  INT_STATUS_CONFIG; self.irq.set_spi(true); }`. (The existing InterruptACK write
  path already clears bits and deasserts when `interrupt_status == 0`, so config-change
  acks work with no further change.)
- Route guest writes to config space (MMIO offset ≥ 0x100) to a new
  `VirtioDevice::config_write(offset - 0x100, data)`. Add `config_write` to the
  `VirtioDevice` trait with a default empty body (blk/net/rng/rtc unaffected).

### `vmm::device_manager` — no change

Balloon is a `VirtioMmio` device, so `FdtKind::VirtioMmio` and the existing mapping
already cover it.

## Wiring (`spike/src/bin/boot.rs`)

Always-on (idle at target 0 = no-op). After the other virtio devices:

```rust
let (balloon, balloon_target) = Balloon::new();
let balloon_mmio = mgr.add(layout::MMIO_WINDOW, move |irq| {
    VirtioMmio::new("virtio-balloon", Box::new(balloon), guest_ram_balloon, irq)
}).expect("add balloon");
// keep `balloon_target: Arc<AtomicU32>` and `balloon_mmio: Arc<Mutex<VirtioMmio>>`
```

Escape state machine: add `Ctrl-A b` → an `Action::Balloon`. The reader thread, on
`Balloon`, toggles:

```rust
const BALLOON_PAGES: u32 = 64 * 256; // 64 MiB in 4 KiB pages
let now = balloon_target.load(Relaxed);
let next = if now == 0 { BALLOON_PAGES } else { 0 };
balloon_target.store(next, Relaxed);
balloon_mmio.lock().unwrap().signal_config_change();
eprintln!("[balloon target -> {} MiB]", next / 256);
```

Restore: a `"virtio-balloon"` arm builds a fresh `Balloon` (target 0) via
`add_restored`; the restored guest starts deflated. (Persisting the target is a
documented TODO.)

## Snapshot

The transport's queue state is snapshotted as for any `VirtioMmio` device. The
balloon's own state (`num_pages` target, `actual`) is **not** persisted in v1 — a
restored balloon starts at target 0. No snapshot-version bump. (TODO: persist the
target so a restored guest re-inflates.)

## Error handling

- Malformed/empty inflate or deflate chain → `push_used(head, 0)`, no page action.
- `madvise_free` on an out-of-range PFN → returns false, that PFN skipped, logged at
  debug. Never aborts.
- Config reads/writes outside the 8-byte config → read 0 / ignored.

## Testing

Unit (in `balloon.rs`; build a `GuestRam` over a `Vec` + a `Virtqueue` with
hand-written descriptors, mirroring `rng.rs`/`queue.rs` tests):
1. **Inflate services the queue.** A chain whose readable descriptor holds two
   `u32` PFNs → `handle_notify(0, ..)` returns `true` and `push_used` records the
   chain head with len 0. (`madvise_free` over the `Vec` backing returns false or is
   a harmless no-op in-test; the assertion is on queue servicing, not the kernel
   effect.)
2. **Deflate services the queue.** `handle_notify(1, ..)` drains a chain and
   `push_used(head, 0)`; returns `true`.
3. **Config read.** With the shared target set to `N`, `config_read(0x00, &mut
   [0;4])` decodes to `N`; `config_read(0x04, ..)` decodes `actual`.
4. **Config write.** `config_write(0x04, &v.to_le_bytes())` then `config_read(0x04,
   ..)` returns `v` (guest-reported `actual` round-trips).
5. **Identity.** `device_id()==5`, `queue_count()==2`, `device_features(0)==0`.
6. **`madvise_free` bounds.** In `guest_ram.rs`, an out-of-range `gpa` returns false;
   an in-range call over a real `libc::mmap`'d region returns true (small mmap test,
   or assert the false/bounds path only if mmap-in-test is awkward).
7. **Transport config-change** (in `mmio.rs`): after `signal_config_change()`, a read
   of InterruptStatus (0x060) has bit 1 set and the irq line was asserted (mock
   `IrqLine` records the `set_spi(true)`).

Live (drivers + interactive): boot; `ps -o rss= -p <pid>` baseline; `Ctrl-A b`; after
a few seconds the guest has inflated and RSS dropped by roughly 64 MiB; `Ctrl-A b`
again deflates; `scripts/restore_test.py` / `restore_clone_test.py` still pass.

## File structure

- Create `crates/devices/src/virtio/balloon.rs` (device + tests).
- Modify `crates/devices/src/virtio/mod.rs` — `pub mod balloon;`.
- Modify `crates/devices/src/virtio/guest_ram.rs` — `madvise_free` (+ test).
- Modify `crates/devices/src/virtio/mmio.rs` — `INT_STATUS_CONFIG`,
  `signal_config_change`, `VirtioDevice::config_write` (default no-op) + config-write
  routing (+ test).
- Modify `spike/src/bin/boot.rs` — `Action::Balloon` + `Ctrl-A b`, always-on wiring,
  restore arm.

End state: the host can reclaim ~64 MiB of guest RAM on demand via `Ctrl-A b` and
give it back; reclaim is real (host RSS drops); snapshot/restore/clone unaffected.
