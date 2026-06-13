# Whole Device-Model Snapshot/Restore Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make snapshot/restore cover the whole device model — every device round-trips its full state (incl. balloon target and vsock connection set), and both fresh boot and restore drive a single device-wiring site with no `match rec.id`.

**Architecture:** Inner-device state rides through the existing `MmioDevice::save/restore` path by extending the `VirtioDevice` trait with default-no-op `save`/`restore`; `VirtioMmioState` gains a `dev: Value` blob. balloon persists `{num_pages, actual}`; vsock persists open connection keys and, on restore, RSTs the guest for each (host UDS peers are gone). `boot.rs` is refactored so a single `setup_devices(mode)` lists every device once; a generic `place()` helper picks `add` (boot) vs `add_restored` (restore, by id). `DeviceManager` is unchanged.

**Tech Stack:** Rust (edition 2024), serde / serde_json, the existing `devices` + `vmm` crates and the `hvf-spike` `boot` binary.

---

## Spec realization note

The spec (`docs/superpowers/specs/2026-06-13-device-model-snapshot-design.md`) sketched a `Vec<DeviceSpec>` of boxed builder closures. The literal form is awkward in Rust because builders are `FnOnce`, can fail (disk open), must stash typed handles, and differ per path in their disk source. This plan delivers the identical outcome — one wiring site consumed by both paths, no `match rec.id`, adding a device = one call — via a `setup_devices(mode)` function plus a generic `place()` helper. Same goal, idiomatic shape.

## File structure

- `crates/devices/src/virtio/mmio.rs` — `VirtioDevice` gains `save`/`restore` (default no-op); `VirtioMmioState` gains `dev: Value`; `save_state`/`MmioDevice::restore` carry the inner blob. (Task 1)
- `crates/devices/src/virtio/balloon.rs` — `Balloon::{save,restore}` for `{num_pages, actual}`. (Task 2)
- `crates/devices/src/virtio/vsock/muxer.rs` — `Muxer` gains `pending_rst`, `save_conns()`, `seed_rst()`, and a flush at the top of `service()`. (Task 3)
- `crates/devices/src/virtio/vsock/mod.rs` — `VsockDevice::{save,restore}` delegating to the muxer. (Task 3)
- `spike/src/bin/boot.rs` — `DeviceContext`, `Mode`, `place()`, `setup_devices()`, `check_known_ids()`; fresh-boot wiring and `run_restore` both call `setup_devices`; the `match rec.id` is deleted. (Tasks 4a, 4b)
- `README.md` + spec TODO notes — vsock/balloon snapshot behavior updated. (Task 5)

---

## Task 1: Inner-device state through VirtioMmio

**Files:**
- Modify: `crates/devices/src/virtio/mmio.rs`
- Test: same file's `#[cfg(test)] mod tests`

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block at the bottom of `crates/devices/src/virtio/mmio.rs`:

```rust
    /// A stateful mock VirtioDevice: its save/restore carries a single counter,
    /// proving the transport round-trips inner device state.
    struct StatefulMock {
        counter: u32,
    }
    impl VirtioDevice for StatefulMock {
        fn device_id(&self) -> u32 { 0xABCD }
        fn device_features(&self, _sel: u32) -> u32 { 0 }
        fn config_read(&self, _offset: u64, data: &mut [u8]) { data.fill(0); }
        fn queue_count(&self) -> usize { 1 }
        fn handle_notify(&mut self, _q: usize, _vq: &mut Virtqueue, _mem: &GuestRam) -> bool { false }
        fn save(&self) -> serde_json::Value { serde_json::json!({ "counter": self.counter }) }
        fn restore(&mut self, v: &serde_json::Value) -> Result<(), String> {
            self.counter = v.get("counter").and_then(|c| c.as_u64()).ok_or("missing counter")? as u32;
            Ok(())
        }
    }

    #[test]
    fn virtio_mmio_roundtrips_inner_device_state() {
        let mem = GuestRam::new(std::ptr::null_mut(), 0, 0);
        let irq: Arc<dyn IrqLine> = Arc::new(crate::virtio::NoopIrq);
        let mut a = VirtioMmio::new("mock", Box::new(StatefulMock { counter: 7 }), mem, irq.clone());
        let saved = a.save();

        let mem2 = GuestRam::new(std::ptr::null_mut(), 0, 0);
        let mut b = VirtioMmio::new("mock", Box::new(StatefulMock { counter: 0 }), mem2, irq);
        b.restore(&saved).expect("restore ok");
        assert_eq!(b.save(), saved, "inner device counter must survive save/restore");
    }

    #[test]
    fn virtio_mmio_state_without_dev_field_deserializes() {
        // Old (pre-inner-state) snapshots have no `dev` key; serde default fills Null.
        let json = serde_json::json!({
            "status": 0, "queue_sel": 0, "device_features_sel": 0,
            "interrupt_status": 0, "queues": []
        });
        let s: VirtioMmioState = serde_json::from_value(json).expect("deserializes with default dev");
        assert_eq!(s.dev, serde_json::Value::Null);
    }
```

