//! Host-side display sink for `--gui`: a `Send + Sync` proxy (`WindowSink`) that
//! forwards frames over an mpsc channel without blocking, plus a `winit` +
//! `softbuffer` event loop that owns the main thread, drains the channel,
//! coalesces to the latest frame, and blits it (B8G8R8A8 -> 0RGB). The virtio-gpu
//! device feeds frames on RESOURCE_FLUSH; an idle tick with no frame clears to a
//! solid color.

use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

use ignition_devices::display::{DisplaySink, Frame};
use winit::keyboard::KeyCode;

/// Map a winit physical key to a Linux evdev key code. Covers the committed subset
/// (letters, digits, common control/navigation/modifier/punctuation keys). Unmapped
/// keys return None and are dropped.
pub fn map_keycode(kc: KeyCode) -> Option<u16> {
    use KeyCode::*;
    Some(match kc {
        KeyA => 30, KeyB => 48, KeyC => 46, KeyD => 32, KeyE => 18, KeyF => 33,
        KeyG => 34, KeyH => 35, KeyI => 23, KeyJ => 36, KeyK => 37, KeyL => 38,
        KeyM => 50, KeyN => 49, KeyO => 24, KeyP => 25, KeyQ => 16, KeyR => 19,
        KeyS => 31, KeyT => 20, KeyU => 22, KeyV => 47, KeyW => 17, KeyX => 45,
        KeyY => 21, KeyZ => 44,
        Digit1 => 2, Digit2 => 3, Digit3 => 4, Digit4 => 5, Digit5 => 6,
        Digit6 => 7, Digit7 => 8, Digit8 => 9, Digit9 => 10, Digit0 => 11,
        Enter => 28, Escape => 1, Backspace => 14, Tab => 15, Space => 57,
        Minus => 12, Equal => 13, BracketLeft => 26, BracketRight => 27,
        Backslash => 43, Semicolon => 39, Quote => 40, Backquote => 41,
        Comma => 51, Period => 52, Slash => 53, CapsLock => 58,
        ArrowUp => 103, ArrowDown => 108, ArrowLeft => 105, ArrowRight => 106,
        ShiftLeft => 42, ShiftRight => 54, ControlLeft => 29, ControlRight => 97,
        AltLeft => 56, AltRight => 100, SuperLeft => 125, SuperRight => 126,
        _ => return None,
    })
}

/// A host-side action triggered by a GUI hotkey chord, dispatched to the
/// `VcpuManager` instead of being forwarded to the guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotAction {
    Reset,
    Snapshot,
    Quit,
}

/// Map a Ctrl+Alt+<letter> chord to a host-side action. Returns None unless BOTH
/// ctrl and alt are held and the key is one of the bound letters, so ordinary
/// typing (and plain Ctrl/Alt combos the guest needs) passes through to the guest.
fn match_hotkey(ctrl: bool, alt: bool, key: winit::keyboard::KeyCode) -> Option<HotAction> {
    use winit::keyboard::KeyCode;
    if !(ctrl && alt) {
        return None;
    }
    // NB: no Ctrl+Alt+C (mark in-memory checkpoint). Resetting to an arbitrary
    // mid-session checkpoint cannot restore the GIC's in-flight interrupt state
    // in place on HVF (hv_gic_set_state mid-run breaks delivery), so it wedges the
    // guest. Reset always targets the quiesced warm-base point, which works.
    match key {
        KeyCode::KeyR => Some(HotAction::Reset),
        KeyCode::KeyS => Some(HotAction::Snapshot),
        KeyCode::KeyX => Some(HotAction::Quit),
        _ => None,
    }
}

