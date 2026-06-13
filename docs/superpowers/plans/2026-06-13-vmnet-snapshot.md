# vmnet / virtio-net Snapshot+Restore (link-bounce) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a `--net` (vmnet) microVM be snapshotted and restored — including multiple clones from one image — re-establishing networking on restore via a virtio-net link-bounce + a busybox carrier-watch service, with no new MMIO device and no new guest package.

**Architecture:** Live-RAM-grab snapshot (unchanged). Add `VIRTIO_NET_F_STATUS` to the existing virtio-net so the VMM can bounce the carrier; quiesce the vmnet RX feeder during the RAM read; relax the snapshot gate to allow net; on restore start a fresh vmnet (new MAC), rebuild virtio-net, and pulse link DOWN→UP. A tiny busybox carrier-watch openrc service in the guest rebinds the driver (adopts the new MAC) and re-DHCPs. Spec: `docs/superpowers/specs/2026-06-13-vmnet-snapshot-design.md`.

**Tech Stack:** Rust (edition 2024), serde_json, the `devices`/`vmm`/`vmnet` crates, the `boot` binary, busybox/openrc rootfs (`kimage/build/build-rootfs.sh`).

---

## Spike gate

Tasks 1–4 build the host-side plumbing and are unit-testable. **Task 5 is a live spike that gates the rest:** it proves whether a link-bounce + busybox driver-rebind makes the guest adopt the new MAC and re-DHCP. If the spike passes, Task 6 ships the carrier-watch service as written. If it fails, STOP and replan Task 6 around the vsock-signal fallback (spec §"open risk") — do not improvise.

## File structure

- `crates/devices/src/virtio/mmio.rs` — `VirtioDevice::set_link` (default no-op); `VirtioMmio::net_set_link`. (Task 1)
- `crates/devices/src/virtio/net.rs` — negotiate `VIRTIO_NET_F_STATUS`, track link state in config, implement `set_link`. (Task 1)
- `spike/src/bin/boot.rs` — `stop_rx` quiesce (Task 2); relax snapshot gate + wire `stop_rx` into the handler (Task 3); restore-net arm + initial link-down + bounce thread (Task 4).
- `kimage/build/build-rootfs.sh` — busybox carrier-watch service. (Task 6)
- `README.md` + spec status. (Task 7)

---

## Task 1: virtio-net F_STATUS + set_link

**Files:**
- Modify: `crates/devices/src/virtio/net.rs`
- Modify: `crates/devices/src/virtio/mmio.rs`
- Test: both files' `#[cfg(test)] mod tests`

- [ ] **Step 1: Write failing net tests**

Add to `mod tests` in `crates/devices/src/virtio/net.rs`:

```rust
    #[test]
    fn status_feature_negotiated_and_link_reported() {
        let mut net = VirtioNet::new(FakeBackend::default());
        // F_STATUS now advertised in the low feature word.
        assert_ne!(net.device_features(0) & (1 << VIRTIO_NET_F_STATUS), 0);
        // Default link is up: status byte (offset 6) has LINK_UP (bit 0).
        let mut st = [0u8; 2];
        net.config_read(6, &mut st);
        assert_eq!(st[0] & 1, 1);
        // set_link(false) clears LINK_UP; set_link(true) sets it.
        net.set_link(false);
        net.config_read(6, &mut st);
        assert_eq!(st[0] & 1, 0);
        net.set_link(true);
        net.config_read(6, &mut st);
        assert_eq!(st[0] & 1, 1);
    }
```

- [ ] **Step 2: Run to verify fail**

`cargo test -p ignition-devices status_feature_negotiated_and_link_reported`
Expected: FAIL — no `VIRTIO_NET_F_STATUS`, no `set_link`, status byte hardcoded.

- [ ] **Step 3: Implement F_STATUS + set_link in net.rs**

Add the feature constant near `VIRTIO_NET_F_MAC` (top of file):

```rust
/// Feature bit: the device exposes a link-status word in config space.
pub const VIRTIO_NET_F_STATUS: u32 = 16;
/// config.status bit 0: link is up (VIRTIO_NET_S_LINK_UP).
const VIRTIO_NET_S_LINK_UP: u16 = 1;
```

