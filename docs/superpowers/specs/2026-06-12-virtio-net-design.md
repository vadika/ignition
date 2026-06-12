# virtio-net (guest networking) — design

Date: 2026-06-12. Milestone: give the guest a network via a virtio-net device backed
by Apple's vmnet.framework in shared/NAT mode, so a real aarch64 Linux on
ignition/HVF can DHCP an address and reach the internet.

## Goal

`sudo boot --net <Image> <rootfs>` brings up `eth0`: the guest DHCPs an IP from
vmnet, pings the gateway, pings an external host (`ping 8.8.8.8`), and resolves DNS
(`nslookup`/`wget` a hostname). Full virtio-net TX/RX + vmnet NAT + DNS, end to end.

## Host backend: vmnet.framework (shared/NAT)

Apple's native VM networking. `VMNET_SHARED_MODE` gives the guest a NAT'd interface
with a built-in DHCP server and NAT to the host's network/internet — no userspace
TCP/IP stack to write. Requires running under **sudo** (the
`com.apple.vm.networking` entitlement is Apple-restricted and does NOT work via
ad-hoc codesign, unlike the hypervisor entitlement). vmnet is callback/dispatch
based (libdispatch / GCD).

## What exists / what's new

- The virtio-mmio transport (`crates/devices/src/virtio/mmio.rs`), split virtqueue
  (`queue.rs`), `GuestRam`, and `IrqLine` are in place but **blk-specific**
  (`DEVICE_ID_BLK`, a single queue, a `blk: VirtioBlk` field, synchronous
  notify→process). They need generalizing.
- The async injection pattern is proven by serial RX (a host thread feeding a device
  + raising the GIC line). virtio-net RX reuses that shape.
- New: a vmnet FFI crate, the virtio-net device, the `VirtioDevice` trait
  generalization of the transport, and the `--net` harness wiring.

## Approach: generalize the transport

Extract a `VirtioDevice` trait that both blk and net implement, and make
`VirtioMmio` generic/dyn over it (chosen over duplicating ~200 lines of mmio
register handling into a separate `VirtioNetMmio`, which would drift). Blk migrates
to implement the trait; net implements it with two queues.

## Components

### `crates/vmnet` (new) — isolated vmnet.framework FFI

Mirrors how `hvf` isolates the hypervisor FFI. `build.rs` links `vmnet` + libdispatch.

- `VmnetBackend::start() -> Result<VmnetBackend, Error>`:
  `vmnet_start_interface(xpc_config, dispatch_queue, handler)` with
  `vmnet_operation_mode_key = VMNET_SHARED_MODE`; the async handler reports status,
  the assigned MAC, MTU, and DHCP params. Fails clearly if not privileged.
- RX: `vmnet_interface_set_event_callback(VMNET_INTERFACE_PACKETS_AVAILABLE, queue,
  cb)`; the callback calls `vmnet_read(iface, &pktdesc, &count)` and **sends each
  frame's bytes over an `mpsc::Sender<Vec<u8>>`** — it never touches the virtio
  device (keeps the dispatch callback fast and lock-free).
- TX: `write_frame(&[u8])` → `vmnet_write(iface, &pktdesc, &count)`.
- Exposes a `NetBackend` trait so the device is testable without vmnet:

```rust
pub trait NetBackend: Send {
    /// Send one ethernet frame to the host network.
    fn write_frame(&self, frame: &[u8]) -> std::io::Result<()>;
    /// The MAC the guest should advertise (vmnet-assigned).
    fn mac(&self) -> [u8; 6];
}
```

The RX `mpsc::Receiver<Vec<u8>>` is handed to the harness, which runs the RX thread.

### `crates/devices/src/virtio/net.rs` (new) — the virtio-net device

Generic over `NetBackend`; implements `VirtioDevice`. Two queues: **RX = 0**,
**TX = 1**. Minimal features: `VIRTIO_NET_F_MAC` only (no checksum/GSO/mergeable-rx
offload — vmnet handles raw ethernet, so each packet is one buffer).

- Every packet carries a 12-byte `virtio_net_hdr` (with `num_buffers`); with offload
  off it is zeroed.
- **TX** (`handle_notify(1)`, vcpu thread): drain each TX chain — read the
  descriptors, skip the 12-byte header, `backend.write_frame(frame)`, push to the
  used ring. Returns `true` to pulse the IRQ.
- **RX** (`inject_rx(frame)`, RX thread): pop a free RX (queue 0) descriptor chain;
  write `[zeroed virtio_net_hdr | frame]` into the guest buffers; push to the used
  ring; signal that the IRQ must be raised. If the RX queue has no free buffers,
  drop the frame and bump a counter.
- `config_read` returns the 6-byte MAC (from `backend.mac()`).

