# virtio-gpu 2D (M1) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a virtio-gpu 2D device so a `--gui` guest binds the Linux `virtio_gpu` driver and its framebuffer console renders live in the macOS window via the existing `DisplaySink` seam.

**Architecture:** A `VirtioGpu` device (`crates/devices/src/virtio/gpu.rs`) implements `VirtioDevice` like `rng.rs`: `handle_notify` pops chains, concatenates device-readable descriptors into a request, dispatches the virtio-gpu controlq command, writes a response into device-writable descriptors. A resource table holds per-resource host pixel buffers (`Arc<Mutex<Vec<u8>>>`); `TRANSFER_TO_HOST_2D` does an SG-correct guest→host copy; `RESOURCE_FLUSH` builds a `Frame` and calls the device's `Box<dyn DisplaySink>`. `--gui` gates the device and threads the `WindowSink` into it; the event loop's `redraw` blits the frame.

**Tech Stack:** Rust (edition 2024), `ignition-devices` (`VirtioDevice`, `Virtqueue`, `GuestRam`, `DisplaySink`/`Frame`/`DirtyRect`), `winit`/`softbuffer` in the spike binary.

---

## File Structure

- `crates/devices/src/virtio/gpu.rs` — **create.** `VirtioGpu`, `Resource2D`, command constants, request/response plumbing, all controlq handlers, the SG transfer, FLUSH→present, unit tests (incl. `CapSink`).
- `crates/devices/src/virtio/mod.rs` — **modify.** Add `pub mod gpu;`.
- `spike/src/bin/boot.rs` — **modify.** Create `WindowSink` before `setup_devices`; add `DeviceContext.display_sink`; register the virtio-gpu device (Boot + `--gui` only); keep `rx` for `run_event_loop`.
- `spike/src/bin/display_sink.rs` — **modify.** `App::redraw` blits a real `Frame` (B8G8R8A8→0RGB, dirty-rect aware); keep the clear path for no-frame.
- `docs/src/features/devices.md`, `docs/src/getting-started/guest-assets.md`, `ROADMAP.md` — **modify.** docs.

**Shared command/format constants and the wire layout** (used across Tasks 1–5; the engineer writes them once in Task 1):

```rust
const VIRTIO_ID_GPU: u32 = 16;

// controlq command types
const GET_DISPLAY_INFO: u32 = 0x0100;
const RESOURCE_CREATE_2D: u32 = 0x0101;
const RESOURCE_UNREF: u32 = 0x0102;
const SET_SCANOUT: u32 = 0x0103;
const RESOURCE_FLUSH: u32 = 0x0104;
const TRANSFER_TO_HOST_2D: u32 = 0x0105;
const RESOURCE_ATTACH_BACKING: u32 = 0x0106;
const RESOURCE_DETACH_BACKING: u32 = 0x0107;

// response types
const RESP_OK_NODATA: u32 = 0x1100;
const RESP_OK_DISPLAY_INFO: u32 = 0x1101;
const RESP_ERR_UNSPEC: u32 = 0x1200;

const CTRL_HDR_LEN: usize = 24; // type:u32 flags:u32 fence_id:u64 ctx_id:u32 padding:u32
const FORMAT_B8G8R8A8_UNORM: u32 = 1;
```

All multi-byte fields are little-endian. The controlq request is: a 24-byte `ctrl_hdr` then a command-specific body. The response is at minimum a 24-byte `ctrl_hdr` echoing `fence_id`/`ctx_id` with `flags = 0`.

---

## Task 1: Device skeleton, request/response plumbing, dispatch scaffold, cursorq

**Files:**
- Create: `crates/devices/src/virtio/gpu.rs`
- Modify: `crates/devices/src/virtio/mod.rs` (add `pub mod gpu;` next to the other `pub mod` device lines)

- [ ] **Step 1: Write the failing tests.** Create `crates/devices/src/virtio/gpu.rs` with the test module (the test helpers are reused by later tasks):

