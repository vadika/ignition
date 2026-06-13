//! virtio-vsock device (guest->host, E1). 3 queues: RX(0), TX(1), EVENT(2).

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

    fn handle_tx(&mut self, vq: &mut Virtqueue, mem: &GuestRam) -> bool {
        let mut serviced = false;
        while let Some(chain) = vq.pop_avail(mem) {
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
                let payload: &[u8] = if bytes.len() > VSOCK_HDR_SIZE { &bytes[VSOCK_HDR_SIZE..] } else { &[] };
                self.muxer.handle_tx(&hdr, payload);
            }
            vq.push_used(mem, chain.head, 0);
            serviced = true;
        }
        serviced
    }

    /// Lay the 44-byte header + payload across the chain's writable descriptors in
    /// order. The Linux virtio_vsock driver posts each RX buffer as TWO descriptors
    /// (a header desc + a data desc), so the payload must spill into the second
    /// descriptor rather than being truncated into the first. Returns bytes written.
    fn write_rx(chain: &crate::virtio::queue::DescChain, mem: &GuestRam, pkt: &RxPacket) -> u32 {
        let mut buf = pkt.hdr.to_bytes().to_vec();
        buf.extend_from_slice(&pkt.data);
        let mut written = 0usize;
        for d in chain.descriptors.iter().filter(|d| d.writable) {
            if written >= buf.len() {
                break;
            }
            let take = std::cmp::min(d.len as usize, buf.len() - written);
            mem.write_slice(d.addr, &buf[written..written + take]);
            written += take;
        }
        written as u32
    }

    fn fill_guest_rx(&mut self, vq: &mut Virtqueue, mem: &GuestRam) -> bool {
        let mut delivered = false;
        while self.muxer.rx_pending() {
            let Some(chain) = vq.pop_avail(mem) else { break };
            if !chain.descriptors.iter().any(|d| d.writable) {
                vq.push_used(mem, chain.head, 0);
                continue;
            }
            let pkt = self.muxer.pop_rx().unwrap();
            let len = Self::write_rx(&chain, mem, &pkt);
            vq.push_used(mem, chain.head, len);
            delivered = true;
        }
        delivered
    }
}

impl VirtioDevice for VsockDevice {
    fn device_id(&self) -> u32 { VIRTIO_ID_VSOCK }
    fn device_features(&self, _sel: u32) -> u32 { 0 }
    fn config_read(&self, offset: u64, data: &mut [u8]) {
        let cfg = VSOCK_GUEST_CID.to_le_bytes();
        for (i, b) in data.iter_mut().enumerate() {
            let o = offset as usize + i;
            *b = if o < cfg.len() { cfg[o] } else { 0 };
        }
    }
    fn queue_count(&self) -> usize { 3 }
    fn handle_notify(&mut self, queue_idx: usize, vq: &mut Virtqueue, mem: &GuestRam) -> bool {
        match queue_idx {
            TXQ => self.handle_tx(vq, mem),
            RXQ => self.fill_guest_rx(vq, mem),
            EVQ => {
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
        self.muxer.service();
        self.fill_guest_rx(rx_vq, mem)
    }
    fn vsock_poll_set(&self) -> Vec<std::os::unix::io::RawFd> {
        self.muxer.poll_set()
    }
    fn save(&self) -> serde_json::Value {
        serde_json::json!({ "conns": self.muxer.save_conns() })
    }
    fn restore(&mut self, v: &serde_json::Value) -> Result<(), String> {
        let conns = v.get("conns").and_then(|c| c.as_array())
            .ok_or("vsock: missing conns array")?;
        let keys = conns.iter().map(|pair| {
            let a = pair.as_array().ok_or("vsock: conn not a pair")?;
            if a.len() != 2 { return Err("vsock: conn not a pair".to_string()); }
            let g = a.first().and_then(|x| x.as_u64()).ok_or("vsock: bad guest_port")? as u32;
            let h = a.get(1).and_then(|x| x.as_u64()).ok_or("vsock: bad host_port")? as u32;
            Ok::<(u32, u32), String>((g, h))
        }).collect::<Result<Vec<_>, _>>()?;
        self.muxer.seed_rst(keys);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::virtio::mmio::VirtioDevice as _;

    #[test]
    fn identity_and_config() {
        let dev = VsockDevice::new(PathBuf::from("/tmp/x/vsock"));
        assert_eq!(dev.device_id(), 19);
        assert_eq!(dev.queue_count(), 3);
        let mut c = [0u8; 8];
        dev.config_read(0, &mut c);
        assert_eq!(u64::from_le_bytes(c), 3);
    }

    #[test]
    fn vsock_save_restore_seeds_rst() {
        // A device with no live conns saves an empty list and restores cleanly.
        let dev = VsockDevice::new(PathBuf::from("/tmp/ign-x/vsock"));
        let saved = dev.save();
        assert_eq!(saved, serde_json::json!({ "conns": [] }));

        // Restoring a saved conn list seeds the muxer's pending RSTs.
        let mut dev2 = VsockDevice::new(PathBuf::from("/tmp/ign-x/vsock"));
        dev2.restore(&serde_json::json!({ "conns": [[1024, 5000]] })).expect("restore ok");
        // The seeded RST surfaces on the next service()/RST drain.
        dev2.muxer.service();
        let pkt = dev2.muxer.pop_rx().expect("RST queued for restored conn");
        assert_eq!((pkt.hdr.dst_port, pkt.hdr.src_port), (1024, 5000));
    }
}
