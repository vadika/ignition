//! virtio-net (virtio 1.0 §5.1): exit-driven TX, async RX injection. No offloads.

use super::guest_ram::GuestRam;
use super::mmio::VirtioDevice;
use super::queue::Virtqueue;

/// Feature bit: the device exposes a MAC in config space.
pub const VIRTIO_NET_F_MAC: u32 = 5;

const DEVICE_ID_NET: u32 = 1;
/// `struct virtio_net_hdr` size with num_buffers (virtio 1.0 §5.1.6).
const NET_HDR_LEN: usize = 12;
/// Reject an absurd frame (defends a malformed TX descriptor `len`).
const MAX_FRAME: usize = 65_536;

/// Host side of the NIC: send frames out, supply the guest's MAC.
pub trait NetBackend: Send {
    fn write_frame(&self, frame: &[u8]) -> std::io::Result<()>;
    fn mac(&self) -> [u8; 6];
}

pub struct VirtioNet<B: NetBackend> {
    backend: B,
    mac: [u8; 6],
    dropped_rx: u64,
}

impl<B: NetBackend> VirtioNet<B> {
    pub fn new(backend: B) -> Self {
        let mac = backend.mac();
        Self { backend, mac, dropped_rx: 0 }
    }

    pub fn dropped_rx(&self) -> u64 {
        self.dropped_rx
    }

    /// Drain the TX queue: strip the 12-byte header, send each frame.
    fn drain_tx(&mut self, vq: &mut Virtqueue, mem: &GuestRam) -> bool {
        let mut serviced = false;
        while let Some(chain) = vq.pop_avail(mem) {
            // Gather the chain's readable bytes into one frame buffer.
            let mut buf = Vec::new();
            let mut oversized = false;
            for d in &chain.descriptors {
                if d.writable {
                    continue; // TX buffers are device-readable
                }
                if buf.len() + d.len as usize > MAX_FRAME {
                    oversized = true;
                    break;
                }
                let mut tmp = vec![0u8; d.len as usize];
                if mem.read_slice(d.addr, &mut tmp) {
                    buf.extend_from_slice(&tmp);
                }
            }
            if oversized {
                log::warn!("virtio-net: dropping oversized TX chain ({} bytes+)", buf.len());
            } else if buf.len() > NET_HDR_LEN {
                let frame = &buf[NET_HDR_LEN..];
                if let Err(e) = self.backend.write_frame(frame) {
                    log::warn!("virtio-net TX write failed: {e}");
                }
            }
            vq.push_used(mem, chain.head, 0);
            serviced = true;
        }
        serviced
    }
}