```rust
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
    /// device wrote (truncated to the used length). One readable desc (req) chained
    /// to one writable desc (4 KiB response buffer).
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
        let resp = submit(&mut gpu, &mut backing, &[0u8; 4]); // shorter than ctrl_hdr
        assert_eq!(resp_type(&resp), RESP_ERR_UNSPEC);
    }

    #[test]
    fn cursorq_acks_zero_length() {
        let mut gpu = new_gpu();
        let mut backing = vec![0u8; 0x4000];
        let m = GuestRam::new(backing.as_mut_ptr(), backing.len(), BASE);
        // one readable cursor command desc; no writable desc needed.
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
```

- [ ] **Step 2: Run tests to verify they fail to compile.** Run: `PATH="$HOME/.cargo/bin:$PATH" cargo test -p ignition-devices gpu 2>&1 | tail -20`. Expect: `cannot find type VirtioGpu`.

- [ ] **Step 3: Implement the skeleton.** Prepend to `crates/devices/src/virtio/gpu.rs` (above the test module), and add `pub mod gpu;` to `crates/devices/src/virtio/mod.rs`:

```rust
//! virtio-gpu (VIRTIO_ID_GPU = 16), 2D only. controlq (0) carries display/resource
//! commands; cursorq (1) is parsed and ack'd (software cursor). One scanout, fixed
//! mode, B8G8R8A8. RESOURCE_FLUSH presents the scanned-out resource through a
//! `DisplaySink`. No VIRGL/3D/blob; no snapshot of GPU state (that is M5).

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
        match cmd {
            // Command handlers are added in later tasks.
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
            let idx = offset as usize + i;
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
```

- [ ] **Step 4: Run tests to verify they pass.** Run: `PATH="$HOME/.cargo/bin:$PATH" cargo test -p ignition-devices gpu 2>&1 | tail -20`. Expect: `4 passed` (identity, unknown_command, short_request, cursorq).

- [ ] **Step 5: Clippy.** Run: `PATH="$HOME/.cargo/bin:$PATH" cargo clippy -p ignition-devices --all-targets 2>&1 | tail -10`. Fix any new warnings (the `_ => ...` single-arm match will warn `clippy::match_single_binding` — if so, temporarily write it as `if cmd == 0 { unreachable!() } else { resp_hdr(RESP_ERR_UNSPEC, fence, ctx) }` is wrong; instead suppress by leaving the match and adding `#[allow(clippy::match_single_binding)]` above the `match cmd` with a comment "arms added in later tasks". Later tasks remove the allow once real arms exist.)

- [ ] **Step 6: Commit.**

```bash
git add crates/devices/src/virtio/gpu.rs crates/devices/src/virtio/mod.rs
git commit -m "feat(devices): virtio-gpu skeleton (controlq plumbing + cursorq ack)"
```

---

## Task 2: GET_DISPLAY_INFO

**Files:** Modify `crates/devices/src/virtio/gpu.rs`.

- [ ] **Step 1: Write the failing test** (add into the `tests` module):

```rust
    #[test]
    fn get_display_info_reports_one_enabled_scanout() {
        let mut gpu = new_gpu();
        let mut backing = vec![0u8; 0x4000];
        let resp = submit(&mut gpu, &mut backing, &hdr(GET_DISPLAY_INFO));
        assert_eq!(resp_type(&resp), RESP_OK_DISPLAY_INFO);
        // ctrl_hdr (24) + 16 * virtio_gpu_display_one (24) = 408 bytes.
        assert_eq!(resp.len(), CTRL_HDR_LEN + 16 * 24);
        // entry 0: rect.width @ +8, rect.height @ +12, enabled @ +16.
        let e0 = CTRL_HDR_LEN;
        assert_eq!(u32::from_le_bytes(resp[e0 + 8..e0 + 12].try_into().unwrap()), 1280);
        assert_eq!(u32::from_le_bytes(resp[e0 + 12..e0 + 16].try_into().unwrap()), 800);
        assert_eq!(u32::from_le_bytes(resp[e0 + 16..e0 + 20].try_into().unwrap()), 1);
        // entry 1 disabled.
        let e1 = CTRL_HDR_LEN + 24;
        assert_eq!(u32::from_le_bytes(resp[e1 + 16..e1 + 20].try_into().unwrap()), 0);
    }
```