Add a `link_up: bool` field to `VirtioNet` and init it `true`:

```rust
pub struct VirtioNet<B: NetBackend> {
    backend: B,
    mac: [u8; 6],
    dropped_rx: u64,
    link_up: bool,
}
```

In `VirtioNet::new`, set `link_up: true` in the struct literal.

Add a `set_link` method to the `impl<B: NetBackend> VirtioNet<B>` block:

```rust
    /// Set the reported link state (config.status LINK_UP). The transport raises a
    /// config-change interrupt so the guest re-reads status (carrier on/off).
    pub fn set_link(&mut self, up: bool) {
        self.link_up = up;
    }
```

In `device_features`, advertise F_STATUS in sel 0:

```rust
    fn device_features(&self, sel: u32) -> u32 {
        if sel == 0 { (1 << VIRTIO_NET_F_MAC) | (1 << VIRTIO_NET_F_STATUS) } else { 0 }
    }
```

In `config_read`, drive the status word from `link_up` (replace the hardcoded `cfg[6] = 1;`):

```rust
        let status: u16 = if self.link_up { VIRTIO_NET_S_LINK_UP } else { 0 };
        cfg[6..8].copy_from_slice(&status.to_le_bytes());
```

Add `set_link` to the `VirtioDevice` impl so the transport can reach it through the trait — first add the trait default (Step 4), then override here:

```rust
    fn set_link(&mut self, up: bool) {
        VirtioNet::set_link(self, up);
    }
```

(Place this override inside `impl<B: NetBackend> VirtioDevice for VirtioNet<B>`.)

- [ ] **Step 4: Add the trait default + transport helper in mmio.rs**

In `crates/devices/src/virtio/net.rs`'s sibling trait file `crates/devices/src/virtio/mmio.rs`, add to the `VirtioDevice` trait (after `vsock_poll_set`, before `save`):

```rust
    /// Set link state for devices that have one (virtio-net). Default: no-op.
    fn set_link(&mut self, _up: bool) {}
```

Add a transport method inside `impl VirtioMmio` (near `signal_config_change`):

```rust
    /// Flip the inner net device's link state and raise a config-change interrupt
    /// so the guest re-reads config.status (carrier off/on). No-op for non-net.
    pub fn net_set_link(&mut self, up: bool) {
        self.dev.set_link(up);
        self.signal_config_change();
    }
```

- [ ] **Step 5: Write failing transport test**

Add to `mod tests` in `crates/devices/src/virtio/mmio.rs` (reuse the existing `NoopIrq`/`GuestRam` patterns; use a recording IrqLine if one exists, else assert on `interrupt_status`). Minimal version asserting the config-change bit:

```rust
    #[test]
    fn net_set_link_raises_config_change() {
        // A mock net device implementing set_link; assert the transport sets the
        // CONFIG interrupt-status bit when link is toggled.
        struct LinkMock { up: bool }
        impl VirtioDevice for LinkMock {
            fn device_id(&self) -> u32 { 1 }
            fn device_features(&self, _s: u32) -> u32 { 0 }
            fn config_read(&self, _o: u64, d: &mut [u8]) { d.fill(0); }
            fn queue_count(&self) -> usize { 2 }
            fn handle_notify(&mut self, _q: usize, _vq: &mut Virtqueue, _m: &GuestRam) -> bool { false }
            fn set_link(&mut self, up: bool) { self.up = up; }
        }
        let mem = GuestRam::new(std::ptr::null_mut(), 0, 0);
        let irq: Arc<dyn IrqLine> = Arc::new(crate::virtio::NoopIrq);
        let mut t = VirtioMmio::new("virtio-net", Box::new(LinkMock { up: true }), mem, irq);
        t.net_set_link(false);
        // INT_STATUS_CONFIG (2) must now be set in the transport's interrupt status.
        let saved = t.save_state();
        assert_ne!(saved.interrupt_status & 2, 0, "config-change interrupt must be raised");
    }
```

If `interrupt_status` isn't surfaced via `save_state` in a usable way here, instead add a `#[cfg(test)] pub(crate) fn interrupt_status(&self) -> u32 { self.interrupt_status }` accessor and assert on it. (Prefer `save_state` if it already carries it — it does.)

