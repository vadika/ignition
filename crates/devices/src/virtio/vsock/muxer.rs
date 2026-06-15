//! Routes guest TX packets to per-(guest_port,host_port) connections, connects host
//! Unix sockets ({uds}_{port}) for guest-initiated REQUESTs, and queues packets bound
//! for the guest (RESPONSE/RST/RW/CREDIT_UPDATE) in `rxq`.

use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use super::connection::{Connection, ConnState, BUF_ALLOC};
use super::packet::*;

/// A packet bound for the guest: header + optional payload.
pub struct RxPacket {
    pub hdr: VsockHeader,
    pub data: Vec<u8>,
}

/// A host control client whose `CONNECT <port>\n` line may arrive in pieces.
struct ControlStream {
    stream: UnixStream,
    buf: Vec<u8>,
}

pub struct Muxer {
    uds_base: PathBuf,
    conns: HashMap<(u32, u32), Connection>, // (guest_port, host_port)
    rxq: VecDeque<RxPacket>,
    /// Connection keys carried over a snapshot; RST'd to the guest on the first service() after restore (host UDS peers no longer exist).
    pending_rst: Vec<(u32, u32)>,
    /// Host control clients mid-CONNECT, keyed by fd.
    ctrl_streams: HashMap<RawFd, ControlStream>,
    /// Ephemeral host-port allocator for host-initiated connections.
    next_host_port: u32,
}

impl Muxer {
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

    /// Open connection keys for the snapshot (guest_port, host_port). Sorted so
    /// snapshots are reproducible (HashMap iteration order is nondeterministic).
    pub fn save_conns(&self) -> Vec<(u32, u32)> {
        let mut keys: Vec<(u32, u32)> = self.conns.keys().copied().collect();
        keys.sort_unstable();
        keys
    }

    /// Seed connections that existed at snapshot time; service() will RST each.
    pub fn seed_rst(&mut self, conns: Vec<(u32, u32)>) {
        self.pending_rst = conns;
    }

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

    /// Per-port host socket path: {uds_base}_{host_port} (same dir as uds_base).
    fn port_path(&self, host_port: u32) -> PathBuf {
        let name = self.uds_base.file_name().and_then(|s| s.to_str()).unwrap_or("vsock");
        self.uds_base.with_file_name(format!("{name}_{host_port}"))
    }

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
                if let Some(pos) = cs.buf.iter().position(|&b| b == b'\n') {
                    let line = String::from_utf8_lossy(&cs.buf[..pos]).trim().to_string();
                    done = true; // the stream moves into a conn or is dropped
                    if let Some(port_str) = line.strip_prefix("CONNECT ")
                        && let Ok(guest_port) = port_str.trim().parse::<u32>()
                    {
                        promote = Some((guest_port, cs.stream.try_clone().expect("dup ctrl stream")));
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
                // ctrl_hdr already sets src_cid=HOST, dst_cid=GUEST for the REQUEST.
                let hdr = Self::ctrl_hdr(OP_REQUEST, guest_port, host_port, 0);
                self.rxq.push_back(RxPacket { hdr, data: Vec::new() });
            }
        }
    }

    /// Drive one guest->host TX packet. `payload` is the RW data (empty otherwise).
    pub fn handle_tx(&mut self, hdr: &VsockHeader, payload: &[u8]) {
        let guest_port = hdr.src_port;
        let host_port = hdr.dst_port;
        let key = (guest_port, host_port);
        match hdr.op {
            OP_REQUEST => {
                let path = self.port_path(host_port);
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
            OP_RESPONSE => {
                if let Some(conn) = self.conns.get_mut(&key)
                    && conn.state() == ConnState::LocalInit
                {
                    conn.confirm_established(hdr.buf_alloc, hdr.fwd_cnt);
                    conn.write_host_raw(format!("OK {host_port}\n").as_bytes());
                }
            }
            _ => {
                if self.conns.contains_key(&key) {
                    self.queue(OP_RST, guest_port, host_port, 0);
                }
            }
        }
    }

    /// Reactor pass: flush pending guest->host data and read host->guest data into rxq.
    /// Removes connections that reached Closed (queuing a final RST).
    pub fn service(&mut self) {
        // Post-restore: RST every connection that existed at snapshot time, once.
        for (guest_port, host_port) in self.pending_rst.drain(..) {
            self.rxq.push_back(RxPacket {
                hdr: Self::ctrl_hdr(OP_RST, guest_port, host_port, 0),
                data: Vec::new(),
            });
        }
        let mut new_rx: Vec<RxPacket> = Vec::new();
        let mut closed: Vec<(u32, u32)> = Vec::new();
        for (key, conn) in self.conns.iter_mut() {
            conn.flush_tx();
            while let Some(data) = conn.read_host() {
                let mut hdr = Self::ctrl_hdr(OP_RW, conn.guest_port, conn.host_port, conn.fwd_cnt());
                hdr.len = data.len() as u32;
                new_rx.push(RxPacket { hdr, data });
            }
            if conn.state() == ConnState::Closed {
                closed.push(*key);
            }
        }
        for pkt in new_rx {
            self.rxq.push_back(pkt);
        }
        for key in closed {
            if let Some(conn) = self.conns.remove(&key) {
                self.queue(OP_RST, conn.guest_port, conn.host_port, 0);
            }
        }
    }

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
}
