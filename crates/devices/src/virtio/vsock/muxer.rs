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
    /// Connection keys carried over a snapshot; RST'd to the guest on the first service() after restore (host UDS peers no longer exist).
    pending_rst: Vec<(u32, u32)>,
}

impl Muxer {
    pub fn new(uds_base: PathBuf) -> Muxer {
        Muxer { uds_base, conns: HashMap::new(), rxq: VecDeque::new(), pending_rst: Vec::new() }
    }

    /// Open connection keys for the snapshot (guest_port, host_port).
    pub fn save_conns(&self) -> Vec<(u32, u32)> {
        self.conns.keys().copied().collect()
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
            OP_RESPONSE => { /* host->guest connect ack — E2 */ }
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

    /// Host fds to poll (POLLIN). Buffered guest->host tx is flushed each service()
    /// tick, so POLLOUT is not needed (and would busy-loop on idle-writable sockets).
    pub fn poll_set(&self) -> Vec<RawFd> {
        self.conns.values().map(|c| c.raw_fd()).collect()
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
}
