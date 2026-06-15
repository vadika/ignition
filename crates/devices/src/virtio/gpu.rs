//! virtio-gpu (VIRTIO_ID_GPU = 16), 2D only. controlq (0) carries display/resource
//! commands; cursorq (1) is parsed and ack'd (software cursor). One scanout, fixed
//! mode, B8G8R8A8. RESOURCE_FLUSH presents the scanned-out resource through a
//! `DisplaySink`. No VIRGL/3D/blob; no snapshot of GPU state (that is M5).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[allow(unused_imports)] // DirtyRect and Frame used in later tasks (RESOURCE_FLUSH)
use crate::display::{DirtyRect, DisplaySink, Frame};

use super::guest_ram::GuestRam;
use super::mmio::VirtioDevice;
use super::queue::{DescChain, Virtqueue};

const VIRTIO_ID_GPU: u32 = 16;

#[allow(dead_code)] // used in later tasks
const GET_DISPLAY_INFO: u32 = 0x0100;
#[allow(dead_code)] // used in later tasks
const RESOURCE_CREATE_2D: u32 = 0x0101;
#[allow(dead_code)] // used in later tasks
const RESOURCE_UNREF: u32 = 0x0102;
#[allow(dead_code)] // used in later tasks
const SET_SCANOUT: u32 = 0x0103;
#[allow(dead_code)] // used in later tasks
const RESOURCE_FLUSH: u32 = 0x0104;
#[allow(dead_code)] // used in later tasks
const TRANSFER_TO_HOST_2D: u32 = 0x0105;
#[allow(dead_code)] // used in later tasks
const RESOURCE_ATTACH_BACKING: u32 = 0x0106;
#[allow(dead_code)] // used in later tasks
const RESOURCE_DETACH_BACKING: u32 = 0x0107;

#[allow(dead_code)] // used in later tasks
const RESP_OK_NODATA: u32 = 0x1100;
#[allow(dead_code)] // used in later tasks
const RESP_OK_DISPLAY_INFO: u32 = 0x1101;
const RESP_ERR_UNSPEC: u32 = 0x1200;

const CTRL_HDR_LEN: usize = 24;
#[allow(dead_code)] // referenced by guests/tests; documents the only accepted format.
const FORMAT_B8G8R8A8_UNORM: u32 = 1;

/// A host-side 2D resource: dimensions, the guest backing SG list, and the host
/// pixel buffer (shared so FLUSH hands a handle to the sink without copying).
struct Resource2D {
    #[allow(dead_code)] // recorded at create; format negotiation is a later milestone.
    format: u32,
    #[allow(dead_code)] // used in later tasks
    width: u32,
    #[allow(dead_code)] // used in later tasks
    height: u32,
    #[allow(dead_code)] // used in later tasks
    backing: Vec<(u64, u32)>,
    #[allow(dead_code)] // used in later tasks
    pixels: Arc<Mutex<Vec<u8>>>,
}

/// virtio-gpu 2D device.
pub struct VirtioGpu {
    #[allow(dead_code)] // used in later tasks (GET_DISPLAY_INFO config)
    width: u32,
    #[allow(dead_code)] // used in later tasks (GET_DISPLAY_INFO config)
    height: u32,
    #[allow(dead_code)] // used in later tasks
    resources: HashMap<u32, Resource2D>,
    #[allow(dead_code)] // used in later tasks
    scanout_res: u32, // resource id bound to scanout 0; 0 = none
    #[allow(dead_code)] // used in later tasks
    sink: Box<dyn DisplaySink>,
}

impl VirtioGpu {
    pub fn new(width: u32, height: u32, sink: Box<dyn DisplaySink>) -> Self {
        VirtioGpu { width, height, resources: HashMap::new(), scanout_res: 0, sink }
    }
}

/// Read a u32/u64 from a little-endian byte slice at `off` (caller bounds-checks).
fn le32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}
#[allow(dead_code)] // used in later tasks
fn le64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
}

