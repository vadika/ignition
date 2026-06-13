# virtio-vsock E1 (guest→host stream) — Design

Date: 2026-06-13. Status: approved design, ready for an implementation plan.

## Context

Sub-project **E** of the "full device model" milestone (A framework, B rng, C RTC,
D balloon — all merged). vsock is the last device and the largest; it is split:

- **E1 (this spec)** — vsock core, guest→host streams over a host Unix socket:
  packet codec, connection state machine, credit flow-control, TX handling, and a
  poll-based RX reactor. Working bar: an in-guest process connects to a host port
  and exchanges bytes with a host listener.
- **E2 (TODO)** — host→guest: the `{uds}` control socket, `CONNECT {port}` command,
  guest-side listeners, host-initiated `REQUEST` injection. Built on E1.

The guest kernel already has `CONFIG_VIRTIO_VSOCKETS` (enabled in the kernel-config
commit). Reference: Firecracker's `src/vmm/src/devices/virtio/vsock/` (packet/csm/
unix muxer) — the protocol and credit model are ported; the epoll reactor is
replaced by ignition's reader-thread + `poll(2)` + self-pipe pattern (as virtio-net
does async RX).

### Existing pieces this builds on

- `VirtioDevice` trait + `VirtioMmio` transport (3-queue capable; `inject_rx` is the
  precedent for an async-RX device — net's reader thread holds `Arc<Mutex<VirtioMmio>>`
  and calls `VirtioMmio::inject_rx`, which drives `VirtioDevice::inject_rx`).
- `Virtqueue` (`pop_avail`, `push_used`), `DescChain`/`Desc`, `GuestRam`
  (`read_slice`/`write_slice`).
- `DeviceManager::add`/`add_restored`, the `virtio,mmio` FDT kind (vsock needs no new
  kind), and the boot harness reader-thread pattern.
- `libc` (poll, AF_UNIX) is a `devices` dependency.

## Goal

A guest can `connect(AF_VSOCK, cid=2, port=P)` and stream bytes to/from a host
process listening on a Unix socket `{uds}_{P}`, with correct credit flow-control so
neither side stalls or overruns. Demonstrable end to end.

Non-goals (TODOs): host→guest (E2); datagram (`SOCK_DGRAM`) — stream only;
snapshotting live connections; the EVENT queue (parked); multiple host UDS bases.

## Protocol constants

```
VIRTIO_ID_VSOCK        = 19
VSOCK_TYPE_STREAM      = 1
VMADDR_CID_HOST        = 2     // host side
GUEST_CID              = 3     // this guest (config.guest_cid)
VSOCK_HDR_SIZE         = 44

ops:  REQUEST=1  RESPONSE=2  RST=3  SHUTDOWN=4  RW=5  CREDIT_UPDATE=6  CREDIT_REQUEST=7
flags (SHUTDOWN): RCV=1  SEND=2
```

Header (little-endian, 44 bytes): `src_cid:u64, dst_cid:u64, src_port:u32,
dst_port:u32, len:u32, type:u16, op:u16, flags:u32, buf_alloc:u32, fwd_cnt:u32`.

## Architecture — module `crates/devices/src/virtio/vsock/`

### `packet.rs`

- Constants above.
- `VsockHeader` with typed getters/setters over a `[u8; 44]` (little-endian).
- TX side: given a `DescChain` + `GuestRam`, read the header from the first
  readable descriptor and expose the payload location(s) (the RW data follows the
  header in the chain). Provide `read_payload(mem, dst: &mut [u8]) -> usize`.
- RX side: a builder that writes a header + payload into a guest RX `DescChain`
  (header into the writable descriptor, then payload), returning total bytes
  written (used-ring `len`).

### `connection.rs` — `Connection`

Per established/connecting guest↔host stream:

```
struct Connection {
    host: UnixStream,           // non-blocking
    state: ConnState,           // PeerInit | Established | Closing | Closed
    // credit accounting (FC model):
    peer_buf_alloc: u32,        // guest's advertised rx buffer
    peer_fwd_cnt: u32,          // guest's flushed count (from its headers)
    fwd_cnt: u32,               // bytes we've flushed to the host stream
    rx_cnt: u32,                // bytes we've sent to the guest (RW)
    tx_cnt: u32,                // bytes we've received from the guest (RW)
    guest_port: u32, host_port: u32, // = (peer_port, local_port)
}
```