Note `GuestRam::new(null, 0, 0)` is only valid because the mock never touches memory. If `GuestRam::new` rejects a null pointer, use a small `vec![0u8; 16]` backing as in the balloon tests.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ignition-devices virtio_mmio_roundtrips_inner_device_state virtio_mmio_state_without_dev_field`
Expected: FAIL — `VirtioDevice` has no `save`/`restore`; `VirtioMmioState` has no `dev` field.

- [ ] **Step 3: Add trait defaults**

In `crates/devices/src/virtio/mmio.rs`, add to the `VirtioDevice` trait (after `vsock_poll_set`, before the closing `}` at line ~57):

```rust
    /// Serialize device-specific state for a snapshot. Default: stateless (Null).
    fn save(&self) -> serde_json::Value {
        serde_json::Value::Null
    }
    /// Apply restored device-specific state. Default: stateless (no-op).
    fn restore(&mut self, _v: &serde_json::Value) -> Result<(), String> {
        Ok(())
    }
```

- [ ] **Step 4: Add the `dev` field to `VirtioMmioState`**

Change the `VirtioMmioState` struct (lines ~89-97) to:

```rust
/// Serializable snapshot of the full virtio-mmio transport state (registers + queues + inner device).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtioMmioState {
    pub status: u32,
    pub queue_sel: u32,
    pub device_features_sel: u32,
    pub interrupt_status: u32,
    pub queues: Vec<QueueSnapshot>,
    /// Inner `VirtioDevice` state blob (Null for stateless devices). `serde(default)`
    /// keeps pre-inner-state snapshots loadable.
    #[serde(default)]
    pub dev: serde_json::Value,
}
```

- [ ] **Step 5: Carry the blob in `save_state` and `MmioDevice::restore`**

In `save_state` (lines ~294-321), add `dev: self.dev.save(),` to the returned `VirtioMmioState { ... }` literal.

In `MmioDevice::restore` (lines ~382-393), after `self.restore_state(&s);` and before `Ok(())`, add:

```rust
        self.dev.restore(&s.dev).map_err(|reason| DeviceMgrError::StateInvalid {
            id: self.id.into(),
            reason,
        })?;
```

(`restore_state` itself stays transport-only and infallible.)

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test -p ignition-devices`
Expected: PASS — all device tests green, including the two new ones.

- [ ] **Step 7: Commit**

```bash
git add crates/devices/src/virtio/mmio.rs
git commit -m "devices: carry inner virtio device state through snapshot transport"
```

---

## Task 2: Balloon save/restore

**Files:**
- Modify: `crates/devices/src/virtio/balloon.rs`
- Test: same file's `#[cfg(test)] mod tests`

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `crates/devices/src/virtio/balloon.rs`:

```rust
    #[test]
    fn save_restore_roundtrips_target_and_actual() {
        let (mut b, t) = Balloon::new();
        t.store(64 * 256, Ordering::Relaxed); // host target = 64 MiB in pages
        b.config_write(0x04, &(40 * 256u32).to_le_bytes()); // guest reported actual
        let saved = b.save();

        let (mut b2, t2) = Balloon::new();
        b2.restore(&saved).expect("restore ok");
        assert_eq!(t2.load(Ordering::Relaxed), 64 * 256, "shared target restored");
        let mut d = [0u8; 8];
        b2.config_read(0x00, &mut d);
        assert_eq!(u32::from_le_bytes(d[0..4].try_into().unwrap()), 64 * 256);
        assert_eq!(u32::from_le_bytes(d[4..8].try_into().unwrap()), 40 * 256);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ignition-devices save_restore_roundtrips_target_and_actual`