#[cfg(test)]
mod hotkey_tests {
    use super::*;
    use winit::keyboard::KeyCode;
    #[test]
    fn ctrl_alt_letters_map_to_actions() {
        assert_eq!(match_hotkey(true, true, KeyCode::KeyR), Some(HotAction::Reset));
        assert_eq!(match_hotkey(true, true, KeyCode::KeyS), Some(HotAction::Snapshot));
        assert_eq!(match_hotkey(true, true, KeyCode::KeyX), Some(HotAction::Quit));
        // Ctrl+Alt+C is intentionally unbound (no in-place mid-session checkpoint).
        assert_eq!(match_hotkey(true, true, KeyCode::KeyC), None);
    }
    #[test]
    fn requires_both_modifiers_and_bound_letter() {
        assert_eq!(match_hotkey(false, true, KeyCode::KeyR), None); // no ctrl
        assert_eq!(match_hotkey(true, false, KeyCode::KeyR), None); // no alt
        assert_eq!(match_hotkey(false, false, KeyCode::KeyR), None); // neither
        assert_eq!(match_hotkey(true, true, KeyCode::KeyA), None); // unbound letter
    }
}

/// Scale a window-physical position to a guest absolute axis coordinate, clamped.
/// `surf_w`/`surf_h` are the physical surface size; `gw`/`gh` the guest resolution.
pub fn scale_pos(px: f64, py: f64, surf_w: u32, surf_h: u32, gw: u32, gh: u32) -> (u32, u32) {
    let clamp = |v: f64, max: u32| -> u32 {
        if v <= 0.0 {
            0
        } else if v >= max as f64 {
            max
        } else {
            v as u32
        }
    };
    let x = px * (gw.saturating_sub(1) as f64) / (surf_w.max(1) as f64 - 1.0).max(1.0);
    let y = py * (gh.saturating_sub(1) as f64) / (surf_h.max(1) as f64 - 1.0).max(1.0);
    (clamp(x, gw - 1), clamp(y, gh - 1))
}

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

/// Blit a B8G8R8A8 frame into a softbuffer `0RGB` u32 buffer sized `surf_w`×`surf_h`
/// (the window's PHYSICAL pixels), scaling the guest image to fill the surface with
/// nearest-neighbor sampling. On a Retina display the surface is larger than the
/// guest mode (e.g. 2560×1600 vs 1280×800), so a 1:1 copy would fill only a corner;
/// scaling fills the whole window. Source pixel bytes are B,G,R,A; the destination
/// u32 is `(R<<16)|(G<<8)|B`.
pub fn blit_frame(buf: &mut [u32], surf_w: u32, surf_h: u32, frame: &Frame) {
    if surf_w == 0 || surf_h == 0 || frame.width == 0 || frame.height == 0 {
        return;
    }
    let src = frame.pixels.lock().unwrap();
    let (fw, fh) = (frame.width as usize, frame.height as usize);
    let (sw, sh) = (surf_w as usize, surf_h as usize);
    for dy in 0..sh {
        let sy = dy * fh / sh; // nearest-neighbor source row
        for dx in 0..sw {
            let sx = dx * fw / sw; // nearest-neighbor source column
            let s = (sy * fw + sx) * 4;
            if s + 3 >= src.len() {
                continue;
            }
            let (b, g, r) = (src[s] as u32, src[s + 1] as u32, src[s + 2] as u32);
            let d = dy * sw + dx;
            if d < buf.len() {
                buf[d] = (r << 16) | (g << 8) | b;
            }
        }
    }
}

/// winit application state: owns the window + softbuffer surface (main thread only).
struct App {
    /// Requested window size in logical points (the physical surface may be larger
    /// on a HiDPI display; `surf_w`/`surf_h` track the actual physical size).
    width: u32,
    height: u32,
    /// Physical surface size (window inner_size in pixels) the buffer is blitted to.
    surf_w: u32,
    surf_h: u32,
    rx: Receiver<Frame>,
    done: Arc<AtomicBool>,
    /// The most recent presented frame, re-blitted on idle redraws so the window
    /// holds its image between guest flushes instead of flashing the clear color.
    last: Option<Frame>,
    /// Force a repaint even with no new frame (first paint, resize). Otherwise an
    /// idle redraw with nothing new skips the present and the window keeps its image.
    force_paint: bool,
    window: Option<Rc<Window>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
    keyboard: Option<std::sync::Arc<std::sync::Mutex<ignition_devices::virtio::mmio::VirtioMmio>>>,
    tablet: Option<std::sync::Arc<std::sync::Mutex<ignition_devices::virtio::mmio::VirtioMmio>>>,
    gw: u32,
    gh: u32,
    /// Live modifier state, updated from `ModifiersChanged`, read to detect the
    /// Ctrl+Alt+<letter> host hotkey chords before forwarding keys to the guest.
    modifiers: winit::keyboard::ModifiersState,
    /// The VM owning this window, so a host hotkey can request reset/checkpoint/
    /// snapshot. `None` outside the GUI reset use-case (keys then always forward).
    manager: Option<std::sync::Arc<ignition_vmm::vstate::vcpu_manager::VcpuManager>>,
}