- [ ] **Step 2: Run to verify it fails.** Run: `PATH="$HOME/.cargo/bin:$PATH" cargo test -p ignition-devices gpu::tests::get_display_info 2>&1 | tail -10`. Expect FAIL (response is `ERR_UNSPEC`, wrong length).

- [ ] **Step 3: Implement.** Add the handler method inside `impl VirtioGpu` (next to `dispatch`):

```rust
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
```

And add the dispatch arm (replace the lone `_ =>` arm; remove the `#[allow(clippy::match_single_binding)]` from Task 1 now that there is a real arm):

```rust
        match cmd {
            GET_DISPLAY_INFO => self.display_info(fence, ctx),
            _ => resp_hdr(RESP_ERR_UNSPEC, fence, ctx),
        }
```

- [ ] **Step 4: Run to verify it passes.** Run: `PATH="$HOME/.cargo/bin:$PATH" cargo test -p ignition-devices gpu 2>&1 | tail -10`. Expect all gpu tests pass.

- [ ] **Step 5: Commit.**

```bash
git add crates/devices/src/virtio/gpu.rs
git commit -m "feat(devices): virtio-gpu GET_DISPLAY_INFO (one fixed scanout)"
```

---

## Task 3: Resource lifecycle — CREATE_2D, UNREF, ATTACH/DETACH_BACKING, SET_SCANOUT

**Files:** Modify `crates/devices/src/virtio/gpu.rs`.

- [ ] **Step 1: Write the failing tests** (add to the `tests` module; these helpers build command bodies):

```rust
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
            r.extend_from_slice(&0u32.to_le_bytes()); // padding
        }
        r
    }

    fn set_scanout_req(scanout_id: u32, resource_id: u32) -> Vec<u8> {
        let mut r = hdr(SET_SCANOUT);
        r.extend_from_slice(&[0u8; 16]); // rect
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
        assert!(gpu.resources.get(&1).is_none());
        assert_eq!(gpu.scanout_res, 0);
    }
```

- [ ] **Step 2: Run to verify they fail.** Run: `PATH="$HOME/.cargo/bin:$PATH" cargo test -p ignition-devices gpu 2>&1 | tail -15`. Expect the three new tests FAIL (`ERR_UNSPEC`, resource absent).

- [ ] **Step 3: Implement.** Add handler methods inside `impl VirtioGpu`:

```rust
    fn create_2d(&mut self, body: &[u8], fence: u64, ctx: u32) -> Vec<u8> {
        if body.len() < 16 {
            return resp_hdr(RESP_ERR_UNSPEC, fence, ctx);
        }
        let id = le32(body, 0);
        let format = le32(body, 4);
        let w = le32(body, 8);
        let h = le32(body, 12);
        let size = (w as usize) * (h as usize) * 4;
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
        let nr = le32(body, 4) as usize;
        let mut sg = Vec::with_capacity(nr);
        for i in 0..nr {
            let off = 8 + i * 16;
            if off + 12 > body.len() {
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
        self.scanout_res = resource_id;
        resp_hdr(RESP_OK_NODATA, fence, ctx)
    }
```

Extend the dispatch match (`body` is the bytes after the header):

```rust
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
```

- [ ] **Step 4: Run to verify they pass.** Run: `PATH="$HOME/.cargo/bin:$PATH" cargo test -p ignition-devices gpu 2>&1 | tail -10`. Expect all gpu tests pass.

- [ ] **Step 5: Commit.**

```bash
git add crates/devices/src/virtio/gpu.rs
git commit -m "feat(devices): virtio-gpu resource lifecycle + set_scanout"
```

---

## Task 4: TRANSFER_TO_HOST_2D (SG-correct guest→host copy)

**Files:** Modify `crates/devices/src/virtio/gpu.rs`.