Expected: FAIL — `Balloon` has no `save`/`restore` overrides (the default no-op leaves target/actual at 0).

- [ ] **Step 3: Implement save/restore**

In `crates/devices/src/virtio/balloon.rs`, add these two methods inside `impl VirtioDevice for Balloon { ... }` (after `handle_notify`, before the closing `}`):

```rust
    fn save(&self) -> serde_json::Value {
        serde_json::json!({
            "num_pages": self.num_pages.load(Ordering::Acquire),
            "actual": self.actual,
        })
    }
    fn restore(&mut self, v: &serde_json::Value) -> Result<(), String> {
        let num_pages = v.get("num_pages").and_then(|x| x.as_u64())
            .ok_or("balloon: missing num_pages")? as u32;
        let actual = v.get("actual").and_then(|x| x.as_u64())
            .ok_or("balloon: missing actual")? as u32;
        // Release pairs with the device's Acquire load in config_bytes().
        self.num_pages.store(num_pages, Ordering::Release);
        self.actual = actual;
        Ok(())
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ignition-devices balloon`
Expected: PASS — the new roundtrip test and existing balloon tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/devices/src/virtio/balloon.rs
git commit -m "devices: snapshot balloon target + actual"
```

---

## Task 3: vsock connection reset + RST

**Files:**
- Modify: `crates/devices/src/virtio/vsock/muxer.rs`
- Modify: `crates/devices/src/virtio/vsock/mod.rs`
- Test: both files' `#[cfg(test)] mod tests`

- [ ] **Step 1: Write the failing muxer test**

Add to `mod tests` in `crates/devices/src/virtio/vsock/muxer.rs`:

```rust
    #[test]
    fn seed_rst_emits_one_rst_per_conn_on_service() {
        let base = std::env::temp_dir().join("ign-vsock-rst/vsock");
        let mut mux = Muxer::new(base);
        mux.seed_rst(vec![(1024, 5000), (2048, 6000)]);
        mux.service();
        let mut ops = Vec::new();
        while let Some(pkt) = mux.pop_rx() {
            ops.push((pkt.hdr.op, pkt.hdr.dst_port, pkt.hdr.src_port));
        }
        assert_eq!(ops.len(), 2, "one RST per seeded connection");
        assert!(ops.iter().all(|&(op, _, _)| op == OP_RST));
        assert!(ops.contains(&(OP_RST, 1024, 5000)));
        assert!(ops.contains(&(OP_RST, 2048, 6000)));
        // Idempotent: a second service() pass emits nothing further.
        mux.service();
        assert!(mux.pop_rx().is_none());
    }

    #[test]
    fn save_conns_lists_open_connection_keys() {
        let dir = std::env::temp_dir().join(format!("ign-vsock-save-{}", std::process::id()));
        let base = dir.join("vsock");
        std::fs::create_dir_all(&dir).unwrap();
        let _l = UnixListener::bind(base.with_file_name("vsock_5000")).unwrap();
        let mut mux = Muxer::new(base);
        mux.handle_tx(&req(1024, 5000), &[]);
        assert_eq!(mux.save_conns(), vec![(1024, 5000)]);
        let _ = std::fs::remove_dir_all(&dir);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ignition-devices seed_rst_emits_one_rst save_conns_lists_open`
Expected: FAIL — `Muxer` has no `seed_rst` / `save_conns`.

- [ ] **Step 3: Add `pending_rst`, `seed_rst`, `save_conns`, and the service flush**

In `crates/devices/src/virtio/vsock/muxer.rs`:

Change the struct (lines ~19-23) to:

```rust
pub struct Muxer {
    uds_base: PathBuf,
    conns: HashMap<(u32, u32), Connection>, // (guest_port, host_port)
    rxq: VecDeque<RxPacket>,
    /// Connection keys carried over a snapshot; RST'd to the guest on the first
    /// service() after restore (host UDS peers no longer exist).
    pending_rst: Vec<(u32, u32)>,
}
```

Change `new` (lines ~26-28) to:

```rust
    pub fn new(uds_base: PathBuf) -> Muxer {
        Muxer { uds_base, conns: HashMap::new(), rxq: VecDeque::new(), pending_rst: Vec::new() }
    }
```

Add these methods inside `impl Muxer` (e.g. after `new`):

```rust
    /// Open connection keys for the snapshot (guest_port, host_port).
    pub fn save_conns(&self) -> Vec<(u32, u32)> {
        self.conns.keys().copied().collect()
    }

    /// Seed connections that existed at snapshot time; service() will RST each.
    pub fn seed_rst(&mut self, conns: Vec<(u32, u32)>) {
        self.pending_rst = conns;
    }
```

At the very top of `service` (before `let mut new_rx ...` at line ~116), add:

```rust
        // Post-restore: RST every connection that existed at snapshot time, once.
        for (guest_port, host_port) in self.pending_rst.drain(..) {
            self.rxq.push_back(RxPacket {
                hdr: Self::ctrl_hdr(OP_RST, guest_port, host_port, 0),
                data: Vec::new(),
            });
        }
```

- [ ] **Step 4: Run the muxer tests**

Run: `cargo test -p ignition-devices -- muxer`
Expected: PASS — new + existing muxer tests pass.

- [ ] **Step 5: Write the failing VsockDevice test**

Add to `mod tests` in `crates/devices/src/virtio/vsock/mod.rs`:

```rust
    use crate::virtio::mmio::VirtioDevice as _;

    #[test]
    fn vsock_save_restore_seeds_rst() {
        // A device with no live conns saves an empty list and restores cleanly.
        let dev = VsockDevice::new(PathBuf::from("/tmp/ign-x/vsock"));
        let saved = dev.save();
        assert_eq!(saved, serde_json::json!({ "conns": [] }));

        // Restoring a saved conn list seeds the muxer's pending RSTs.
        let mut dev2 = VsockDevice::new(PathBuf::from("/tmp/ign-x/vsock"));
        dev2.restore(&serde_json::json!({ "conns": [[1024, 5000]] })).expect("restore ok");
        // The seeded RST surfaces on the next service()/RST drain.
        dev2.muxer.service();
        let pkt = dev2.muxer.pop_rx().expect("RST queued for restored conn");
        assert_eq!((pkt.hdr.dst_port, pkt.hdr.src_port), (1024, 5000));
    }
```

(`muxer` is a private field; this test is in the same module so it can read it.)

- [ ] **Step 6: Run test to verify it fails**

Run: `cargo test -p ignition-devices vsock_save_restore_seeds_rst`
Expected: FAIL — `VsockDevice` uses the default no-op `save`/`restore`, so `save()` returns `Null` and no RST is seeded.

- [ ] **Step 7: Implement VsockDevice save/restore**

In `crates/devices/src/virtio/vsock/mod.rs`, add these methods inside `impl VirtioDevice for VsockDevice { ... }` (after `vsock_poll_set`, before the closing `}` at line ~119):

```rust
    fn save(&self) -> serde_json::Value {
        serde_json::json!({ "conns": self.muxer.save_conns() })
    }
    fn restore(&mut self, v: &serde_json::Value) -> Result<(), String> {
        let conns = v.get("conns").and_then(|c| c.as_array())
            .ok_or("vsock: missing conns array")?;
        let keys = conns.iter().map(|pair| {
            let a = pair.as_array().ok_or("vsock: conn not a pair")?;
            let g = a.first().and_then(|x| x.as_u64()).ok_or("vsock: bad guest_port")? as u32;
            let h = a.get(1).and_then(|x| x.as_u64()).ok_or("vsock: bad host_port")? as u32;
            Ok::<(u32, u32), String>((g, h))
        }).collect::<Result<Vec<_>, _>>()?;
        self.muxer.seed_rst(keys);
        Ok(())
    }
```

- [ ] **Step 8: Run the vsock tests**

Run: `cargo test -p ignition-devices -- vsock`
Expected: PASS.

- [ ] **Step 9: Commit**

```bash
git add crates/devices/src/virtio/vsock/muxer.rs crates/devices/src/virtio/vsock/mod.rs
git commit -m "devices: vsock snapshots conn set and RSTs guest on restore"
```

---

## Task 4a: Unified device wiring for fresh boot

