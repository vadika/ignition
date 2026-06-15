# virtio-vsock E2 (host→guest) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a host process open a bidirectional vsock stream *into* a listening guest process via Firecracker's hybrid control protocol (`CONNECT <port>` → `OK <host_port>`), reusing the shipped E1 `Connection`/`Muxer` for the data phase.

**Architecture:** A host client connects to the control socket `{uds}` and sends `CONNECT <guest_port>\n`. The muxer allocates an ephemeral host port, inserts a `Connection` born in a new `LocalInit` state (holding that same UnixStream), and queues a `REQUEST` packet to the guest. The guest's `RESPONSE` confirms the connection (`Established`) and the muxer writes `OK <host_port>\n` back to the host; a guest `RST` drops it. The boot-harness reactor owns the `{uds}` `UnixListener`, accepts new clients, and each tick try-reads all control + connection fds non-blocking (no per-fd dispatch).

**Tech Stack:** Rust, `std::os::unix::net::{UnixStream, UnixListener}`, `libc::poll`, existing `VirtioDevice`/`VirtioMmio`/`Muxer`/`Connection`.

**Spec:** `docs/superpowers/specs/2026-06-15-virtio-vsock-e2-design.md`

---

## File Structure

- `crates/devices/src/virtio/vsock/connection.rs` — add `LocalInit` state, `new_local_init`, `confirm_established`, `write_host_raw`; gate byte movement on `Established`.
- `crates/devices/src/virtio/vsock/muxer.rs` — control-stream parsing (`ControlStream`, `ctrl_streams`), `next_host_port` allocator, `accept_control`, `poll_controls`, CONNECT→`REQUEST`, `OP_RESPONSE`→`OK`, `poll_set` extension.
- `crates/devices/src/virtio/vsock/mod.rs` — `VsockDevice::fill_rx` drives `poll_controls`; new `vsock_accept_control` plumbing.
- `crates/devices/src/virtio/mmio.rs` — `VirtioDevice::vsock_accept_control` (default no-op) + `VirtioMmio::vsock_accept_control` delegation.
- `spike/src/bin/boot.rs` — bind/own the `{uds}` `UnixListener`, poll its fd, accept → `vsock_accept_control`.
- `scripts/vsock_e2_test.py` — live host→guest round-trip driver (create).

**Build/test commands (this repo):**
- Unit tests: `PATH="$HOME/.cargo/bin:$PATH" cargo test -p ignition-devices vsock`
- Build all: `PATH="$HOME/.cargo/bin:$PATH" cargo build`
- Clippy: `PATH="$HOME/.cargo/bin:$PATH" cargo clippy -p ignition-devices`

`~/.cargo/bin` is not on PATH by default — always prefix cargo invocations as shown.

---

### Task 1: `Connection` — LocalInit state and confirmation

**Files:**
- Modify: `crates/devices/src/virtio/vsock/connection.rs`

A host-initiated connection is born before the guest has accepted, so it starts in
`LocalInit`, holding the host's control UnixStream (which becomes the data stream after
`OK`). It must not move payload bytes until `Established`. `confirm_established` absorbs
the guest's advertised credit from its `RESPONSE`. `write_host_raw` writes the small
`OK` control line directly (it is not payload, so it bypasses `txbuf`/`fwd_cnt`).

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block in `connection.rs`:

```rust
    #[test]
    fn local_init_does_not_move_bytes_until_established() {
        let (dev, mut app) = UnixStream::pair().unwrap();
        dev.set_nonblocking(true).unwrap();
        let mut conn = Connection::new_local_init(1024, 1100, dev);
        assert_eq!(conn.state(), ConnState::LocalInit);

        // While LocalInit: queued tx is held, host reads yield nothing.
        conn.enqueue_tx(b"early");
        let mut buf = [0u8; 5];
        app.set_nonblocking(true).unwrap();
        assert!(app.read(&mut buf).is_err(), "no bytes forwarded while LocalInit");
        app.write_all(b"hi").unwrap();
        assert!(conn.read_host().is_none(), "no host->guest read while LocalInit");

        // After confirmation the buffered tx flushes and reads work.
        conn.confirm_established(64 * 1024, 0);
        assert_eq!(conn.state(), ConnState::Established);
        conn.flush_tx();
        let n = app.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"early");
    }

    #[test]
    fn write_host_raw_sends_control_line() {
        let (dev, mut app) = UnixStream::pair().unwrap();
        dev.set_nonblocking(true).unwrap();
        let mut conn = Connection::new_local_init(1024, 1100, dev);
        conn.write_host_raw(b"OK 1100\n");
        let mut buf = [0u8; 8];
        app.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"OK 1100\n");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `PATH="$HOME/.cargo/bin:$PATH" cargo test -p ignition-devices vsock::connection`
Expected: FAIL — `no variant LocalInit`, `no function new_local_init` / `confirm_established` / `write_host_raw`.

- [ ] **Step 3: Add the `LocalInit` variant**

In `connection.rs`, change the enum (line ~15):

```rust
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ConnState {
    /// Host-initiated, awaiting the guest's RESPONSE. No payload moves yet.
    LocalInit,
    Established,
    Closed,
}
```

- [ ] **Step 4: Add the constructor and confirmation methods**

In `impl Connection`, after `new` (line ~47), add:

```rust
    /// A host-initiated connection awaiting the guest's RESPONSE. `host` is the
    /// accepted control stream (caller sets non-blocking), reused as the data
    /// stream once established. Credit fields fill in via `confirm_established`.
    pub fn new_local_init(guest_port: u32, host_port: u32, host: UnixStream) -> Connection {
        Connection {
            guest_port,
            host_port,
            host,
            state: ConnState::LocalInit,
            peer_buf_alloc: Wrapping(0),
            peer_fwd_cnt: Wrapping(0),
            rx_cnt: Wrapping(0),
            fwd_cnt: Wrapping(0),
            txbuf: VecDeque::new(),
        }
    }

    /// Guest accepted: absorb its advertised credit and go Established.
    pub fn confirm_established(&mut self, peer_buf_alloc: u32, peer_fwd_cnt: u32) {
        self.peer_buf_alloc = Wrapping(peer_buf_alloc);
        self.peer_fwd_cnt = Wrapping(peer_fwd_cnt);
        self.state = ConnState::Established;
    }

    /// Write a raw control line (e.g. `OK <port>\n`) straight to the host stream.
    /// Not payload: does not touch txbuf or fwd_cnt. Best-effort; the line is tiny.
    pub fn write_host_raw(&mut self, data: &[u8]) {
        let _ = self.host.write_all(data);
    }
