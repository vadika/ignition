//! virtio-gpu (VIRTIO_ID_GPU = 16), 2D only. controlq (0) carries display/resource
//! commands; cursorq (1) is parsed and ack'd (software cursor). One scanout, fixed
//! mode, B8G8R8A8. RESOURCE_FLUSH presents the scanned-out resource through a
//! `DisplaySink`. No VIRGL/3D/blob. save/restore snapshots resource-table metadata
//! and scanout binding (pixel contents are reconstructed from guest backing).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::display::{DirtyRect, DisplaySink, Frame};

use super::guest_ram::GuestRam;
use super::mmio::VirtioDevice;
use super::queue::{DescChain, Virtqueue};

const VIRTIO_ID_GPU: u32 = 16;

const GET_DISPLAY_INFO: u32 = 0x0100;
const RESOURCE_CREATE_2D: u32 = 0x0101;
const RESOURCE_UNREF: u32 = 0x0102;
const SET_SCANOUT: u32 = 0x0103;
const RESOURCE_FLUSH: u32 = 0x0104;
const TRANSFER_TO_HOST_2D: u32 = 0x0105;
const RESOURCE_ATTACH_BACKING: u32 = 0x0106;
const RESOURCE_DETACH_BACKING: u32 = 0x0107;

/// ctrl_hdr flag: request carries a fence the device must signal in the response.
const VIRTIO_GPU_FLAG_FENCE: u32 = 1 << 0;

const RESP_OK_NODATA: u32 = 0x1100;
const RESP_OK_DISPLAY_INFO: u32 = 0x1101;
const RESP_ERR_UNSPEC: u32 = 0x1200;

const CTRL_HDR_LEN: usize = 24;
/// config events_read bit: scanout topology / mode changed; guest re-queries
/// GET_DISPLAY_INFO. Matches VIRTIO_GPU_EVENT_DISPLAY.
const VIRTIO_GPU_EVENT_DISPLAY: u32 = 0x0001;
/// Cap on a single 2D resource's host pixel buffer — bounds a guest-driven
/// allocation. 256 MiB dwarfs any real scanout (1280x800x4 = 4 MiB).
const MAX_RESOURCE_BYTES: usize = 256 * 1024 * 1024;
#[allow(dead_code)] // referenced by guests/tests; documents the only accepted format.
const FORMAT_B8G8R8A8_UNORM: u32 = 1;

/// A host-side 2D resource: dimensions, the guest backing SG list, and the host
/// pixel buffer (shared so FLUSH hands a handle to the sink without copying).
struct Resource2D {
    #[allow(dead_code)] // recorded at create; format negotiation is a later milestone.
    format: u32,
    width: u32,
    height: u32,
    backing: Vec<(u64, u32)>,
    pixels: Arc<Mutex<Vec<u8>>>,
}

/// virtio-gpu 2D device.
pub struct VirtioGpu {
    width: u32,
    height: u32,
    resources: HashMap<u32, Resource2D>,
    scanout_res: u32, // resource id bound to scanout 0; 0 = none
    sink: Box<dyn DisplaySink>,
    events_read: u32,
    // Scanout-0 connector status reported in GET_DISPLAY_INFO. Toggled false->true
    // (a connector-cycle) around a mode change so a wlroots kiosk (cage) that only
    // picks its mode at output creation tears down and recreates the output at the
    // new preferred mode. See display_sink.rs about_to_wait.
    enabled: bool,
}