- [ ] **Step 1: Write the failing test** (the SG-straddle test — a 4×1 B8G8R8A8 row, i.e. 16 bytes, whose backing is split 10 + 6 across two segments):

```rust
    #[test]
    fn transfer_reassembles_fragmented_backing() {
        let mut gpu = new_gpu();
        let mut backing = vec![0u8; 0x8000];
        // resource: 4x1, 16 bytes of pixels.
        submit(&mut gpu, &mut backing, &create_2d_req(1, 4, 1));
        // backing SG: seg A = 10 bytes @ 0x1000, seg B = 6 bytes @ 0x2000 (straddles).
        submit(&mut gpu, &mut backing, &attach_backing_req(1, &[(0x1000, 10), (0x2000, 6)]));
        // lay a known 16-byte pattern across the two guest segments.
        let m = GuestRam::new(backing.as_mut_ptr(), backing.len(), BASE);
        let pat: Vec<u8> = (0..16u8).collect();
        m.write_slice(0x1000, &pat[0..10]);
        m.write_slice(0x2000, &pat[10..16]);
        // TRANSFER_TO_HOST_2D rect {0,0,4,1} offset 0 resource 1.
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
```

- [ ] **Step 2: Run to verify it fails.** Run: `PATH="$HOME/.cargo/bin:$PATH" cargo test -p ignition-devices gpu::tests::transfer 2>&1 | tail -10`. Expect FAIL (`ERR_UNSPEC` / host buffer still zero).

- [ ] **Step 3: Implement.** Add the SG reader (free function) and the handler:

```rust
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
        let seg_end = seg_base + seg_len;
        let cur = logical_start + out_off as u64; // next logical byte we still need
        if cur >= seg_base && cur < seg_end {
            let within = cur - seg_base; // offset into this segment
            let avail = (seg_len - within) as usize;
            let n = std::cmp::min(out.len() - out_off, avail);
            mem.read_slice(gpa + within, &mut out[out_off..out_off + n]);
            out_off += n;
        }
        seg_base = seg_end;
    }
}
```

```rust
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
            let logical = offset + ((y as u64) * (r.width as u64) + rx as u64) * 4;
            let host_off = ((y as usize) * (r.width as usize) + rx as usize) * 4;
            let row_bytes = (row_w as usize) * 4;
            read_backing(&r.backing, mem, logical, &mut host[host_off..host_off + row_bytes]);
        }
        resp_hdr(RESP_OK_NODATA, fence, ctx)
    }
```

Add the dispatch arm (this command needs `mem`, which `dispatch` already receives as `_mem`; rename `_mem` to `mem` in the `dispatch` signature now that it is used):

```rust
            TRANSFER_TO_HOST_2D => self.transfer_2d(body, mem, fence, ctx),
```

- [ ] **Step 4: Run to verify it passes.** Run: `PATH="$HOME/.cargo/bin:$PATH" cargo test -p ignition-devices gpu 2>&1 | tail -10`. Expect all gpu tests pass.

- [ ] **Step 5: Commit.**

```bash
git add crates/devices/src/virtio/gpu.rs
git commit -m "feat(devices): virtio-gpu TRANSFER_TO_HOST_2D (SG-correct copy)"
```

---

## Task 5: RESOURCE_FLUSH → DisplaySink::present

**Files:** Modify `crates/devices/src/virtio/gpu.rs`.

- [ ] **Step 1: Write the failing tests** (with a capturing sink that records presented frames):

```rust
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
        submit(&mut gpu, &mut backing, &flush_req(2)); // not the scanned-out one
        assert!(captured.lock().unwrap().is_empty());
    }
```

- [ ] **Step 2: Run to verify they fail.** Run: `PATH="$HOME/.cargo/bin:$PATH" cargo test -p ignition-devices gpu::tests::flush 2>&1 | tail -10`. Expect FAIL (no frame captured).

- [ ] **Step 3: Implement.** Add the handler:

```rust
    fn flush(&mut self, body: &[u8], fence: u64, ctx: u32) -> Vec<u8> {
        if body.len() < 20 {
            return resp_hdr(RESP_ERR_UNSPEC, fence, ctx);
        }
        let rx = le32(body, 0);
        let ry = le32(body, 4);
        let rw = le32(body, 8);
        let rh = le32(body, 12);
        let id = le32(body, 16);
        // Only present the resource currently bound to the scanout (0 = none).
        if id != 0 && id == self.scanout_res {
            if let Some(r) = self.resources.get(&id) {
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
        }
        resp_hdr(RESP_OK_NODATA, fence, ctx)
    }
```

Add the dispatch arm:

```rust
            RESOURCE_FLUSH => self.flush(body, fence, ctx),
```

- [ ] **Step 4: Run to verify they pass + whole crate clippy.** Run: `PATH="$HOME/.cargo/bin:$PATH" cargo test -p ignition-devices gpu 2>&1 | tail -10` (all pass), then `PATH="$HOME/.cargo/bin:$PATH" cargo clippy -p ignition-devices --all-targets 2>&1 | tail -10` (clean; remove any leftover `#[allow(clippy::match_single_binding)]` and the `#[allow(dead_code)]` on `format`/`FORMAT_*` if they are now used — `format` is still only stored, keep its allow; the match now has many arms so its allow is gone).

- [ ] **Step 5: Commit.**

```bash
git add crates/devices/src/virtio/gpu.rs
git commit -m "feat(devices): virtio-gpu RESOURCE_FLUSH presents to DisplaySink"
```

---

## Task 6: Wire the device into boot.rs (--gui, Boot mode)

**Files:** Modify `spike/src/bin/boot.rs`.

Context: the `--gui` branch currently does `let (_sink, rx) = display_sink::WindowSink::new();` and drops `_sink`. The device must receive that sink. `setup_devices(mgr, ctx, mode)` runs earlier in `main` than the `--gui` branch, so the sink must be created before `setup_devices` and stashed in `ctx`.

- [ ] **Step 1: Add the sink field to `DeviceContext`.** In the `struct DeviceContext { ... }` definition, add a field (after `net: bool,`):

```rust
    /// Display sink for the virtio-gpu device (Some only in --gui boot). Taken by
    /// the gpu builder; None means no virtio-gpu device is added.
    display_sink: Option<Box<dyn ignition_devices::display::DisplaySink>>,
```

- [ ] **Step 2: Register the device in `setup_devices`.** Just before the final `Ok(())` of `setup_devices`, add (mirrors the vsock block; `take()` moves the sink into the builder closure):

```rust
    // virtio-gpu — present only when a display sink was provided (--gui, boot).
    // GUI restore is M5; the device is not added in restore mode.
    if let (Mode::Boot, Some(sink)) = (&mode, ctx.display_sink.take()) {
        let mem = ctx.guest_ram();
        place::<VirtioMmio, _>(mgr, &mode, "virtio-gpu", layout::MMIO_WINDOW,
            move |irq| VirtioMmio::new(
                "virtio-gpu",
                Box::new(ignition_devices::virtio::gpu::VirtioGpu::new(1280, 800, sink)),
                mem,
                irq,
            ))?;
    }
```

- [ ] **Step 3: Create the sink before `setup_devices` and keep `rx` for the event loop.** Find where `main` builds the `DeviceContext` for the boot path and calls `setup_devices`. Before constructing the context, add:

```rust
    // In --gui, create the display sink/receiver pair up front: the sink goes to the
    // virtio-gpu device (via DeviceContext), the receiver to the event loop.
    let gui_rx = if gui {
        let (sink, rx) = display_sink::WindowSink::new();
        gui_sink = Some(Box::new(sink) as Box<dyn ignition_devices::display::DisplaySink>);
        Some(rx)
    } else {
        None
    };
```

Declare `let mut gui_sink: Option<Box<dyn ignition_devices::display::DisplaySink>> = None;` just before this block, and set the `DeviceContext`'s `display_sink: gui_sink` field when the context literal is built (replace the field's initializer; if other context fields are set with `..Default` there is none here — set it explicitly in the struct literal).

