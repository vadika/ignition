# virtio-vsock E1 (guest→host) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A guest process connects to a host port over vsock and streams bytes to/from a host Unix-socket listener, with credit flow-control (guest→host direction; host→guest is E2).

**Architecture:** A `vsock/` module — packet codec, a per-connection state machine + credit accounting, and a `Muxer` keyed by `(guest_port, host_port)` — wrapped by a `VsockDevice` (`VirtioDevice`, 3 queues). TX runs synchronously in `handle_notify`; RX is driven by a reader thread that `poll(2)`s the host connection fds (woken by a self-pipe) and fills the guest RX queue via a new `VirtioMmio::poll_vsock_rx`.

**Tech Stack:** Rust (edition 2024), `std::os::unix::net::UnixStream`, `libc::poll`, the existing `Virtqueue`/`GuestRam`/`VirtioMmio`/`DeviceManager`.

**Spec:** `docs/superpowers/specs/2026-06-13-virtio-vsock-e1-design.md`

---

## File structure

- `crates/devices/src/virtio/vsock/packet.rs` *(new)* — header consts + codec.
- `crates/devices/src/virtio/vsock/connection.rs` *(new)* — `Connection` CSM + credit + txbuf.
- `crates/devices/src/virtio/vsock/muxer.rs` *(new)* — `Muxer` + `RxPacket`.
- `crates/devices/src/virtio/vsock/mod.rs` *(new)* — `VsockDevice` (`VirtioDevice`).
- `crates/devices/src/virtio/mod.rs` *(modify)* — `pub mod vsock;`.
- `crates/devices/src/virtio/mmio.rs` *(modify)* — `VirtioDevice::fill_rx` (default) + `VirtioMmio::poll_vsock_rx`.
- `spike/src/bin/boot.rs` *(modify)* — `--vsock-uds` flag, wire `VsockDevice`, spawn RX reactor, restore arm.

---

## Task 1: Packet codec (`packet.rs`)

**Files:** Create `crates/devices/src/virtio/vsock/packet.rs`

- [ ] **Step 1: Write the failing test — create the file:**

```rust
//! virtio-vsock packet header (44 bytes, little-endian) + protocol constants.

pub const VIRTIO_ID_VSOCK: u32 = 19;
pub const VSOCK_TYPE_STREAM: u16 = 1;
pub const VSOCK_CID_HOST: u64 = 2;
pub const VSOCK_GUEST_CID: u64 = 3;
pub const VSOCK_HDR_SIZE: usize = 44;

// ops
pub const OP_REQUEST: u16 = 1;
pub const OP_RESPONSE: u16 = 2;
pub const OP_RST: u16 = 3;
pub const OP_SHUTDOWN: u16 = 4;
pub const OP_RW: u16 = 5;
pub const OP_CREDIT_UPDATE: u16 = 6;
pub const OP_CREDIT_REQUEST: u16 = 7;

// shutdown flags
pub const SHUTDOWN_F_RECV: u32 = 1;
pub const SHUTDOWN_F_SEND: u32 = 2;

/// A 44-byte vsock header. Field offsets per the virtio spec.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VsockHeader {
    pub src_cid: u64,
    pub dst_cid: u64,
    pub src_port: u32,
    pub dst_port: u32,
    pub len: u32,
    pub type_: u16,
    pub op: u16,
    pub flags: u32,
    pub buf_alloc: u32,
    pub fwd_cnt: u32,
}

impl VsockHeader {
    pub fn to_bytes(&self) -> [u8; VSOCK_HDR_SIZE] {
        let mut b = [0u8; VSOCK_HDR_SIZE];
        b[0..8].copy_from_slice(&self.src_cid.to_le_bytes());
        b[8..16].copy_from_slice(&self.dst_cid.to_le_bytes());
        b[16..20].copy_from_slice(&self.src_port.to_le_bytes());
        b[20..24].copy_from_slice(&self.dst_port.to_le_bytes());
        b[24..28].copy_from_slice(&self.len.to_le_bytes());
        b[28..30].copy_from_slice(&self.type_.to_le_bytes());
        b[30..32].copy_from_slice(&self.op.to_le_bytes());
        b[32..36].copy_from_slice(&self.flags.to_le_bytes());
        b[36..40].copy_from_slice(&self.buf_alloc.to_le_bytes());
        b[40..44].copy_from_slice(&self.fwd_cnt.to_le_bytes());
        b
    }

    pub fn from_bytes(b: &[u8]) -> Option<VsockHeader> {
        if b.len() < VSOCK_HDR_SIZE {
            return None;
        }
        let u64a = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        let u32a = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let u16a = |o: usize| u16::from_le_bytes(b[o..o + 2].try_into().unwrap());
        Some(VsockHeader {
            src_cid: u64a(0),
            dst_cid: u64a(8),
            src_port: u32a(16),
            dst_port: u32a(20),
            len: u32a(24),
            type_: u16a(28),
            op: u16a(30),
            flags: u32a(32),
            buf_alloc: u32a(36),
            fwd_cnt: u32a(40),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrips_all_fields() {
        let h = VsockHeader {
            src_cid: 2, dst_cid: 3, src_port: 1024, dst_port: 5555, len: 16,
            type_: VSOCK_TYPE_STREAM, op: OP_RW, flags: 0, buf_alloc: 65536, fwd_cnt: 42,
        };
        let back = VsockHeader::from_bytes(&h.to_bytes()).unwrap();
        assert_eq!(back, h);
    }

    #[test]
    fn short_buffer_rejected() {
        assert!(VsockHeader::from_bytes(&[0u8; 10]).is_none());
    }
}
```