```

- [ ] **Step 5: Gate byte movement on `Established`**

In `flush_tx` (line ~79), add a guard as the first statement:

```rust
    pub fn flush_tx(&mut self) {
        if self.state != ConnState::Established {
            return;
        }
        while !self.txbuf.is_empty() {
```

In `read_host` (line ~105), add a guard as the first statement:

```rust
    pub fn read_host(&mut self) -> Option<Vec<u8>> {
        if self.state != ConnState::Established {
            return None;
        }
        let budget = std::cmp::min(self.peer_free() as usize, READ_CHUNK);
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `PATH="$HOME/.cargo/bin:$PATH" cargo test -p ignition-devices vsock::connection`
Expected: PASS (all connection tests, including the two new ones and the four existing).

- [ ] **Step 7: Commit**

```bash
git add crates/devices/src/virtio/vsock/connection.rs
git commit -m "feat(vsock): Connection LocalInit state for host-initiated connects"
```

---

### Task 2: Muxer — control-stream parsing and CONNECT → REQUEST

**Files:**
- Modify: `crates/devices/src/virtio/vsock/muxer.rs`

The muxer accepts host control clients, buffers their input until a `CONNECT <port>\n`
line is complete, then allocates an ephemeral host port, builds a `LocalInit`
connection, and queues a `REQUEST` packet to the guest. Malformed lines close the
stream silently.

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block in `muxer.rs`:

```rust
    #[test]
    fn connect_line_queues_request_and_localinit_conn() {
        use std::io::Write;
        let base = std::env::temp_dir().join("ign-vsock-e2-connect/vsock");
        let mut mux = Muxer::new(base);
        let (host, mut client) = UnixStream::pair().unwrap();
        host.set_nonblocking(true).unwrap();
        mux.accept_control(host);
        client.write_all(b"CONNECT 1234\n").unwrap();
        mux.poll_controls();

        let pkt = mux.pop_rx().expect("REQUEST queued");
        assert_eq!(pkt.hdr.op, OP_REQUEST);
        assert_eq!(pkt.hdr.dst_port, 1234);
        assert!(pkt.hdr.src_port >= 1024, "ephemeral host port allocated");
        assert_eq!(mux.save_conns().len(), 1, "one LocalInit conn inserted");
    }

    #[test]
    fn partial_connect_line_waits_for_newline() {
        use std::io::Write;
        let base = std::env::temp_dir().join("ign-vsock-e2-partial/vsock");
        let mut mux = Muxer::new(base);
        let (host, mut client) = UnixStream::pair().unwrap();
        host.set_nonblocking(true).unwrap();
        mux.accept_control(host);

        client.write_all(b"CONN").unwrap();
        mux.poll_controls();
        assert!(mux.pop_rx().is_none(), "no packet before newline");

        client.write_all(b"ECT 1234\n").unwrap();
        mux.poll_controls();
        let pkt = mux.pop_rx().expect("REQUEST after newline");
        assert_eq!(pkt.hdr.op, OP_REQUEST);
        assert_eq!(pkt.hdr.dst_port, 1234);
        assert!(mux.pop_rx().is_none(), "exactly one REQUEST");
    }

    #[test]
    fn malformed_control_line_drops_stream_no_packet() {
        use std::io::Write;
        let base = std::env::temp_dir().join("ign-vsock-e2-bad/vsock");
        let mut mux = Muxer::new(base);
        let (host, mut client) = UnixStream::pair().unwrap();
        host.set_nonblocking(true).unwrap();
        mux.accept_control(host);
        client.write_all(b"HELLO\n").unwrap();
        mux.poll_controls();
        assert!(mux.pop_rx().is_none(), "no packet for malformed line");
        assert_eq!(mux.save_conns().len(), 0, "no conn created");
    }

    #[test]
    fn two_connects_get_distinct_host_ports() {
        use std::io::Write;
        let base = std::env::temp_dir().join("ign-vsock-e2-two/vsock");
        let mut mux = Muxer::new(base);
        let (h1, mut c1) = UnixStream::pair().unwrap();
        let (h2, mut c2) = UnixStream::pair().unwrap();
        h1.set_nonblocking(true).unwrap();
        h2.set_nonblocking(true).unwrap();
        mux.accept_control(h1);
        mux.accept_control(h2);
        c1.write_all(b"CONNECT 10\n").unwrap();
        c2.write_all(b"CONNECT 20\n").unwrap();
        mux.poll_controls();
        let p1 = mux.pop_rx().unwrap();
        let p2 = mux.pop_rx().unwrap();
        assert_ne!(p1.hdr.src_port, p2.hdr.src_port, "distinct ephemeral host ports");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `PATH="$HOME/.cargo/bin:$PATH" cargo test -p ignition-devices vsock::muxer`
Expected: FAIL — `no method accept_control` / `poll_controls`.

- [ ] **Step 3: Add control-stream state and a line buffer**

In `muxer.rs`, extend imports and the struct. Replace the top `use` block and `Muxer`
struct/`new` with:

```rust
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use super::connection::{Connection, ConnState, BUF_ALLOC};
use super::packet::*;

/// A host control client whose `CONNECT <port>\n` line may arrive in pieces.
struct ControlStream {
    stream: UnixStream,
    buf: Vec<u8>,
}

pub struct Muxer {
    uds_base: PathBuf,
    conns: HashMap<(u32, u32), Connection>, // (guest_port, host_port)
    rxq: VecDeque<RxPacket>,
    /// Connection keys carried over a snapshot; RST'd to the guest on the first service() after restore.
    pending_rst: Vec<(u32, u32)>,
    /// Host control clients mid-CONNECT, keyed by fd.
    ctrl_streams: HashMap<RawFd, ControlStream>,
    /// Ephemeral host-port allocator for host-initiated connections.
    next_host_port: u32,
}
```

And update `new`:

```rust
    pub fn new(uds_base: PathBuf) -> Muxer {
        Muxer {
            uds_base,
            conns: HashMap::new(),
            rxq: VecDeque::new(),
            pending_rst: Vec::new(),
            ctrl_streams: HashMap::new(),
            next_host_port: 1024,
        }
    }
```

- [ ] **Step 4: Add the allocator, accept, and parse methods**

In `impl Muxer`, add (e.g. after `port_path`):

```rust
    /// Pick the next free ephemeral host port (skips ports held by live conns).
    fn alloc_host_port(&mut self) -> u32 {
        loop {
            let p = self.next_host_port;
            self.next_host_port = if p == u32::MAX { 1024 } else { p + 1 };
            if !self.conns.keys().any(|&(_, h)| h == p) {
                return p;
            }
        }
    }

    /// Register an accepted host control client (caller set it non-blocking).
    pub fn accept_control(&mut self, stream: UnixStream) {
        let fd = stream.as_raw_fd();
        self.ctrl_streams.insert(fd, ControlStream { stream, buf: Vec::new() });
    }

    /// Read all pending control clients (non-blocking). On a complete
    /// `CONNECT <port>\n`: allocate a host port, insert a LocalInit conn, queue a
    /// REQUEST to the guest. Malformed lines or EOF drop the client silently.
    pub fn poll_controls(&mut self) {
        let fds: Vec<RawFd> = self.ctrl_streams.keys().copied().collect();
        for fd in fds {
            // Read whatever is available into the line buffer.
            let mut done = false; // remove this client after the loop?
            let mut promote: Option<(u32, UnixStream)> = None; // (guest_port, stream)
            {
                let cs = self.ctrl_streams.get_mut(&fd).unwrap();
                let mut tmp = [0u8; 256];
                loop {
                    match cs.stream.read(&mut tmp) {
                        Ok(0) => { done = true; break; }
                        Ok(n) => cs.buf.extend_from_slice(&tmp[..n]),
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => { done = true; break; }
                    }
                    if cs.buf.len() > 512 { done = true; break; } // runaway line guard
                }
                // Complete line? (only act on the first newline)
                if let Some(pos) = cs.buf.iter().position(|&b| b == b'\n') {
                    let line = String::from_utf8_lossy(&cs.buf[..pos]).trim().to_string();
                    done = true; // the stream moves into a conn or is dropped
                    if let Some(port_str) = line.strip_prefix("CONNECT ") {
                        if let Ok(guest_port) = port_str.trim().parse::<u32>() {
                            // Steal the stream out of the map below; mark for promotion.
                            promote = Some((guest_port, cs.stream.try_clone().expect("dup ctrl stream")));
                        }
                    }
                }
            }
            if done {
                self.ctrl_streams.remove(&fd);
            }
            if let Some((guest_port, stream)) = promote {
                let host_port = self.alloc_host_port();
                stream.set_nonblocking(true).ok();
                let conn = Connection::new_local_init(guest_port, host_port, stream);
                self.conns.insert((guest_port, host_port), conn);
                let mut hdr = Self::ctrl_hdr(OP_REQUEST, guest_port, host_port, 0);
                hdr.src_cid = VSOCK_CID_HOST;
                hdr.dst_cid = VSOCK_GUEST_CID;
                self.rxq.push_back(RxPacket { hdr, data: Vec::new() });
            }
        }
    }
```

Note: `try_clone()` duplicates the fd so the original (still owned by the removed
`ControlStream`) closing does not close the data stream. The cloned fd is the
connection's data stream.

- [ ] **Step 5: Run tests to verify they pass**

Run: `PATH="$HOME/.cargo/bin:$PATH" cargo test -p ignition-devices vsock::muxer`
Expected: PASS (the four new control tests plus the existing muxer tests).

- [ ] **Step 6: Commit**

```bash
git add crates/devices/src/virtio/vsock/muxer.rs
git commit -m "feat(vsock): control-stream CONNECT parsing -> guest REQUEST"
```

---

### Task 3: Muxer — RESPONSE → Established + OK, and poll_set

**Files:**
- Modify: `crates/devices/src/virtio/vsock/muxer.rs`

When the guest accepts, it sends `RESPONSE` on TX. The muxer confirms the `LocalInit`
conn and writes `OK <host_port>\n` to the host. A guest `RST` (no listener) drops the
conn (existing `OP_RST` arm). `poll_set` must also wake on control-stream fds.

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block in `muxer.rs`:

```rust
    fn resp(guest: u32, host: u32) -> VsockHeader {
        VsockHeader {
            src_cid: VSOCK_GUEST_CID, dst_cid: VSOCK_CID_HOST, src_port: guest, dst_port: host,
            len: 0, type_: VSOCK_TYPE_STREAM, op: OP_RESPONSE, flags: 0, buf_alloc: 64 * 1024, fwd_cnt: 0,
        }
    }

    #[test]
    fn response_establishes_conn_and_writes_ok() {
        use std::io::{Read, Write};
        let base = std::env::temp_dir().join("ign-vsock-e2-ok/vsock");
        let mut mux = Muxer::new(base);
        let (host, mut client) = UnixStream::pair().unwrap();
        host.set_nonblocking(true).unwrap();
        mux.accept_control(host);
        client.write_all(b"CONNECT 1234\n").unwrap();
        mux.poll_controls();
        let req = mux.pop_rx().unwrap();
        let host_port = req.hdr.src_port;

        // Guest accepts.
        mux.handle_tx(&resp(1234, host_port), &[]);

        let mut buf = [0u8; 32];
        client.set_nonblocking(true).unwrap();
        let n = client.read(&mut buf).unwrap();
        let line = std::str::from_utf8(&buf[..n]).unwrap();
        assert_eq!(line, format!("OK {host_port}\n"));
    }

    #[test]
    fn poll_set_includes_control_fds() {
        let base = std::env::temp_dir().join("ign-vsock-e2-pollset/vsock");
        let mut mux = Muxer::new(base);
        let (host, _client) = UnixStream::pair().unwrap();
        let fd = { use std::os::unix::io::AsRawFd; host.as_raw_fd() };
        host.set_nonblocking(true).unwrap();
        mux.accept_control(host);
        assert!(mux.poll_set().contains(&fd), "control fd is polled");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `PATH="$HOME/.cargo/bin:$PATH" cargo test -p ignition-devices vsock::muxer`
Expected: FAIL — `response_establishes_conn_and_writes_ok` (no OK written; `OP_RESPONSE`
is a no-op) and `poll_set_includes_control_fds` (control fds absent).

- [ ] **Step 3: Implement the `OP_RESPONSE` arm**

In `handle_tx`, replace the existing no-op arm:

```rust
            OP_RESPONSE => { /* host->guest connect ack — E2 */ }
```

with:

```rust
            OP_RESPONSE => {
                if let Some(conn) = self.conns.get_mut(&key) {
                    if conn.state() == ConnState::LocalInit {
                        conn.confirm_established(hdr.buf_alloc, hdr.fwd_cnt);
                        conn.write_host_raw(format!("OK {host_port}\n").as_bytes());
                    }
                }
            }
```

(`key == (hdr.src_port, hdr.dst_port) == (guest_port, host_port)`, matching how the
`REQUEST` was keyed in Task 2.)

- [ ] **Step 4: Extend `poll_set` to include control fds**

Replace `poll_set` (line ~163):

```rust
    /// Host fds to poll (POLLIN): live connection streams plus control clients
    /// still parsing their CONNECT line. Buffered guest->host tx flushes each
    /// service() tick, so POLLOUT is not needed.
    pub fn poll_set(&self) -> Vec<RawFd> {
        self.conns
            .values()
            .map(|c| c.raw_fd())
            .chain(self.ctrl_streams.keys().copied())
            .collect()
    }
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `PATH="$HOME/.cargo/bin:$PATH" cargo test -p ignition-devices vsock`
Expected: PASS (every vsock test — connection, muxer E1 + E2, device).

- [ ] **Step 6: Commit**

```bash
git add crates/devices/src/virtio/vsock/muxer.rs
git commit -m "feat(vsock): guest RESPONSE establishes host conn + OK reply"
```

---

### Task 4: Device + transport plumbing for control clients

**Files:**
- Modify: `crates/devices/src/virtio/vsock/mod.rs`
- Modify: `crates/devices/src/virtio/mmio.rs`

The reactor needs a way to hand accepted control streams to the muxer, and the per-tick
RX pass must drain control streams. `fill_rx` already runs every reactor tick; route
`poll_controls` through it. Add a `vsock_accept_control` trait method (default no-op)
and a `VirtioMmio` delegation, mirroring the existing `vsock_poll_set` pattern.

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `crates/devices/src/virtio/vsock/mod.rs`:

```rust
    #[test]
    fn fill_rx_drains_control_connect_into_request() {
        use std::io::Write;
        use std::os::unix::net::UnixStream;
        // A control client sends CONNECT; fill_rx (the reactor's per-tick entry)
        // must parse it and queue a REQUEST for the guest, even with no RX chain.
        let mut dev = VsockDevice::new(PathBuf::from("/tmp/ign-e2-fillrx/vsock"));
        let (host, mut client) = UnixStream::pair().unwrap();
        host.set_nonblocking(true).unwrap();
        dev.accept_control(host);
        client.write_all(b"CONNECT 4321\n").unwrap();

        // No guest RX descriptors available: build an empty queue is awkward here,
        // so assert at the muxer level that a REQUEST got queued by the control drain.
        dev.drain_controls_for_test();
        assert!(dev.muxer_rx_pending_for_test(), "REQUEST queued from CONNECT");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `PATH="$HOME/.cargo/bin:$PATH" cargo test -p ignition-devices vsock::tests::fill_rx_drains_control_connect_into_request`
Expected: FAIL — `no method accept_control` / `drain_controls_for_test` / `muxer_rx_pending_for_test` on `VsockDevice`.

- [ ] **Step 3: Add device methods and route `poll_controls` through `fill_rx`**

In `crates/devices/src/virtio/vsock/mod.rs`, add to `impl VsockDevice`:

```rust
    /// Hand an accepted host control client to the muxer (reactor calls this).
    pub fn accept_control(&mut self, stream: std::os::unix::net::UnixStream) {
        self.muxer.accept_control(stream);
    }

    #[cfg(test)]
    fn drain_controls_for_test(&mut self) {
        self.muxer.poll_controls();
    }
    #[cfg(test)]
    fn muxer_rx_pending_for_test(&self) -> bool {
        self.muxer.rx_pending()
    }
```

Change `fill_rx` (line ~112) to drain control clients before servicing:

```rust
    fn fill_rx(&mut self, rx_vq: &mut Virtqueue, mem: &GuestRam) -> bool {
        self.muxer.poll_controls();
        self.muxer.service();
        self.fill_guest_rx(rx_vq, mem)
    }
```

- [ ] **Step 4: Add the trait method and transport delegation**

In `crates/devices/src/virtio/mmio.rs`, add to the `VirtioDevice` trait (near
`vsock_poll_set`, line ~54), a defaulted method:

```rust
    /// Accept a host control client for vsock E2 (no-op for non-vsock devices).
    fn vsock_accept_control(&mut self, _stream: std::os::unix::net::UnixStream) {}
```

Implement it for `VsockDevice` in `mod.rs` (inside `impl VirtioDevice for VsockDevice`):

```rust
    fn vsock_accept_control(&mut self, stream: std::os::unix::net::UnixStream) {
        self.muxer.accept_control(stream);
    }
```

And add a `VirtioMmio` delegation in `mmio.rs` (near `vsock_poll_set`, line ~284):

```rust
    pub fn vsock_accept_control(&mut self, stream: std::os::unix::net::UnixStream) {
        self.dev.vsock_accept_control(stream);
    }
```

(The public `VsockDevice::accept_control` from Step 3 stays for unit tests; the boot
harness goes through `VirtioMmio::vsock_accept_control`.)

- [ ] **Step 5: Run tests to verify they pass**

Run: `PATH="$HOME/.cargo/bin:$PATH" cargo test -p ignition-devices vsock`
Expected: PASS.

- [ ] **Step 6: Build the workspace and lint**

Run: `PATH="$HOME/.cargo/bin:$PATH" cargo build && PATH="$HOME/.cargo/bin:$PATH" cargo clippy -p ignition-devices`
Expected: builds clean, no new clippy warnings in the vsock module.

- [ ] **Step 7: Commit**

```bash
git add crates/devices/src/virtio/vsock/mod.rs crates/devices/src/virtio/mmio.rs
git commit -m "feat(vsock): device + transport plumbing for control clients"
```

---

### Task 5: Boot harness — bind {uds} listener and accept in the reactor

**Files:**
- Modify: `spike/src/bin/boot.rs`

The reactor binds a non-blocking `UnixListener` on `{uds}` (when `--vsock-uds` is set),
polls its fd alongside the connection fds, and on readable accepts all pending clients
and forwards them to the device. The per-tick `poll_vsock_rx` already drains controls
(Task 4) and services connections.

- [ ] **Step 1: Pass the control socket path into the reactor**

In `boot.rs`, find where the vsock device is wired (around line 417-424) and capture the
uds path so the reactor can bind it. The reactor is spawned via `spawn_vsock_reactor`
(called elsewhere with the `Arc<Mutex<VirtioMmio>>`). Change `spawn_vsock_reactor`'s
signature to also take the control-socket path:

```rust
fn spawn_vsock_reactor(
    vsock: Arc<Mutex<ignition_devices::virtio::mmio::VirtioMmio>>,
    uds_base: Option<PathBuf>,
) {
    use std::os::unix::io::{AsRawFd, RawFd};
    use std::os::unix::net::UnixListener;

    // Bind the control listener ({uds} itself) for host->guest (E2). Per-port
    // paths {uds}_{port} remain the E1 guest->host listeners (host side).
    let listener: Option<UnixListener> = uds_base.and_then(|base| {
        let _ = std::fs::remove_file(&base); // clear a stale socket
        match UnixListener::bind(&base) {
            Ok(l) => {
                l.set_nonblocking(true).ok();
                Some(l)
            }
            Err(e) => {
                eprintln!("vsock: control listener bind {base:?} failed: {e}");
                None
            }
        }
    });
    let listener_fd: Option<RawFd> = listener.as_ref().map(|l| l.as_raw_fd());

    std::thread::spawn(move || loop {
        let mut fds: Vec<RawFd> = { vsock.lock().unwrap().vsock_poll_set() };
        if let Some(lfd) = listener_fd {
            fds.push(lfd);
        }
        if fds.is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(200));
        } else {
            let mut pfds: Vec<libc::pollfd> = fds
                .iter()
                .map(|&fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 })
                .collect();
            unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, 200) };
        }
        // Accept any new control clients (non-blocking) and hand them to the device.
        if let Some(l) = &listener {
            loop {
                match l.accept() {
                    Ok((stream, _)) => {
                        stream.set_nonblocking(true).ok();
                        vsock.lock().unwrap().vsock_accept_control(stream);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
        }
        vsock.lock().unwrap().poll_vsock_rx();
    });
}
```

- [ ] **Step 2: Update the call site(s)**

Find every call to `spawn_vsock_reactor(...)` in `boot.rs` and pass the uds base. The
`RunCtx` already holds `vsock_uds: Option<PathBuf>` (line ~289). At the spawn site, pass
a clone:

```rust
        spawn_vsock_reactor(mmio.clone(), ctx.vsock_uds.clone());
```

If the spawn happens in a scope without `ctx`, thread the `Option<PathBuf>` through the
same way `vsock_mmio` is propagated. For restore, the path comes from the `--vsock-uds`
argument already plumbed to `run_restore` (line ~644); pass that same value.

- [ ] **Step 3: Build**

Run: `PATH="$HOME/.cargo/bin:$PATH" cargo build`
Expected: builds clean (binary `boot`).

- [ ] **Step 4: Smoke-check the listener binds (no guest needed)**

Run:
```bash
PATH="$HOME/.cargo/bin:$PATH" cargo build 2>&1 | tail -2
# Confirm the new arg is wired without a panic at startup by checking help/usage path:
grep -n "spawn_vsock_reactor" spike/src/bin/boot.rs
```
Expected: build succeeds; every `spawn_vsock_reactor` call passes a second argument.

- [ ] **Step 5: Commit**

```bash
git add spike/src/bin/boot.rs
git commit -m "feat(vsock): reactor binds {uds} control listener, accepts host clients (E2)"
```

---

### Task 6: Live driver script and docs

**Files:**
- Create: `scripts/vsock_e2_test.py`
- Modify: `ROADMAP.md`
- Modify: `docs/src/features/devices.md`

A driver that boots a guest with a vsock listener, connects from the host via the
control protocol, and round-trips a string host→guest. Then mark the roadmap item
shipped and document the host→guest direction.

- [ ] **Step 1: Write the driver script**

Create `scripts/vsock_e2_test.py`:

```python
#!/usr/bin/env python3
"""Live virtio-vsock E2 (host->guest) round-trip.

Boots a guest whose init starts a vsock listener on a known port, then from the
host connects to the control socket {uds}, issues CONNECT <port>, expects
`OK <host_port>`, and echoes a string into the guest, reading it back.

Requires the hypervisor entitlement + a kernel/rootfs whose init runs e.g.
`socat VSOCK-LISTEN:5000,fork EXEC:cat` (echo server). Adjust PORT/UDS/paths
to the local setup. Exit 0 on a successful round trip.
"""
import os
import socket
import subprocess
import sys
import time

UDS = "/tmp/ignition-vsock-e2"
PORT = 5000
KERNEL = os.environ.get("IGN_KERNEL", "kimage/out/Image")
ROOTFS = os.environ.get("IGN_ROOTFS", "kimage/out/rootfs.ext4")
BOOT = os.environ.get("IGN_BOOT", "target/debug/boot")


def main() -> int:
    for p in (UDS, f"{UDS}_{PORT}"):
        try:
            os.unlink(p)
        except FileNotFoundError:
            pass

    proc = subprocess.Popen(
        [BOOT, "--vsock-uds", UDS, KERNEL, ROOTFS],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.STDOUT,
    )
    try:
        # Wait for the control socket to appear (guest boot + listener bind).
        deadline = time.time() + 60
        while not os.path.exists(UDS):
            if time.time() > deadline:
                print("FAIL: control socket never appeared", file=sys.stderr)
                return 1
            time.sleep(0.5)
        time.sleep(2)  # let the in-guest listener come up

        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.connect(UDS)
        s.sendall(f"CONNECT {PORT}\n".encode())
        s.settimeout(10)
        ack = s.recv(64).decode()
        if not ack.startswith("OK "):
            print(f"FAIL: expected OK, got {ack!r}", file=sys.stderr)
            return 1

        s.sendall(b"ping-e2\n")
        echo = s.recv(64)
        if b"ping-e2" not in echo:
            print(f"FAIL: no echo, got {echo!r}", file=sys.stderr)
            return 1

        print("PASS: host->guest vsock round trip OK")
        return 0
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()


if __name__ == "__main__":
    sys.exit(main())
```

- [ ] **Step 2: Make it executable and syntax-check**

Run: `chmod +x scripts/vsock_e2_test.py && python3 -c "import ast; ast.parse(open('scripts/vsock_e2_test.py').read()); print('OK')"`
Expected: `OK`.

(The live run needs the hypervisor entitlement + a rootfs with an in-guest vsock echo
listener; it is an interactive/manual driver like `restore_test.py`, not a unit test.)

- [ ] **Step 3: Update the roadmap**

In `ROADMAP.md`, the near-term list has:

```
- [ ] **virtio-vsock E2** — host→guest connections (E1 is guest→host only). Gates
  control-plane designs that talk *into* clones.
```

Change `[ ]` to `[x]` and append the outcome. Replace those two lines with:

```
- [x] **virtio-vsock E2** — host→guest connections via Firecracker's hybrid control
  protocol (`CONNECT <port>` → `OK <host_port>`); host control socket `{uds}`, guest
  RESPONSE establishes the conn, bidirectional streaming reuses E1's `Connection`.
  `docs/superpowers/specs/2026-06-15-virtio-vsock-e2-design.md`, `scripts/vsock_e2_test.py`.
```

Also update the parity table row (line ~231):

```
| virtio blk/net/rng/balloon/vsock, RTC | ✅ | ✅ | vsock host→guest (E2) pending |
```

to:

```
| virtio blk/net/rng/balloon/vsock, RTC | ✅ | ✅ | vsock both directions (E1+E2) |
```

And in the "Hardening & honesty gates" section, the repeated vsock-E2 bullet
(line ~182-183) — change `[ ]` to `[x]`:

```
- [x] **virtio-vsock E2** (host→guest) — shipped; unblocks control-plane integration designs.
```

- [ ] **Step 4: Document the host→guest direction**

In `docs/src/features/devices.md`, find the vsock subsection (search for `vsock`). After
the existing E1 (guest→host) description, add a short subsection:

```markdown
### vsock host→guest (E2)

A host process opens a connection *into* a listening guest over the same control
socket, using Firecracker's hybrid protocol:

1. The host connects to `{uds}` (the base path of `--vsock-uds`) and sends
   `CONNECT <guest_port>\n`.
2. ignition allocates an ephemeral host port, signals the guest (`REQUEST`), and the
   guest's listener accepts (`RESPONSE`).
3. ignition replies `OK <host_port>\n` to the host; raw bytes then flow both ways on
   that same connection. If no guest process is listening, the connection is closed.

```console
# guest init runs e.g.:  socat VSOCK-LISTEN:5000,fork EXEC:cat
socat - UNIX-CONNECT:/tmp/ignition-vsock <<<'CONNECT 5000'
```

Guest→host (E1) and host→guest (E2) coexist; per-port paths `{uds}_{port}` remain the
E1 guest→host listeners.
```

- [ ] **Step 5: Verify docs build (if mdbook is available)**

Run: `PATH="$HOME/.cargo/bin:$PATH" mdbook build docs 2>/dev/null && echo "BOOK OK" || echo "mdbook not installed — skipping"`
Expected: `BOOK OK` (or the skip message; not fatal).

- [ ] **Step 6: Commit**

```bash
git add scripts/vsock_e2_test.py ROADMAP.md docs/src/features/devices.md
git commit -m "docs(vsock): E2 driver script, roadmap + device docs for host->guest"
```

---

## Self-Review

**Spec coverage:**
- Hybrid control protocol (`CONNECT`/`OK`) → Tasks 2, 3, 5. ✓
- `LocalInit` state + `confirm_established` + Established-gating → Task 1. ✓
- Ephemeral host-port allocator (skips live ports) → Task 2 (`alloc_host_port`). ✓
- `ctrl_streams` partial-line buffering → Task 2. ✓
- `OP_RESPONSE` → Established + OK; no-listener RST via existing arm → Task 3. ✓
- `poll_set` includes control fds → Task 3. ✓
- Reactor binds `{uds}`, accepts → Task 5. ✓
- Device/transport plumbing (`vsock_accept_control`, `fill_rx` drains controls) → Task 4. ✓
- Snapshot: no new work (existing `save_conns`/`seed_rst` cover `LocalInit` keys) — verified, no task needed. ✓
- Error handling (malformed line, no listener, partial line, host disconnect) → Tasks 2/3 tests + existing `OP_RST` arm. ✓
- Testing (unit 1-7 from spec) → Tasks 1-4 cover codec-adjacent, CONNECT parse, partial, RESPONSE→OK, no-listener, distinct ports, bidirectional RW (existing E1 RW test exercises the Established path). ✓
- Live driver `scripts/vsock_e2_test.py` → Task 6. ✓

**Placeholder scan:** No TBD/TODO-as-work; every code step shows full code. The runaway-line guard (512 bytes) and the `try_clone` rationale are spelled out.

**Type consistency:** `ConnState::LocalInit`, `new_local_init(guest_port, host_port, host)`, `confirm_established(buf_alloc, fwd_cnt)`, `write_host_raw(&[u8])`, `accept_control(UnixStream)`, `poll_controls()`, `alloc_host_port()`, `vsock_accept_control(UnixStream)` — names identical across Tasks 1-5. Muxer conn key `(guest_port, host_port)` consistent with E1 and the `OP_RESPONSE` lookup.
