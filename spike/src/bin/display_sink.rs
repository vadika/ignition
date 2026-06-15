//! Host-side display sink for `--gui`: a `Send + Sync` proxy (`WindowSink`) that
//! forwards frames over an mpsc channel without blocking, plus a `winit` +
//! `softbuffer` event loop that owns the main thread, drains the channel,
//! coalesces to the latest frame, and presents it. This milestone has no
//! virtio-gpu device, so no frames arrive and the window clears to a solid color.

use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

use ignition_devices::display::{DisplaySink, Frame};

use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

/// Solid clear color while no device drives the framebuffer. softbuffer treats each
/// u32 as `0RGB` (top byte ignored), so this is R=0x20 G=0x24 B=0x28 (dark slate).
const CLEAR_0RGB: u32 = 0x0020_2428;

/// `Send + Sync` proxy handed to the VMM/device side. `present` forwards to the UI
/// thread and never blocks. A later milestone hands this to the virtio-gpu device.
pub struct WindowSink {
    tx: Sender<Frame>,
}

impl WindowSink {
    /// Create the proxy and its paired receiver (drained by `run_event_loop`).
    pub fn new() -> (WindowSink, Receiver<Frame>) {
        let (tx, rx) = mpsc::channel();
        (WindowSink { tx }, rx)
    }
}

impl DisplaySink for WindowSink {
    fn present(&self, frame: Frame) {
        let _ = self.tx.send(frame);
    }
}

/// Drain the channel and return only the most recent frame (a backlog collapses to
/// one blit). Pure and window-free so it is unit-testable.
pub fn coalesce(rx: &Receiver<Frame>) -> Option<Frame> {
    let mut latest = None;
    while let Ok(f) = rx.try_recv() {
        latest = Some(f);
    }
    latest
}

/// Blit a B8G8R8A8 frame into a softbuffer `0RGB` u32 buffer (`surf_w`×`surf_h`),
/// honoring the frame's dirty rect (clamped to both surfaces). Source pixel bytes
/// are B,G,R,A; the destination u32 is `(R<<16)|(G<<8)|B`.
pub fn blit_frame(buf: &mut [u32], surf_w: u32, surf_h: u32, frame: &Frame) {
    let src = frame.pixels.lock().unwrap();
    let x0 = frame.dirty.x.min(surf_w);
    let y0 = frame.dirty.y.min(surf_h);
    let x1 = frame.dirty.x.saturating_add(frame.dirty.w).min(surf_w).min(frame.width);
    let y1 = frame.dirty.y.saturating_add(frame.dirty.h).min(surf_h).min(frame.height);
    // Index math in usize: frame.width is guest-sized, so `y * frame.width` can
    // exceed u32 even when the dirty rect is clamped to the small window.
    let (fw, sw) = (frame.width as usize, surf_w as usize);
    for y in y0..y1 {
        for x in x0..x1 {
            let s = ((y as usize) * fw + x as usize) * 4;
            if s + 3 >= src.len() {
                continue;
            }
            let (b, g, r) = (src[s] as u32, src[s + 1] as u32, src[s + 2] as u32);
            let d = (y as usize) * sw + x as usize;
            if d < buf.len() {
                buf[d] = (r << 16) | (g << 8) | b;
            }
        }
    }
}

/// winit application state: owns the window + softbuffer surface (main thread only).
struct App {
    width: u32,
    height: u32,
    rx: Receiver<Frame>,
    done: Arc<AtomicBool>,
    window: Option<Rc<Window>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
}

impl App {
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
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let attrs = Window::default_attributes()
            .with_title("ignition")
            .with_inner_size(LogicalSize::new(self.width, self.height))
            .with_resizable(false);
        let window = Rc::new(event_loop.create_window(attrs).expect("create window"));
        let context = softbuffer::Context::new(window.clone()).expect("softbuffer context");
        let mut surface =
            softbuffer::Surface::new(&context, window.clone()).expect("softbuffer surface");
        surface
            .resize(
                NonZeroU32::new(self.width).unwrap_or(NonZeroU32::MIN),
                NonZeroU32::new(self.height).unwrap_or(NonZeroU32::MIN),
            )
            .expect("surface resize");
        self.window = Some(window.clone());
        self.surface = Some(surface);
        window.request_redraw();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => self.redraw(),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Acquire pairs with the VMM thread's Release store of `done` (Apple Silicon
        // is weakly ordered): observe everything that happened before it was set.
        if self.done.load(Ordering::Acquire) {
            event_loop.exit();
            return;
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(
            std::time::Instant::now() + Duration::from_millis(16),
        ));
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }
}

/// Run the winit event loop on the calling thread (must be the main thread on
/// macOS). Returns when the window closes or `done` is set. Drains `rx` and clears
/// the window to a solid color each frame.
pub fn run_event_loop(rx: Receiver<Frame>, done: Arc<AtomicBool>, width: u32, height: u32) {
    let event_loop = EventLoop::new().expect("winit event loop");
    event_loop.set_control_flow(ControlFlow::WaitUntil(
        std::time::Instant::now() + Duration::from_millis(16),
    ));
    let mut app = App {
        width,
        height,
        rx,
        done,
        window: None,
        surface: None,
    };
    if let Err(e) = event_loop.run_app(&mut app) {
        eprintln!("[gui] event loop exited with error: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use ignition_devices::display::{DirtyRect, DisplaySink};

    fn frame(id: u32) -> Frame {
        Frame {
            scanout_id: id,
            width: 1,
            height: 1,
            stride: 4,
            dirty: DirtyRect { x: 0, y: 0, w: 1, h: 1 },
            pixels: Arc::new(Mutex::new(vec![0u8; 4])),
        }
    }

    #[test]
    fn window_sink_forwards_frame() {
        let (sink, rx) = WindowSink::new();
        sink.present(frame(7));
        let got = rx.try_recv().expect("frame should arrive");
        assert_eq!(got.scanout_id, 7);
    }

    #[test]
    fn window_sink_does_not_block_after_receiver_dropped() {
        let (sink, rx) = WindowSink::new();
        drop(rx);
        sink.present(frame(1));
    }

    #[test]
    fn coalesce_returns_last_frame() {
        let (sink, rx) = WindowSink::new();
        sink.present(frame(1));
        sink.present(frame(2));
        sink.present(frame(3));
        let got = coalesce(&rx).expect("some frame");
        assert_eq!(got.scanout_id, 3);
        assert!(coalesce(&rx).is_none(), "channel drained");
    }

    #[test]
    fn coalesce_empty_is_none() {
        let (_sink, rx) = WindowSink::new();
        assert!(coalesce(&rx).is_none());
    }

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
        assert_eq!(buf[0], (0x33u32 << 16) | (0x22 << 8) | 0x11);
        assert_eq!(buf[1], (0x66u32 << 16) | (0x55 << 8) | 0x44);
    }
}