- [ ] **Step 2: Run** `cargo test -p ignition-devices virtio::vsock::packet` → FAIL (module not declared).
- [ ] **Step 3:** In `crates/devices/src/virtio/mod.rs` add `pub mod vsock;`; create `crates/devices/src/virtio/vsock/mod.rs` with `pub mod packet;` (other submodules added in later tasks).
- [ ] **Step 4: Run** `cargo test -p ignition-devices virtio::vsock::packet && cargo clippy -p ignition-devices` → PASS (2 tests), 0 warnings.
- [ ] **Step 5: Commit**

```bash
git add crates/devices/src/virtio/vsock/ crates/devices/src/virtio/mod.rs
git commit -m "feat(devices): vsock packet header codec + protocol constants"
```

(Plain commit messages throughout — no Co-Authored-By / Generated-with trailers.)

---

## Task 2: `Connection` (CSM + credit + txbuf)

**Files:** Create `crates/devices/src/virtio/vsock/connection.rs`; modify `vsock/mod.rs` (`pub mod connection;`).

- [ ] **Step 1: Write the failing test — create `connection.rs`:**

```rust
//! A single guest↔host vsock stream. The host side is a non-blocking UnixStream;
//! credit accounting follows the virtio-vsock model so neither side overruns.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::num::Wrapping;
use std::os::unix::net::UnixStream;

/// Our advertised RX buffer capacity (bytes the guest may send us before waiting
/// for a credit update). Also bounds a single host→guest read.
pub const BUF_ALLOC: u32 = 64 * 1024;
/// Max payload per host→guest RW packet (fits a typical guest RX descriptor).
pub const READ_CHUNK: usize = 4096;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ConnState {
    Established,
    Closed,
}

pub struct Connection {
    pub guest_port: u32,
    pub host_port: u32,
    host: UnixStream,
    state: ConnState,
    /// Guest's advertised receive buffer + flushed count (from its headers).
    peer_buf_alloc: Wrapping<u32>,
    peer_fwd_cnt: Wrapping<u32>,
    /// Bytes we've sent to the guest (RW). Bounded by the guest's credit.
    rx_cnt: Wrapping<u32>,
    /// Bytes we've flushed to the host stream (advertised to the guest as fwd_cnt).
    fwd_cnt: Wrapping<u32>,
    /// Guest→host bytes not yet written to the host (non-blocking backpressure).
    txbuf: VecDeque<u8>,
}

impl Connection {
    /// Wrap an already-connected host stream (set non-blocking by the caller).
    pub fn new(guest_port: u32, host_port: u32, host: UnixStream, peer_buf_alloc: u32, peer_fwd_cnt: u32) -> Connection {
        Connection {
            guest_port,
            host_port,
            host,
            state: ConnState::Established,
            peer_buf_alloc: Wrapping(peer_buf_alloc),
            peer_fwd_cnt: Wrapping(peer_fwd_cnt),
            rx_cnt: Wrapping(0),
            fwd_cnt: Wrapping(0),
            txbuf: VecDeque::new(),
        }
    }

    pub fn state(&self) -> ConnState {
        self.state
    }
    pub fn fwd_cnt(&self) -> u32 {
        self.fwd_cnt.0
    }
    /// The raw fd, for the reactor's poll set.
    pub fn raw_fd(&self) -> std::os::unix::io::RawFd {
        use std::os::unix::io::AsRawFd;
        self.host.as_raw_fd()
    }

    /// Absorb the guest's advertised credit from any inbound header.
    pub fn update_peer_credit(&mut self, buf_alloc: u32, fwd_cnt: u32) {
        self.peer_buf_alloc = Wrapping(buf_alloc);
        self.peer_fwd_cnt = Wrapping(fwd_cnt);
    }

    /// Bytes we may still send to the guest right now.
    pub fn peer_free(&self) -> u32 {
        (self.peer_buf_alloc - (self.rx_cnt - self.peer_fwd_cnt)).0
    }

    /// Queue guest→host payload and flush as much as the non-blocking socket takes.
    pub fn enqueue_tx(&mut self, data: &[u8]) {
        self.txbuf.extend(data.iter().copied());
        self.flush_tx();
    }

    /// Flush buffered guest→host bytes to the host stream (non-blocking). Advances
    /// fwd_cnt by what was written. Stops on WouldBlock; marks Closed on hard error.
    pub fn flush_tx(&mut self) {
        while let (false, _) = (self.txbuf.is_empty(), ()) {
            let (front, _) = self.txbuf.as_slices();
            if front.is_empty() {
                self.txbuf.make_contiguous();
                continue;
            }
            match self.host.write(front) {
                Ok(0) => {
                    self.state = ConnState::Closed;
                    return;
                }
                Ok(n) => {
                    for _ in 0..n {
                        self.txbuf.pop_front();
                    }
                    self.fwd_cnt += Wrapping(n as u32);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    self.state = ConnState::Closed;
                    return;
                }
            }
        }
    }

    /// Read up to `min(peer_free, READ_CHUNK)` bytes of host→guest data. Returns
    /// `Some(bytes)` (advancing rx_cnt), `None` if nothing available right now, or
    /// transitions to Closed on EOF/error (returns None).
    pub fn read_host(&mut self) -> Option<Vec<u8>> {
        let budget = std::cmp::min(self.peer_free() as usize, READ_CHUNK);
        if budget == 0 {
            return None;
        }
        let mut buf = vec![0u8; budget];
        match self.host.read(&mut buf) {
            Ok(0) => {
                self.state = ConnState::Closed;
                None
            }
            Ok(n) => {
                buf.truncate(n);
                self.rx_cnt += Wrapping(n as u32);
                Some(buf)
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => None,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => None,
            Err(_) => {
                self.state = ConnState::Closed;
                None
            }
        }
    }

    pub fn close(&mut self) {
        self.state = ConnState::Closed;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn pair() -> (Connection, UnixStream) {
        // host-app side = `app`; device side = `dev` (wrapped in Connection).
        let (dev, app) = UnixStream::pair().unwrap();
        dev.set_nonblocking(true).unwrap();
        (Connection::new(1024, 5555, dev, BUF_ALLOC, 0), app)
    }

    #[test]
    fn guest_to_host_write_reaches_app() {
        let (mut conn, mut app) = pair();
        conn.enqueue_tx(b"hello");
        assert_eq!(conn.fwd_cnt(), 5);
        let mut buf = [0u8; 5];
        app.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"hello");
    }

    #[test]
    fn host_to_guest_read_returns_bytes() {
        let (mut conn, mut app) = pair();
        app.write_all(b"world").unwrap();
        let got = conn.read_host().unwrap();
        assert_eq!(got, b"world");
    }

    #[test]
    fn peer_free_caps_at_credit() {
        let (dev, _app) = UnixStream::pair().unwrap();
        dev.set_nonblocking(true).unwrap();
        // guest advertises only 4 bytes of buffer
        let mut conn = Connection::new(1, 2, dev, 4, 0);
        assert_eq!(conn.peer_free(), 4);
        conn.rx_cnt = Wrapping(4); // sent 4 already
        assert_eq!(conn.peer_free(), 0);
    }

    #[test]
    fn eof_marks_closed() {
        let (mut conn, app) = pair();
        drop(app); // host app hangs up
        assert!(conn.read_host().is_none());
        assert_eq!(conn.state(), ConnState::Closed);
    }
}
```

