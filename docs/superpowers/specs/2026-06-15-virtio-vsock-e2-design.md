# virtio-vsock E2 (host→guest stream) — Design

Date: 2026-06-15. Status: approved design, ready for an implementation plan.

## Context

Sub-project **E** of the device-model milestone. E1 (guest→host streams over a host
Unix socket) is shipped: packet codec, `Connection` state machine, credit
flow-control, TX handling, and a poll-based RX reactor. E2 completes vsock by adding
the **host→guest** direction, built entirely on E1's pieces.

E1 reference: `docs/superpowers/specs/2026-06-13-virtio-vsock-e1-design.md`.
Existing code: `crates/devices/src/virtio/vsock/{packet.rs, connection.rs, muxer.rs, mod.rs}`.

In E1 a connection is born `Established` because the host-side `UnixStream::connect`
succeeds synchronously inside `handle_tx(OP_REQUEST)`. E2 introduces the opposite
flow: a **host** process initiates, and the connection must wait for the guest's
`RESPONSE` before it is established — a new `LocalInit` state.

The protocol is **Firecracker's hybrid vsock** control protocol, wire-identical, so
`socat ... UNIX-CONNECT:{uds}` and existing Firecracker tooling work unmodified.

### Existing pieces this builds on

- `Connection` (host↔guest byte streaming, credit accounting) — reused unchanged for
  the data phase; only a new birth-state and a confirm transition are added.
- `Muxer` — `conns: HashMap<(guest_port,host_port), Connection>`, `rxq`, `handle_tx`,
  `service`, `poll_set`, `pop_rx`, `seed_rst`/`save_conns` (snapshot).
- The boot-harness RX reactor thread (`poll(2)` + self-pipe pattern) and
  `VirtioMmio::poll_vsock_rx`.
- `--vsock-uds <path>`: E1 uses per-port paths `{path}_{port}` (host listens there for
  guest→host). The base path `{path}` itself was explicitly reserved for E2's control
  socket. No new CLI flag.

## Goal

A host process connects to the control socket `{uds}`, issues `CONNECT <guest_port>`,
and — if a guest process is listening on that vsock port — gets a bidirectional byte
stream into the guest, with correct credit flow-control. Demonstrable end to end with
`socat` on the host and a vsock listener in the guest.