### `crates/devices/src/virtio/mmio.rs` (refactor) — `VirtioDevice` trait

```rust
pub trait VirtioDevice: Send {
    fn device_id(&self) -> u32;                 // blk=2, net=1
    fn features(&self) -> u64;
    fn config_read(&self, offset: u64, data: &mut [u8]);
    fn queue_count(&self) -> usize;             // blk=1, net=2
    /// Service a QueueNotify on `queue_idx`; returns whether to raise the IRQ.
    fn handle_notify(&mut self, queue_idx: usize, mem: &GuestRam) -> bool;
}
```

`VirtioMmio` holds `dev: Box<dyn VirtioDevice>` and a `Vec<Virtqueue>` sized by
`queue_count()`; `QueueSel` (0x030) selects the active queue for the queue-config
registers; `QueueNotify` (0x050) dispatches to `dev.handle_notify(idx, mem)`. Blk
migrates verbatim into a `VirtioDevice` impl (device_id 2, one queue, the existing
`process`). The net device owns its queues' access for RX injection (the device, not
the transport, holds the `Virtqueue`s so the RX thread can reach them — exact
ownership split decided in the plan; the transport reads/writes queue config through
the device).

### `spike/src/bin/boot.rs` (wiring)

- `--net` flag (opt-in; without it nothing changes and no sudo is needed).
- When set: `VmnetBackend::start()`, build the net device over it, register a third
  virtio-mmio window (a new `NET_BASE`/`NET_SPI` in `layout.rs`), add a
  `virtio_mmio` FDT node for it, and spawn the **RX thread**: `for frame in
  rx_receiver { net.lock().inject_rx(frame) → pulse the net GIC line }`.
- The RX thread coexists with the serial reader thread, the vcpu thread(s), and the
  `TermiosGuard`.

## Concurrency

- The net device is `Arc<Mutex<…>>`: TX (vcpu thread via bus MMIO) and RX inject
  (RX thread) both take the lock briefly; the `Virtqueue` indices are the shared
  state it serializes.
- The vmnet GCD callback only does `mpsc` sends — no device lock — so the dispatch
  queue never blocks on guest work.
- **`InterruptStatus` (0x60) accumulates:** net raises the line from both
  TX-completion and async RX, so the used-buffer-notification bit is sticky —
  OR-set on any used-ring update from either queue/thread, cleared only on guest ACK
  (0x64). This generalizes the current single-in-flight blk latch (noted at
  `mmio.rs:164`).

## Error handling

- vmnet start failure (not privileged / no entitlement) → a clear message
  ("virtio-net needs sudo for vmnet shared mode") and exit; non-net boots are
  unaffected.
- RX queue full → drop frame + counter (TCP/UDP retransmit recovers). Malformed or
  oversized TX descriptor, or a `write_frame` error → drop + `log::warn`, keep
  running.
- `--net` without sudo → the vmnet start error path above.

## Testing

- **Unit (no vmnet, `FakeBackend` capturing/yielding frames):**
  - TX: enqueue a `[hdr | frame]` chain, `handle_notify(1)` → assert `FakeBackend`
    received the frame with the 12-byte header stripped; used ring advanced.
  - RX: `inject_rx(frame)` against a programmed RX queue → assert the guest buffer
    holds `[zeroed virtio_net_hdr | frame]`, the used ring advanced, and the IRQ was
    signalled. RX-queue-empty → frame dropped, counter bumped.
  - `features() == 1 << VIRTIO_NET_F_MAC`; `config_read` returns `backend.mac()`.
- **Unit (transport):** existing blk-via-mmio tests pass after the `VirtioDevice`
  generalization; add a net-via-mmio `QueueNotify` test with `FakeBackend`.
- **Integration (the bar), piped console under sudo:** `sudo target/debug/boot --net
  kimage/out/Image kimage/out/rootfs.ext4` → `udhcpc eth0` gets an IP, `ping -c1`
  the gateway, `ping -c1 8.8.8.8`, and `nslookup`/`wget` a hostname all succeed. The
  vmnet FFI is exercised here (not unit-testable — needs the framework + privileges).

Guest-side: the rootfs must run a DHCP client on `eth0` (kimage's responsibility,
like the SMP kernel config and the getty).

## Out of scope

- **Offloads** (checksum, TSO/GSO, mergeable RX buffers) — `VIRTIO_NET_F_MAC` only;
  one buffer per packet, zeroed header.
- **Multiple NICs / bridged or host-only vmnet modes** — single shared-mode NIC.
- **Inbound port forwarding** and throughput tuning — outbound NAT reachability is
  the bar.
- **virtio-net control queue** (`VIRTIO_NET_F_CTRL_VQ`) — not negotiated.
