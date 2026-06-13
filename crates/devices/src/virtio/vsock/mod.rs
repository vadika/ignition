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

    fn write_rx(chain_addr: u64, chain_cap: usize, mem: &GuestRam, pkt: &RxPacket) -> u32 {
        let mut buf = pkt.hdr.to_bytes().to_vec();
        buf.extend_from_slice(&pkt.data);
        let n = std::cmp::min(buf.len(), chain_cap);
        mem.write_slice(chain_addr, &buf[..n]);
        n as u32
    }

    fn fill_guest_rx(&mut self, vq: &mut Virtqueue, mem: &GuestRam) -> bool {
        let mut delivered = false;
        while self.muxer.rx_pending() {
            let Some(chain) = vq.pop_avail(mem) else { break };
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