impl App {
    /// Resize the softbuffer surface to the window's current physical size.
    fn resize_surface(&mut self) {
        let (Some(window), Some(surface)) = (self.window.as_ref(), self.surface.as_mut()) else {
            return;
        };
        let size = window.inner_size();
        self.surf_w = size.width.max(1);
        self.surf_h = size.height.max(1);
        let _ = surface.resize(
            NonZeroU32::new(self.surf_w).unwrap_or(NonZeroU32::MIN),
            NonZeroU32::new(self.surf_h).unwrap_or(NonZeroU32::MIN),
        );
    }

    fn redraw(&mut self) {
        // Take any newly presented frame; if none arrived, keep showing the last one.
        let got_new = if let Some(frame) = coalesce(&self.rx) {
            self.last = Some(frame);
            true
        } else {
            false
        };
        // Nothing changed since the last present: leave the window as-is (no blink,
        // no wasted full-surface rescale).
        if !got_new && !self.force_paint {
            return;
        }
        self.force_paint = false;
        let (surf_w, surf_h) = (self.surf_w, self.surf_h);
        let Some(surface) = self.surface.as_mut() else { return };
        let mut buf = match surface.buffer_mut() {
            Ok(b) => b,
            Err(_) => return,
        };
        match &self.last {
            Some(frame) => blit_frame(&mut buf, surf_w, surf_h, frame),
            // Nothing has ever been presented: clear to the slate color once.
            None => buf.fill(CLEAR_0RGB),
        }
        let _ = buf.present();
    }