- `peer_free() = peer_buf_alloc - (rx_cnt - peer_fwd_cnt)` (Wrapping) — max bytes we
  may send to the guest right now.
- On guest `RW`: write payload to `host` (track `tx_cnt`); on `WouldBlock` buffer
  (a small per-conn tx buffer) and retry on writable.
- Host readable → read up to `min(peer_free, buf)` bytes → emit an `RW` RX packet;
  advance `rx_cnt`.
- `SHUTDOWN`(flags) → half-close the host stream direction(s); when both closed and
  tx drained → emit `RST`, state `Closed`.
- Provide credit: set `buf_alloc`/`fwd_cnt` on every RX packet; emit a proactive
  `CREDIT_UPDATE` when our `fwd_cnt` advanced materially or on `CREDIT_REQUEST`.

### `muxer.rs` — `Muxer`

```
struct Muxer {
    uds_base: PathBuf,                       // {uds}; per-port = {uds}_{port}
    conns: HashMap<(u32,u32), Connection>,   // (guest_port, host_port)
    rxq: VecDeque<VsockRxPacket>,            // pending control/data packets for the guest
    wake: WakePipe,                          // self-pipe write end (signals the reactor)
}
```

- `handle_tx(hdr, payload)`:
  - `REQUEST` (dst=host port P, src=guest port G): connect a non-blocking
    `UnixStream` to `{uds}_{P}`. Success → insert `Connection{PeerInit→Established}`,
    queue `RESPONSE`. Failure → queue `RST`. Signal `wake` (new fd to poll).
  - `RW`: look up conn by `(src,dst)`; write payload to host; update credit;
    queue a `CREDIT_UPDATE` if needed.
  - `CREDIT_REQUEST`: queue `CREDIT_UPDATE`.
  - `CREDIT_UPDATE`: absorb `buf_alloc`/`fwd_cnt`.
  - `SHUTDOWN`: drive the conn's close; queue `RST` when done; `wake`.
  - Unknown op / unknown conn for non-REQUEST → queue `RST`.
- `service_readable(fds)`: for each readable host fd, pull data into `RW` RX packets
  (credit-bounded); on EOF/error queue `SHUTDOWN`/`RST` and drop the conn.
- `fill_guest_rx(vq, mem) -> bool`: pop `rxq` packets into guest RX descriptors until
  the queue or `rxq` drains; `push_used`. Returns true if any packet delivered.
- `poll_fds() -> Vec<RawFd>`: snapshot of live connection fds for the reactor.

### `mod.rs` — `VsockDevice`

`VirtioDevice`:
- `device_id() = 19`, `device_features(_) = 0`, `queue_count() = 3`.
- `config_read`: 8-byte config, `guest_cid: u64 = 3` at offset 0.
- `handle_notify(queue_idx, vq, mem)`:
  - TX (queue 1): drain `pop_avail`; for each chain, parse header (+payload) and call
    `muxer.handle_tx`; `push_used`. After draining, attempt `fill_guest_rx` (responses
    to requests can go out immediately) — or rely on the reactor wakeup.
  - RX (queue 0): the guest just supplied RX buffers; call `muxer.fill_guest_rx`.
  - EVENT (queue 2): drain + `push_used(0)`, no action.
- `fill_rx(rx_vq, mem) -> bool` (the new defaulted `VirtioDevice` method): the reactor
  entry — `muxer.service_readable(...)` then `muxer.fill_guest_rx(rx_vq, mem)`.

Holds the `Muxer`. The device is `Send` (the `UnixStream`s and the map move with it).

### Transport hook (`mmio.rs`)

Add `VirtioDevice::fill_rx(&mut self, rx_vq: &mut Virtqueue, mem: &GuestRam) -> bool`
(default `false`) and `VirtioMmio::poll_vsock_rx(&mut self) -> bool` that locks the RX
queue (queue 0) and calls `self.dev.fill_rx(...)`, raising the used IRQ if it returns
true. (One defaulted trait method; mirrors `inject_rx`.)

### RX reactor (boot harness)

A reader thread holding `Arc<Mutex<VirtioMmio>>` + the read end of the wake self-pipe:

```
loop {
    let fds = { lock vsock; dev.muxer.poll_fds() };   // snapshot, then unlock
    poll(fds + wake_read, timeout=200ms);
    drain wake_read;
    { lock vsock; vsock.poll_vsock_rx(); }            // service readable + fill guest RX + IRQ
}
```

