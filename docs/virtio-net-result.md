# virtio-net milestone — implementation complete; live run pending

Date: 2026-06-12. Status: **code complete, reviewed, builds + unit-tests green.**
The final `sudo` networking integration (DHCP + ping + DNS) must be run by hand
(passwordless sudo is unavailable to the automation, and the rootfs DHCP client is
kimage-side).

## What landed

- **Transport generalized** (`crates/devices/src/virtio/mmio.rs`): `VirtioDevice`
  trait (`device_id`/`device_features`/`config_read`/`queue_count`/`handle_notify`/
  `inject_rx`); `VirtioMmio` over `Box<dyn VirtioDevice>` + per-queue `QueueState`;
  QueueNotify-value-as-index; hardened feature-sel clamp + QueueReady invariant. Blk
  migrated onto the trait verbatim.
- **virtio-net device** (`crates/devices/src/virtio/net.rs`): generic over a
  `NetBackend` trait. TX (exit-driven): drain the TX queue, strip the 12-byte
  `virtio_net_hdr`, `write_frame`; drops oversized chains. RX (async): prepend a
  header (`num_buffers=1`, rest zeroed), write into a free RX buffer, raise the IRQ;
  drops + counts when the RX queue is empty. Features = `VIRTIO_NET_F_MAC` only.
- **vmnet backend** (`crates/vmnet`): vmnet.framework shared/NAT mode via a C shim
  (`vmnet_shim.c`) that hides the Objective-C block ABI. RX callback is panic-guarded
  and read-size-clamped; frames flow over an `mpsc` channel. `VmnetBackend: NetBackend`.
- **Harness** (`spike/src/bin/boot.rs` + `layout.rs`): `--net` flag (opt-in);
  second virtio-mmio window `NET_BASE=0x0a00_0200`, `NET_SPI=2` (INTID 34); a
  `FdtDevice::VirtioNet` FDT node; an RX thread draining vmnet into the device.

## Verification done

- Unit: 26 device tests (incl. 4 net: features/MAC, TX-strip, RX-prepend, RX-drop) +
  26 arch tests; workspace builds; 0 clippy.
- Reviews (spec + code-quality per task, opus on the concurrency + FFI + final):
  caught and fixed the QueueReady invariant, the oversized-TX truncation, the FFI
  panic-across-boundary + read clamp, the stale GuestRam threading invariant, the
  `num_buffers` value, and the misleading `FdtDevice::VirtioBlk`-for-NIC node.
- No-`--net` regression: still boots to login (login=1).
- The `--net` path is wired: without sudo it fails cleanly with
  "vmnet_start_interface failed (run under sudo for shared mode)".

## To finish — run by hand (needs sudo)

1. Verify vmnet itself starts:
   `sudo target/debug/vmnet-smoke` → expect `vmnet up: mac [..]`.
2. Full boot (the bar) — `eth0` DHCP, ping gateway, ping 8.8.8.8, DNS. The rootfs
   must run a DHCP client on `eth0` (kimage side). Re-sign first if rebuilt:
   `./scripts/sign.sh target/debug/boot`, then `sudo target/debug/boot --net
   kimage/out/Image kimage/out/rootfs.ext4` and at the shell:
   `ip link set eth0 up && udhcpc -i eth0`; `ip addr show eth0`; `ping -c1 8.8.8.8`;
   `nslookup example.com` (or `wget -qO- http://example.com`).

## Most-likely failure point (from the final review)

If DHCP DISCOVER goes out (vmnet logs a lease attempt) but no OFFER is processed,
suspect the **net RX interrupt reaching the guest** (GIC SPI 2 / INTID 34) — the
virtio data path is unit-proven and TX/blk use the same `set_spi` mechanism, so the
risk is SPI-34 delivery, not the virtio logic. Second-most-likely is purely
guest-side: no DHCP client on `eth0`.

## Follow-ups

- The FDT `create_virtio_node` is shared by `VirtioBlk`/`VirtioNet` variants (fine —
  the kernel reads the device id from the mmio registers; a NIC-specific node can
  diverge later).
- No offloads / no mergeable-RX / no control queue (out of scope; `F_MAC` only).
- vmnet `VmnetBackend` has no `Drop`/`vmnet_stop_interface` (process-lifetime
  singleton; teardown would need callback de-registration first to avoid a UAF).