- [ ] **Step 2: Run** `cargo test -p ignition-devices virtio::vsock::connection` → FAIL (module not declared).
- [ ] **Step 3:** Add `pub mod connection;` to `vsock/mod.rs`.
- [ ] **Step 4: Run** `cargo test -p ignition-devices virtio::vsock::connection && cargo clippy -p ignition-devices` → PASS (4 tests), 0 warnings.

> If clippy flags the `while let (false, _)` flush loop as unidiomatic, rewrite as `while !self.txbuf.is_empty() { ... }` with the same body — the intent is "flush until empty or WouldBlock". Keep behavior identical.

- [ ] **Step 5: Commit**

```bash
git add crates/devices/src/virtio/vsock/connection.rs crates/devices/src/virtio/vsock/mod.rs
git commit -m "feat(devices): vsock Connection state machine + credit + txbuf"
```

---

## Task 3: `Muxer` (routing + RX queue + reactor helpers)

**Files:** Create `crates/devices/src/virtio/vsock/muxer.rs`; modify `vsock/mod.rs` (`pub mod muxer;`).

- [ ] **Step 1: Write the failing test — create `muxer.rs`:**

```rust
//! Routes guest TX packets to per-(guest_port,host_port) connections, connects host
//! Unix sockets ({uds}_{port}) for guest-initiated REQUESTs, and queues packets bound
//! for the guest (RESPONSE/RST/RW/CREDIT_UPDATE) in `rxq`.

use std::collections::{HashMap, VecDeque};
use std::os::unix::io::RawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use super::connection::{Connection, ConnState, BUF_ALLOC};
use super::packet::*;

/// A packet bound for the guest: header + optional payload.
pub struct RxPacket {
    pub hdr: VsockHeader,
    pub data: Vec<u8>,
}

pub struct Muxer {
    uds_base: PathBuf,
    conns: HashMap<(u32, u32), Connection>, // (guest_port, host_port)
    rxq: VecDeque<RxPacket>,
}

impl Muxer {
    pub fn new(uds_base: PathBuf) -> Muxer {
        Muxer { uds_base, conns: HashMap::new(), rxq: VecDeque::new() }
    }

    /// Build a guest-bound header with our credit for `conn` filled in.
    fn ctrl_hdr(op: u16, guest_port: u32, host_port: u32, fwd_cnt: u32) -> VsockHeader {
        VsockHeader {
            src_cid: VSOCK_CID_HOST,
            dst_cid: VSOCK_GUEST_CID,
            src_port: host_port,
            dst_port: guest_port,
            len: 0,
            type_: VSOCK_TYPE_STREAM,
            op,
            flags: 0,
            buf_alloc: BUF_ALLOC,
            fwd_cnt,
        }
    }

    fn queue(&mut self, op: u16, guest_port: u32, host_port: u32, fwd_cnt: u32) {
        self.rxq.push_back(RxPacket { hdr: Self::ctrl_hdr(op, guest_port, host_port, fwd_cnt), data: Vec::new() });
    }

    /// Drive one guest→host TX packet. `payload` is the RW data (empty otherwise).
    pub fn handle_tx(&mut self, hdr: &VsockHeader, payload: &[u8]) {
        let guest_port = hdr.src_port;
        let host_port = hdr.dst_port;
        let key = (guest_port, host_port);
        match hdr.op {
            OP_REQUEST => {
                let path = self.uds_base.with_file_name(format!(
                    "{}_{}",
                    self.uds_base.file_name().and_then(|s| s.to_str()).unwrap_or("vsock"),
                    host_port
                ));
                match UnixStream::connect(&path).and_then(|s| {
                    s.set_nonblocking(true)?;
                    Ok(s)
                }) {
                    Ok(stream) => {
                        let conn = Connection::new(guest_port, host_port, stream, hdr.buf_alloc, hdr.fwd_cnt);
                        let fwd = conn.fwd_cnt();
                        self.conns.insert(key, conn);
                        self.queue(OP_RESPONSE, guest_port, host_port, fwd);
                    }
                    Err(_) => self.queue(OP_RST, guest_port, host_port, 0),
                }
            }
            OP_RW => {
                if let Some(conn) = self.conns.get_mut(&key) {
                    conn.update_peer_credit(hdr.buf_alloc, hdr.fwd_cnt);
                    conn.enqueue_tx(payload);
                    let fwd = conn.fwd_cnt();
                    self.queue(OP_CREDIT_UPDATE, guest_port, host_port, fwd);
                } else {
                    self.queue(OP_RST, guest_port, host_port, 0);
                }
            }
            OP_CREDIT_REQUEST => {
                if let Some(conn) = self.conns.get_mut(&key) {
                    conn.update_peer_credit(hdr.buf_alloc, hdr.fwd_cnt);
                    let fwd = conn.fwd_cnt();
                    self.queue(OP_CREDIT_UPDATE, guest_port, host_port, fwd);
                }
            }
            OP_CREDIT_UPDATE => {
                if let Some(conn) = self.conns.get_mut(&key) {
                    conn.update_peer_credit(hdr.buf_alloc, hdr.fwd_cnt);
                }
            }
            OP_SHUTDOWN | OP_RST => {
                if let Some(mut conn) = self.conns.remove(&key) {
                    conn.close();
                    self.queue(OP_RST, guest_port, host_port, 0);
                }
            }
            OP_RESPONSE => { /* host→guest connect ack — E2 */ }
            _ => {
                if self.conns.contains_key(&key) {
                    self.queue(OP_RST, guest_port, host_port, 0);
                }
            }
        }
    }

    /// Reactor pass: flush pending guest→host data and read host→guest data into rxq.
    /// Removes connections that reached Closed (queuing a final RST).
    pub fn service(&mut self) {
        let mut closed = Vec::new();
        for (key, conn) in self.conns.iter_mut() {
            conn.flush_tx();
            while let Some(data) = conn.read_host() {
                let mut hdr = Self::ctrl_hdr(OP_RW, conn.guest_port, conn.host_port, conn.fwd_cnt());
                hdr.len = data.len() as u32;
                self.rxq.push_back(RxPacket { hdr, data });
            }
            if conn.state() == ConnState::Closed {
                closed.push(*key);
            }
        }
        for key in closed {
            if let Some(conn) = self.conns.remove(&key) {
                self.queue(OP_RST, conn.guest_port, conn.host_port, 0);
            }
        }
    }

    /// Host fds to poll (POLLIN). Buffered guest→host tx is flushed each `service()`
    /// tick, so POLLOUT is not needed (and would busy-loop on idle-writable sockets).
    pub fn poll_set(&self) -> Vec<RawFd> {
        self.conns.values().map(|c| c.raw_fd()).collect()
    }

    /// Pop the next guest-bound packet (header + payload), if any.
    pub fn pop_rx(&mut self) -> Option<RxPacket> {
        self.rxq.pop_front()
    }
    pub fn rx_pending(&self) -> bool {
        !self.rxq.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::net::UnixListener;

    fn req(guest: u32, host: u32) -> VsockHeader {
        VsockHeader {
            src_cid: VSOCK_GUEST_CID, dst_cid: VSOCK_CID_HOST, src_port: guest, dst_port: host,
            len: 0, type_: VSOCK_TYPE_STREAM, op: OP_REQUEST, flags: 0, buf_alloc: 64 * 1024, fwd_cnt: 0,
        }
    }

    #[test]
    fn request_to_listening_host_yields_response() {
        let dir = std::env::temp_dir().join(format!("ign-vsock-{}", std::process::id()));
        let base = dir.join("vsock");
        std::fs::create_dir_all(&dir).unwrap();
        let _l = UnixListener::bind(base.with_file_name("vsock_5000")).unwrap();

        let mut mux = Muxer::new(base);
        mux.handle_tx(&req(1024, 5000), &[]);
        let pkt = mux.pop_rx().expect("a packet queued");
        assert_eq!(pkt.hdr.op, OP_RESPONSE);
        assert_eq!(pkt.hdr.dst_port, 1024);
        assert_eq!(pkt.hdr.src_port, 5000);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn request_to_missing_host_yields_rst() {
        let base = std::env::temp_dir().join("ign-vsock-none/vsock");
        let mut mux = Muxer::new(base);
        mux.handle_tx(&req(1024, 6001), &[]);
        let pkt = mux.pop_rx().unwrap();
        assert_eq!(pkt.hdr.op, OP_RST);
    }

    #[test]
    fn rw_forwards_payload_to_host() {
        let dir = std::env::temp_dir().join(format!("ign-vsock-rw-{}", std::process::id()));
        let base = dir.join("vsock");
        std::fs::create_dir_all(&dir).unwrap();
        let l = UnixListener::bind(base.with_file_name("vsock_5001")).unwrap();

        let mut mux = Muxer::new(base);
        mux.handle_tx(&req(2048, 5001), &[]);
        let (mut app, _addr) = l.accept().unwrap();
        let _ = mux.pop_rx(); // RESPONSE

        let mut rw = req(2048, 5001);
        rw.op = OP_RW;
        mux.handle_tx(&rw, b"ping");
        let mut buf = [0u8; 4];
        app.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"ping");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
```