**Files:**
- Modify: `spike/src/bin/boot.rs`
- Test: `spike/src/bin/boot.rs` `#[cfg(test)] mod tests`

This task introduces `Mode`, `DeviceContext`, `place()`, `check_known_ids()`, and `setup_devices()`, and switches the **fresh-boot** path to use `setup_devices`. The restore path is switched in Task 4b. Existing behavior must stay identical.

- [ ] **Step 1: Write the failing test for `check_known_ids`**

Add to the existing `#[cfg(test)] mod tests` block in `spike/src/bin/boot.rs`:

```rust
    #[test]
    fn check_known_ids_accepts_known_and_rejects_unknown() {
        use vmm::device_manager::DeviceRecord;
        use devices::device::FdtKind;
        let rec = |id: &str| DeviceRecord {
            id: id.into(), base: 0, size: 0x200, spi: 0,
            fdt_kind: FdtKind::VirtioMmio, state: serde_json::Value::Null,
        };
        let ok = vec![rec("serial"), rec("virtio-blk"), rec("virtio-balloon"), rec("vsock")];
        assert!(super::check_known_ids(&ok).is_ok());
        let bad = vec![rec("serial"), rec("mystery-device")];
        assert!(super::check_known_ids(&bad).is_err());
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p hvf-spike --bin boot check_known_ids`
Expected: FAIL — `check_known_ids` does not exist.

- [ ] **Step 3: Add `Mode`, `DeviceContext`, `place`, `check_known_ids`**

In `spike/src/bin/boot.rs`, add near the top-level item definitions (after the imports, before `fn main`):

```rust
use std::sync::atomic::AtomicU32;
use devices::virtio::mmio::VirtioMmio;
use vmm::device_manager::{DeviceManager, DeviceRecord};

/// The known set of snapshot-able device ids (boot_timer is add_fixed: no record).
const KNOWN_DEVICE_IDS: &[&str] = &[
    "serial", "virtio-blk", "virtio-rng", "rtc", "virtio-balloon", "vsock", "virtio-net",
];

/// Fail if a snapshot record names a device this binary cannot rebuild.
fn check_known_ids(records: &[DeviceRecord]) -> io::Result<()> {
    for rec in records {
        if !KNOWN_DEVICE_IDS.contains(&rec.id.as_str()) {
            return Err(io::Error::other(format!("unknown device id in snapshot: {}", rec.id)));
        }
    }
    Ok(())
}

/// Whether we are wiring a fresh boot or restoring from a record set.
enum Mode<'a> {
    Boot,
    Restore(&'a [DeviceRecord]),
}

/// Inputs the device builders read, plus the typed handles they stash for the
/// console reader / Ctrl-A b / vsock reactor.
struct DeviceContext {
    host: *mut u8,
    ram_size: u64,
    disk: Option<PathBuf>,      // boot: original rootfs; restore: private instance copy
    vsock_uds: Option<PathBuf>,
    net: bool,                  // boot-only; never set on restore
    // outputs
    serial: Option<Arc<Mutex<Serial<FlushWriter>>>>,
    balloon_target: Option<Arc<AtomicU32>>,
    balloon: Option<Arc<Mutex<VirtioMmio>>>,
    vsock_mmio: Option<Arc<Mutex<VirtioMmio>>>,
    net_mmio: Option<Arc<Mutex<VirtioMmio>>>,
}

impl DeviceContext {
    fn guest_ram(&self) -> GuestRam {
        GuestRam::new(self.host, self.ram_size as usize, layout::RAM_BASE)
    }
}

/// Place a device once for whichever mode we're in: fresh `add` (boot, new
/// window/SPI) or `add_restored` (restore, saved window/SPI + state applied).
/// In restore mode a device with no matching record is skipped (returns None).
fn place<D, F>(
    mgr: &mut DeviceManager,
    mode: &Mode,
    id: &str,
    window: u64,
    build: F,
) -> io::Result<Option<Arc<Mutex<D>>>>
where
    D: devices::device::MmioDevice + 'static,
    F: FnOnce(Arc<dyn devices::virtio::IrqLine>) -> D,
{
    match mode {
        Mode::Boot => Ok(Some(mgr.add(window, build).map_err(io::Error::other)?)),
        Mode::Restore(recs) => match recs.iter().find(|r| r.id == id) {
            Some(rec) => Ok(Some(mgr.add_restored(rec, build).map_err(io::Error::other)?)),
            None => Ok(None),
        },
    }
}
```

