//! virtio-rng (VIRTIO_ID_RNG): fills guest-posted buffers with host entropy.
//! A single device-writable queue; the guest hands us writable descriptors and we
//! fill them from the OS CSPRNG via getentropy.

use std::os::raw::c_void;

use super::guest_ram::GuestRam;
use super::mmio::VirtioDevice;
use super::queue::Virtqueue;

/// VIRTIO_ID_RNG.
const VIRTIO_ID_RNG: u32 = 4;

/// Fill `buf` from the OS CSPRNG. `getentropy` accepts at most 256 bytes per call.
/// With a valid pointer and `len <= 256` it cannot fail in normal operation, so a
/// nonzero return is a programming/environment error and panics rather than
/// silently producing weak entropy.
fn fill_random(buf: &mut [u8]) {
    for chunk in buf.chunks_mut(256) {
        let ret = unsafe { libc::getentropy(chunk.as_mut_ptr() as *mut c_void, chunk.len()) };
        assert_eq!(ret, 0, "getentropy failed: {}", std::io::Error::last_os_error());
    }
}

/// Stateless virtio entropy source.
pub struct VirtioRng;

impl VirtioRng {
    pub fn new() -> Self {
        VirtioRng
    }
}

impl Default for VirtioRng {
    fn default() -> Self {
        Self::new()
    }
}

impl VirtioDevice for VirtioRng {
    fn device_id(&self) -> u32 {
        VIRTIO_ID_RNG
    }

    fn device_features(&self, _sel: u32) -> u32 {
        0
    }

    fn config_read(&self, _offset: u64, _data: &mut [u8]) {
        // rng has no device-specific config space.
    }

    fn queue_count(&self) -> usize {
        1
    }

    fn handle_notify(&mut self, _queue_idx: usize, vq: &mut Virtqueue, mem: &GuestRam) -> bool {
        let mut serviced = false;
        while let Some(chain) = vq.pop_avail(mem) {
            let mut written = 0u32;
            for d in &chain.descriptors {
                if d.writable {
                    let mut buf = vec![0u8; d.len as usize];
                    fill_random(&mut buf);
                    if mem.write_slice(d.addr, &buf) {
                        written += d.len;
                    }
                }
            }
            vq.push_used(mem, chain.head, written);
            serviced = true;
        }
        serviced
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: u64 = 0x4000_0000;
    const DESC: u64 = BASE + 0x1000;
    const AVAIL: u64 = BASE + 0x2000;
    const USED: u64 = BASE + 0x3000;
    const DATA: u64 = BASE + 0x500;

    fn mem(backing: &mut Vec<u8>) -> GuestRam {
        GuestRam::new(backing.as_mut_ptr(), backing.len(), BASE)
    }

    // VIRTQ_DESC_F_NEXT = 1, VIRTQ_DESC_F_WRITE = 2.
    fn write_desc(m: &GuestRam, i: u64, addr: u64, len: u32, flags: u16, next: u16) {
        let d = DESC + i * 16;
        m.write_slice(d, &addr.to_le_bytes());
        m.write_slice(d + 8, &len.to_le_bytes());
        m.write_slice(d + 12, &flags.to_le_bytes());
        m.write_slice(d + 14, &next.to_le_bytes());
    }

    fn offer_head0(m: &GuestRam) {
        m.write_u16(AVAIL + 2, 1); // avail.idx = 1
        m.write_u16(AVAIL + 4, 0); // ring[0] = desc 0
    }

    #[test]
    fn identity() {
        let rng = VirtioRng::new();
        assert_eq!(rng.device_id(), 4);
        assert_eq!(rng.queue_count(), 1);
        assert_eq!(rng.device_features(0), 0);
        assert_eq!(rng.device_features(1), 0);
    }

    #[test]
    fn fills_writable_descriptor() {
        let mut backing = vec![0xAAu8; 0x4000];
        let m = mem(&mut backing);
        write_desc(&m, 0, DATA, 64, 2, 0);
        offer_head0(&m);
        let mut vq = Virtqueue::new(8, DESC, AVAIL, USED);

        let mut rng = VirtioRng::new();
        assert!(rng.handle_notify(0, &mut vq, &m));

        assert_eq!(m.read_u32(USED + 4), Some(0));
        assert_eq!(m.read_u32(USED + 8), Some(64));
        let mut out = [0u8; 64];
        assert!(m.read_slice(DATA, &mut out));
        assert!(out.iter().any(|&b| b != 0xAA), "rng did not write entropy");
    }

    #[test]
    fn read_only_chain_fills_nothing() {
        let mut backing = vec![0xAAu8; 0x4000];
        let m = mem(&mut backing);
        write_desc(&m, 0, DATA, 64, 0, 0);
        offer_head0(&m);
        let mut vq = Virtqueue::new(8, DESC, AVAIL, USED);

        let mut rng = VirtioRng::new();
        assert!(rng.handle_notify(0, &mut vq, &m));

        assert_eq!(m.read_u32(USED + 4), Some(0));
        assert_eq!(m.read_u32(USED + 8), Some(0));
        let mut out = [0u8; 64];
        assert!(m.read_slice(DATA, &mut out));
        assert!(out.iter().all(|&b| b == 0xAA), "non-writable desc must not be filled");
    }

    #[test]
    fn multi_descriptor_chain_fills_all_writable() {
        let mut backing = vec![0xAAu8; 0x4000];
        let m = mem(&mut backing);
        write_desc(&m, 0, DATA, 16, 1 | 2, 1);
        write_desc(&m, 1, DATA + 0x100, 32, 2, 0);
        offer_head0(&m);
        let mut vq = Virtqueue::new(8, DESC, AVAIL, USED);

        let mut rng = VirtioRng::new();
        assert!(rng.handle_notify(0, &mut vq, &m));

        assert_eq!(m.read_u32(USED + 4), Some(0));
        assert_eq!(m.read_u32(USED + 8), Some(16 + 32));
        let mut a = [0u8; 16];
        let mut b = [0u8; 32];
        assert!(m.read_slice(DATA, &mut a));
        assert!(m.read_slice(DATA + 0x100, &mut b));
        assert!(a.iter().any(|&x| x != 0xAA));
        assert!(b.iter().any(|&x| x != 0xAA));
    }
}
