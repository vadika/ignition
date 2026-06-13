# Whole Device-Model Snapshot/Restore — Design

Date: 2026-06-13

## Goal

Make snapshot/restore cover the **whole device model** with two properties:

1. **Full per-device state fidelity** — every device round-trips its complete
   state, not just the virtio transport+queue registers. Closes the balloon-target
   and vsock-connection gaps.
2. **Registry-driven, single-wiring restore** — one device-spec list is the source
   of truth that *both* fresh boot and restore consume. The hand-coded
   `match rec.id` in `boot.rs` is removed entirely.

## Background — what exists today

- Self-describing snapshot v2: `Vec<DeviceRecord>` (`id`, `base`, `size`, `spi`,
  `fdt_kind`, `state: serde_json::Value`).
- Generic replay: `DeviceManager::add_restored(rec, build)` places at the saved
  base/SPI and applies `rec.state` via `MmioDevice::restore`.
- Per-device `save`/`restore` for serial (`SerialSnapshot`), RTC (`Pl031`), and
  virtio **transport+queue** state (`VirtioMmioState`: status, queue_sel,
  features_sel, interrupt_status, per-queue desc/avail/used + last_avail/used).

### Gaps closed by this work

- `VirtioMmio::save_state` captures only the transport — `self.dev` (the inner
  `VirtioDevice`: balloon target, vsock connections) is lost.
- The restore path in `spike/src/bin/boot.rs` is a hand-coded `match rec.id`; every
  new device must be edited in two places (boot setup + restore arm).
- vsock restored as an empty device; balloon target not persisted.

## Architecture

A single `Vec<DeviceSpec>` is the source of truth for the device set. Both paths
iterate it; the only difference is fresh allocation vs. saved-resource replay.

```rust
struct DeviceSpec {
    id: &'static str,
    window: u64,
    build: Box<dyn FnMut(Arc<dyn IrqLine>, &mut DeviceContext) -> Arc<Mutex<dyn MmioDevice>>>,
}

struct DeviceContext {
    // inputs
    disk: PathBuf,
    vsock_uds: Option<PathBuf>,
    // outputs — filled in during build, read after the loop
    serial: Option<Arc<Mutex<Serial<FlushWriter>>>>,
    balloon_target: Option<Arc<AtomicU32>>,
}
```

- **`device_specs(...)`** builds the conditional list: serial / rng / rtc / blk /
  balloon always; vsock iff `--vsock-uds`. net is excluded (snapshots are blocked
  under `--net`). Each builder constructs its device, stashes any typed sub-handle
  it owns into `ctx`, and returns the `dyn MmioDevice` upcast.
- **Fresh boot:** for each spec, `mgr.add(spec.window, |irq| (spec.build)(irq, &mut ctx))`
  — allocates fresh windows/SPIs.
- **Restore:** for each record, look up the spec by `rec.id`, then
  `mgr.add_restored(rec, |irq| (spec.build)(irq, &mut ctx))` — replays the saved
  base/SPI; `add_restored` applies `rec.state` via `MmioDevice::restore`. Unknown
  id → `DeviceMgrError::UnknownDeviceId`.
- After either loop, `ctx.serial` / `ctx.balloon_target` hold the handles the stdin
  reader thread and the Ctrl-A b handler need.
- **`boot_timer` stays special** — registered via `add_fixed` (no record, no FDT
  node, no snapshot state), outside the spec loop.

### The typed-handle wrinkle

Two devices need more than the `dyn MmioDevice` handle after construction:

- **serial** — the stdin reader thread calls `Serial::enqueue` (RX injection),
  which needs the concrete `Arc<Mutex<Serial<FlushWriter>>>`.
- **balloon** — the Ctrl-A b handler bumps the shared `Arc<AtomicU32>` target.

Resolved by the `DeviceContext` output fields: each builder stashes a clone of its
typed sub-handle into `ctx` during build, and returns the `dyn` upcast. The build
closures live in `boot.rs` where the concrete types are known, so this is type-safe.

## Per-device state fidelity