Adjust the `use` lines if any of these symbols are already imported (avoid duplicate imports — `DeviceManager` is already imported at line 33; remove the duplicate in the snippet if so, keeping a single import).

- [ ] **Step 4: Add `setup_devices`**

Add this function to `spike/src/bin/boot.rs` (top-level, near `place`):

```rust
/// The single device-wiring site. Lists every snapshot-able device once; both
/// fresh boot and restore call it. Fills `ctx`'s output handles. boot_timer is
/// wired separately by the caller (add_fixed: no record/FDT/state).
fn setup_devices(mgr: &mut DeviceManager, ctx: &mut DeviceContext, mode: Mode) -> io::Result<()> {
    if let Mode::Restore(recs) = &mode {
        check_known_ids(recs)?;
    }

    // Serial — always first (its base matches the earlycon address in the cmdline).
    if let Some(s) = place(mgr, &mode, "serial", layout::MMIO_WINDOW,
        |irq| Serial::with_irq(FlushWriter, irq))? {
        ctx.serial = Some(s);
    }

    // PL031 RTC — always-on; ignores irq (time-only).
    place::<Pl031, _>(mgr, &mode, "rtc", layout::MMIO_WINDOW, |_irq| Pl031::new())?;

    // virtio-rng — always-on, stateless.
    {
        let mem = ctx.guest_ram();
        place::<VirtioMmio, _>(mgr, &mode, "virtio-rng", layout::MMIO_WINDOW,
            move |irq| VirtioMmio::new("virtio-rng", Box::new(VirtioRng::new()), mem, irq))?;
    }

    // virtio-balloon — always-on; the shared target survives restore via its state.
    {
        let mem = ctx.guest_ram();
        let (balloon_dev, target) = Balloon::new();
        if let Some(h) = place(mgr, &mode, "virtio-balloon", layout::MMIO_WINDOW,
            move |irq| VirtioMmio::new("virtio-balloon", Box::new(balloon_dev), mem, irq))? {
            ctx.balloon_target = Some(target);
            ctx.balloon = Some(h);
        }
    }

    // virtio-blk — present iff a disk source was provided (boot) or a record exists (restore).
    if let Some(disk) = ctx.disk.clone() {
        let file = fs::OpenOptions::new().read(true).write(true).open(&disk)
            .map_err(|e| io::Error::other(format!("open disk {}: {e}", disk.display())))?;
        let blk = VirtioBlk::new(file).map_err(|e| io::Error::other(format!("VirtioBlk::new: {e}")))?;
        let mem = ctx.guest_ram();
        place::<VirtioMmio, _>(mgr, &mode, "virtio-blk", layout::MMIO_WINDOW,
            move |irq| VirtioMmio::new("virtio-blk", Box::new(blk), mem, irq))?;
    }

    // virtio-net — boot-only (snapshots are blocked under --net, so no restore arm).
    if ctx.net {
        let (backend, rx) = ignition_vmnet::VmnetBackend::start()
            .expect("vmnet start failed (run boot under sudo for --net)");
        let mem = ctx.guest_ram();
        let net_dev = VirtioNet::new(backend);
        if let Some(h) = place(mgr, &mode, "virtio-net", layout::MMIO_WINDOW,
            move |irq| VirtioMmio::new("virtio-net", Box::new(net_dev), mem, irq))? {
            let h2 = h.clone();
            std::thread::spawn(move || {
                for frame in rx { h2.lock().unwrap().inject_rx(&frame); }
            });
            ctx.net_mmio = Some(h);
        }
    }

    // virtio-vsock — present iff a uds base was provided (boot) or a record exists (restore).
    let want_vsock = matches!(mode, Mode::Restore(_)) || ctx.vsock_uds.is_some();
    if want_vsock {
        let uds = ctx.vsock_uds.clone()
            .unwrap_or_else(|| std::env::temp_dir().join("ignition-vsock"));
        let mem = ctx.guest_ram();
        let vsock_dev = VsockDevice::new(uds);
        if let Some(h) = place(mgr, &mode, "vsock", layout::MMIO_WINDOW,
            move |irq| VirtioMmio::new("vsock", Box::new(vsock_dev), mem, irq))? {
            ctx.vsock_mmio = Some(h);
        }
    }

    Ok(())
}
```