- [ ] **Step 6: Run tests**

`cargo test -p ignition-devices` — expect all pass (new net + transport tests included).

- [ ] **Step 7: Commit**

```bash
git add crates/devices/src/virtio/net.rs crates/devices/src/virtio/mmio.rs
git commit -m "devices: virtio-net F_STATUS + link-bounce via config-change"
```
Plain message — no Co-Authored-By, no Generated-with trailer.

---

## Task 2: RX-feeder quiesce for net snapshot

**Files:**
- Modify: `spike/src/bin/boot.rs`
- Test: `spike/src/bin/boot.rs` `#[cfg(test)] mod tests`

The vmnet RX feeder thread injects frames into guest RAM under the device mutex; the snapshot handler reads RAM without it. A shared `stop_rx` flag gates the feeder so the handler can read a stable image.

- [ ] **Step 1: Write a failing unit test for the gate helper**

We extract the feeder's per-frame decision into a tiny pure helper so it's testable without a live vmnet. Add to `spike/src/bin/boot.rs` `#[cfg(test)] mod tests`:

```rust
    #[test]
    fn rx_gate_skips_when_stopped() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let stop = Arc::new(AtomicBool::new(false));
        assert!(super::rx_should_inject(&stop));   // not stopped -> inject
        stop.store(true, Ordering::Release);
        assert!(!super::rx_should_inject(&stop));   // stopped -> skip
    }
```

- [ ] **Step 2: Run to verify fail**

`cargo test -p hvf-spike --bin boot rx_gate_skips_when_stopped` — FAIL (no `rx_should_inject`).

- [ ] **Step 3: Add the helper**

Add to `spike/src/bin/boot.rs` (top-level, near `place`):

```rust
use std::sync::atomic::{AtomicBool, Ordering};

/// The vmnet RX feeder injects a frame only when not quiesced for a snapshot.
fn rx_should_inject(stop_rx: &std::sync::Arc<AtomicBool>) -> bool {
    !stop_rx.load(Ordering::Acquire)
}
```

- [ ] **Step 4: Run to verify pass**

`cargo test -p hvf-spike --bin boot rx_gate_skips_when_stopped` — PASS.

- [ ] **Step 5: Commit**

```bash
git add spike/src/bin/boot.rs
git commit -m "spike: rx-feeder quiesce gate helper for net snapshot"
```

---

## Task 3: Relax snapshot gate; wire stop_rx into the feeder and handler

**Files:**
- Modify: `spike/src/bin/boot.rs`

The net arm in `setup_devices` must create a shared `stop_rx`, hand it to the feeder, and expose it (plus the net device handle) so the snapshot handler can quiesce + drain before reading RAM. The gate changes from `smp == 1 && !net` to `smp == 1`.

- [ ] **Step 1: Add `stop_rx` + net handle to `DeviceContext`**

In the `DeviceContext` struct, add fields:

```rust
    rx_stop: Option<std::sync::Arc<AtomicBool>>, // set when a net feeder is running
```

(Initialize `rx_stop: None` in BOTH `DeviceContext { ... }` literals — the boot path (~line 512) and the restore path (~line 729).)

- [ ] **Step 2: Wire the feeder to the gate in the net arm**

In `setup_devices`, replace the net arm body (lines ~366-380) with one that:
- builds the device,
- creates `stop_rx`,
- the feeder checks `rx_should_inject` before locking+injecting,
- stashes `stop_rx` into `ctx.rx_stop` and the handle into `ctx.net_mmio`,
- and works in **both** boot (`ctx.net`) and restore (record present) modes:

```rust
    // virtio-net — boot iff --net; restore iff a record exists. A fresh vmnet
    // interface each time (new MAC), so clones get distinct MAC + DHCP lease.
    let want_net = match &mode {
        Mode::Boot => ctx.net,
        Mode::Restore(recs) => recs.iter().any(|r| r.id == "virtio-net"),
    };
    if want_net {
        let (backend, rx) = ignition_vmnet::VmnetBackend::start()
            .map_err(|e| io::Error::other(format!("vmnet start failed (need sudo for --net): {e}")))?;
        let mem = ctx.guest_ram();
        let net_dev = VirtioNet::new(backend);
        if let Some(h) = place(mgr, &mode, "virtio-net", layout::MMIO_WINDOW,
            move |irq| VirtioMmio::new("virtio-net", Box::new(net_dev), mem, irq))? {
            let stop_rx = std::sync::Arc::new(AtomicBool::new(false));
            let h2 = h.clone();
            let stop2 = stop_rx.clone();
            std::thread::spawn(move || {
                for frame in rx {
                    if rx_should_inject(&stop2) {
                        h2.lock().unwrap().inject_rx(&frame);
                    }
                }
            });
            ctx.rx_stop = Some(stop_rx);
            ctx.net_mmio = Some(h);
        }
    }
```

- [ ] **Step 3: Relax the snapshot-handler gate and quiesce RAM read**

Change the gate at line ~576 from `if smp == 1 && !net {` to `if smp == 1 {`. Update the stale comment at ~572-585 (drop "non-net"/"sole RAM writer" wording; explain the quiesce instead).

Capture the feeder stop flag + net handle into the handler closure. Before the snapshot closure, add (alongside the other captured clones):

```rust
        let rx_stop_snap = ctx.rx_stop.clone();
        let net_mmio_snap = ctx.net_mmio.clone();
```

Inside the handler, BEFORE building `ram_slice`/reading RAM, quiesce the feeder:

```rust
            // Quiesce the vmnet RX feeder so it can't write guest RAM mid-read.
            if let Some(stop) = &rx_stop_snap {
                stop.store(true, Ordering::Release);
                // Drain any in-flight inject by taking the device lock once.
                if let Some(net) = &net_mmio_snap {
                    let _ = net.lock().unwrap();
                }
            }
```

After `write_snapshot(...)` completes (success or failure), resume the feeder:

```rust
            if let Some(stop) = &rx_stop_snap {
                stop.store(false, Ordering::Release);
            }
```

Note: `ctx` is consumed earlier when building handles; ensure `ctx.rx_stop`/`ctx.net_mmio` are cloned out before `ctx` goes out of scope (do the `.clone()` right after `setup_devices` returns, near where `serial`/`balloon` are extracted).

- [ ] **Step 4: Build + test + clippy**

```
cargo build -p hvf-spike --bin boot
cargo test -p hvf-spike --bin boot
cargo clippy --workspace --all-targets -- -D warnings
```
Expected: clean build, tests pass, no clippy warnings. Fix issues without `#[allow]`.

- [ ] **Step 5: Commit**

```bash
git add spike/src/bin/boot.rs
git commit -m "spike: allow net snapshots (smp==1), quiesce rx feeder during RAM read"
```

---

## Task 4: Restore-net wiring + link-bounce

**Files:**
- Modify: `spike/src/bin/boot.rs` (`fn run_restore`)

