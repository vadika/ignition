//! A single guest<->host vsock stream. The host side is a non-blocking UnixStream;
//! credit accounting follows the virtio-vsock model so neither side overruns.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::num::Wrapping;
use std::os::unix::net::UnixStream;

/// Our advertised RX buffer capacity (bytes the guest may send us before waiting
/// for a credit update). Also bounds a single host->guest read.
pub const BUF_ALLOC: u32 = 64 * 1024;
/// Max payload per host->guest RW packet (fits a typical guest RX descriptor).
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
    peer_buf_alloc: Wrapping<u32>,
    peer_fwd_cnt: Wrapping<u32>,
    rx_cnt: Wrapping<u32>,
    fwd_cnt: Wrapping<u32>,
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
    pub fn raw_fd(&self) -> std::os::unix::io::RawFd {
        use std::os::unix::io::AsRawFd;
        self.host.as_raw_fd()
    }

    // used by Task 3 muxer
    #[allow(dead_code)]
    pub fn update_peer_credit(&mut self, buf_alloc: u32, fwd_cnt: u32) {
        self.peer_buf_alloc = Wrapping(buf_alloc);
        self.peer_fwd_cnt = Wrapping(fwd_cnt);
    }

    /// Bytes we may still send to the guest right now.
    pub fn peer_free(&self) -> u32 {
        (self.peer_buf_alloc - (self.rx_cnt - self.peer_fwd_cnt)).0
    }

    /// Queue guest->host payload and flush as much as the non-blocking socket takes.
    pub fn enqueue_tx(&mut self, data: &[u8]) {
        self.txbuf.extend(data.iter().copied());
        self.flush_tx();
    }

    /// Flush buffered guest->host bytes to the host stream (non-blocking). Advances
    /// fwd_cnt by what was written. Stops on WouldBlock; marks Closed on hard error.
    pub fn flush_tx(&mut self) {
        while !self.txbuf.is_empty() {
            self.txbuf.make_contiguous();
            let (front, _) = self.txbuf.as_slices();
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

    /// Read up to `min(peer_free, READ_CHUNK)` bytes of host->guest data. Returns
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
        let mut conn = Connection::new(1, 2, dev, 4, 0);
        assert_eq!(conn.peer_free(), 4);
        conn.rx_cnt = Wrapping(4);
        assert_eq!(conn.peer_free(), 0);
    }

    #[test]
    fn eof_marks_closed() {
        let (mut conn, app) = pair();
        drop(app);
        assert!(conn.read_host().is_none());
        assert_eq!(conn.state(), ConnState::Closed);
    }
}
