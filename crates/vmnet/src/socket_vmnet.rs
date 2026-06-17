//! socket_vmnet client backend: talks to the socket_vmnet daemon over a unix
//! stream, so guest networking needs no sudo (the daemon holds the privilege).
//! Frame protocol (socket_vmnet): 4-byte big-endian length prefix + ethernet
//! frame, both directions. We generate the guest MAC; socket_vmnet learns it.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Mutex;

use ignition_devices::virtio::net::NetBackend;

/// Reject an implausible frame-length header (matches virtio-net MAX_FRAME).
const MAX_FRAME: usize = 65_536;

/// A random locally-administered unicast MAC (`02:..`). Fresh per call, so every
/// boot and restore gets a distinct MAC -> distinct DHCP lease.
pub fn generate_mac() -> [u8; 6] {
    let mut b = [0u8; 6];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut b);
    }
    b[0] = (b[0] & 0xFE) | 0x02; // clear multicast bit, set locally-administered
    b
}

pub struct SocketVmnetBackend {
    write: Mutex<UnixStream>,
    mac: [u8; 6],
}

impl SocketVmnetBackend {
    /// Connect to the socket_vmnet daemon. Returns the backend + the RX frame
    /// receiver (wired to the existing RX feeder exactly like VmnetBackend).
    pub fn start(socket_path: &Path) -> std::io::Result<(SocketVmnetBackend, Receiver<Vec<u8>>)> {
        let stream = UnixStream::connect(socket_path).map_err(|e| {
            std::io::Error::other(format!(
                "--net needs socket_vmnet at {} ({e}). Run scripts/install-socket-vmnet.sh, \
                 or pass --net-direct for the in-process sudo path.",
                socket_path.display()
            ))
        })?;
        let reader = stream.try_clone()?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || reader_loop(reader, tx));
        Ok((SocketVmnetBackend { write: Mutex::new(stream), mac: generate_mac() }, rx))
    }
}

fn reader_loop(mut s: UnixStream, tx: Sender<Vec<u8>>) {
    loop {
        let mut lenb = [0u8; 4];
        if s.read_exact(&mut lenb).is_err() {
            break;
        }
        let n = u32::from_be_bytes(lenb) as usize;
        if n == 0 || n > MAX_FRAME {
            log::warn!("socket_vmnet: bad frame length {n}; closing RX");
            break;
        }
        let mut buf = vec![0u8; n];
        if s.read_exact(&mut buf).is_err() {
            break;
        }
        if tx.send(buf).is_err() {
            break;
        }
    }
}

impl NetBackend for SocketVmnetBackend {
    fn write_frame(&self, frame: &[u8]) -> std::io::Result<()> {
        let mut s = self.write.lock().unwrap();
        s.write_all(&(frame.len() as u32).to_be_bytes())?;
        s.write_all(frame)?;
        Ok(())
    }
    fn mac(&self) -> [u8; 6] {
        self.mac
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn mac_is_unicast_laa_and_random() {
        let a = generate_mac();
        let b = generate_mac();
        // bit0 (0x01) = 0 -> unicast; bit1 (0x02) = 1 -> locally administered.
        assert_eq!(a[0] & 0x03, 0x02);
        assert_ne!(a, b);
    }

    #[test]
    fn framing_roundtrip_against_fake_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sv.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut lenb = [0u8; 4];
            s.read_exact(&mut lenb).unwrap();
            let n = u32::from_be_bytes(lenb) as usize;
            let mut buf = vec![0u8; n];
            s.read_exact(&mut buf).unwrap();
            assert_eq!(buf, b"hello");
            let f = b"world!";
            s.write_all(&(f.len() as u32).to_be_bytes()).unwrap();
            s.write_all(f).unwrap();
            thread::sleep(Duration::from_millis(100));
        });

        let (backend, rx) = SocketVmnetBackend::start(&path).unwrap();
        backend.write_frame(b"hello").unwrap();
        let got = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(got, b"world!");
        server.join().unwrap();
    }
}
