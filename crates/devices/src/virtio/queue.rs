//! Minimal split virtqueue (virtio 1.0 §2.6), processed synchronously.

use super::guest_ram::GuestRam;

const VIRTQ_DESC_F_NEXT: u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2;
const DESC_SIZE: u64 = 16;

/// One resolved descriptor.
#[derive(Debug, PartialEq, Eq)]
pub struct Desc {
    pub addr: u64,
    pub len: u32,
    /// Device-writable (VIRTQ_DESC_F_WRITE set).
    pub writable: bool,
}

/// A resolved descriptor chain.
#[derive(Debug, PartialEq, Eq)]
pub struct DescChain {
    pub head: u16,
    pub descriptors: Vec<Desc>,
}

pub struct Virtqueue {
    size: u16,
    desc_addr: u64,
    driver_addr: u64, // available ring
    device_addr: u64, // used ring
    last_avail_idx: u16,
    used_idx: u16,
}

impl Virtqueue {
    pub fn new(size: u16, desc_addr: u64, driver_addr: u64, device_addr: u64) -> Self {
        Self { size, desc_addr, driver_addr, device_addr, last_avail_idx: 0, used_idx: 0 }
    }

    /// The next not-yet-seen available chain, or `None` if drained.
    ///
    /// avail ring layout: `{flags: u16, idx: u16, ring: [u16; size]}`.
    pub fn pop_avail(&mut self, mem: &GuestRam) -> Option<DescChain> {
        if self.size == 0 {
            return None;
        }
        let avail_idx = mem.read_u16(self.driver_addr + 2)?;
        if self.last_avail_idx == avail_idx {
            return None;
        }
        let slot = self.last_avail_idx % self.size;
        let head = mem.read_u16(self.driver_addr + 4 + u64::from(slot) * 2)?;
        // Consume the avail entry even if it turns out malformed, so a bad index
        // can't stall the ring (it is dropped, not retried).
        self.last_avail_idx = self.last_avail_idx.wrapping_add(1);
        if head >= self.size {
            return None; // malformed head index; entry consumed and dropped
        }

        let mut descriptors = Vec::new();
        let mut idx = head;
        // Bounded by `size` to defend against a malformed cyclic chain.
        for _ in 0..self.size {
            if idx >= self.size {
                break; // malformed next index; end the chain here
            }
            let d = self.desc_addr + u64::from(idx) * DESC_SIZE;
            let addr = mem.read_u64(d)?;
            let len = mem.read_u32(d + 8)?;
            let flags = mem.read_u16(d + 12)?;
            let next = mem.read_u16(d + 14)?;
            descriptors.push(Desc { addr, len, writable: flags & VIRTQ_DESC_F_WRITE != 0 });
            // Note: VIRTQ_DESC_F_INDIRECT (bit 2) is not supported — an indirect
            // descriptor is treated as a direct buffer. The block driver does not
            // use indirect descriptors, so this does not arise in practice.
            if flags & VIRTQ_DESC_F_NEXT == 0 {
                break;
            }
            idx = next;
        }
        Some(DescChain { head, descriptors })
    }

    /// Returns `(last_avail_idx, used_idx)` — the consumer/producer positions, for snapshots.
    pub fn indices(&self) -> (u16, u16) {
        (self.last_avail_idx, self.used_idx)
    }

    /// Restore the positions onto a queue rebuilt from the same ring addresses.
    pub fn set_indices(&mut self, last_avail: u16, used: u16) {
        self.last_avail_idx = last_avail;
        self.used_idx = used;
    }