- [ ] **Step 4: Use `gui_rx` in the `--gui` branch.** In the boot tail's `if gui { ... }` block, replace `let (_sink, rx) = display_sink::WindowSink::new();` with:

```rust
        let rx = gui_rx.expect("gui implies a receiver was created");
```

(the `done`/thread/`run_event_loop(rx, done, 1280, 800)` lines stay as they are).

- [ ] **Step 5: Build.** Run: `PATH="$HOME/.cargo/bin:$PATH" cargo build -p ignition-spike --bin boot 2>&1 | tail -20`. Fix compile errors:
  - `Mode` must be in scope at the `setup_devices` addition (it already is — `setup_devices` matches on `mode`).
  - The `DeviceContext` literal must now include `display_sink: gui_sink` — moving `gui_sink` there; ensure `gui_rx` is computed before `gui_sink` is moved (the snippet computes `gui_rx` which sets `gui_sink`, so order: declare `gui_sink`, compute `gui_rx` (sets `gui_sink`), then build the context using `gui_sink`).
  - If `setup_devices` takes `ctx` by `&mut`, `ctx.display_sink.take()` works; confirm the signature (`fn setup_devices(mgr, ctx: &mut DeviceContext, mode)`).

- [ ] **Step 6: Verify non-GUI unchanged + clippy + tests + sign.** Run:
`PATH="$HOME/.cargo/bin:$PATH" cargo test -p ignition-spike --bin boot 2>&1 | tail -10`
`PATH="$HOME/.cargo/bin:$PATH" cargo clippy -p ignition-spike --all-targets 2>&1 | tail -10`
`./scripts/sign.sh target/debug/boot 2>&1 | tail -3`
Expect tests pass, clippy clean (pre-existing fuzz warnings excepted), signing OK.

- [ ] **Step 7: Commit.**

```bash
git add spike/src/bin/boot.rs
git commit -m "feat(spike): register virtio-gpu device under --gui (boot mode)"
```

---

## Task 7: Blit real frames in the event loop

**Files:** Modify `spike/src/bin/display_sink.rs`.

Context: M2's `App::redraw` always clears to `CLEAR_0RGB`. Now it must blit a presented `Frame` (B8G8R8A8 bytes → softbuffer `0RGB` u32), honoring the dirty rect, and only clear when there is no frame.

- [ ] **Step 1: Write the failing test** (a pure blit helper, window-free, into the `tests` module of `display_sink.rs`):

```rust
    #[test]
    fn blit_converts_bgra_to_0rgb() {
        use std::sync::{Arc, Mutex};
        use ignition_devices::display::{DirtyRect, Frame};
        // 2x1 surface: pixel0 = B,G,R,A = (0x11,0x22,0x33,0xff); pixel1 = (0x44,0x55,0x66,0xff)
        let px = vec![0x11, 0x22, 0x33, 0xff, 0x44, 0x55, 0x66, 0xff];
        let frame = Frame {
            scanout_id: 0,
            width: 2,
            height: 1,
            stride: 8,
            dirty: DirtyRect { x: 0, y: 0, w: 2, h: 1 },
            pixels: Arc::new(Mutex::new(px)),
        };
        let mut buf = vec![0u32; 2];
        blit_frame(&mut buf, 2, 1, &frame);
        // 0RGB: (R<<16)|(G<<8)|B
        assert_eq!(buf[0], (0x33 << 16) | (0x22 << 8) | 0x11);
        assert_eq!(buf[1], (0x66 << 16) | (0x55 << 8) | 0x44);
    }
```

- [ ] **Step 2: Run to verify it fails.** Run: `PATH="$HOME/.cargo/bin:$PATH" cargo test -p ignition-spike --bin boot blit 2>&1 | tail -10`. Expect FAIL (`blit_frame` not found).

- [ ] **Step 3: Implement.** Add the pure helper near `coalesce` in `display_sink.rs`:

```rust
/// Blit a B8G8R8A8 frame into a softbuffer `0RGB` u32 buffer (`surf_w`×`surf_h`),
/// honoring the frame's dirty rect (clamped to both surfaces). Source pixel bytes
/// are B,G,R,A; the destination u32 is `(R<<16)|(G<<8)|B`.
pub fn blit_frame(buf: &mut [u32], surf_w: u32, surf_h: u32, frame: &Frame) {
    let src = frame.pixels.lock().unwrap();
    let x0 = frame.dirty.x.min(surf_w);
    let y0 = frame.dirty.y.min(surf_h);
    let x1 = frame.dirty.x.saturating_add(frame.dirty.w).min(surf_w).min(frame.width);
    let y1 = frame.dirty.y.saturating_add(frame.dirty.h).min(surf_h).min(frame.height);
    for y in y0..y1 {
        for x in x0..x1 {
            let s = ((y * frame.width + x) * 4) as usize;
            if s + 3 >= src.len() {
                continue;
            }
            let (b, g, r) = (src[s] as u32, src[s + 1] as u32, src[s + 2] as u32);
            let d = (y * surf_w + x) as usize;
            if d < buf.len() {
                buf[d] = (r << 16) | (g << 8) | b;
            }
        }
    }
}
```

Update `App::redraw` to use it (replace the body that currently always fills `CLEAR_0RGB`):

```rust
    fn redraw(&mut self) {
        let Some(surface) = self.surface.as_mut() else { return };
        let mut buf = match surface.buffer_mut() {
            Ok(b) => b,
            Err(_) => return,
        };
        match coalesce(&self.rx) {
            Some(frame) => blit_frame(&mut buf, self.width, self.height, &frame),
            None => buf.fill(CLEAR_0RGB),
        }
        let _ = buf.present();
    }
```

(Note: a `None` only clears when nothing has ever been presented; once a frame arrives the window shows it. If you want the last frame to persist across redraws with no new frame, that is a later refinement — for fbcon the guest re-flushes on every change, so clearing on idle is acceptable. Keep it simple per YAGNI.)

- [ ] **Step 4: Run to verify it passes + clippy.** Run: `PATH="$HOME/.cargo/bin:$PATH" cargo test -p ignition-spike --bin boot 2>&1 | tail -10` and `PATH="$HOME/.cargo/bin:$PATH" cargo clippy -p ignition-spike --all-targets 2>&1 | tail -10`. Expect pass + clean.

- [ ] **Step 5: Re-sign + commit.**

```bash
./scripts/sign.sh target/debug/boot
git add spike/src/bin/display_sink.rs
git commit -m "feat(spike): blit virtio-gpu frames into the window (BGRA->0RGB)"
```

---

## Task 8: Documentation

**Files:** Modify `docs/src/features/devices.md`, `docs/src/getting-started/guest-assets.md`, `ROADMAP.md`.

- [ ] **Step 1: Update the GUI section in `docs/src/features/devices.md`.** Replace the last paragraph of the "GUI display (software-rendered)" section (the one starting "This is the structural foundation...") with:

```markdown
A **virtio-gpu** device (2D only, device id 16) is added under `--gui`: the Linux
`virtio_gpu` driver binds it, `/dev/dri/card0` and `/dev/fb0` appear, and the kernel
framebuffer console renders live in the macOS window. `RESOURCE_FLUSH` from the guest
presents the scanned-out resource through the display sink; `TRANSFER_TO_HOST_2D`
copies guest pixels (scatter-gather correct) into a host buffer. No 3D/VIRGL/Venus, no
display resize or hotplug, and snapshot of GPU state is a later milestone.

The guest kernel must be built with `CONFIG_DRM`, `CONFIG_DRM_VIRTIO_GPU`,
`CONFIG_DRM_FBDEV_EMULATION`, `CONFIG_FB`, and `CONFIG_FRAMEBUFFER_CONSOLE`.
```