- [ ] **Step 2: Run** `cargo test -p ignition-devices virtio::vsock::muxer` → FAIL (module not declared).
- [ ] **Step 3:** Add `pub mod muxer;` to `vsock/mod.rs`.
- [ ] **Step 4: Run** `cargo test -p ignition-devices virtio::vsock::muxer && cargo clippy -p ignition-devices` → PASS (3 tests), 0 warnings.

> Note the `{uds}_{port}` path construction: `uds_base` is e.g. `/tmp/x/vsock`; the per-port path is `/tmp/x/vsock_5000` (same dir, filename + `_port`). The `with_file_name(format!("{filename}_{port}"))` builds exactly that. The tests bind a real `UnixListener` at that path to exercise connect.

- [ ] **Step 5: Commit**

```bash
git add crates/devices/src/virtio/vsock/muxer.rs crates/devices/src/virtio/vsock/mod.rs
git commit -m "feat(devices): vsock Muxer (connect, route, host RX, RST)"
```

---

## Task 4: `VsockDevice` (`VirtioDevice`, 3 queues)

**Files:** Modify `crates/devices/src/virtio/vsock/mod.rs`.

- [ ] **Step 1: Write the failing test — set `vsock/mod.rs` to:**

```rust
//! virtio-vsock device (guest→host, E1). 3 queues: RX(0), TX(1), EVENT(2).

pub mod connection;
pub mod muxer;
pub mod packet;

use std::path::PathBuf;

use super::guest_ram::GuestRam;
use super::mmio::VirtioDevice;
use super::queue::Virtqueue;
use muxer::{Muxer, RxPacket};
use packet::*;

const RXQ: usize = 0;
const TXQ: usize = 1;
const EVQ: usize = 2;

pub struct VsockDevice {
    muxer: Muxer,
}

impl VsockDevice {
    pub fn new(uds_base: PathBuf) -> VsockDevice {
        VsockDevice { muxer: Muxer::new(uds_base) }
    }

    /// Drain a guest TX chain: parse header (first readable desc) + payload, route it.
    fn handle_tx(&mut self, vq: &mut Virtqueue, mem: &GuestRam) -> bool {
        let mut serviced = false;
        while let Some(chain) = vq.pop_avail(mem) {
            // Gather all readable bytes of the chain (header + payload).
            let mut bytes = Vec::new();
            for d in &chain.descriptors {
                if !d.writable {
                    let mut buf = vec![0u8; d.len as usize];
                    if mem.read_slice(d.addr, &mut buf) {
                        bytes.extend_from_slice(&buf);
                    }
                }
            }
            if let Some(hdr) = VsockHeader::from_bytes(&bytes) {
                let payload = if bytes.len() > VSOCK_HDR_SIZE { &bytes[VSOCK_HDR_SIZE..] } else { &[] };
                self.muxer.handle_tx(&hdr, payload);
            }
            vq.push_used(mem, chain.head, 0);
            serviced = true;
        }
        serviced
    }

    /// Write one queued guest-bound packet into a writable RX chain.
    fn write_rx(chain_addr: u64, chain_cap: usize, mem: &GuestRam, pkt: &RxPacket) -> u32 {
        let mut buf = pkt.hdr.to_bytes().to_vec();
        buf.extend_from_slice(&pkt.data);
        let n = std::cmp::min(buf.len(), chain_cap);
        mem.write_slice(chain_addr, &buf[..n]);
        n as u32
    }

    /// Fill the guest RX queue from the muxer's pending packets.
    fn fill_guest_rx(&mut self, vq: &mut Virtqueue, mem: &GuestRam) -> bool {
        let mut delivered = false;
        while self.muxer.rx_pending() {
            let Some(chain) = vq.pop_avail(mem) else { break };
            // First writable descriptor is the RX buffer.
            let Some(d) = chain.descriptors.iter().find(|d| d.writable) else {
                vq.push_used(mem, chain.head, 0);
                continue;
            };
            let pkt = self.muxer.pop_rx().unwrap();
            let len = Self::write_rx(d.addr, d.len as usize, mem, &pkt);
            vq.push_used(mem, chain.head, len);
            delivered = true;
        }
        delivered
    }
}

impl VirtioDevice for VsockDevice {
    fn device_id(&self) -> u32 {
        VIRTIO_ID_VSOCK
    }
    fn device_features(&self, _sel: u32) -> u32 {
        0
    }
    fn config_read(&self, offset: u64, data: &mut [u8]) {
        // config: le64 guest_cid at 0x00.
        let cfg = VSOCK_GUEST_CID.to_le_bytes();
        for (i, b) in data.iter_mut().enumerate() {
            let o = offset as usize + i;
            *b = if o < cfg.len() { cfg[o] } else { 0 };
        }
    }
    fn queue_count(&self) -> usize {
        3
    }
    fn handle_notify(&mut self, queue_idx: usize, vq: &mut Virtqueue, mem: &GuestRam) -> bool {
        match queue_idx {
            TXQ => {
                let did = self.handle_tx(vq, mem);
                // RESPONSEs/CREDITs queued by TX are delivered on the RX queue by the
                // reactor (poll_vsock_rx); nothing more to do here.
                did
            }
            RXQ => self.fill_guest_rx(vq, mem),
            EVQ => {
                // Parked: just complete any buffers the guest posts.
                let mut did = false;
                while let Some(chain) = vq.pop_avail(mem) {
                    vq.push_used(mem, chain.head, 0);
                    did = true;
                }
                did
            }
            _ => false,
        }
    }
    fn fill_rx(&mut self, rx_vq: &mut Virtqueue, mem: &GuestRam) -> bool {
        // Reactor entry: read host data into rxq, then fill the guest RX queue.
        self.muxer.service();
        self.fill_guest_rx(rx_vq, mem)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_and_config() {
        let dev = VsockDevice::new(PathBuf::from("/tmp/x/vsock"));
        assert_eq!(dev.device_id(), 19);
        assert_eq!(dev.queue_count(), 3);
        let mut c = [0u8; 8];
        dev.config_read(0, &mut c);
        assert_eq!(u64::from_le_bytes(c), 3);
    }
}
```