- [ ] **Step 5: Switch fresh boot to `setup_devices`**

In `fn main`'s boot path (the `mgr.add(...)` block, lines ~362-434), replace the per-device `mgr.add(...)` calls for serial / rtc / rng / balloon / blk / net / vsock with:

```rust
    let mut ctx = DeviceContext {
        host: host as *mut u8,
        ram_size: RAM_SIZE,
        disk: disk_path.as_ref().map(PathBuf::from),
        vsock_uds: vsock_uds.clone(),
        net,
        serial: None, balloon_target: None, balloon: None, vsock_mmio: None, net_mmio: None,
    };
    setup_devices(&mut mgr, &mut ctx, Mode::Boot).expect("device setup failed");
    let serial = ctx.serial.clone().expect("serial device");
    let balloon_target = ctx.balloon_target.clone().expect("balloon target");
    let balloon = ctx.balloon.clone().expect("balloon device");
    if let Some(vsock_mmio) = ctx.vsock_mmio.clone() {
        spawn_vsock_reactor(vsock_mmio);
        eprintln!("virtio-vsock: enabled (host uds base {})",
            ctx.vsock_uds.as_ref().unwrap().display());
    }
    if disk_path.is_some() {
        eprintln!("virtio : /dev/vda backed by {}", disk_path.as_ref().unwrap());
    }
    if net { eprintln!("virtio-net: enabled (vmnet shared/NAT)"); }
```

Keep the `mgr.add_fixed(layout::BOOT_TIMER_ADDR, ...)` boot-timer block exactly as-is (lines ~438-443), after this. Remove the now-dead `(balloon_dev, balloon_target) = Balloon::new()` and the individual device blocks they replaced. Ensure the later uses of `serial`, `balloon_target`, `balloon` still resolve (they now come from `ctx`).

- [ ] **Step 6: Build and run tests**

Run: `cargo build -p hvf-spike --bin boot && cargo test -p hvf-spike --bin boot`
Expected: builds clean; `check_known_ids` test and existing boot tests pass.

- [ ] **Step 7: Commit**

```bash
git add spike/src/bin/boot.rs
git commit -m "spike: single setup_devices wiring site for fresh boot"
```

---

## Task 4b: Restore path uses the unified wiring

**Files:**
- Modify: `spike/src/bin/boot.rs` (`fn run_restore`)

- [ ] **Step 1: Replace the `match rec.id` loop**

In `fn run_restore`, replace the entire device-restore section (the `let mut serial_handle = None;` through the end of the `for rec in &snap.devices { match ... }` loop and the two `ok_or_else` handle extractions, lines ~621-691) with:

```rust
    // Private disk instance so clones are independent (only if the snapshot has a disk).
    let disk = if snap.devices.iter().any(|r| r.id == "virtio-blk") {
        let instance_disk = std::env::temp_dir()
            .join(format!("ignition-instance-{}.img", process::id()));
        fs::copy(&paths.disk, &instance_disk)?;
        Some(instance_disk)
    } else {
        None
    };

    let mut ctx = DeviceContext {
        host: host as *mut u8,
        ram_size: RAM_SIZE,
        disk,
        vsock_uds: vsock_uds.clone(),
        net: false, // snapshots never contain net
        serial: None, balloon_target: None, balloon: None, vsock_mmio: None, net_mmio: None,
    };
    setup_devices(&mut mgr, &mut ctx, Mode::Restore(&snap.devices))?;

    let serial = ctx.serial.clone().ok_or_else(|| io::Error::other("snapshot had no serial device"))?;
    let balloon_target = ctx.balloon_target.clone()
        .ok_or_else(|| io::Error::other("snapshot had no balloon device"))?;
    let balloon = ctx.balloon.clone()
        .ok_or_else(|| io::Error::other("snapshot had no balloon device"))?;
    if let Some(vsock_mmio) = ctx.vsock_mmio.clone() {
        spawn_vsock_reactor(vsock_mmio);
    }
```

The subsequent `let frozen = mgr.freeze();` and everything after stays unchanged (the `serial`, `balloon_target`, `balloon` bindings are still in scope).

