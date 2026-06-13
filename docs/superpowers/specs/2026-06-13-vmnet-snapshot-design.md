# vmnet / virtio-net Snapshot+Restore (link-bounce) — Design

Date: 2026-06-13

## Goal

Allow a `--net` (vmnet shared/NAT) microVM to be snapshotted and restored,
including **multiple clones from one image**, with networking re-established on
restore. No new MMIO device and no new guest package: drive the guest's existing
busybox/ifupdown DHCP via the existing virtio-net link status plus a tiny
carrier-watch openrc service.

> Rootfs reality: the image is Alpine busybox + openrc + ifupdown-ng. There is
> **no udev** — networking is a one-shot `ifup -a` from `/etc/local.d/network.start`
> at boot (busybox udhcpc). So the guest-side reaction is a small busybox
> carrier-watch service, not a udev rule.

## Background — why net is excluded today

The snapshot handler is installed only when `smp == 1 && !net`
(`spike/src/bin/boot.rs`). A `--net` VM therefore has no snapshot capability. Two
real obstacles:

1. **MAC / IP collision across clones.** vmnet assigns the MAC at
   `ig_vmnet_start` (`mac_out`); the guest reads it once at probe via
   `VIRTIO_NET_F_MAC` and bakes it into its netdev, and DHCPs an IP at boot. All
   clones resume from the same guest RAM, so they share the frozen MAC + IP. On a
   shared vmnet L2 segment, duplicate MAC *and* duplicate IP both collide.
2. **RX-thread race.** The vmnet RX feeder thread writes guest RAM via
   `inject_rx` under the `VirtioMmio` mutex; the snapshot handler reads RAM at the
   Canceled exit without that lock → a torn snapshot.

## How Firecracker handles it (and why we differ)

FC's `network-for-clones.md`: the guest is **immutable** (same MAC, same static
IP across clones); all per-clone uniqueness is **host-side via Linux network
namespaces** — each clone's tap lives in its own netns (identical tap-name /
tap-IP / guest-IP don't collide), a veth pair + `iptables` MASQUERADE/DNAT give
egress/ingress with unique per-clone host addresses. macOS/vmnet has **no netns
equivalent** (shared mode = one bridged subnet), so FC's primary scheme is
unavailable.

FC's documented **fallback** (no netns / tap-from-a-pool) is exactly our path:
*"the guest will have to be made aware (via vsock or other channel) that it needs
to reconfigure its network."* We use the same idea, but signal the guest through
the existing virtio-net **link status** (a carrier bounce) and let it re-DHCP,
rather than pushing a static reconfig.

## Architecture

Live-RAM-grab snapshot model is unchanged (no guest suspend). On restore we start
a fresh vmnet interface (new MAC, fresh DHCP-able), rebuild virtio-net, then bounce
the link; the guest's standard udev/DHCP machinery re-acquires identity. Each
restore = a distinct vmnet interface ⇒ unique MAC ⇒ (vmnet DHCP is MAC-keyed) a
distinct IP per clone.

### Components

1. **Relaxed snapshot gate** (`spike/src/bin/boot.rs`). Install the snapshot
   handler when `smp == 1` (net allowed); multi-vCPU still excluded. Restoring a
   net snapshot requires `sudo` (vmnet).

2. **RX-thread quiesce.** A shared `AtomicBool stop_rx` the vmnet feeder checks
   before each `inject_rx`. The snapshot handler sets it, then briefly takes the
   net device's mutex (draining any in-flight inject), then reads RAM. In-flight
   RX frames are dropped (connections reset anyway).

3. **virtio-net `VIRTIO_NET_F_STATUS`** (`crates/devices/src/virtio/net.rs`). Add
   the feature bit to `device_features(0)` and serve the `status` LINK_UP bit from
   config (the 8-byte config image already exposes byte 6). Add
   `set_link(&mut self, up: bool)` that flips the status bit; the transport raises
   a config-change interrupt (the transport already has `signal_config_change` /
   `INT_STATUS_CONFIG`). The guest virtio-net reacts with `netif_carrier_off/on`.
   No new device.

4. **Restore wiring** (`setup_devices`, restore mode). When a `"virtio-net"`
   record exists: start a fresh `VmnetBackend` (new MAC Y), rebuild `VirtioNet`,
   spawn its RX feeder (with `stop_rx`), and `place` at the saved base/SPI. Drops
   the current `net: false` hardcode / "net snapshots never happen" assumption in
   `run_restore`. After resume, the VMM pulses link DOWN→UP via `set_link`.

5. **Guest carrier-watch service** (`kimage/build/build-rootfs.sh`). A tiny
   busybox openrc service that polls `/sys/class/net/eth0/carrier`; on a down→up
   transition it rebinds the virtio-net driver via sysfs (`unbind`→`bind`) so it
   re-probes and re-reads MAC Y, then `ifdown eth0; ifup eth0` (busybox udhcpc
   re-leases). Pure busybox shell — no new package, no custom MMIO device. (There
   is no udev in this rootfs; this service is the equivalent reaction path.)

### virtio-net needs no inner save/restore

