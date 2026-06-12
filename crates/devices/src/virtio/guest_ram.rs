//! A bounds-checked view of guest RAM for synchronous virtio DMA.
//!
//! Wraps a raw pointer into the host mmap that backs guest RAM. It is touched
//! only on the vCPU thread during an MMIO exit, when the guest is paused — so the
//! access is exclusive and single-threaded, justifying the `unsafe impl Send`.

pub struct GuestRam {
    ptr: *mut u8,
    len: usize,
    base: u64,
}

// SAFETY: only accessed on the vCPU thread while the guest is paused inside an
// MMIO exit (the device is dispatched synchronously from the run loop). No
// concurrent access occurs.
unsafe impl Send for GuestRam {}

impl GuestRam {
    /// `ptr`/`len` describe the host mapping; `base` is the guest physical
    /// address it is mapped at.
    pub fn new(ptr: *mut u8, len: usize, base: u64) -> Self {
        Self { ptr, len, base }
    }

    fn offset(&self, gpa: u64, n: usize) -> Option<usize> {
        let off = usize::try_from(gpa.checked_sub(self.base)?).ok()?;
        if off.checked_add(n)? <= self.len {
            Some(off)
        } else {
            None
        }
    }

    pub fn read_slice(&self, gpa: u64, out: &mut [u8]) -> bool {
        match self.offset(gpa, out.len()) {
            Some(off) => {
                // SAFETY: bounds checked by `offset`; exclusive access (see above).
                unsafe { std::ptr::copy_nonoverlapping(self.ptr.add(off), out.as_mut_ptr(), out.len()) };
                true
            }
            None => false,
        }
    }

    pub fn write_slice(&self, gpa: u64, data: &[u8]) -> bool {
        match self.offset(gpa, data.len()) {
            Some(off) => {
                // SAFETY: bounds checked by `offset`; exclusive access (see above).
                unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), self.ptr.add(off), data.len()) };
                true
            }
            None => false,
        }
    }

    pub fn read_u16(&self, gpa: u64) -> Option<u16> {
        let mut b = [0u8; 2];
        self.read_slice(gpa, &mut b).then(|| u16::from_le_bytes(b))
    }
    pub fn read_u32(&self, gpa: u64) -> Option<u32> {
        let mut b = [0u8; 4];
        self.read_slice(gpa, &mut b).then(|| u32::from_le_bytes(b))
    }
    pub fn read_u64(&self, gpa: u64) -> Option<u64> {
        let mut b = [0u8; 8];
        self.read_slice(gpa, &mut b).then(|| u64::from_le_bytes(b))
    }
    pub fn write_u16(&self, gpa: u64, v: u16) -> bool {
        self.write_slice(gpa, &v.to_le_bytes())
    }
    pub fn write_u32(&self, gpa: u64, v: u32) -> bool {
        self.write_slice(gpa, &v.to_le_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ram(backing: &mut Vec<u8>, base: u64) -> GuestRam {
        GuestRam::new(backing.as_mut_ptr(), backing.len(), base)
    }

    #[test]
    fn round_trip_within_bounds() {
        let mut backing = vec![0u8; 0x1000];
        let m = ram(&mut backing, 0x4000_0000);
        assert!(m.write_u32(0x4000_0010, 0xdead_beef));
        assert_eq!(m.read_u32(0x4000_0010), Some(0xdead_beef));
        assert!(m.write_slice(0x4000_0020, &[1, 2, 3, 4]));
        let mut out = [0u8; 4];
        assert!(m.read_slice(0x4000_0020, &mut out));
        assert_eq!(out, [1, 2, 3, 4]);
    }

    #[test]
    fn out_of_bounds_rejected() {
        let mut backing = vec![0u8; 0x100];
        let m = ram(&mut backing, 0x4000_0000);
        assert!(!m.write_u32(0x4000_00fe, 0)); // crosses the end
        assert_eq!(m.read_u32(0x3fff_ffff), None); // below base
        assert_eq!(m.read_u64(0x5000_0000), None); // far above
    }
}