impl<B: NetBackend> VirtioDevice for VirtioNet<B> {
    fn device_id(&self) -> u32 {
        DEVICE_ID_NET
    }
    fn device_features(&self, sel: u32) -> u32 {
        if sel == 0 { 1 << VIRTIO_NET_F_MAC } else { 0 }
    }
    fn config_read(&self, offset: u64) -> u32 {
        // Config space: bytes 0..6 = MAC, 6..8 = status (link up). Word-addressed.
        let mut cfg = [0u8; 8];
        cfg[..6].copy_from_slice(&self.mac);
        // status = VIRTIO_NET_S_LINK_UP (1) — only meaningful if F_STATUS negotiated
        // (it isn't), but harmless to expose.
        cfg[6] = 1;
        let off = offset as usize;
        let mut word = [0u8; 4];
        for (i, b) in word.iter_mut().enumerate() {
            *b = *cfg.get(off + i).unwrap_or(&0);
        }
        u32::from_le_bytes(word)
    }
    fn queue_count(&self) -> usize {
        2 // RX = 0, TX = 1
    }
    fn handle_notify(&mut self, queue_idx: usize, vq: &mut Virtqueue, mem: &GuestRam) -> bool {
        match queue_idx {
            1 => self.drain_tx(vq, mem),
            // RX notifies (guest replenishing buffers) need no service here.
            _ => false,
        }
    }
    fn inject_rx(&mut self, vq: &mut Virtqueue, mem: &GuestRam, frame: &[u8]) -> bool {
        let Some(chain) = vq.pop_avail(mem) else {
            self.dropped_rx += 1;
            return false;
        };
        // Write [zeroed 12-byte hdr | frame] across the chain's writable buffers.
        let mut payload = vec![0u8; NET_HDR_LEN];
        payload.extend_from_slice(frame);
        let mut written = 0usize;
        let mut off = 0usize;
        for d in &chain.descriptors {
            if !d.writable || off >= payload.len() {
                continue;
            }
            let n = (d.len as usize).min(payload.len() - off);
            if mem.write_slice(d.addr, &payload[off..off + n]) {
                off += n;
                written += n;
            }
        }
        vq.push_used(mem, chain.head, written as u32);
        written > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::virtio::guest_ram::GuestRam;
    use crate::virtio::mmio::VirtioDevice;
    use crate::virtio::queue::Virtqueue;
    use std::sync::{Arc, Mutex};

    const BASE: u64 = 0x4000_0000;

    /// Captures TX frames; yields a fixed MAC.
    #[derive(Default, Clone)]
    struct FakeBackend(Arc<Mutex<Vec<Vec<u8>>>>);
    impl NetBackend for FakeBackend {
        fn write_frame(&self, frame: &[u8]) -> std::io::Result<()> {
            self.0.lock().unwrap().push(frame.to_vec());
            Ok(())
        }
        fn mac(&self) -> [u8; 6] {
            [0x52, 0x54, 0x00, 0x12, 0x34, 0x56]
        }
    }

    fn ram(backing: &mut Vec<u8>) -> GuestRam {
        GuestRam::new(backing.as_mut_ptr(), backing.len(), BASE)
    }

    fn write_desc(m: &GuestRam, desc: u64, i: u64, addr: u64, len: u32, flags: u16, next: u16) {
        let d = desc + i * 16;
        m.write_slice(d, &addr.to_le_bytes());
        m.write_slice(d + 8, &len.to_le_bytes());
        m.write_slice(d + 12, &flags.to_le_bytes());
        m.write_slice(d + 14, &next.to_le_bytes());
    }

    #[test]
    fn features_and_config_expose_mac() {
        let net = VirtioNet::new(FakeBackend::default());
        assert_eq!(net.device_id(), 1);
        assert_eq!(net.queue_count(), 2);
        assert_eq!(net.device_features(0) & (1 << VIRTIO_NET_F_MAC), 1 << VIRTIO_NET_F_MAC);
        // config offset 0 = MAC[0..4] little-endian as the device exposes it.
        assert_eq!(net.config_read(0), u32::from_le_bytes([0x52, 0x54, 0x00, 0x12]));
    }

    #[test]
    fn tx_strips_header_and_writes_frame() {
        // TX chain: one descriptor holding [12-byte hdr | 4-byte frame].
        let mut backing = vec![0u8; 0x6000];
        let desc = BASE + 0x1000;
        let avail = BASE + 0x2000;
        let used = BASE + 0x3000;
        let buf = BASE + 0x0100;
        {
            let m = ram(&mut backing);
            let mut pkt = vec![0u8; 12];
            pkt.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
            m.write_slice(buf, &pkt);
            write_desc(&m, desc, 0, buf, pkt.len() as u32, 0, 0); // read-only, end
            m.write_u16(avail + 2, 1);
            m.write_u16(avail + 4, 0);
        }
        let m = ram(&mut backing);
        let backend = FakeBackend::default();
        let mut net = VirtioNet::new(backend.clone());
        let mut vq = Virtqueue::new(8, desc, avail, used);
        assert!(net.handle_notify(1, &mut vq, &m)); // TX = queue 1
        assert_eq!(backend.0.lock().unwrap().as_slice(), &[vec![0xde, 0xad, 0xbe, 0xef]]);
        assert_eq!(m.read_u16(used + 2), Some(1));
    }

    #[test]
    fn rx_prepends_header_into_guest_buffer() {
        // RX queue pre-filled with one writable buffer big enough for hdr + frame.
        let mut backing = vec![0u8; 0x6000];
        let desc = BASE + 0x1000;
        let avail = BASE + 0x2000;
        let used = BASE + 0x3000;
        let buf = BASE + 0x0100;
        {
            let m = ram(&mut backing);
            write_desc(&m, desc, 0, buf, 2048, 2, 0); // WRITE, end
            m.write_u16(avail + 2, 1);
            m.write_u16(avail + 4, 0);
        }
        let m = ram(&mut backing);
        let mut net = VirtioNet::new(FakeBackend::default());
        let mut vq = Virtqueue::new(8, desc, avail, used);
        let frame = [0x11, 0x22, 0x33];
        assert!(net.inject_rx(&mut vq, &m, &frame));
        // Buffer holds [12 zero bytes | frame].
        let mut out = [0u8; 15];
        m.read_slice(buf, &mut out);
        assert_eq!(&out[..12], &[0u8; 12]);
        assert_eq!(&out[12..15], &frame);
        // used.len = hdr + frame = 15.
        assert_eq!(m.read_u32(used + 8), Some(15));
        assert_eq!(m.read_u16(used + 2), Some(1));
    }

    #[test]
    fn rx_with_no_buffer_drops_and_returns_false() {
        let mut backing = vec![0u8; 0x6000];
        let desc = BASE + 0x1000;
        let avail = BASE + 0x2000;
        let used = BASE + 0x3000;
        let m = ram(&mut backing); // avail.idx stays 0 -> no free buffer
        let mut net = VirtioNet::new(FakeBackend::default());
        let mut vq = Virtqueue::new(8, desc, avail, used);
        assert!(!net.inject_rx(&mut vq, &m, &[0x11, 0x22]));
        assert_eq!(net.dropped_rx(), 1);
    }
}