Inner device state rides through the existing `MmioDevice::save/restore` by
extending the inner virtio trait:

```rust
trait VirtioDevice {
    // ... existing methods ...
    fn save(&self) -> serde_json::Value { serde_json::Value::Null } // default: stateless
    fn restore(&mut self, _v: &serde_json::Value) -> Result<(), String> { Ok(()) }
}
```

- `VirtioMmioState` gains one field: `dev: serde_json::Value`. `save_state` fills it
  with `self.dev.save()`; `restore_state` calls `self.dev.restore(&s.dev)` after the
  transport+queue restore. A parse/restore failure surfaces as
  `DeviceMgrError::StateInvalid { id, reason }`.
- **rng / blk / net:** default no-op — truly stateless (the disk image is copied
  into the snapshot dir; entropy needs nothing).
- **balloon:** `save → {num_pages, actual}`; `restore` writes both (sets the shared
  `AtomicU32` target and `actual`). The inflated pages themselves are already in
  `memory.bin`; only the driver-facing config must match so the guest driver and the
  device agree after resume.

### vsock — reset + RST

Host UDS peers do not survive a snapshot, so connections are reset honestly (this is
effectively what Firecracker does):

- `save → { conns: [(local_port, peer_port), ...] }` — just the open connection
  keys, no socket fds.
- `restore` opens **no** host sockets. It seeds a `pending_rst: Vec<(u32, u32)>` in
  the muxer. On the first `service` / RX poll after resume, the muxer pushes one RST
  packet per key onto the guest RX queue. The guest's `AF_VSOCK` sockets receive
  `ECONNRESET` and tear down cleanly.

## Data flow

- **Snapshot:** `FrozenDevices::save()` → per-record `dev.save()` (virtio now
  includes transport + `self.dev.save()`; vsock lists open conns) →
  `write_snapshot`.
- **Restore:** `read_snapshot` → `restore_all` (spec lookup + `add_restored` per
  record) → per device build default + `MmioDevice::restore(state)` (virtio applies
  transport + inner `dev.restore`; vsock seeds pending RSTs) → `gic_restore` after
  the vCPU exists → vCPU restore. Unchanged from today otherwise.

## Error handling

- Unknown device id in records → `UnknownDeviceId` (exists).
- Queue-count mismatch → `StateInvalid` (exists).
- Inner-device state parse/restore error → `StateInvalid { id, reason }` (new path,
  existing variant).
- Missing required handle after `restore_all` (e.g. no serial) → `io::Error` (exists).

## Testing

- **Unit:**
  - balloon `save`/`restore` roundtrip (num_pages + actual).
  - `VirtioMmioState` with a non-null `dev` blob serde-roundtrips.
  - vsock `save` lists open conns; `restore` seeds a matching `pending_rst`.
  - vsock pushes an RST packet on the first service after restore.
  - `device_specs(flags)` yields the expected id set per flag combo (with/without
    `vsock_uds`).
- **Integration:** build specs → place all → snapshot records → `restore_all` from
  those records reconstructs the same id/base/spi set, with no `match rec.id`.
- **Live (existing harness):** `scripts/restore_test.py` stays green; inflate balloon
  to N before snapshot → after restore the balloon config reports the same
  num_pages/actual; a vsock conn open at snapshot → the guest sees a connection reset
  after restore.

## Scope / YAGNI

- net excluded (snapshot already blocked under `--net`).
- `boot_timer` stays an `add_fixed` special case (no state).
- No disk dirty-block tracking — full copy as today.
- vsock full reconnect explicitly out of scope.
- Multi-vCPU snapshot still out of scope.

## Status: implemented 2026-06-13

Delivered via plan `docs/superpowers/plans/2026-06-13-device-model-snapshot.md`.
The `Vec<DeviceSpec>` was realized as `setup_devices(mode)` + a generic `place()`
helper (see the plan's realization note). All devices round-trip full state; the
restore-time `match rec.id` is gone. 119 workspace tests pass, clippy clean, live
restore verified (idle ~0% CPU, responsive).
