//! Guest networking backends. `VmnetBackend` calls vmnet.framework in-process
//! (needs sudo); `SocketVmnetBackend` talks to the socket_vmnet daemon (no sudo).

mod socket_vmnet;
pub use socket_vmnet::{generate_mac, SocketVmnetBackend};

use std::os::raw::c_void;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Mutex;

use ignition_devices::virtio::net::NetBackend;

#[repr(C)]
struct IgVmnet {
    _private: [u8; 0],
}

type FrameCb = extern "C" fn(*mut c_void, *const u8, usize);

unsafe extern "C" {
    fn ig_vmnet_start(mac_out: *mut u8, cb: FrameCb, ctx: *mut c_void) -> *mut IgVmnet;
    fn ig_vmnet_write(h: *mut IgVmnet, data: *const u8, len: usize) -> i32;
}

/// The RX callback context: a channel sender for received frames.
struct RxCtx {
    tx: Sender<Vec<u8>>,
}

extern "C" fn on_frame(ctx: *mut c_void, data: *const u8, len: usize) {
    // A panic must not unwind into the C caller (UB). Swallow it.
    // SAFETY: ctx is the leaked Box<RxCtx>; data/len describe one frame,
    // bounded by the shim's buffer.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let ctx = unsafe { &*(ctx as *const RxCtx) };
        let frame = unsafe { std::slice::from_raw_parts(data, len) }.to_vec();
        let _ = ctx.tx.send(frame);
    }));
}

pub struct VmnetBackend {
    handle: Mutex<*mut IgVmnet>,
    mac: [u8; 6],
}
// SAFETY: the shim's interface_ref is internally synchronized on its own serial
// dispatch queue; we serialize writes behind the Mutex.
unsafe impl Send for VmnetBackend {}
unsafe impl Sync for VmnetBackend {}

impl VmnetBackend {
    /// Start vmnet shared mode. Returns the backend + the RX frame receiver.
    pub fn start() -> std::io::Result<(VmnetBackend, Receiver<Vec<u8>>)> {
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = Box::into_raw(Box::new(RxCtx { tx })) as *mut c_void;
        let mut mac = [0u8; 6];
        // SAFETY: on_frame matches FrameCb; ctx outlives the interface (leaked).
        let handle = unsafe { ig_vmnet_start(mac.as_mut_ptr(), on_frame, ctx) };
        if handle.is_null() {
            return Err(std::io::Error::other(
                "vmnet_start_interface failed (run under sudo for shared mode)",
            ));
        }
        Ok((VmnetBackend { handle: Mutex::new(handle), mac }, rx))
    }
}

impl NetBackend for VmnetBackend {
    fn write_frame(&self, frame: &[u8]) -> std::io::Result<()> {
        let h = *self.handle.lock().unwrap();
        // SAFETY: h is a valid handle for the process lifetime.
        let rc = unsafe { ig_vmnet_write(h, frame.as_ptr(), frame.len()) };
        if rc == 0 { Ok(()) } else { Err(std::io::Error::other("vmnet_write failed")) }
    }
    fn mac(&self) -> [u8; 6] {
        self.mac
    }
}