/// A bare response header echoing fence/ctx, flags = 0.
fn resp_hdr(resp_type: u32, fence_id: u64, ctx_id: u32) -> Vec<u8> {
    let mut h = vec![0u8; CTRL_HDR_LEN];
    h[0..4].copy_from_slice(&resp_type.to_le_bytes());
    h[8..16].copy_from_slice(&fence_id.to_le_bytes());
    h[16..20].copy_from_slice(&ctx_id.to_le_bytes());
    h
}

/// Concatenate all device-readable descriptors into one request byte vector.
fn read_request(chain: &DescChain, mem: &GuestRam) -> Vec<u8> {
    let mut req = Vec::new();
    for d in &chain.descriptors {
        if !d.writable {
            let mut buf = vec![0u8; d.len as usize];
            if mem.read_slice(d.addr, &mut buf) {
                req.extend_from_slice(&buf);
            }
        }
    }
    req
}

/// Write `resp` across the device-writable descriptors in order; return bytes written.
fn write_response(chain: &DescChain, mem: &GuestRam, resp: &[u8]) -> u32 {
    let mut off = 0usize;
    for d in &chain.descriptors {
        if d.writable && off < resp.len() {
            let n = std::cmp::min(d.len as usize, resp.len() - off);
            if mem.write_slice(d.addr, &resp[off..off + n]) {
                off += n;
            }
        }
    }
    off as u32
}

impl VirtioGpu {
    /// Dispatch one controlq request, returning the response bytes.
    fn dispatch(&mut self, req: &[u8], _mem: &GuestRam) -> Vec<u8> {
        if req.len() < CTRL_HDR_LEN {
            return resp_hdr(RESP_ERR_UNSPEC, 0, 0);
        }
        let cmd = le32(req, 0);
        let fence = le64(req, 8);
        let ctx = le32(req, 16);
        // Command handlers are added in later tasks.
        #[allow(clippy::match_single_binding)] // real arms added in later tasks
        match cmd {
            _ => resp_hdr(RESP_ERR_UNSPEC, fence, ctx),
        }
    }
}

impl VirtioDevice for VirtioGpu {
    fn device_id(&self) -> u32 {
        VIRTIO_ID_GPU
    }

    fn device_features(&self, _sel: u32) -> u32 {
        0
    }

    fn config_read(&self, offset: u64, data: &mut [u8]) {
        // config space (16 bytes): events_read(0), events_clear(4),
        // num_scanouts(8) = 1, num_capsets(12) = 0. Serve arbitrary widths.
        let mut cfg = [0u8; 16];
        cfg[8..12].copy_from_slice(&1u32.to_le_bytes()); // num_scanouts = 1
        for (i, b) in data.iter_mut().enumerate() {
            let idx = (offset as usize).saturating_add(i);
            *b = if idx < cfg.len() { cfg[idx] } else { 0 };
        }
    }

    fn queue_count(&self) -> usize {
        2
    }