- [ ] **Step 2: Run** `cargo test -p ignition-devices virtio::vsock` → FAIL to compile until `fill_rx` exists on the trait (Task 5 adds it). **Do Task 5 before re-running** — or temporarily expect the compile error naming `fill_rx`. (The `fill_rx` method here overrides the trait method added in Task 5.)
- [ ] **Step 3:** (covered by Task 5 — the trait method). After Task 5, run again.
- [ ] **Step 4: Run** (after Task 5) `cargo test -p ignition-devices virtio::vsock && cargo clippy -p ignition-devices` → PASS, 0 warnings.
- [ ] **Step 5: Commit**

```bash
git add crates/devices/src/virtio/vsock/mod.rs
git commit -m "feat(devices): VsockDevice (3 queues, TX routing, RX fill, config CID)"
```

> Because Task 4's `fill_rx` overrides a trait method introduced in Task 5, commit Task 5 first if the subagent prefers a compiling tree at each commit; otherwise commit Task 4 + Task 5 back-to-back. The reviewer should treat 4+5 as a pair.

---

## Task 5: Transport hook — `fill_rx` + `poll_vsock_rx`

**Files:** Modify `crates/devices/src/virtio/mmio.rs`.

- [ ] **Step 1: Write the failing test** — add to the `mmio.rs` test module:

```rust
#[test]
fn poll_vsock_rx_drives_fill_rx_and_irq() {
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct RecIrq { level: Mutex<Option<bool>> }
    impl crate::virtio::IrqLine for RecIrq {
        fn set_spi(&self, level: bool) { *self.level.lock().unwrap() = Some(level); }
    }

    struct FillDev { filled: bool }
    impl VirtioDevice for FillDev {
        fn device_id(&self) -> u32 { 19 }
        fn device_features(&self, _: u32) -> u32 { 0 }
        fn config_read(&self, _: u64, _: &mut [u8]) {}
        fn queue_count(&self) -> usize { 3 }
        fn handle_notify(&mut self, _: usize, _: &mut Virtqueue, _: &GuestRam) -> bool { false }
        fn fill_rx(&mut self, _rx: &mut Virtqueue, _mem: &GuestRam) -> bool { self.filled = true; true }
    }

    let backing = Box::leak(vec![0u8; 0x1000].into_boxed_slice());
    let mem = GuestRam::new(backing.as_mut_ptr(), backing.len(), 0x4000_0000);
    let irq = Arc::new(RecIrq::default());
    let mut t = VirtioMmio::new("vsock", Box::new(FillDev { filled: false }), mem, irq.clone());
    // queue 0 must be configured for poll_vsock_rx to build a Virtqueue; set it ready.
    t.write(0, 0x030, &0u32.to_le_bytes());  // QueueSel = 0
    t.write(0, 0x038, &8u32.to_le_bytes());  // QueueNum = 8
    t.write(0, 0x080, &0x1000u32.to_le_bytes()); // desc lo
    t.write(0, 0x090, &0x2000u32.to_le_bytes()); // driver lo
    t.write(0, 0x0a0, &0x3000u32.to_le_bytes()); // device lo
    t.write(0, 0x044, &1u32.to_le_bytes());  // QueueReady = 1

    assert!(t.poll_vsock_rx());
    assert_eq!(*irq.level.lock().unwrap(), Some(true));
}
```

(Adjust the register offsets to the ones this file already uses in its other queue-setup tests — grep the test module for `0x038`/`0x044`/`0x080` and mirror exactly.)

- [ ] **Step 2: Run** `cargo test -p ignition-devices virtio::mmio::tests::poll_vsock_rx` → FAIL (no `fill_rx`/`poll_vsock_rx`).
- [ ] **Step 3:** Add to the `VirtioDevice` trait (after `config_write`):

```rust
    /// Reactor entry for async-RX devices (vsock): read host-side data and fill the
    /// RX virtqueue. Default: no-op.
    fn fill_rx(&mut self, _rx_vq: &mut Virtqueue, _mem: &GuestRam) -> bool {
        false
    }
```