Non-goals (unchanged TODOs): the EVENT queue (parked); serializing live connections
across snapshot (restored conns are RST'd, as in E1); datagram (`SOCK_DGRAM`).

## Hybrid control protocol (Firecracker-exact)

```
host → device:  CONNECT <guest_port>\n      // ASCII, decimal port
device → host:  OK <assigned_host_port>\n   // on guest RESPONSE; then raw bytes
device → host:  (connection closed)         // on guest RST / no listener / error
```

After `OK`, the same host `UnixStream` carries raw payload bytes both directions
(it becomes the connection's data stream — there is no separate data socket for the
host-initiated direction).

## Architecture

### Data flow (the new direction)

1. Host connects to `{uds}`, sends `CONNECT 1234\n`.
2. Muxer allocates an ephemeral `host_port`, inserts a `Connection` in `LocalInit`
   (carrying that host `UnixStream`), and queues a `REQUEST` RX packet
   (`src_cid=host, src_port=host_port, dst_cid=guest, dst_port=1234`) to the guest.
3. Guest has a listener on 1234 → kernel sends `RESPONSE` on TX. Muxer matches the
   `LocalInit` conn, transitions it to `Established`, and writes `OK <host_port>\n`
   to the host stream. Bytes then flow via the existing E1 RW machinery.
4. Guest has no listener on 1234 → kernel sends `RST` on TX. Muxer drops the conn and
   closes the host stream (no `OK`).

Guest→host (E1) is unchanged and coexists: E1 conns are born `Established`; E2 conns
pass through `LocalInit` first. Both live in the same `conns` map and stream through
the same `Connection` code once `Established`.

### `connection.rs`

- Add `ConnState::LocalInit`.
- `Connection::new_local_init(guest_port, host_port, host: UnixStream) -> Connection`
  — born `LocalInit`; `host` is the accepted control stream (set non-blocking),
  reused as the data stream after confirmation. Credit fields start zeroed and are
  populated by `confirm_established`.
- `confirm_established(&mut self, peer_buf_alloc: u32, peer_fwd_cnt: u32)` —
  `LocalInit → Established`, absorbing the guest's advertised credit from its
  `RESPONSE` header.
- Byte movement is gated on `Established`: `flush_tx` and `read_host` return early
  (no-op / `None`) while `LocalInit`, so no data is forwarded before `OK`.

### `muxer.rs`

New state:
- `next_host_port: u32` — ephemeral allocator, starts at `1024`, increments, skips any
  port currently present as a `host_port` in `conns` (wraps back to `1024` past
  `u32::MAX`, still skipping live ports).
- `ctrl_streams: HashMap<RawFd, ControlStream>` — host clients accepted on `{uds}` whose
  `CONNECT` line is not yet complete. `ControlStream` buffers partial input until `\n`.

New / changed methods:
- `accept_control(&mut self, stream: UnixStream)` — set non-blocking, insert into
  `ctrl_streams` keyed by fd. (The listener `accept()` itself happens in the reactor.)
- `read_control(&mut self, fd: RawFd)` — read available bytes into the buffer. On a
  complete `CONNECT <port>\n`:
  - parse the decimal port; on parse failure or a non-`CONNECT` verb → remove and
    drop the stream (close), no conn, no packet.
  - allocate `host_port` via `next_host_port`; move the `UnixStream` out of
    `ctrl_streams` into a `Connection::new_local_init`; insert into `conns`; queue a
    `REQUEST` packet to the guest.
  - on host EOF/error before the line completes → remove and drop the stream.
- `handle_tx` `OP_RESPONSE` arm (today a no-op at the `/* host->guest connect ack — E2 */`
  comment): look up the `(guest_port, host_port)` conn; if `LocalInit`,
  `confirm_established(hdr.buf_alloc, hdr.fwd_cnt)` and write `OK <host_port>\n` to its
  stream. If no matching `LocalInit` conn → ignore.
- `handle_tx` `OP_RST`/`OP_SHUTDOWN` already removes the conn and closes its stream;
  for a `LocalInit` conn this correctly drops it with no `OK` (the existing arm calls
  `conn.close()` then queues a guest-facing RST — harmless for a host-initiated conn
  the guest just rejected; the guest ignores an RST for an unknown conn).
- `poll_set()` extends to include every `ctrl_streams` fd (so partial `CONNECT` lines
  wake the reactor). `LocalInit`/`Established` conn data fds are already covered (they
  are in `conns`). The control **listener** fd is owned and polled by the reactor, not
  the muxer.

### `mod.rs` — `VsockDevice`

- Hold an `Option<UnixListener>` for `{uds}` (bound when `--vsock-uds` is set; `None`
  otherwise — e.g. unit tests that drive the muxer directly).
- No queue-count change (still 3; EVENT parked).
- `service` / `fill_rx` behavior unchanged beyond what the muxer now queues.

### `mmio.rs`

No new trait method. The reactor uses the existing `service` + `poll_vsock_rx`.

### RX reactor (boot harness, `spike/src/bin/boot.rs`)

Extend the existing reader thread:

```
listener_fd = vsock control listener ({uds}), non-blocking
loop {
    fds = { lock vsock; muxer.poll_set() } + listener_fd + wake_read
    poll(fds, timeout=200ms)
    drain wake_read
    lock vsock:
        if listener_fd readable: loop accept() (non-blocking) → muxer.accept_control(s)
        for each readable ctrl_streams fd: muxer.read_control(fd)
        vsock.poll_vsock_rx()   // service() + fill guest RX + IRQ
    unlock
}
```

Lock is never held across `poll`. `accept` drains until `WouldBlock`. A since-closed
fd yields `POLLNVAL` → handled by the next `poll_set` re-snapshot.

## Backend / CLI

`--vsock-uds <path>` (existing): when set, additionally bind a `UnixListener` on
`{path}` for the control protocol. Per-port paths `{path}_{port}` remain the E1
guest→host listeners. A stale `{path}` socket file is removed before bind.

## Snapshot

No new work and no version bump. `save_conns` already enumerates all conn keys
(`LocalInit` and `Established` alike); `seed_rst` queues one guest-facing RST per key
on the first `service()` after restore. Host control streams do not survive a
snapshot. The control listener is re-bound from `--vsock-uds` on restore exactly as on
a fresh boot. In-flight `ctrl_streams` (host clients mid-`CONNECT`) are not part of a
snapshot and are simply absent after restore.

## Error handling

- Malformed / non-`CONNECT` control line → close the host stream; no conn, no packet.
- `CONNECT` to a guest port with no listener → guest `RST` → drop conn, close host
  stream, no `OK`.
- Host disconnects during `LocalInit` (after `REQUEST`, before `RESPONSE`) → drop the
  conn; the guest's eventual `RESPONSE`/`RST` finds no conn and is ignored.
- Host-port allocator collision with a live conn → skip to the next free port.
- Partial `CONNECT` line split across reads → buffered in `ControlStream` until `\n`.
- `accept`/`read`/`write` `EINTR`/`WouldBlock` → retry / back off, never panic.

## Testing

Unit (no entitlement; `UnixStream::pair` / `UnixListener` for host/guest sides):

1. **CONNECT parse** — `read_control` fed `CONNECT 1234\n`: a `REQUEST` is queued with
   `dst_port == 1234`, the conn is `LocalInit`, `host_port >= 1024`.
2. **Partial line** — feed `CONN` then `ECT 1234\n` in two `read_control` calls:
   exactly one `REQUEST` after the newline, nothing queued before it.
3. **RESPONSE → OK** — a `LocalInit` conn + a guest `RESPONSE` header: conn becomes
   `Established` and the control stream receives `OK <host_port>\n`.
4. **No guest listener** — `LocalInit` + guest `RST`: conn dropped, no `OK` written.
5. **Malformed line** — `HELLO\n`: stream closed, no conn, no packet queued.
6. **Distinct host ports** — two CONNECTs allocate two different `host_port`s.
7. **Bidirectional RW once established** — drive E1's RW path on an E2-established
   conn: host→guest and guest→host payloads both transfer; credit advances.

Live (driver + interactive): host `socat - UNIX-CONNECT:{uds}` then type
`CONNECT 1234`; guest runs `socat VSOCK-LISTEN:1234,fork -` (or a small python vsock
listener); echo a string each direction. `scripts/restore_test.py` and
`scripts/restore_clone_test.py` stay green. New `scripts/vsock_e2_test.py` automates
the host→guest round trip.

## File structure

- Modify `crates/devices/src/virtio/vsock/connection.rs` — `ConnState::LocalInit`,
  `new_local_init`, `confirm_established`, `Established`-gating in `flush_tx`/`read_host`.
- Modify `crates/devices/src/virtio/vsock/muxer.rs` — `next_host_port`, `ctrl_streams`
  + `ControlStream`, `accept_control`, `read_control`, `OP_RESPONSE` arm, `poll_set`
  extension.
- Modify `crates/devices/src/virtio/vsock/mod.rs` — optional control `UnixListener`,
  wiring.
- Modify `spike/src/bin/boot.rs` — bind `{uds}` listener, reactor accept + read_control.
- Create `scripts/vsock_e2_test.py` — host→guest live round-trip driver.

## End state

A host process opens a stream into a listening guest process over vsock via the
Firecracker hybrid control protocol, with credit flow-control, completing the
host→guest half of vsock. EVENT queue and live-connection snapshot remain TODOs.
This unblocks the adoption-track control-plane designs that talk *into* clones.