    /// Carry out a host-side hotkey action against this window's VM. No-op (beyond a
    /// log line) when no manager is wired or no reset point exists for `Reset`.
    fn dispatch_hotkey(&self, action: HotAction, event_loop: &ActiveEventLoop) {
        let Some(mgr) = &self.manager else {
            return;
        };
        match action {
            HotAction::Reset => {
                if mgr.has_reset_point() {
                    eprintln!("\n[gui] reset to checkpoint");
                    mgr.request_reset();
                } else {
                    eprintln!("\n[gui] reset: no checkpoint (only available after --restore)");
                }
            }
            HotAction::Snapshot => {
                eprintln!("\n[gui] snapshot requested");
                mgr.request_snapshot();
            }
            HotAction::Quit => {
                eprintln!("\n[gui] closing window");
                event_loop.exit();
            }
        }
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
        let surface =
            softbuffer::Surface::new(&context, window.clone()).expect("softbuffer surface");
        self.window = Some(window.clone());
        self.surface = Some(surface);
        // Size the surface to the window's PHYSICAL pixels (may be > logical on HiDPI).
        self.resize_surface();
        self.force_paint = true;
        // A CLI-launched window may not become the key window automatically on macOS;
        // ask for focus so keyboard events flow without a manual click.
        window.focus_window();
        window.request_redraw();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(_) => {
                self.resize_surface();
                self.force_paint = true;
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => self.redraw(),
            WindowEvent::ModifiersChanged(m) => {
                self.modifiers = m.state();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                use ignition_devices::virtio::input::InputEvent;
                use winit::keyboard::PhysicalKey;
                // Intercept host hotkey chords BEFORE forwarding to the guest: a
                // Ctrl+Alt+<letter> chord is swallowed (press and release) and
                // dispatched to the VM so the guest never sees it.
                if let PhysicalKey::Code(kc) = event.physical_key {
                    let ctrl = self.modifiers.control_key();
                    let alt = self.modifiers.alt_key();
                    if let Some(action) = match_hotkey(ctrl, alt, kc) {
                        if event.state.is_pressed() {
                            self.dispatch_hotkey(action, event_loop);
                        }
                        return;
                    }
                }
                if let PhysicalKey::Code(kc) = event.physical_key
                    && let (Some(code), Some(kbd)) = (map_keycode(kc), &self.keyboard)
                {
                    let value = if event.state.is_pressed() { 1 } else { 0 };
                    let evs = [
                        InputEvent { etype: 1, code, value },       // EV_KEY
                        InputEvent { etype: 0, code: 0, value: 0 }, // EV_SYN/SYN_REPORT
                    ];
                    // Best-effort: recover the guard if a vCPU panic poisoned the
                    // device mutex, so a GUI event can't mask the original crash.
                    let _ = kbd.lock().unwrap_or_else(|p| p.into_inner()).inject_input(&evs);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                use ignition_devices::virtio::input::InputEvent;
                if let Some(tab) = &self.tablet {
                    let (x, y) = scale_pos(position.x, position.y, self.surf_w, self.surf_h, self.gw, self.gh);
                    let evs = [
                        InputEvent { etype: 3, code: 0, value: x }, // EV_ABS ABS_X
                        InputEvent { etype: 3, code: 1, value: y }, // EV_ABS ABS_Y
                        InputEvent { etype: 0, code: 0, value: 0 }, // EV_SYN
                    ];
                    let _ = tab.lock().unwrap_or_else(|p| p.into_inner()).inject_input(&evs);
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                use ignition_devices::virtio::input::InputEvent;
                use winit::event::MouseButton;
                if let Some(tab) = &self.tablet {
                    let code = match button {
                        MouseButton::Left => 0x110u16,
                        MouseButton::Right => 0x111,
                        MouseButton::Middle => 0x112,
                        _ => return,
                    };
                    let value = if state.is_pressed() { 1 } else { 0 };
                    let evs = [
                        InputEvent { etype: 1, code, value }, // EV_KEY BTN_*
                        InputEvent { etype: 0, code: 0, value: 0 },
                    ];
                    let _ = tab.lock().unwrap_or_else(|p| p.into_inner()).inject_input(&evs);
                }
            }
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
#[allow(clippy::too_many_arguments)]
pub fn run_event_loop(
    rx: Receiver<Frame>,
    done: Arc<AtomicBool>,
    width: u32,
    height: u32,
    keyboard: Option<std::sync::Arc<std::sync::Mutex<ignition_devices::virtio::mmio::VirtioMmio>>>,
    tablet: Option<std::sync::Arc<std::sync::Mutex<ignition_devices::virtio::mmio::VirtioMmio>>>,
    gw: u32,
    gh: u32,
    manager: Option<std::sync::Arc<ignition_vmm::vstate::vcpu_manager::VcpuManager>>,
) {
    let event_loop = EventLoop::new().expect("winit event loop");
    event_loop.set_control_flow(ControlFlow::WaitUntil(
        std::time::Instant::now() + Duration::from_millis(16),
    ));
    let mut app = App {
        width,
        height,
        surf_w: width,
        surf_h: height,
        rx,
        done,
        last: None,
        force_paint: true,
        window: None,
        surface: None,
        keyboard,
        tablet,
        gw,
        gh,
        modifiers: winit::keyboard::ModifiersState::empty(),
        manager,
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
    fn keycode_maps_known_keys() {
        use winit::keyboard::KeyCode;
        assert_eq!(map_keycode(KeyCode::KeyA), Some(30));
        assert_eq!(map_keycode(KeyCode::Enter), Some(28));
        assert_eq!(map_keycode(KeyCode::Space), Some(57));
        assert_eq!(map_keycode(KeyCode::Digit1), Some(2));
        assert_eq!(map_keycode(KeyCode::ArrowUp), Some(103));
        assert_eq!(map_keycode(KeyCode::F13), None);
    }

    #[test]
    fn pointer_scale_maps_corners() {
        assert_eq!(scale_pos(0.0, 0.0, 2560, 1600, 1280, 800), (0, 0));
        assert_eq!(scale_pos(2559.0, 1599.0, 2560, 1600, 1280, 800), (1279, 799));
        assert_eq!(scale_pos(99999.0, 99999.0, 2560, 1600, 1280, 800), (1279, 799));
        assert_eq!(scale_pos(-5.0, -5.0, 2560, 1600, 1280, 800), (0, 0));
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