Its only identity (MAC) comes from the live backend, and transport+queues are
already snapshotted generically. On restore it is rebuilt from a *fresh* vmnet
(MAC Y); when the guest rebinds the driver it resets the device (status→0 clears
queues) and re-negotiates, discarding the stale restored queue state. So nothing
device-specific is persisted — the generic `DeviceRecord` suffices.

## Data flow

- **Snapshot (net VM):** Ctrl-A s → set `stop_rx` → take net device mutex (drain
  in-flight inject) → read RAM + GIC + device records (generic) → write snapshot.
- **Restore:** read records → `setup_devices(Mode::Restore)` starts fresh vmnet
  (MAC Y), rebuilds virtio-net, spawns feeder → resume vCPU → VMM pulses link
  DOWN→UP → guest carrier-watch service rebinds the driver (adopts MAC Y) and
  re-DHCPs → connectivity.

## The open risk (spike-gated)

The carrier-watch + re-DHCP trigger is straightforward. The **MAC adoption** is
the uncertain part: a carrier bounce alone does not make the kernel re-read the
virtio MAC (it is cached at probe). Picking up MAC Y requires a driver re-probe,
which the carrier-watch service does via sysfs `unbind`→`bind`. Open questions the
spike must answer: does the busybox virtio-net rebind cleanly re-read MAC Y, and
is a carrier-watch poll a reliable enough restore signal (vs. an ordinary flap)?

**Spike during implementation:** boot a net VM, snapshot, restore, pulse link
DOWN→UP, confirm the carrier-watch service rebinds, adopts MAC Y, and re-DHCPs. If
reliable → done with pure busybox. If the rebind/poll proves unreliable → fall
back to reusing **vsock** (an existing device) as the out-of-band restore signal;
still no new device, no new package.

## Error handling

- vmnet start fails on restore (e.g. no `sudo`) → clear error, abort restore.
- A net record present but vmnet unavailable → fail loudly (do not silently drop
  the NIC).
- `stop_rx` must be honored before the RAM read; if the feeder can't be quiesced,
  abort the snapshot rather than write a torn image.

## Testing

- **Unit:** `device_features(0)` includes `VIRTIO_NET_F_STATUS`; config `status`
  reports LINK_UP; `set_link(false)`/`set_link(true)` flip the bit and set
  `interrupt_status & INT_STATUS_CONFIG` + pulse the SPI; `stop_rx` makes the
  feeder skip injects.
- **Integration:** the snapshot handler is installed for `smp == 1` with net;
  restore wiring starts a vmnet backend and places virtio-net at the saved
  base/SPI from a net record.
- **Spike (gates the design):** live boot→snapshot→restore→link-bounce; the
  carrier-watch service rebinds, the guest adopts MAC Y and re-DHCPs.
- **Live clone test:** restore one net snapshot into two instances; assert
  distinct MAC + IP; both reach the internet.

## Spike result (2026-06-13) — PASS

Live boot→snapshot→restore on Apple Silicon. After the VMM's link bounce, the
manual carrier-watch sequence worked:

- `unbind`→`bind` of the virtio_net driver made the guest **re-read the new MAC**
  (a fresh locally-administered MAC from the new vmnet interface); `eth0` kept its
  name. ✓
- `ifdown eth0; ifup eth0` → `udhcpc` obtained a fresh lease (`192.168.2.5`),
  `ping 8.8.8.8` ~12 ms. ✓

Findings folded into the plan:
- busybox `ip` has no `-br` flag — use plain `ip link/addr show`.
- A pre-existing `ifupdown-ng/dhcp: eval: ... syntax error` spam appears (the
  boot-time `ifup -a` hits it too); functionally harmless (lease still obtained).
  Tracked as a separate optional rootfs fix, not part of this work.
- The rebind itself flaps the carrier (down→up), which would re-trigger the
  watcher → **the carrier-watch service needs a cooldown after acting** to avoid a
  rebind loop.

Verdict: proceed with the busybox carrier-watch service (no vsock fallback needed).

## Status: implemented 2026-06-13

Via plan `docs/superpowers/plans/2026-06-13-vmnet-snapshot.md`. Host-side code
(F_STATUS + link-bounce, rx-feeder quiesce, `smp==1` gate, restore-net wiring) and
the guest carrier-watch service are in. The spike (above) verified the core
mechanism end-to-end. **2-clone live test PASSED** (rootfs rebuilt with the
carrier-watch service): both clones auto-reconnected on restore with no manual
step, got distinct IPs, and reached the internet.

## Scope / YAGNI

- single-vCPU snapshot only (unchanged); multi-vCPU net out.
- net restore requires `sudo` (vmnet).
- active connections reset on restore (link bounce — accepted).
- no new MMIO device, no new guest package — `F_STATUS` on the existing virtio-net
  + a tiny busybox carrier-watch openrc service (+ optional vsock fallback if the
  spike fails).
- suspend-assisted snapshot (PSCI `SYSTEM_SUSPEND` + kernel `PM_SLEEP`, using the
  kernel's own `.freeze`/`.restore` + resume hooks) explicitly out — a cleaner but
  much larger alternative model, documented as future work.
- host-side per-clone NAT / netns-equivalent (FC's primary scheme) out —
  unavailable on vmnet shared mode.
