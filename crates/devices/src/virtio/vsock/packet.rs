//! virtio-vsock packet header (44 bytes, little-endian) + protocol constants.

pub const VIRTIO_ID_VSOCK: u32 = 19;
pub const VSOCK_TYPE_STREAM: u16 = 1;
pub const VSOCK_CID_HOST: u64 = 2;
pub const VSOCK_GUEST_CID: u64 = 3;
pub const VSOCK_HDR_SIZE: usize = 44;

// ops
pub const OP_REQUEST: u16 = 1;
pub const OP_RESPONSE: u16 = 2;
pub const OP_RST: u16 = 3;
pub const OP_SHUTDOWN: u16 = 4;
pub const OP_RW: u16 = 5;
pub const OP_CREDIT_UPDATE: u16 = 6;
pub const OP_CREDIT_REQUEST: u16 = 7;

// shutdown flags
pub const SHUTDOWN_F_RECV: u32 = 1;
pub const SHUTDOWN_F_SEND: u32 = 2;

/// A 44-byte vsock header. Field offsets per the virtio spec.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VsockHeader {
    pub src_cid: u64,
    pub dst_cid: u64,
    pub src_port: u32,
    pub dst_port: u32,
    pub len: u32,
    pub type_: u16,
    pub op: u16,
    pub flags: u32,
    pub buf_alloc: u32,
    pub fwd_cnt: u32,
}

impl VsockHeader {
    pub fn to_bytes(&self) -> [u8; VSOCK_HDR_SIZE] {
        let mut b = [0u8; VSOCK_HDR_SIZE];
        b[0..8].copy_from_slice(&self.src_cid.to_le_bytes());
        b[8..16].copy_from_slice(&self.dst_cid.to_le_bytes());
        b[16..20].copy_from_slice(&self.src_port.to_le_bytes());
        b[20..24].copy_from_slice(&self.dst_port.to_le_bytes());
        b[24..28].copy_from_slice(&self.len.to_le_bytes());
        b[28..30].copy_from_slice(&self.type_.to_le_bytes());
        b[30..32].copy_from_slice(&self.op.to_le_bytes());
        b[32..36].copy_from_slice(&self.flags.to_le_bytes());
        b[36..40].copy_from_slice(&self.buf_alloc.to_le_bytes());
        b[40..44].copy_from_slice(&self.fwd_cnt.to_le_bytes());
        b
    }

    pub fn from_bytes(b: &[u8]) -> Option<VsockHeader> {
        if b.len() < VSOCK_HDR_SIZE {
            return None;
        }
        let u64a = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        let u32a = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let u16a = |o: usize| u16::from_le_bytes(b[o..o + 2].try_into().unwrap());
        Some(VsockHeader {
            src_cid: u64a(0),
            dst_cid: u64a(8),
            src_port: u32a(16),
            dst_port: u32a(20),
            len: u32a(24),
            type_: u16a(28),
            op: u16a(30),
            flags: u32a(32),
            buf_alloc: u32a(36),
            fwd_cnt: u32a(40),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrips_all_fields() {
        let h = VsockHeader {
            src_cid: 2, dst_cid: 3, src_port: 1024, dst_port: 5555, len: 16,
            type_: VSOCK_TYPE_STREAM, op: OP_RW, flags: 0, buf_alloc: 65536, fwd_cnt: 42,
        };
        let back = VsockHeader::from_bytes(&h.to_bytes()).unwrap();
        assert_eq!(back, h);
    }

    #[test]
    fn short_buffer_rejected() {
        assert!(VsockHeader::from_bytes(&[0u8; 10]).is_none());
    }
}