The TX path writes one byte to the wake pipe whenever it adds/closes a connection, so
a blocked `poll` re-snapshots the new fd set. The lock is never held across `poll`.
`poll` of a since-closed fd returns `POLLNVAL` → handled by re-snapshot. A 200 ms
timeout bounds latency for the snapshot race without busy-looping.

## Backend / CLI

`--vsock-uds <path>`: when set, vsock is wired (else not present). Guest `connect`
to host port `P` → `{path}_{P}`. Host listener creates `{path}_{P}` and accepts.
(Host→guest control socket `{path}` itself is E2.)

## Data flow summary

1. Guest opens AF_VSOCK to (cid=2, port=P): kernel sends `REQUEST` on TX.
2. Device connects `{uds}_{P}`; on success queues `RESPONSE` (state Established),
   `wake`s the reactor → `RESPONSE` delivered on RX → guest's `connect()` returns.
3. Guest `write()` → `RW` on TX → device writes to host stream.
4. Host writes → reactor reads → `RW` on RX (≤ guest credit) → guest `read()`.
5. Either side closes → `SHUTDOWN`/`RST`; conn removed; `wake`.

## Error handling

- Connect failure / unknown destination → `RST` to guest.
- Host stream EOF or error → `SHUTDOWN` then `RST`; drop the connection.
- Short/malformed header (< 44 bytes in the chain) → drop the packet.
- Unknown op → ignore (or `RST` for an unknown established conn).
- Guest credit exhausted (`peer_free == 0`) → stop emitting RW until a
  `CREDIT_UPDATE`/credit advance; never overrun the guest.
- `poll`/`read`/`write` `EINTR`/`WouldBlock` → retry/back off, never panic.

## Snapshot

E1 does not serialize live connections. A `VsockDevice` restores with an empty
muxer; any open connections at snapshot time are lost (documented TODO). The device
is wired only when `--vsock-uds` is given, so a no-vsock snapshot is unaffected; a
vsock snapshot restores the device with no live conns. No snapshot-version bump.

## Testing

Unit (no entitlement; use `socketpair`/`UnixStream::pair` as the host side):
1. **Packet codec**: build a 44-byte header with all fields, parse it back — every
   field round-trips; payload read/write over a `GuestRam`-backed chain.
2. **Connection REQUEST→Established**: feed a `REQUEST`; with a connectable mock host
   (a pre-bound `UnixListener` on a temp `{uds}_{P}`), assert a `RESPONSE` is queued
   and state is Established; a connect to a missing path queues `RST`.
3. **RW guest→host**: established conn + `RW` payload → bytes appear on the host
   `UnixStream`; `tx_cnt` advances.
4. **RW host→guest + credit**: write bytes to the host end → `service_readable` +
   `fill_guest_rx` emit an `RW` packet into a guest RX chain with the right payload;
   `peer_free` caps the emitted length (set `peer_buf_alloc` small, assert capping).
5. **SHUTDOWN**: `SHUTDOWN` → conn closes, `RST` queued.
6. **Identity/config**: `device_id()==19`, `queue_count()==3`, `config_read` yields
   `guest_cid == 3`.

Live (drivers + interactive): host listener on `{uds}_1234`; in the guest run a vsock
client (`socat - VSOCK-CONNECT:2:1234`, or a small busybox/python connect) and echo a
string round-trip; `dmesg | grep -i vsock` shows `virtio_vsock` bound;
`scripts/restore_test.py`/`restore_clone_test.py` still pass.

## File structure

- Create `crates/devices/src/virtio/vsock/{packet.rs, connection.rs, muxer.rs, mod.rs}`.
- Modify `crates/devices/src/virtio/mod.rs` — `pub mod vsock;`.
- Modify `crates/devices/src/virtio/mmio.rs` — `VirtioDevice::fill_rx` (default) +
  `VirtioMmio::poll_vsock_rx`.
- Modify `spike/src/bin/boot.rs` — `--vsock-uds` flag, wire `VsockDevice` (when set),
  spawn the RX reactor thread, restore arm.

End state: a guest process streams bytes to/from a host listener over vsock with
credit flow-control; the device binds (`virtio_vsock`) and completes the guest→host
half of Firecracker's vsock. E2 (host→guest) and connection-snapshot are TODOs.