    fn handle_notify(&mut self, queue_idx: usize, vq: &mut Virtqueue, mem: &GuestRam) -> bool {
        let mut serviced = false;
        while let Some(chain) = vq.pop_avail(mem) {
            if queue_idx == 1 {
                // cursorq: parse-and-ack; software cursor, image ignored.
                vq.push_used(mem, chain.head, 0);
                serviced = true;
                continue;
            }
            let req = read_request(&chain, mem);
            let resp = self.dispatch(&req, mem);
            let written = write_response(&chain, mem, &resp);
            vq.push_used(mem, chain.head, written);
            serviced = true;
        }
        serviced
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::NoopSink;

    const BASE: u64 = 0x4000_0000;
    const DESC: u64 = BASE + 0x1000;
    const AVAIL: u64 = BASE + 0x2000;
    const USED: u64 = BASE + 0x3000;
    const REQ: u64 = BASE + 0x100;
    const RESP: u64 = BASE + 0x800;

    // VIRTQ_DESC_F_NEXT = 1, VIRTQ_DESC_F_WRITE = 2.
    fn write_desc(m: &GuestRam, i: u64, addr: u64, len: u32, flags: u16, next: u16) {
        let d = DESC + i * 16;
        m.write_slice(d, &addr.to_le_bytes());
        m.write_slice(d + 8, &len.to_le_bytes());
        m.write_slice(d + 12, &flags.to_le_bytes());
        m.write_slice(d + 14, &next.to_le_bytes());
    }

    /// Build a 24-byte ctrl_hdr with the given command type (fence_id = 0xABCD).
    fn hdr(cmd: u32) -> Vec<u8> {
        let mut h = vec![0u8; CTRL_HDR_LEN];
        h[0..4].copy_from_slice(&cmd.to_le_bytes());
        h[8..16].copy_from_slice(&0xABCDu64.to_le_bytes());
        h
    }

    /// Submit `req` on the controlq (queue 0) and return the response bytes the
    /// device wrote (truncated to the used length).
    fn submit(gpu: &mut VirtioGpu, backing: &mut [u8], req: &[u8]) -> Vec<u8> {
        let m = GuestRam::new(backing.as_mut_ptr(), backing.len(), BASE);
        m.write_slice(REQ, req);
        write_desc(&m, 0, REQ, req.len() as u32, 1, 1); // readable, ->1
        write_desc(&m, 1, RESP, 4096, 2, 0);            // writable
        m.write_u16(AVAIL + 2, 1); // avail.idx = 1
        m.write_u16(AVAIL + 4, 0); // ring[0] = desc 0
        let mut vq = Virtqueue::new(8, DESC, AVAIL, USED);
        assert!(gpu.handle_notify(0, &mut vq, &m));
        let used_len = m.read_u32(USED + 8).unwrap();
        let mut out = vec![0u8; used_len as usize];
        m.read_slice(RESP, &mut out);
        out
    }

    fn resp_type(resp: &[u8]) -> u32 {
        u32::from_le_bytes(resp[0..4].try_into().unwrap())
    }

    fn new_gpu() -> VirtioGpu {
        VirtioGpu::new(1280, 800, Box::new(NoopSink))
    }

    #[test]
    fn identity() {
        let gpu = new_gpu();
        assert_eq!(gpu.device_id(), 16);
        assert_eq!(gpu.queue_count(), 2);
        assert_eq!(gpu.device_features(0), 0);
        assert_eq!(gpu.device_features(1), 0);
    }

    #[test]
    fn unknown_command_errs_and_uses_buffer() {
        let mut gpu = new_gpu();
        let mut backing = vec![0u8; 0x4000];
        let resp = submit(&mut gpu, &mut backing, &hdr(0x0999));
        assert_eq!(resp_type(&resp), RESP_ERR_UNSPEC);
    }

    #[test]
    fn short_request_errs() {
        let mut gpu = new_gpu();
        let mut backing = vec![0u8; 0x4000];
        let resp = submit(&mut gpu, &mut backing, &[0u8; 4]);
        assert_eq!(resp_type(&resp), RESP_ERR_UNSPEC);
    }

    #[test]
    fn cursorq_acks_zero_length() {
        let mut gpu = new_gpu();
        let mut backing = vec![0u8; 0x4000];
        let m = GuestRam::new(backing.as_mut_ptr(), backing.len(), BASE);
        m.write_slice(REQ, &hdr(0x0300)); // UPDATE_CURSOR
        write_desc(&m, 0, REQ, CTRL_HDR_LEN as u32, 0, 0);
        m.write_u16(AVAIL + 2, 1);
        m.write_u16(AVAIL + 4, 0);
        let mut vq = Virtqueue::new(8, DESC, AVAIL, USED);
        assert!(gpu.handle_notify(1, &mut vq, &m));
        assert_eq!(m.read_u32(USED + 4), Some(0)); // used elem id = head 0
        assert_eq!(m.read_u32(USED + 8), Some(0)); // used len = 0
    }
}