The restore-mode net arm already starts a fresh vmnet (Task 3's `want_net`). Now: start the restored NIC with link DOWN, then a thread raises it UP shortly after resume so the guest observes a down→up carrier transition.

- [ ] **Step 1: Start the restored link down, then bounce up**

In `run_restore`, after `setup_devices(&mut mgr, &mut ctx, Mode::Restore(&snap.devices))?` and after extracting handles, but before `manager.run_restored(...)`, add:

```rust
    // Net restore: present the link as DOWN, then raise it after resume so the
    // guest's carrier-watch sees a down->up edge and re-inits (new MAC + DHCP).
    if let Some(net) = ctx.net_mmio.clone() {
        net.lock().unwrap().net_set_link(false);
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(1500));
            net.lock().unwrap().net_set_link(true);
        });
    }
```

(`ctx.net_mmio` is populated by the restore-mode net arm when the snapshot has a virtio-net record. If there's no net record it's `None` and this is skipped.)

- [ ] **Step 2: Build + test + clippy**

```
cargo build -p hvf-spike --bin boot && scripts/sign.sh target/debug/boot
cargo test -p hvf-spike --bin boot
cargo clippy --workspace --all-targets -- -D warnings
```
Expected: clean.

- [ ] **Step 3: Commit**

```bash
git add spike/src/bin/boot.rs
git commit -m "spike: restore vmnet (fresh iface) and bounce link down->up after resume"
```

---

## Task 5: SPIKE — does the guest adopt the new MAC + re-DHCP? (live, gates Task 6)

**No code.** This is a manual live experiment. Requires the hypervisor entitlement, `sudo` (vmnet), and the real kernel/rootfs. It validates the guest-side reaction BEFORE building the carrier-watch service.

- [ ] **Step 1: Boot a net VM, confirm baseline networking**

```bash
sudo target/debug/boot --net --snap-dir vmnetsnap kimage/out/Image kimage/out/rootfs.ext4
# in guest: note MAC + IP
ip addr show eth0
ping -c1 1.1.1.1
```

- [ ] **Step 2: Snapshot, then restore into a fresh process**

In the booted guest console press `Ctrl-A s` (snapshot written), then `Ctrl-A x`. Then:

```bash
sudo target/debug/boot --restore vmnetsnap
```
The VMM raises the link ~1.5s after resume (Task 4). Observe whether the guest's `eth0` carrier goes down→up:
```bash
# in restored guest:
cat /sys/class/net/eth0/carrier
ip -br link show eth0     # note the MAC the device now reports
```

- [ ] **Step 2b: Manually emulate the carrier-watch action**

Run, by hand in the restored guest, the exact sequence the service will automate, and record whether each step works:

```sh
# rebind virtio-net so it re-reads the new MAC from config
DEV=$(basename "$(readlink /sys/class/net/eth0/device)")
echo "$DEV" > /sys/bus/virtio/drivers/virtio_net/unbind
echo "$DEV" > /sys/bus/virtio/drivers/virtio_net/bind
ip -br link show eth0    # did the MAC change to the new vmnet MAC?
ifdown eth0; ifup eth0   # busybox udhcpc re-lease
ip addr show eth0        # new IP?
ping -c1 1.1.1.1         # connectivity restored?
```

- [ ] **Step 3: Decision gate — record the outcome in the spec**

Append a short "Spike result" note to `docs/superpowers/specs/2026-06-13-vmnet-snapshot-design.md` stating, factually: did the rebind re-read the new MAC; did `ifup` get a fresh lease; did connectivity return; any surprises (e.g. eth0 renamed to eth1 on rebind, driver path differences).

```bash
git add docs/superpowers/specs/2026-06-13-vmnet-snapshot-design.md
git commit -m "spec: vmnet snapshot spike result"
```

- [ ] **Step 4: Branch the plan on the result**

- **PASS** (rebind adopts MAC + `ifup` re-leases + connectivity): proceed to Task 6 as written, encoding exactly the working command sequence (account for any device-name/path quirks found).
- **FAIL** (rebind doesn't re-read MAC, or no signal distinguishes restore from a flap): STOP. Replan Task 6 around the vsock-signal fallback from spec §"open risk" (host writes a restore marker + the new MAC over the existing vsock channel; a guest reader applies `ip link set address` + `ifup`). Do not improvise a third mechanism.

---

## Task 6: Guest carrier-watch service (only after Task 5 PASS)

**Files:**
- Modify: `kimage/build/build-rootfs.sh`

Add a busybox background poller that, on a carrier down→up edge, runs the exact sequence Task 5 validated. Pure shell, no new package.

- [ ] **Step 1: Add the service to the alpine-provision block**

In `build-rootfs.sh`, inside the first `docker run ... alpine:3.19 sh -euxc '...'` block, after the `boottime.start` lines (~line 56) and before `rc-update add local boot`, add:

```sh
  # Net re-init on restore: a snapshot restore starts a fresh vmnet interface
  # (new MAC) and the VMM bounces the virtio-net link down->up. This poller sees
  # the carrier edge, rebinds virtio-net so it re-reads the new MAC, and re-DHCPs.
  # Pure busybox; no udev in this image. (See vmnet-snapshot-design spec.)
  printf "%s\n" \
    "#!/bin/sh" \
    "( prev=1" \
    "  while :; do" \
    "    cur=\$(cat /sys/class/net/eth0/carrier 2>/dev/null || echo 0)" \
    "    if [ \"\$prev\" = 0 ] && [ \"\$cur\" = 1 ]; then" \
    "      d=\$(basename \"\$(readlink /sys/class/net/eth0/device)\")" \
    "      echo \"\$d\" > /sys/bus/virtio/drivers/virtio_net/unbind 2>/dev/null" \
    "      echo \"\$d\" > /sys/bus/virtio/drivers/virtio_net/bind 2>/dev/null" \
    "      ifdown eth0 2>/dev/null; ifup eth0" \
    "    fi" \
    "    prev=\$cur" \
    "    sleep 1" \
    "  done ) &" \
    > /etc/local.d/netwatch.start
  chmod +x /etc/local.d/netwatch.start
```

Adjust the rebind/`ifup` lines to match exactly what Task 5 proved (e.g. if rebind renames `eth0`→`eth1`, watch the right interface, or re-resolve the device after bind).

- [ ] **Step 2: Rebuild the rootfs**

The user rebuilds the rootfs image (`bash kimage/build/build-rootfs.sh`) and refreshes `kimage/out/rootfs.ext4`. (This step runs Docker; the implementer should request the rebuilt image rather than assume it.)

- [ ] **Step 3: Commit the script change**

```bash
git add kimage/build/build-rootfs.sh
git commit -m "rootfs: carrier-watch service re-inits net (rebind + dhcp) on restore"
```

---

## Task 7: Live clone verification + docs

**Files:**
- Modify: `README.md`
- Modify: `docs/superpowers/specs/2026-06-13-vmnet-snapshot-design.md`

- [ ] **Step 1: Full gate**

`cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings` — all green.

- [ ] **Step 2: Live clone test**

```bash
cargo build -p hvf-spike --bin boot && scripts/sign.sh target/debug/boot
# snapshot a net VM (Ctrl-A s, Ctrl-A x), then restore two clones in separate terminals:
sudo target/debug/boot --restore vmnetsnap     # terminal A
sudo target/debug/boot --restore vmnetsnap     # terminal B
# in each guest after the auto re-init settles:
ip -br addr show eth0     # expect DISTINCT MAC + IP per clone
ping -c1 1.1.1.1          # expect connectivity in both
```
Expected: two clones, distinct MAC + IP, both reach the internet.

- [ ] **Step 3: Update README**

In `README.md`'s virtio-net bullet, append: "snapshot/restore supported (single-vCPU, `sudo`); on restore a fresh vmnet interface is started and the guest re-DHCPs via a carrier-watch service, so clones get distinct MAC+IP. Active connections reset."

In the Snapshot/restore bullet, drop the implicit "no net" restriction.

- [ ] **Step 4: Mark the spec implemented**

Append to the spec a "Status: implemented" note (date, plan ref, clone-test result).

- [ ] **Step 5: Commit**

```bash
git add README.md docs/superpowers/specs/2026-06-13-vmnet-snapshot-design.md
git commit -m "docs: vmnet snapshot/restore + clone networking"
```

---

## Notes for the implementer

- **Tasks 1–3 are unit-testable and subagent-friendly. Tasks 4–7 need a real device/sudo/rootfs** — run them inline with the human (they have the entitlement + rebuild the rootfs).
- **Do not skip the Task 5 gate.** The whole guest-side mechanism rests on the rebind actually re-reading the new MAC. If it doesn't, the carrier-watch service is futile — switch to the vsock fallback.
- **`*mut u8` / Send:** the snapshot handler already captures `host as usize`; do not capture raw pointers in the new feeder/bounce threads — they capture `Arc<Mutex<VirtioMmio>>` (Send) and the `Arc<AtomicBool>` only.
- **Single-vCPU only** stays enforced for restore (`assert_eq!(snap.config.vcpu_count, 1)` in `run_restore` is unchanged).
- The bounce thread holds an `Arc<Mutex<VirtioMmio>>`; the vCPU run loop also dispatches MMIO through the bus to the same mutex — brief lock contention at the 1.5s bounce is fine (guest is running).
