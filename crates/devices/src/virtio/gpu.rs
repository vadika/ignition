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

const GET_DISPLAY_INFO: u32 = 0x0100;
const RESOURCE_CREATE_2D: u32 = 0x0101;
const RESOURCE_UNREF: u32 = 0x0102;
const SET_SCANOUT: u32 = 0x0103;
#[allow(dead_code)] // used in later tasks
const RESOURCE_FLUSH: u32 = 0x0104;
#[allow(dead_code)] // used in later tasks
const TRANSFER_TO_HOST_2D: u32 = 0x0105;
const RESOURCE_ATTACH_BACKING: u32 = 0x0106;
const RESOURCE_DETACH_BACKING: u32 = 0x0107;

const RESP_OK_NODATA: u32 = 0x1100;
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
    #[allow(dead_code)] // read in tests; used by TRANSFER_TO_HOST_2D in later tasks
    width: u32,
    #[allow(dead_code)] // read in tests; used by TRANSFER_TO_HOST_2D in later tasks
    height: u32,
    backing: Vec<(u64, u32)>,
    #[allow(dead_code)] // used in later tasks
    pixels: Arc<Mutex<Vec<u8>>>,
}

/// virtio-gpu 2D device.
pub struct VirtioGpu {
    width: u32,
    height: u32,
    resources: HashMap<u32, Resource2D>,
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
        let body = &req[CTRL_HDR_LEN..];
        match cmd {
            GET_DISPLAY_INFO => self.display_info(fence, ctx),
            RESOURCE_CREATE_2D => self.create_2d(body, fence, ctx),
            RESOURCE_UNREF => self.unref(body, fence, ctx),
            RESOURCE_ATTACH_BACKING => self.attach_backing(body, fence, ctx),
            RESOURCE_DETACH_BACKING => self.detach_backing(body, fence, ctx),
            SET_SCANOUT => self.set_scanout(body, fence, ctx),
            _ => resp_hdr(RESP_ERR_UNSPEC, fence, ctx),
        }
    }

    fn create_2d(&mut self, body: &[u8], fence: u64, ctx: u32) -> Vec<u8> {
        if body.len() < 16 {
            return resp_hdr(RESP_ERR_UNSPEC, fence, ctx);
        }
        let id = le32(body, 0);
        let format = le32(body, 4);
        let w = le32(body, 8);
        let h = le32(body, 12);
        // Reject a w*h*4 that overflows usize: a malformed guest must not wrap the
        // size to a tiny buffer that later TRANSFER writes would overrun.
        let Some(size) = (w as usize)
            .checked_mul(h as usize)
            .and_then(|n| n.checked_mul(4))
        else {
            return resp_hdr(RESP_ERR_UNSPEC, fence, ctx);
        };
        self.resources.insert(id, Resource2D {
            format,
            width: w,
            height: h,
            backing: Vec::new(),
            pixels: Arc::new(Mutex::new(vec![0u8; size])),
        });
        resp_hdr(RESP_OK_NODATA, fence, ctx)
    }

    fn unref(&mut self, body: &[u8], fence: u64, ctx: u32) -> Vec<u8> {
        if body.len() < 4 {
            return resp_hdr(RESP_ERR_UNSPEC, fence, ctx);
        }
        let id = le32(body, 0);
        self.resources.remove(&id);
        if self.scanout_res == id {
            self.scanout_res = 0;
        }
        resp_hdr(RESP_OK_NODATA, fence, ctx)
    }

    fn attach_backing(&mut self, body: &[u8], fence: u64, ctx: u32) -> Vec<u8> {
        if body.len() < 8 {
            return resp_hdr(RESP_ERR_UNSPEC, fence, ctx);
        }
        let id = le32(body, 0);
        // Each mem_entry is 16 bytes ({addr:u64, len:u32, pad:u32}). Cap the count
        // to what the body can actually hold so a bogus nr_entries can't drive a
        // huge `with_capacity` reservation (OOM-abort) before the loop guards it.
        let nr = (le32(body, 4) as usize).min(body.len().saturating_sub(8) / 16);
        let mut sg = Vec::with_capacity(nr);
        for i in 0..nr {
            let off = 8 + i * 16;
            if off + 16 > body.len() {
                break;
            }
            sg.push((le64(body, off), le32(body, off + 8)));
        }
        match self.resources.get_mut(&id) {
            Some(r) => {
                r.backing = sg;
                resp_hdr(RESP_OK_NODATA, fence, ctx)
            }
            None => resp_hdr(RESP_ERR_UNSPEC, fence, ctx),
        }
    }

    fn detach_backing(&mut self, body: &[u8], fence: u64, ctx: u32) -> Vec<u8> {
        if body.len() < 4 {
            return resp_hdr(RESP_ERR_UNSPEC, fence, ctx);
        }
        let id = le32(body, 0);
        if let Some(r) = self.resources.get_mut(&id) {
            r.backing.clear();
        }
        resp_hdr(RESP_OK_NODATA, fence, ctx)
    }

    fn set_scanout(&mut self, body: &[u8], fence: u64, ctx: u32) -> Vec<u8> {
        if body.len() < 24 {
            return resp_hdr(RESP_ERR_UNSPEC, fence, ctx);
        }
        // body: rect(16) + scanout_id(4) + resource_id(4). Only scanout 0 exists.
        let resource_id = le32(body, 20);
        // Binding to a nonexistent resource is an error (virtio 1.2 §5.7.6.8);
        // resource_id 0 disables the scanout and is always allowed.
        if resource_id != 0 && !self.resources.contains_key(&resource_id) {
            return resp_hdr(RESP_ERR_UNSPEC, fence, ctx);
        }
        self.scanout_res = resource_id;
        resp_hdr(RESP_OK_NODATA, fence, ctx)
    }

    fn display_info(&self, fence: u64, ctx: u32) -> Vec<u8> {
        let mut resp = resp_hdr(RESP_OK_DISPLAY_INFO, fence, ctx);
        for i in 0..16u32 {
            let mut one = [0u8; 24]; // rect{x,y,w,h} + enabled + flags
            if i == 0 {
                one[8..12].copy_from_slice(&self.width.to_le_bytes());
                one[12..16].copy_from_slice(&self.height.to_le_bytes());
                one[16..20].copy_from_slice(&1u32.to_le_bytes()); // enabled
            }
            resp.extend_from_slice(&one);
        }
        resp
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
    fn get_display_info_reports_one_enabled_scanout() {
        let mut gpu = new_gpu();
        let mut backing = vec![0u8; 0x4000];
        let resp = submit(&mut gpu, &mut backing, &hdr(GET_DISPLAY_INFO));
        assert_eq!(resp_type(&resp), RESP_OK_DISPLAY_INFO);
        assert_eq!(resp.len(), CTRL_HDR_LEN + 16 * 24);
        let e0 = CTRL_HDR_LEN;
        assert_eq!(u32::from_le_bytes(resp[e0 + 8..e0 + 12].try_into().unwrap()), 1280);
        assert_eq!(u32::from_le_bytes(resp[e0 + 12..e0 + 16].try_into().unwrap()), 800);
        assert_eq!(u32::from_le_bytes(resp[e0 + 16..e0 + 20].try_into().unwrap()), 1);
        let e1 = CTRL_HDR_LEN + 24;
        assert_eq!(u32::from_le_bytes(resp[e1 + 16..e1 + 20].try_into().unwrap()), 0);
    }

    fn create_2d_req(id: u32, w: u32, h: u32) -> Vec<u8> {
        let mut r = hdr(RESOURCE_CREATE_2D);
        r.extend_from_slice(&id.to_le_bytes());
        r.extend_from_slice(&FORMAT_B8G8R8A8_UNORM.to_le_bytes());
        r.extend_from_slice(&w.to_le_bytes());
        r.extend_from_slice(&h.to_le_bytes());
        r
    }
    fn attach_backing_req(id: u32, entries: &[(u64, u32)]) -> Vec<u8> {
        let mut r = hdr(RESOURCE_ATTACH_BACKING);
        r.extend_from_slice(&id.to_le_bytes());
        r.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        for &(addr, len) in entries {
            r.extend_from_slice(&addr.to_le_bytes());
            r.extend_from_slice(&len.to_le_bytes());
            r.extend_from_slice(&0u32.to_le_bytes());
        }
        r
    }
    fn set_scanout_req(scanout_id: u32, resource_id: u32) -> Vec<u8> {
        let mut r = hdr(SET_SCANOUT);
        r.extend_from_slice(&[0u8; 16]);
        r.extend_from_slice(&scanout_id.to_le_bytes());
        r.extend_from_slice(&resource_id.to_le_bytes());
        r
    }

    #[test]
    fn create_and_attach_backing() {
        let mut gpu = new_gpu();
        let mut backing = vec![0u8; 0x4000];
        assert_eq!(resp_type(&submit(&mut gpu, &mut backing, &create_2d_req(1, 8, 4))), RESP_OK_NODATA);
        assert_eq!(resp_type(&submit(&mut gpu, &mut backing,
            &attach_backing_req(1, &[(0x1000, 64), (0x2000, 64)]))), RESP_OK_NODATA);
        let r = gpu.resources.get(&1).expect("resource 1 exists");
        assert_eq!((r.width, r.height), (8, 4));
        assert_eq!(r.pixels.lock().unwrap().len(), 8 * 4 * 4);
        assert_eq!(r.backing, vec![(0x1000, 64), (0x2000, 64)]);
    }

    #[test]
    fn create_2d_with_overflowing_dims_errs() {
        let mut gpu = new_gpu();
        let mut backing = vec![0u8; 0x4000];
        // w*h*4 overflows usize → must be rejected, not wrapped to a tiny buffer.
        let resp = submit(&mut gpu, &mut backing, &create_2d_req(1, 0x8000_0000, 0x8000_0000));
        assert_eq!(resp_type(&resp), RESP_ERR_UNSPEC);
        assert!(!gpu.resources.contains_key(&1));
    }

    #[test]
    fn set_scanout_to_missing_resource_errs() {
        let mut gpu = new_gpu();
        let mut backing = vec![0u8; 0x4000];
        let resp = submit(&mut gpu, &mut backing, &set_scanout_req(0, 42));
        assert_eq!(resp_type(&resp), RESP_ERR_UNSPEC);
        assert_eq!(gpu.scanout_res, 0);
    }

    #[test]
    fn set_scanout_binds_and_unbinds() {
        let mut gpu = new_gpu();
        let mut backing = vec![0u8; 0x4000];
        submit(&mut gpu, &mut backing, &create_2d_req(1, 8, 4));
        submit(&mut gpu, &mut backing, &set_scanout_req(0, 1));
        assert_eq!(gpu.scanout_res, 1);
        submit(&mut gpu, &mut backing, &set_scanout_req(0, 0));
        assert_eq!(gpu.scanout_res, 0);
    }

    #[test]
    fn unref_removes_resource_and_clears_scanout() {
        let mut gpu = new_gpu();
        let mut backing = vec![0u8; 0x4000];
        submit(&mut gpu, &mut backing, &create_2d_req(1, 8, 4));
        submit(&mut gpu, &mut backing, &set_scanout_req(0, 1));
        let mut unref = hdr(RESOURCE_UNREF);
        unref.extend_from_slice(&1u32.to_le_bytes());
        unref.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(resp_type(&submit(&mut gpu, &mut backing, &unref)), RESP_OK_NODATA);
        assert!(!gpu.resources.contains_key(&1));
        assert_eq!(gpu.scanout_res, 0);
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