    /// Append a used element and publish it (the `idx` store happens last).
    ///
    /// used ring layout: `{flags: u16, idx: u16, ring: [{id: u32, len: u32}; size]}`.
    pub fn push_used(&mut self, mem: &GuestRam, head: u16, len: u32) {
        if self.size == 0 {
            return;
        }
        let slot = self.used_idx % self.size;
        let elem = self.device_addr + 4 + u64::from(slot) * 8;
        mem.write_u32(elem, u32::from(head));
        mem.write_u32(elem + 4, len);
        self.used_idx = self.used_idx.wrapping_add(1);
        mem.write_u16(self.device_addr + 2, self.used_idx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Memory map for the tests: desc table @0x1000, avail @0x2000, used @0x3000.
    const BASE: u64 = 0x4000_0000;
    const DESC: u64 = BASE + 0x1000;
    const AVAIL: u64 = BASE + 0x2000;
    const USED: u64 = BASE + 0x3000;

    fn mem(backing: &mut Vec<u8>) -> GuestRam {
        GuestRam::new(backing.as_mut_ptr(), backing.len(), BASE)
    }

    fn write_desc(m: &GuestRam, i: u64, addr: u64, len: u32, flags: u16, next: u16) {
        let d = DESC + i * 16;
        m.write_slice(d, &addr.to_le_bytes());
        m.write_slice(d + 8, &len.to_le_bytes());
        m.write_slice(d + 12, &flags.to_le_bytes());
        m.write_slice(d + 14, &next.to_le_bytes());
    }

    #[test]
    fn pop_single_descriptor_chain() {
        let mut backing = vec![0u8; 0x4000];
        let m = mem(&mut backing);
        write_desc(&m, 0, 0x4000_0500, 16, 0, 0); // no NEXT
        m.write_u16(AVAIL + 2, 1); // avail.idx = 1
        m.write_u16(AVAIL + 4, 0); // ring[0] = desc 0

        let mut vq = Virtqueue::new(8, DESC, AVAIL, USED);
        let chain = vq.pop_avail(&m).unwrap();
        assert_eq!(chain.head, 0);
        assert_eq!(chain.descriptors, vec![Desc { addr: 0x4000_0500, len: 16, writable: false }]);
        assert!(vq.pop_avail(&m).is_none()); // drained
    }

    #[test]
    fn pop_walks_next_and_marks_writable() {
        let mut backing = vec![0u8; 0x4000];
        let m = mem(&mut backing);
        write_desc(&m, 0, 0x4000_0500, 16, VIRTQ_DESC_F_NEXT, 1); // -> desc 1
        write_desc(&m, 1, 0x4000_0600, 512, VIRTQ_DESC_F_WRITE, 0); // writable, end
        m.write_u16(AVAIL + 2, 1);
        m.write_u16(AVAIL + 4, 0);

        let mut vq = Virtqueue::new(8, DESC, AVAIL, USED);
        let chain = vq.pop_avail(&m).unwrap();
        assert_eq!(chain.descriptors.len(), 2);
        assert!(!chain.descriptors[0].writable);
        assert!(chain.descriptors[1].writable);
        assert_eq!(chain.descriptors[1].addr, 0x4000_0600);
    }

    #[test]
    fn push_used_writes_element_and_bumps_idx() {
        let mut backing = vec![0u8; 0x4000];
        let m = mem(&mut backing);
        let mut vq = Virtqueue::new(8, DESC, AVAIL, USED);
        vq.push_used(&m, 3, 512);
        assert_eq!(m.read_u32(USED + 4), Some(3)); // ring[0].id
        assert_eq!(m.read_u32(USED + 8), Some(512)); // ring[0].len
        assert_eq!(m.read_u16(USED + 2), Some(1)); // used.idx
    }

    #[test]
    fn pop_drops_out_of_range_head() {
        let mut backing = vec![0u8; 0x4000];
        let m = mem(&mut backing);
        write_desc(&m, 0, 0x4000_0500, 16, 0, 0); // valid desc 0
        m.write_u16(AVAIL + 2, 2); // avail.idx = 2 (two entries)
        m.write_u16(AVAIL + 4, 255); // ring[0] = bogus head 255 (>= size 8)
        m.write_u16(AVAIL + 6, 0); // ring[1] = valid head 0

        let mut vq = Virtqueue::new(8, DESC, AVAIL, USED);
        assert!(vq.pop_avail(&m).is_none()); // bogus entry dropped, not retried
        let chain = vq.pop_avail(&m).unwrap(); // the next entry still works
        assert_eq!(chain.head, 0);
    }

    #[test]
    fn push_used_wraps_the_ring() {
        let mut backing = vec![0u8; 0x4000];
        let m = mem(&mut backing);
        let mut vq = Virtqueue::new(8, DESC, AVAIL, USED);
        for i in 0..9u32 {
            vq.push_used(&m, i as u16, i);
        }
        // 9th element (i=8) wrapped to slot 0; used.idx advanced to 9.
        assert_eq!(m.read_u32(USED + 4), Some(8)); // ring[0].id overwritten by the 9th
        assert_eq!(m.read_u16(USED + 2), Some(9));
    }
}