- [ ] **Step 2: Remove now-unused imports/bindings**

Delete any imports made dead by removing the match (e.g. a stray `use std::sync::atomic::AtomicU32;` already moved to top-level in Task 4a — keep exactly one). Build will flag duplicates/unused.

- [ ] **Step 3: Build and run tests**

Run: `cargo build -p hvf-spike --bin boot && cargo test -p hvf-spike --bin boot`
Expected: builds clean (no `match rec.id` remains); tests pass.

- [ ] **Step 4: Clippy gate**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 5: Commit**

```bash
git add spike/src/bin/boot.rs
git commit -m "spike: restore path drives shared setup_devices, drop match rec.id"
```

---

## Task 5: Verification, live test, and docs

**Files:**
- Modify: `README.md`
- Modify: `docs/superpowers/specs/2026-06-13-device-model-snapshot-design.md` (note completion)

- [ ] **Step 1: Full workspace test + clippy**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings`
Expected: all suites green, zero clippy warnings.

- [ ] **Step 2: Live restore smoke test**

Run:
```bash
cargo build -p hvf-spike --bin boot && scripts/sign.sh target/debug/boot
python3 scripts/restore_test.py
```
Expected: boot → snapshot → restore cycle reports the restored guest idling at ~0% CPU and responsive (same pass criteria as before — no regression from the wiring refactor).

- [ ] **Step 3: Live balloon-target persistence check**

Manual (documented in README §Snapshot for the reader; verifier runs it once):
```bash
target/debug/boot --snap-dir mysnap kimage/out/Image kimage/out/rootfs.ext4
# in guest: trigger balloon (Ctrl-A b) to inflate, then Ctrl-A s to snapshot, Ctrl-A x
target/debug/boot --restore mysnap
# in guest: cat /sys/devices/.../virtio*/.../  or `free` — balloon target/actual match pre-snapshot
```
Expected: the restored guest's balloon config reports the same num_pages/actual that were live at snapshot time (not reset to 0).

- [ ] **Step 4: Update README**

In `README.md`, update the device-model bullets (lines ~30-31):

```markdown
  - **virtio-vsock** — guest→host streams over a host Unix socket (`--vsock-uds`);
    on restore, live connections are reset (the guest is RST'd, since host peers are gone).
  - **virtio-balloon** — on-demand memory reclaim (`Ctrl-A b`); the inflation
    target survives snapshot/restore.
```

And in the Snapshot/restore status bullet (lines ~35-36), append: "all devices restore their full state (transport + queues + per-device: balloon target, vsock connection reset)."

- [ ] **Step 5: Mark the spec done**

Append to `docs/superpowers/specs/2026-06-13-device-model-snapshot-design.md`:

```markdown

## Status: implemented 2026-06-13

Delivered via plan `docs/superpowers/plans/2026-06-13-device-model-snapshot.md`.
The `Vec<DeviceSpec>` was realized as `setup_devices(mode)` + a generic `place()`
helper (see the plan's realization note). All devices round-trip full state.
```

- [ ] **Step 6: Commit**

```bash
git add README.md docs/superpowers/specs/2026-06-13-device-model-snapshot-design.md
git commit -m "docs: device-model snapshot complete (balloon target + vsock reset)"
```

---

## Notes for the implementer

- **`GuestRam::new` with a null pointer** is used in Task 1's transport test only because the mock never reads memory. If `GuestRam::new` asserts non-null, back it with `vec![0u8; 16]` and keep a binding alive for the test's duration (see `balloon.rs` tests for the pattern).
- **`*mut u8` in `DeviceContext` is not `Send`/`Sync`** — that's fine; `DeviceContext` lives entirely on the setup thread and is dropped before the vCPU thread starts. Do not store it past `setup_devices`.
- **Do not change `DeviceManager`** — `add_restored` already calls `MmioDevice::restore`, which (after Task 1) restores inner device state too.
- **`spawn_vsock_reactor`** keeps its existing `Arc<Mutex<VirtioMmio>>` signature; the handle now comes from `ctx.vsock_mmio`.
- The `serde_json::Value` field on `VirtioMmioState` keeps `Eq` because `DeviceRecord` (which also holds a `Value`) already derives `Eq` in this codebase — the serde_json version in use implements `Value: Eq`.