- [ ] **Step 2: Add the kernel configs to `docs/src/getting-started/guest-assets.md`.** Find the "Rebuild the kernel" section and append a note:

```markdown
For the GUI (virtio-gpu) milestone, the kernel config also needs `CONFIG_DRM=y`,
`CONFIG_DRM_VIRTIO_GPU=y`, `CONFIG_DRM_FBDEV_EMULATION=y`, `CONFIG_FB=y`, and
`CONFIG_FRAMEBUFFER_CONSOLE=y` so `/dev/dri/card0` + `/dev/fb0` appear and fbcon binds.
```

- [ ] **Step 3: Update `ROADMAP.md`.** Under the GUI/devices area, add a line noting virtio-gpu 2D (M1) shipped: the device renders the guest framebuffer console into the `--gui` window; 3D and GPU-snapshot are out of scope. (Place it near the existing device list / parity table consistent with how other shipped items are recorded.)

- [ ] **Step 4: Verify the book builds.** Run: `PATH="$HOME/.cargo/bin:$PATH" mdbook build docs 2>&1 | tail -5` (skip if `mdbook` absent). Expect success, no broken links.

- [ ] **Step 5: Commit.**

```bash
git add docs/src/features/devices.md docs/src/getting-started/guest-assets.md ROADMAP.md
git commit -m "docs: virtio-gpu 2D (M1) device + kernel configs"
```

---

## Manual integration verification (after all tasks; needs the artemis2 kernel rebuild + entitlement)

Not automated (needs a guest kernel rebuilt with the DRM configs + a GUI session). Run by hand:

1. Rebuild the guest kernel on `artemis2` with the DRM/virtio-gpu/fbcon configs (per `docs/src/getting-started/guest-assets.md`); copy `Image` back to `kimage/out/`.
2. `target/debug/boot --gui kimage/out/Image kimage/out/rootfs.ext4` — `dmesg | grep -iE "virtio_gpu|drm"` shows the driver binding; `/dev/dri/card0` and `/dev/fb0` exist; the kernel console renders in the macOS window and scrolls as the guest prints.
3. Regression: `boot` without `--gui`, `boot --restore`, `boot --fuzz` open no window and behave as before; `boot --gui` without the DRM kernel still boots (the guest just won't bind virtio-gpu) and the window stays at the clear color.

---

## Self-Review Notes

- **Spec coverage:** device id 16 / 2 queues / features 0 + config space (Task 1) ✓; GET_DISPLAY_INFO 16-entry (Task 2) ✓; CREATE_2D/UNREF/ATTACH/DETACH/SET_SCANOUT (Task 3) ✓; SG-correct TRANSFER_TO_HOST_2D + straddle test (Task 4) ✓; RESOURCE_FLUSH→present + CapSink (Task 5) ✓; cursorq ack (Task 1) ✓; unknown→ERR_UNSPEC, no panic (Task 1) ✓; --gui gating + WindowSink threading, Boot-only (Task 6) ✓; real blit BGRA→0RGB dirty-rect (Task 7) ✓; docs + kernel configs + ROADMAP (Task 8) ✓. Snapshot deferred to M5 per spec (device not added in restore mode — Task 6).
- **Type consistency:** `VirtioGpu::new(u32, u32, Box<dyn DisplaySink>)`, `Resource2D{format,width,height,backing:Vec<(u64,u32)>,pixels:Arc<Mutex<Vec<u8>>>}`, `dispatch(&[u8], &GuestRam)`, handler signatures `(&mut self, body:&[u8], [mem,] fence:u64, ctx:u32)->Vec<u8>`, `read_backing(&[(u64,u32)],&GuestRam,u64,&mut [u8])`, `blit_frame(&mut [u32],u32,u32,&Frame)` are used identically across tasks. `Frame`/`DirtyRect`/`DisplaySink` match the M2 `crates/devices/src/display.rs` definitions.
- **No placeholders:** every code step is complete; the `dispatch` match is built up arm-by-arm across Tasks 1–5 (each task shows the exact arm to add), which is incremental TDD, not a placeholder.