impl VirtioGpu {
    pub fn new(width: u32, height: u32, sink: Box<dyn DisplaySink>) -> Self {
        VirtioGpu {
            width,
            height,
            resources: HashMap::new(),
            scanout_res: 0,
            sink,
            events_read: 0,
            enabled: true,
        }
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

/// Copy `out.len()` bytes starting at logical offset `logical_start` from the
/// scatter-gather backing into `out`. A span may straddle multiple segments.
fn read_backing(sg: &[(u64, u32)], mem: &GuestRam, logical_start: u64, out: &mut [u8]) {
    let mut seg_base = 0u64; // cumulative logical offset at the start of this segment
    let mut out_off = 0usize;
    for &(gpa, len) in sg {
        if out_off >= out.len() {
            break;
        }
        let seg_len = len as u64;
        let seg_end = seg_base.saturating_add(seg_len);
        // `logical_start`/`gpa` come from the guest; use checked math so a malformed
        // request degrades to zeroed pixels rather than a debug-build overflow panic.
        let Some(cur) = logical_start.checked_add(out_off as u64) else { break };
        if cur >= seg_base && cur < seg_end {
            let within = cur - seg_base; // offset into this segment
            let avail = (seg_len - within) as usize;
            let n = std::cmp::min(out.len() - out_off, avail);
            let dst = &mut out[out_off..out_off + n];
            // A bad guest GPA (overflow or out of RAM) must not leave stale pixels.
            match gpa.checked_add(within) {
                Some(src) if mem.read_slice(src, dst) => {}
                _ => dst.fill(0),
            }
            out_off += n;
        }
        seg_base = seg_end;
    }
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
    fn dispatch(&mut self, req: &[u8], mem: &GuestRam) -> Vec<u8> {
        if req.len() < CTRL_HDR_LEN {
            return resp_hdr(RESP_ERR_UNSPEC, 0, 0);
        }
        let cmd = le32(req, 0);
        let flags = le32(req, 4);
        let fence = le64(req, 8);
        let ctx = le32(req, 16);
        let body = &req[CTRL_HDR_LEN..];
        let mut resp = match cmd {
            GET_DISPLAY_INFO => self.display_info(fence, ctx),
            RESOURCE_CREATE_2D => self.create_2d(body, fence, ctx),
            RESOURCE_UNREF => self.unref(body, fence, ctx),
            RESOURCE_ATTACH_BACKING => self.attach_backing(body, fence, ctx),
            RESOURCE_DETACH_BACKING => self.detach_backing(body, fence, ctx),
            SET_SCANOUT => self.set_scanout(body, fence, ctx),
            TRANSFER_TO_HOST_2D => self.transfer_2d(body, mem, fence, ctx),
            RESOURCE_FLUSH => self.flush(body, mem, fence, ctx),
            _ => resp_hdr(RESP_ERR_UNSPEC, fence, ctx),
        };
        // Signal the fence on a fenced request: echo VIRTIO_GPU_FLAG_FENCE + fence_id
        // so the guest can complete the command's fence. wlroots/DRM page-flips are
        // fenced; without this the flip never completes and the compositor renders one
        // frame then stalls. fence_id is already echoed by resp_hdr at bytes 8..16.
        if flags & VIRTIO_GPU_FLAG_FENCE != 0 && resp.len() >= CTRL_HDR_LEN {
            resp[4..8].copy_from_slice(&VIRTIO_GPU_FLAG_FENCE.to_le_bytes());
        }
        resp
    }

    fn transfer_2d(&mut self, body: &[u8], mem: &GuestRam, fence: u64, ctx: u32) -> Vec<u8> {
        if body.len() < 32 {
            return resp_hdr(RESP_ERR_UNSPEC, fence, ctx);
        }
        let rx = le32(body, 0);
        let ry = le32(body, 4);
        let rw = le32(body, 8);
        let rh = le32(body, 12);
        let offset = le64(body, 16);
        let id = le32(body, 24);
        let Some(r) = self.resources.get(&id) else {
            return resp_hdr(RESP_ERR_UNSPEC, fence, ctx);
        };
        // Clamp the rect to the resource so a bad request cannot index out of bounds.
        let x_end = rx.saturating_add(rw).min(r.width);
        let y_end = ry.saturating_add(rh).min(r.height);
        if rx >= r.width || ry >= r.height {
            return resp_hdr(RESP_OK_NODATA, fence, ctx); // nothing in bounds
        }
        let row_w = x_end - rx; // pixels per row to copy
        let mut host = r.pixels.lock().unwrap();
        for y in ry..y_end {
            // row_logical is bounded by the (checked) buffer size; only the
            // guest-supplied `offset` can overflow, so guard that add.
            let row_logical = ((y as u64) * (r.width as u64) + rx as u64) * 4;
            let Some(logical) = offset.checked_add(row_logical) else { continue };
            let host_off = ((y as usize) * (r.width as usize) + rx as usize) * 4;
            let row_bytes = (row_w as usize) * 4;
            read_backing(&r.backing, mem, logical, &mut host[host_off..host_off + row_bytes]);
        }
        resp_hdr(RESP_OK_NODATA, fence, ctx)
    }

    fn create_2d(&mut self, body: &[u8], fence: u64, ctx: u32) -> Vec<u8> {
        if body.len() < 16 {
            return resp_hdr(RESP_ERR_UNSPEC, fence, ctx);
        }
        let id = le32(body, 0);
        let format = le32(body, 4);
        let w = le32(body, 8);
        let h = le32(body, 12);
        // Reject a w*h*4 that overflows usize (malformed guest must not wrap the
        // size to a tiny buffer that later TRANSFER writes would overrun) AND reject
        // a valid-but-absurd size so a guest can't drive a multi-GiB allocation that
        // aborts the VMM. 256 MiB is far above any 1280x800-class scanout buffer.
        let Some(size) = (w as usize)
            .checked_mul(h as usize)
            .and_then(|n| n.checked_mul(4))
            .filter(|&n| n <= MAX_RESOURCE_BYTES)
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

    fn flush(&mut self, body: &[u8], mem: &GuestRam, fence: u64, ctx: u32) -> Vec<u8> {
        if body.len() < 20 {
            return resp_hdr(RESP_ERR_UNSPEC, fence, ctx);
        }
        let rx = le32(body, 0);
        let ry = le32(body, 4);
        let rw = le32(body, 8);
        let rh = le32(body, 12);
        let id = le32(body, 16);
        // Only present the resource currently bound to the scanout (0 = none).
        if id != 0 && id == self.scanout_res
            && let Some(r) = self.resources.get(&id)
        {
            // Re-read the whole scanout from its guest backing before presenting.
            // The guest framebuffer IS the backing, and fbcon scrolls by memmove
            // inside it without re-transferring the moved region; relying only on the
            // small per-FLUSH TRANSFER rects would leave our host copy stale (garbled
            // scrollback). A full read keeps the host buffer == the live framebuffer.
            {
                let mut host = r.pixels.lock().unwrap();
                let len = host.len();
                read_backing(&r.backing, mem, 0, &mut host[..len]);
            }
            let frame = Frame {
                scanout_id: 0,
                width: r.width,
                height: r.height,
                stride: r.width * 4,
                dirty: DirtyRect { x: rx, y: ry, w: rw, h: rh },
                pixels: r.pixels.clone(),
            };
            self.sink.present(frame);
        }
        resp_hdr(RESP_OK_NODATA, fence, ctx)
    }

    fn display_info(&self, fence: u64, ctx: u32) -> Vec<u8> {
        let mut resp = resp_hdr(RESP_OK_DISPLAY_INFO, fence, ctx);
        for i in 0..16u32 {
            let mut one = [0u8; 24]; // rect{x,y,w,h} + enabled + flags
            if i == 0 {
                one[8..12].copy_from_slice(&self.width.to_le_bytes());
                one[12..16].copy_from_slice(&self.height.to_le_bytes());
                one[16..20].copy_from_slice(&(self.enabled as u32).to_le_bytes()); // enabled
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

    fn save(&self) -> serde_json::Value {
        let resources: Vec<serde_json::Value> = self
            .resources
            .iter()
            .map(|(id, r)| {
                let backing: Vec<serde_json::Value> = r
                    .backing
                    .iter()
                    .map(|&(gpa, len)| serde_json::json!({ "gpa": gpa, "len": len }))
                    .collect();
                serde_json::json!({
                    "id": id,
                    "format": r.format,
                    "width": r.width,
                    "height": r.height,
                    "backing": backing,
                })
            })
            .collect();
        serde_json::json!({ "resources": resources, "scanout_res": self.scanout_res })
    }

    fn restore(&mut self, v: &serde_json::Value) -> Result<(), String> {
        let arr = v
            .get("resources")
            .and_then(|x| x.as_array())
            .ok_or("gpu: missing resources")?;
        let mut resources = HashMap::new();
        for e in arr {
            let id = e.get("id").and_then(|x| x.as_u64()).ok_or("gpu: resource missing id")? as u32;
            let format =
                e.get("format").and_then(|x| x.as_u64()).ok_or("gpu: resource missing format")? as u32;
            let width =
                e.get("width").and_then(|x| x.as_u64()).ok_or("gpu: resource missing width")? as u32;
            let height =
                e.get("height").and_then(|x| x.as_u64()).ok_or("gpu: resource missing height")? as u32;
            let size = (width as usize)
                .checked_mul(height as usize)
                .and_then(|n| n.checked_mul(4))
                .filter(|&n| n <= MAX_RESOURCE_BYTES)
                .ok_or("gpu: restored resource size invalid")?;
            let backing_arr =
                e.get("backing").and_then(|x| x.as_array()).ok_or("gpu: resource missing backing")?;
            let mut backing = Vec::with_capacity(backing_arr.len());
            for b in backing_arr {
                let gpa = b.get("gpa").and_then(|x| x.as_u64()).ok_or("gpu: backing missing gpa")?;
                let len =
                    b.get("len").and_then(|x| x.as_u64()).ok_or("gpu: backing missing len")? as u32;
                backing.push((gpa, len));
            }
            resources.insert(id, Resource2D {
                format,
                width,
                height,
                backing,
                pixels: Arc::new(Mutex::new(vec![0u8; size])),
            });
        }
        let scanout_res =
            v.get("scanout_res").and_then(|x| x.as_u64()).ok_or("gpu: missing scanout_res")? as u32;
        if scanout_res != 0 && !resources.contains_key(&scanout_res) {
            return Err("gpu: scanout_res names a missing resource".to_string());
        }
        self.resources = resources;
        self.scanout_res = scanout_res;
        Ok(())
    }

    fn present_scanout(&self, mem: &GuestRam) {
        if self.scanout_res == 0 {
            return;
        }
        let Some(r) = self.resources.get(&self.scanout_res) else { return };
        {
            let mut host = r.pixels.lock().unwrap();
            let len = host.len();
            read_backing(&r.backing, mem, 0, &mut host[..len]);
        }
        let frame = Frame {
            scanout_id: 0,
            width: r.width,
            height: r.height,
            stride: r.width * 4,
            dirty: DirtyRect { x: 0, y: 0, w: r.width, h: r.height },
            pixels: r.pixels.clone(),
        };
        self.sink.present(frame);
    }

    fn device_features(&self, _sel: u32) -> u32 {
        0
    }

    fn config_read(&self, offset: u64, data: &mut [u8]) {
        // config space (16 bytes): events_read(0), events_clear(4),
        // num_scanouts(8) = 1, num_capsets(12) = 0. Serve arbitrary widths.
        let mut cfg = [0u8; 16];
        cfg[0..4].copy_from_slice(&self.events_read.to_le_bytes()); // events_read
        cfg[8..12].copy_from_slice(&1u32.to_le_bytes()); // num_scanouts = 1
        for (i, b) in data.iter_mut().enumerate() {
            let idx = (offset as usize).saturating_add(i);
            *b = if idx < cfg.len() { cfg[idx] } else { 0 };
        }
    }

    fn config_write(&mut self, offset: u64, data: &[u8]) {
        // events_clear (offset 4, u32): the guest acks events by writing the bits
        // to clear. Ignore writes elsewhere (events_read/num_scanouts are RO).
        if offset == 4 && data.len() >= 4 {
            let clear = u32::from_le_bytes(data[0..4].try_into().unwrap());
            self.events_read &= !clear;
        }
    }

    fn set_display_mode(&mut self, w: u32, h: u32) -> bool {
        self.width = w;
        self.height = h;
        self.enabled = true;
        self.events_read |= VIRTIO_GPU_EVENT_DISPLAY;
        true
    }

    fn set_display_enabled(&mut self, en: bool) -> bool {
        self.enabled = en;
        self.events_read |= VIRTIO_GPU_EVENT_DISPLAY;
        true
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
    fn fenced_request_echoes_fence_flag_and_id() {
        let mut gpu = new_gpu();
        let mut backing = vec![0u8; 0x4000];
        // GET_DISPLAY_INFO with VIRTIO_GPU_FLAG_FENCE set + fence_id 0xABCD (from hdr()).
        let mut req = hdr(GET_DISPLAY_INFO);
        req[4..8].copy_from_slice(&1u32.to_le_bytes()); // flags = VIRTIO_GPU_FLAG_FENCE
        let resp = submit(&mut gpu, &mut backing, &req);
        assert_eq!(u32::from_le_bytes(resp[4..8].try_into().unwrap()), 1); // fence flag echoed
        assert_eq!(u64::from_le_bytes(resp[8..16].try_into().unwrap()), 0xABCD); // fence_id
    }

    #[test]
    fn unfenced_request_has_zero_flags() {
        let mut gpu = new_gpu();
        let mut backing = vec![0u8; 0x4000];
        let resp = submit(&mut gpu, &mut backing, &hdr(GET_DISPLAY_INFO));
        assert_eq!(u32::from_le_bytes(resp[4..8].try_into().unwrap()), 0); // no fence flag
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
    fn create_2d_with_absurd_but_valid_size_errs() {
        let mut gpu = new_gpu();
        let mut backing = vec![0u8; 0x4000];
        // 65536*65536*4 = 16 GiB: fits usize (no overflow) but exceeds the cap.
        let resp = submit(&mut gpu, &mut backing, &create_2d_req(1, 0x10000, 0x10000));
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
    fn transfer_with_huge_offset_does_not_panic() {
        let mut gpu = new_gpu();
        let mut backing = vec![0u8; 0x8000];
        submit(&mut gpu, &mut backing, &create_2d_req(1, 4, 1));
        submit(&mut gpu, &mut backing, &attach_backing_req(1, &[(BASE + 0x4000, 16)]));
        let mut t = hdr(TRANSFER_TO_HOST_2D);
        t.extend_from_slice(&0u32.to_le_bytes()); // x
        t.extend_from_slice(&0u32.to_le_bytes()); // y
        t.extend_from_slice(&4u32.to_le_bytes()); // w
        t.extend_from_slice(&1u32.to_le_bytes()); // h
        t.extend_from_slice(&u64::MAX.to_le_bytes()); // offset: overflows if unchecked
        t.extend_from_slice(&1u32.to_le_bytes()); // resource_id
        t.extend_from_slice(&0u32.to_le_bytes()); // padding
        assert_eq!(resp_type(&submit(&mut gpu, &mut backing, &t)), RESP_OK_NODATA);
    }

    #[test]
    fn transfer_reassembles_fragmented_backing() {
        let mut gpu = new_gpu();
        let mut backing = vec![0u8; 0x8000];
        submit(&mut gpu, &mut backing, &create_2d_req(1, 4, 1)); // 4x1 => 16 bytes
        // backing SG: seg A = 10 bytes @ 0x1000, seg B = 6 bytes @ 0x2000 (straddles).
        submit(&mut gpu, &mut backing, &attach_backing_req(1, &[(BASE + 0x4000, 10), (BASE + 0x5000, 6)]));
        let m = GuestRam::new(backing.as_mut_ptr(), backing.len(), BASE);
        let pat: Vec<u8> = (0..16u8).collect();
        m.write_slice(BASE + 0x4000, &pat[0..10]);
        m.write_slice(BASE + 0x5000, &pat[10..16]);
        let mut t = hdr(TRANSFER_TO_HOST_2D);
        t.extend_from_slice(&0u32.to_le_bytes()); // x
        t.extend_from_slice(&0u32.to_le_bytes()); // y
        t.extend_from_slice(&4u32.to_le_bytes()); // w
        t.extend_from_slice(&1u32.to_le_bytes()); // h
        t.extend_from_slice(&0u64.to_le_bytes()); // offset
        t.extend_from_slice(&1u32.to_le_bytes()); // resource_id
        t.extend_from_slice(&0u32.to_le_bytes()); // padding
        assert_eq!(resp_type(&submit(&mut gpu, &mut backing, &t)), RESP_OK_NODATA);
        let host = gpu.resources.get(&1).unwrap().pixels.lock().unwrap();
        assert_eq!(&host[..], &pat[..], "fragmented backing must reassemble in order");
    }

    #[derive(Clone)]
    struct CapSink(Arc<Mutex<Vec<Frame>>>);
    impl DisplaySink for CapSink {
        fn present(&self, frame: Frame) {
            self.0.lock().unwrap().push(frame);
        }
    }

    fn flush_req(id: u32) -> Vec<u8> {
        let mut r = hdr(RESOURCE_FLUSH);
        r.extend_from_slice(&[0u8; 16]); // rect {0,0,0,0}
        r.extend_from_slice(&id.to_le_bytes());
        r.extend_from_slice(&0u32.to_le_bytes()); // padding
        r
    }

    #[test]
    fn flush_of_scanned_out_resource_presents_one_frame() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let mut gpu = VirtioGpu::new(1280, 800, Box::new(CapSink(captured.clone())));
        let mut backing = vec![0u8; 0x4000];
        submit(&mut gpu, &mut backing, &create_2d_req(1, 8, 4));
        submit(&mut gpu, &mut backing, &set_scanout_req(0, 1));
        assert_eq!(resp_type(&submit(&mut gpu, &mut backing, &flush_req(1))), RESP_OK_NODATA);
        let frames = captured.lock().unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].scanout_id, 0);
        assert_eq!((frames[0].width, frames[0].height), (8, 4));
        assert_eq!(frames[0].stride, 8 * 4);
        assert_eq!(frames[0].pixels.lock().unwrap().len(), 8 * 4 * 4);
    }

    #[test]
    fn flush_of_non_scanned_out_resource_presents_nothing() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let mut gpu = VirtioGpu::new(1280, 800, Box::new(CapSink(captured.clone())));
        let mut backing = vec![0u8; 0x4000];
        submit(&mut gpu, &mut backing, &create_2d_req(1, 8, 4));
        submit(&mut gpu, &mut backing, &create_2d_req(2, 8, 4));
        submit(&mut gpu, &mut backing, &set_scanout_req(0, 1));
        submit(&mut gpu, &mut backing, &flush_req(2));
        assert!(captured.lock().unwrap().is_empty());
    }

    #[test]
    fn save_restore_roundtrips_resource_table_and_scanout() {
        let mut gpu = new_gpu();
        let mut backing = vec![0u8; 0x4000];
        submit(&mut gpu, &mut backing, &create_2d_req(7, 8, 4));
        submit(&mut gpu, &mut backing, &attach_backing_req(7, &[(0x1000, 64), (0x2000, 64)]));
        submit(&mut gpu, &mut backing, &set_scanout_req(0, 7));
        let saved = gpu.save();

        let mut gpu2 = new_gpu();
        gpu2.restore(&saved).expect("restore ok");
        assert_eq!(gpu2.scanout_res, 7);
        let r = gpu2.resources.get(&7).expect("resource 7 restored");
        assert_eq!((r.format, r.width, r.height), (FORMAT_B8G8R8A8_UNORM, 8, 4));
        assert_eq!(r.backing, vec![(0x1000, 64), (0x2000, 64)]);
        assert_eq!(r.pixels.lock().unwrap().len(), 8 * 4 * 4); // rebuilt zeroed
    }

    #[test]
    fn restore_rejects_dangling_scanout() {
        let mut gpu = new_gpu();
        // scanout_res names a resource that is not in the table → invalid snapshot.
        let bad = serde_json::json!({
            "resources": [{ "id": 1, "format": 1, "width": 8, "height": 4, "backing": [] }],
            "scanout_res": 999
        });
        assert!(gpu.restore(&bad).is_err());
    }

    #[test]
    fn restore_rejects_absurd_resource_size() {
        let mut gpu = new_gpu();
        let bad = serde_json::json!({
            "resources": [{ "id": 1, "format": 1, "width": 0x10000, "height": 0x10000, "backing": [] }],
            "scanout_res": 0
        });
        assert!(gpu.restore(&bad).is_err());
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

    #[test]
    fn present_scanout_reads_backing_and_presents() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let mut gpu = VirtioGpu::new(1280, 800, Box::new(CapSink(captured.clone())));
        let mut backing = vec![0u8; 0x8000];
        submit(&mut gpu, &mut backing, &create_2d_req(1, 4, 1)); // 4x1 = 16 bytes
        submit(&mut gpu, &mut backing, &attach_backing_req(1, &[(BASE + 0x4000, 16)]));
        submit(&mut gpu, &mut backing, &set_scanout_req(0, 1));
        let m = GuestRam::new(backing.as_mut_ptr(), backing.len(), BASE);
        let pat: Vec<u8> = (0..16u8).collect();
        m.write_slice(BASE + 0x4000, &pat);

        gpu.present_scanout(&m);

        let frames = captured.lock().unwrap();
        assert_eq!(frames.len(), 1, "one frame presented");
        assert_eq!((frames[0].width, frames[0].height), (4, 1));
        assert_eq!(&frames[0].pixels.lock().unwrap()[..], &pat[..], "scanout re-read from backing");
    }

    #[test]
    fn set_display_mode_updates_dims_and_raises_event() {
        let mut gpu = new_gpu();
        assert!(gpu.set_display_mode(1024, 768));

        // events_read (config offset 0) carries EVENT_DISPLAY.
        let mut cfg = [0u8; 4];
        gpu.config_read(0, &mut cfg);
        assert_eq!(u32::from_le_bytes(cfg), VIRTIO_GPU_EVENT_DISPLAY);

        // GET_DISPLAY_INFO now reports the new scanout-0 dimensions.
        let mut backing = vec![0u8; 0x4000];
        let resp = submit(&mut gpu, &mut backing, &hdr(GET_DISPLAY_INFO));
        let w = u32::from_le_bytes(resp[32..36].try_into().unwrap()); // hdr(24)+rect.x,y(8) -> w
        let h = u32::from_le_bytes(resp[36..40].try_into().unwrap());
        assert_eq!((w, h), (1024, 768));
    }

    #[test]
    fn connector_cycle_toggles_enabled_in_display_info() {
        let mut gpu = new_gpu();
        let enabled = |g: &mut VirtioGpu| -> u32 {
            let mut backing = vec![0u8; 0x4000];
            let resp = submit(g, &mut backing, &hdr(GET_DISPLAY_INFO));
            u32::from_le_bytes(resp[40..44].try_into().unwrap()) // hdr(24)+rect(16) -> enabled
        };
        assert_eq!(enabled(&mut gpu), 1, "starts connected");

        // Phase 1: disconnect raises the event and reports enabled=0.
        assert!(gpu.set_display_enabled(false));
        let mut cfg = [0u8; 4];
        gpu.config_read(0, &mut cfg);
        assert_eq!(u32::from_le_bytes(cfg), VIRTIO_GPU_EVENT_DISPLAY);
        assert_eq!(enabled(&mut gpu), 0, "disconnected");

        // Phase 2: a mode change reconnects (enabled back to 1) at the new dims.
        assert!(gpu.set_display_mode(1024, 768));
        assert_eq!(enabled(&mut gpu), 1, "reconnected");
    }

    #[test]
    fn config_write_events_clear_clears_event() {
        let mut gpu = new_gpu();
        gpu.set_display_mode(800, 600);
        // Guest acks by writing the bit to events_clear (config offset 4).
        gpu.config_write(4, &VIRTIO_GPU_EVENT_DISPLAY.to_le_bytes());
        let mut cfg = [0u8; 4];
        gpu.config_read(0, &mut cfg);
        assert_eq!(u32::from_le_bytes(cfg), 0);
    }

    #[test]
    fn present_scanout_with_no_scanout_presents_nothing() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let gpu = VirtioGpu::new(1280, 800, Box::new(CapSink(captured.clone())));
        let mut backing = vec![0u8; 0x1000];
        let m = GuestRam::new(backing.as_mut_ptr(), backing.len(), BASE);
        gpu.present_scanout(&m); // scanout_res == 0
        assert!(captured.lock().unwrap().is_empty());
    }
}