Add to the inherent `impl VirtioMmio` block — `poll_vsock_rx`, which builds the RX queue (queue 0) the same way `notify`/`inject_rx` build a queue and calls `fill_rx`, asserting the used IRQ on delivery:

```rust
    /// Reactor hook: drive the device's async RX (queue 0) and raise the used IRQ if
    /// anything was delivered. Mirrors how `inject_rx` accesses the RX queue.
    pub fn poll_vsock_rx(&mut self) -> bool {
        // Build the RX virtqueue (index 0) from its programmed state, like inject_rx.
        let Some(mut vq) = self.build_queue(0) else { return false };
        let delivered = self.dev.fill_rx(&mut vq, &self.mem);
        self.store_queue(0, vq);
        if delivered {
            self.interrupt_status |= INT_STATUS_USED;
            self.irq.set_spi(true);
        }
        delivered
    }
```

> Match the EXACT helper `inject_rx` uses to obtain a `Virtqueue` from queue state and to persist updated indices. Read the current `inject_rx` body and reuse the same mechanism (it may be inline rather than `build_queue`/`store_queue` helpers — if so, mirror that inline code for queue 0). Do not invent new helpers if `inject_rx` already inlines this; copy its approach.

- [ ] **Step 4: Run** `cargo test -p ignition-devices virtio::mmio virtio::vsock && cargo clippy -p ignition-devices` → PASS, 0 warnings. (This is also when Task 4's `VsockDevice` compiles.)
- [ ] **Step 5: Commit**

```bash
git add crates/devices/src/virtio/mmio.rs
git commit -m "feat(devices): VirtioDevice::fill_rx + VirtioMmio::poll_vsock_rx (vsock RX reactor hook)"
```

---

## Task 6: Boot wiring — `--vsock-uds` + RX reactor thread

**Files:** Modify `spike/src/bin/boot.rs`.

- [ ] **Step 1: Read** the arg-parse block (`--net`/`--snap-dir` arms), the fresh-boot device adds, the `run_restore` device loop, and the net RX reader-thread wiring (`net_mmio.lock().unwrap().inject_rx(&frame)` inside a `std::thread::spawn`).

- [ ] **Step 2: Add the flag + import.** In imports: `use devices::virtio::vsock::VsockDevice;`. Add a `let mut vsock_uds: Option<PathBuf> = None;` near `let mut net = false;`, and an arm:

```rust
            "--vsock-uds" => {
                let v = it.next().expect("--vsock-uds needs a path");
                vsock_uds = Some(PathBuf::from(v));
            }
```

- [ ] **Step 3: Fresh-boot wiring + reactor.** After the other device adds, when `vsock_uds` is set:

```rust
if let Some(ref uds) = vsock_uds {
    let guest_ram_vsock = GuestRam::new(host as *mut u8, RAM_SIZE as usize, layout::RAM_BASE);
    let vsock_dev = VsockDevice::new(uds.clone());
    let vsock_mmio = mgr
        .add(layout::MMIO_WINDOW, move |irq| {
            VirtioMmio::new("vsock", Box::new(vsock_dev), guest_ram_vsock, irq)
        })
        .expect("add vsock");
    spawn_vsock_reactor(vsock_mmio);
    eprintln!("virtio-vsock: enabled (host uds base {})", uds.display());
}
```

Add the reactor function (poll the host fds, woken every 200 ms; on any wake, drive RX). For E1, a simple timed poll over the device's fd snapshot is sufficient and avoids a self-pipe; the 200 ms tick bounds latency and TX-created connections are picked up on the next tick:

```rust
fn spawn_vsock_reactor(vsock: Arc<Mutex<devices::virtio::mmio::VirtioMmio>>) {
    use std::os::unix::io::RawFd;
    std::thread::spawn(move || {
        loop {
            // Snapshot fds under the lock, then poll unlocked.
            let fds: Vec<RawFd> = {
                let g = vsock.lock().unwrap();
                g.vsock_poll_set()
            };
            if fds.is_empty() {
                std::thread::sleep(std::time::Duration::from_millis(200));
            } else {
                // POLLIN only: idle sockets are almost always writable, so polling
                // POLLOUT would busy-loop. Buffered guest→host tx is flushed each
                // tick inside service() (called by poll_vsock_rx below).
                let mut pfds: Vec<libc::pollfd> = fds
                    .iter()
                    .map(|&fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 })
                    .collect();
                // 200 ms timeout: also re-checks for newly-connected fds.
                unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, 200) };
            }
            // Drive RX (reads host data, fills guest RX, raises IRQ).
            vsock.lock().unwrap().poll_vsock_rx();
        }
    });
}
```

Add a thin accessor on `VirtioMmio` so the reactor can read the device's poll set without downcasting — in `mmio.rs` add:

```rust
    /// vsock reactor support: the host fds the device wants polled. Empty for
    /// non-vsock devices (default trait method returns none).
    pub fn vsock_poll_set(&self) -> Vec<std::os::unix::io::RawFd> {
        self.dev.vsock_poll_set()
    }
```

and a defaulted trait method on `VirtioDevice`:

```rust
    /// Host fds an async-RX device wants the reactor to poll. Default: none.
    fn vsock_poll_set(&self) -> Vec<std::os::unix::io::RawFd> {
        Vec::new()
    }
```

and implement it on `VsockDevice` (mod.rs) as `fn vsock_poll_set(&self) -> Vec<std::os::unix::io::RawFd> { self.muxer.poll_set() }` (Muxer::poll_set already returns `Vec<RawFd>`). (Add these three small pieces; they are part of Task 6.)

- [ ] **Step 4: Restore wiring.** In `run_restore`, add a `"vsock"` match arm that rebuilds a fresh `VsockDevice` (empty muxer — connections are not snapshotted, per spec) and spawns the reactor:

```rust
"vsock" => {
    // Connections are not part of the snapshot (E1 TODO); restore an empty device.
    // The uds base is taken from the --vsock-uds flag on the restore invocation, or
    // skip the reactor if absent.
    let guest_ram_vsock = GuestRam::new(host as *mut u8, RAM_SIZE as usize, layout::RAM_BASE);
    let uds = vsock_uds.clone().unwrap_or_else(|| std::env::temp_dir().join("ignition-vsock"));
    let vsock_dev = VsockDevice::new(uds);
    let handle = mgr.add_restored(rec, move |irq| {
        VirtioMmio::new("vsock", Box::new(vsock_dev), guest_ram_vsock, irq)
    }).map_err(io::Error::other)?;
    spawn_vsock_reactor(handle);
}
```

(Restore's arg parsing must also accept `--vsock-uds`; the flag is parsed in the same `main` arg loop before `run_restore` is dispatched, so `vsock_uds` is in scope — thread it into `run_restore` like `snap_dir`. If `run_restore` is a separate fn, pass `vsock_uds` as a parameter.)

- [ ] **Step 5: Build, sign, gate.**

```bash
cargo build --workspace && cargo clippy --workspace && cargo test --workspace
scripts/sign.sh target/debug/boot
```
Expected: clean build, 0 clippy, all suites green.

- [ ] **Step 6: Live verification.**

Host listener + guest client round-trip:
```bash
# 1) Start a host listener on the per-port socket BEFORE booting:
UDS=/tmp/ign-vsock
rm -f ${UDS}_1234
( socat UNIX-LISTEN:${UDS}_1234,fork EXEC:'/bin/cat' & ) 2>/dev/null   # echo server
# 2) Boot with vsock:
target/debug/boot --vsock-uds $UDS kimage/out/Image kimage/out/rootfs.ext4
# 3) In the guest (type slowly via pty), confirm the driver bound and round-trip:
#    dmesg | grep -i vsock        -> "virtio_vsock" registered
#    echo hello | socat - VSOCK-CONNECT:2:1234   -> prints "hello" (echoed by host cat)
```
Report: whether `virtio_vsock` bound, and whether the echo round-trip returned the string. If the guest lacks `socat`, use a tiny busybox/python vsock client (AF_VSOCK=40, cid=2, port=1234). If no round-trip, report dmesg + whether the host listener accepted a connection.

Snapshot/restore/clone regression (vsock not in the snapshot path unless `--vsock-uds` used; run the standard drivers which don't pass it):
```bash
rm -rf snapshot snapshot2
python3 scripts/restore_test.py
python3 scripts/restore_clone_test.py
```
Report the RESULT lines.

- [ ] **Step 7: Commit**

```bash
git add spike/src/bin/boot.rs crates/devices/src/virtio/mmio.rs crates/devices/src/virtio/vsock/mod.rs
git commit -m "feat(boot): wire virtio-vsock (--vsock-uds) with poll-based RX reactor"
```

---

## Notes for the implementer

- **TDD order caveat:** Task 4 (`VsockDevice`) references `VirtioDevice::fill_rx`, added in Task 5. Implement Task 5's trait method first (or commit 4+5 together) so the crate compiles. The reviewer should treat Tasks 4 and 5 as a pair.
- **Reactor simplification vs the spec:** the spec described a self-pipe wakeup; this plan uses a 200 ms timed `poll` instead (simpler, no pipe). TX-created connections are picked up on the next tick — acceptable latency for E1. A self-pipe wakeup is a future refinement (note it in the commit / a code comment).
- **Credit:** `peer_free = peer_buf_alloc - (rx_cnt - peer_fwd_cnt)` (Wrapping). We never emit a host→guest `RW` larger than `peer_free` (capped in `Connection::read_host`). We advertise `BUF_ALLOC` + our `fwd_cnt` on every guest-bound header so the guest keeps crediting us.
- **Non-blocking host sockets:** set right after `connect`. Guest→host writes buffer in `txbuf` on `WouldBlock` and flush in `service()`; host→guest reads cap at `peer_free`.
- **Single writable RX descriptor assumption:** `fill_guest_rx` writes a packet into the first writable descriptor of an RX chain and caps to its length (`READ_CHUNK ≤ 4096`, headers 44 B, so a normal ≥4 KB guest RX buffer always fits). Multi-descriptor RX chains are not split (fine for E1).
- **No `layout`/`fdt`/`device_manager`/`snapshot` changes** — vsock is a `virtio,mmio` device (reuses the kind). It is wired only with `--vsock-uds`; connections aren't snapshotted (E1 TODO).
- **Device id string `"vsock"`** must match across the fresh `add`, the restore arm, and is what `VirtioMmio::snapshot_id()` returns.
- **`VsockDevice` must be `Send`** (it moves into `Box<dyn VirtioDevice>` shared with the reactor via `Arc<Mutex<VirtioMmio>>`): `UnixStream` + `HashMap` + `VecDeque` + `PathBuf` are all `Send`. No `Sync` needed (the `Mutex` provides it).
