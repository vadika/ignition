# GUI Display Refactor (M2 structural) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Invert `boot`'s threading so the macOS `winit` event loop owns the main thread and the whole VMM runs on spawned threads, behind a new `--gui` flag, and establish a non-blocking `DisplaySink` seam — opening a blank software-rendered window with no virtio-gpu device yet.

**Architecture:** A `DisplaySink` trait + `NoopSink` + `Frame` type live in `crates/devices` (mirroring the `IrqLine`/`NoopIrq` pattern). The `spike` binary adds a `WindowSink` (an `mpsc::Sender<Frame>` proxy that is `Send + Sync` and never blocks) plus a `winit`+`softbuffer` event-loop runner that drains the channel, coalesces to the latest frame, and (this milestone) clears the window to a solid color. `boot` gains `--gui`: when set, `spawn_stdin_reader` + sandbox apply + `manager.run` move onto a spawned VMM thread and the event loop runs on main; when unset, behavior is byte-for-byte unchanged.

**Tech Stack:** Rust (edition 2024), `winit = "0.30"`, `softbuffer = "0.4"`, existing `ignition-devices`/`ignition-vmm`/`ignition-sandbox` crates, HVF.

---

## File Structure

- `crates/devices/src/display.rs` — **create.** `DirtyRect`, `Frame`, `DisplaySink`, `NoopSink` + unit tests. No GUI deps.
- `crates/devices/src/lib.rs` — **modify.** Add `pub mod display;`.
- `spike/Cargo.toml` — **modify.** Add `winit` + `softbuffer`.
- `spike/src/bin/display_sink.rs` — **create.** `WindowSink`, `coalesce` (pure, testable), `run_event_loop` (winit `ApplicationHandler`) + unit tests for the non-winit parts.
- `spike/src/bin/boot.rs` — **modify.** `mod display_sink;`, `--gui` flag, main-thread inversion at the boot tail.
- `docs/src/features/devices.md` — **modify.** Document `--gui` and the software-display approach (pixels not wired until M1).

---

## Task 1: DisplaySink seam in `crates/devices`

**Files:**
- Create: `crates/devices/src/display.rs`
- Modify: `crates/devices/src/lib.rs:18` (add module declaration after `pub mod virtio;`)

- [ ] **Step 1: Write the failing tests**

Create `crates/devices/src/display.rs` with the test module first (types referenced will not exist yet):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn dummy_frame() -> Frame {
        Frame {
            scanout_id: 0,
            width: 4,
            height: 2,
            stride: 16,
            dirty: DirtyRect { x: 0, y: 0, w: 4, h: 2 },
            pixels: Arc::new(Mutex::new(vec![0u8; 4 * 2 * 4])),
        }
    }

    #[test]
    fn noop_sink_discards_without_panic() {
        let sink = NoopSink;
        sink.present(dummy_frame()); // must not panic, must not block
    }

    #[test]
    fn frame_clone_shares_pixel_arc() {
        let f = dummy_frame();
        let before = Arc::strong_count(&f.pixels);
        let g = f.clone();
        assert_eq!(Arc::strong_count(&f.pixels), before + 1);
        assert_eq!(g.width, 4);
    }

    #[test]
    fn dirty_rect_equality() {
        assert_eq!(
            DirtyRect { x: 1, y: 2, w: 3, h: 4 },
            DirtyRect { x: 1, y: 2, w: 3, h: 4 }
        );
        assert_ne!(
            DirtyRect { x: 0, y: 0, w: 1, h: 1 },
            DirtyRect { x: 0, y: 0, w: 2, h: 1 }
        );
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail to compile**

Run: `PATH="$HOME/.cargo/bin:$PATH" cargo test -p ignition-devices display 2>&1 | tail -20`
Expected: compile error — `cannot find type Frame` / `DirtyRect` / `NoopSink` in this scope.

- [ ] **Step 3: Implement the types**

Prepend to `crates/devices/src/display.rs` (above the test module):

```rust
//! Host display seam. Mirrors the `IrqLine`/`NoopIrq` pattern: a trait the device
//! crate owns, a no-op default for the manager/tests, and a real implementation
//! supplied by the binary. The virtio-gpu device (a later milestone) will hold a
//! `Box<dyn DisplaySink>` and call `present` on FLUSH.

use std::sync::{Arc, Mutex};

/// A rectangle of the scanout that changed, in pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirtyRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// One presentable frame: a handle to the scanout's host pixel buffer plus the
/// geometry needed to blit it. `pixels` is shared (not copied) so a FLUSH hands
/// over a handle rather than a memcpy. Pixel format is fixed B8G8R8A8 for v1.
#[derive(Clone)]
pub struct Frame {
    pub scanout_id: u32,
    pub width: u32,
    pub height: u32,
    /// Bytes per row of `pixels`.
    pub stride: u32,
    pub dirty: DirtyRect,
    pub pixels: Arc<Mutex<Vec<u8>>>,
}

/// Host display sink. Implementations must be `Send + Sync`, and `present` must be
/// non-blocking — drop or coalesce frames rather than block a vCPU thread.
pub trait DisplaySink: Send + Sync {
    fn present(&self, frame: Frame);
}

/// A `DisplaySink` that discards every frame — for the manager and tests, and for
/// the default headless (no `--gui`) path.
pub struct NoopSink;

impl DisplaySink for NoopSink {
    fn present(&self, _frame: Frame) {}
}
```

Then add the module declaration in `crates/devices/src/lib.rs` immediately after the existing `pub mod virtio;` line:

```rust
pub mod display;
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `PATH="$HOME/.cargo/bin:$PATH" cargo test -p ignition-devices display 2>&1 | tail -20`
Expected: `test result: ok. 3 passed`.

- [ ] **Step 5: Verify the whole crate still builds clean**

Run: `PATH="$HOME/.cargo/bin:$PATH" cargo clippy -p ignition-devices --all-targets 2>&1 | tail -10`
Expected: no warnings, no errors.

- [ ] **Step 6: Commit**

```bash
git add crates/devices/src/display.rs crates/devices/src/lib.rs
git commit -m "feat(devices): DisplaySink seam (trait + NoopSink + Frame)"
```

---

## Task 2: `WindowSink` proxy, coalesce helper, and winit event loop in `spike`

**Files:**
- Modify: `spike/Cargo.toml` (add dependencies)
- Create: `spike/src/bin/display_sink.rs`

- [ ] **Step 1: Add the windowing dependencies**

In `spike/Cargo.toml`, under `[dependencies]`, add:

```toml
winit = "0.30"
softbuffer = "0.4"
```

- [ ] **Step 2: Write the failing tests**

Create `spike/src/bin/display_sink.rs` with only the imports + test module first:

```rust
use std::sync::mpsc::Receiver;
use ignition_devices::display::Frame;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::sync::mpsc;
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
        sink.present(frame(1)); // must return, not panic or block
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
}
```

- [ ] **Step 3: Run the tests to verify they fail to compile**

Run: `PATH="$HOME/.cargo/bin:$PATH" cargo test -p ignition-spike --bin boot display_sink 2>&1 | tail -20`
Expected: compile error — `cannot find function coalesce` / `WindowSink` not found. (If `cargo` first downloads/builds `winit`+`softbuffer`, that is fine.)

> Note: this test target compiles via `boot.rs`, which does not yet declare `mod display_sink;`. Add that declaration now so the module is part of the crate: in `spike/src/bin/boot.rs`, near the other top-level `mod`/`use` lines, add `mod display_sink;`. (Task 3 uses it; declaring it here lets the tests compile.)

- [ ] **Step 4: Implement `WindowSink`, `coalesce`, and the event loop**

Replace the import line at the top of `spike/src/bin/display_sink.rs` (keep the test module below) with the full implementation:

```rust
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
        // Non-blocking: drop the frame if the UI side is gone.
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
            // No virtio-gpu device this milestone: nothing to blit, clear the window.
            // A later milestone blits `frame.dirty` from `frame.pixels` here.
            Some(_frame) => buf.fill(CLEAR_0RGB),
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
        let window = Rc::new(
            event_loop
                .create_window(attrs)
                .expect("create window"),
        );
        let context = softbuffer::Context::new(window.clone()).expect("softbuffer context");
        let mut surface =
            softbuffer::Surface::new(&context, window.clone()).expect("softbuffer surface");
        surface
            .resize(
                NonZeroU32::new(self.width).unwrap(),
                NonZeroU32::new(self.height).unwrap(),
            )
            .expect("surface resize");
        self.window = Some(window.clone());
        self.surface = Some(surface);
        window.request_redraw();
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => self.redraw(),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // VMM thread finished (guest powered off / vCPU error) → leave the loop.
        if self.done.load(Ordering::Relaxed) {
            event_loop.exit();
            return;
        }
        // Poll ~60 Hz: re-check `done` and repaint without busy-spinning.
        event_loop.set_control_flow(ControlFlow::wait_duration(Duration::from_millis(16)));
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
    event_loop.set_control_flow(ControlFlow::wait_duration(Duration::from_millis(16)));
    let mut app = App {
        width,
        height,
        rx,
        done,
        window: None,
        surface: None,
    };
    let _ = event_loop.run_app(&mut app);
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `PATH="$HOME/.cargo/bin:$PATH" cargo test -p ignition-spike --bin boot display_sink 2>&1 | tail -20`
Expected: `test result: ok. 4 passed`.

- [ ] **Step 6: Verify clippy is clean for the spike crate**

Run: `PATH="$HOME/.cargo/bin:$PATH" cargo clippy -p ignition-spike --all-targets 2>&1 | tail -15`
Expected: no errors. (If clippy flags the two identical `buf.fill` match arms with `match_like_matches_macro` or `clippy::all` complains about the redundant match, collapse to a single `let _ = coalesce(&self.rx); buf.fill(CLEAR_0RGB);` with a comment that a later milestone blits the frame — keep the `coalesce` call so the channel still drains.)

- [ ] **Step 7: Commit**

```bash
git add spike/Cargo.toml spike/src/bin/display_sink.rs spike/src/bin/boot.rs
git commit -m "feat(spike): WindowSink proxy + winit/softbuffer event loop"
```

---

## Task 3: `--gui` flag and main-thread inversion in `boot.rs`

**Files:**
- Modify: `spike/src/bin/boot.rs` — flag declaration (~line 562), flag parse arm (~line 638, alongside `--no-sandbox`), usage string (~line 712), and the boot tail (~lines 1014–1043).

- [ ] **Step 1: Declare the flag**

In `spike/src/bin/boot.rs`, alongside the other `let mut` flag declarations (next to `let mut no_sandbox = false;`), add:

```rust
    let mut gui = false;
```

- [ ] **Step 2: Parse the flag**

In the argument `match`, next to the `"--no-sandbox"` arm, add:

```rust
            "--gui" => {
                gui = true;
            }
```

- [ ] **Step 3: Add the flag to the usage string**

In the usage `eprintln!` (the one listing `[--no-sandbox]`), add `[--gui]` to the flag list so the help text stays accurate.

- [ ] **Step 4: Invert the boot tail behind `--gui`**

Replace the boot tail — the block that currently reads (around lines 1029–1042):

```rust
    let sb_paths = ignition_sandbox::SandboxPaths {
        readable: [Some(PathBuf::from(&positionals[0])), positionals.get(1).map(PathBuf::from)]
            .into_iter().flatten().collect(),
        writable: [Some(store.clone()), Some(std::env::temp_dir()),
                   vsock_uds.as_ref().and_then(|u| u.parent().map(PathBuf::from))]
            .into_iter().flatten().collect(),
    };
    apply_or_exit(&sb_paths, no_sandbox);

    // Run. Earlycon + virtio MMIO exits are dispatched through the bus.
    match manager.run(entry, fdt_addr) {
        Ok(()) => eprintln!("\n[vcpus exited cleanly]"),
        Err(e) => eprintln!("\n[vcpu error: {e}]"),
    }
}
```

with:

```rust
    let sb_paths = ignition_sandbox::SandboxPaths {
        readable: [Some(PathBuf::from(&positionals[0])), positionals.get(1).map(PathBuf::from)]
            .into_iter().flatten().collect(),
        writable: [Some(store.clone()), Some(std::env::temp_dir()),
                   vsock_uds.as_ref().and_then(|u| u.parent().map(PathBuf::from))]
            .into_iter().flatten().collect(),
    };

    if gui {
        // GUI mode: the winit event loop must own the main thread on macOS, so the
        // VMM (sandbox apply + the vCPU join loop) moves to a spawned thread and the
        // event loop runs on main. `manager` is an Arc; cloning shares the VMM.
        // `_sink` establishes the present seam (a later milestone hands it to the
        // virtio-gpu device); for now no frames flow and the window clears to a color.
        // The `TermiosGuard` (`termios`) stays alive in this scope; when the event
        // loop returns and `main` returns, the guard's Drop restores the terminal and
        // the process exits (killing the VMM thread). Window close → loop exit; VMM
        // done → loop exit.
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (_sink, rx) = display_sink::WindowSink::new();
        let done_vmm = done.clone();
        let mgr = manager.clone();
        std::thread::spawn(move || {
            apply_or_exit(&sb_paths, no_sandbox);
            match mgr.run(entry, fdt_addr) {
                Ok(()) => eprintln!("\n[vcpus exited cleanly]"),
                Err(e) => eprintln!("\n[vcpu error: {e}]"),
            }
            done_vmm.store(true, std::sync::atomic::Ordering::Relaxed);
        });
        display_sink::run_event_loop(rx, done, 1280, 800);
    } else {
        apply_or_exit(&sb_paths, no_sandbox);

        // Run. Earlycon + virtio MMIO exits are dispatched through the bus.
        match manager.run(entry, fdt_addr) {
            Ok(()) => eprintln!("\n[vcpus exited cleanly]"),
            Err(e) => eprintln!("\n[vcpu error: {e}]"),
        }
    }
}
```

- [ ] **Step 5: Build the binary**

Run: `PATH="$HOME/.cargo/bin:$PATH" cargo build -p ignition-spike --bin boot 2>&1 | tail -15`
Expected: builds. (If the compiler flags `termios` as unused in the `gui` branch, it is not — it is dropped at the end of `main` in both branches; do not remove it. If it warns the `_sink` binding is unused, the leading underscore already suppresses that.)

- [ ] **Step 6: Verify non-GUI behavior is unchanged (existing tests + clippy)**

Run: `PATH="$HOME/.cargo/bin:$PATH" cargo test -p ignition-spike --bin boot 2>&1 | tail -15 && PATH="$HOME/.cargo/bin:$PATH" cargo clippy -p ignition-spike --all-targets 2>&1 | tail -10`
Expected: all tests pass; clippy clean. The `--gui`-off path is the same statements as before, just in an `else` block.

- [ ] **Step 7: Re-sign the binary**

Run: `./scripts/sign.sh target/debug/boot 2>&1 | tail -5` (the entitlement requirement is unchanged; adding windowing deps does not alter it — re-sign because the binary was relinked).
Expected: signing succeeds.

- [ ] **Step 8: Commit**

```bash
git add spike/src/bin/boot.rs
git commit -m "feat(spike): --gui flag inverts main thread to winit event loop"
```

---

## Task 4: Document the `--gui` flag

**Files:**
- Modify: `docs/src/features/devices.md` (append a short section)

- [ ] **Step 1: Add a GUI section**

Append to `docs/src/features/devices.md`:

```markdown
## GUI display (software-rendered)

`boot --gui <kernel> <rootfs>` opens a 1280x800 macOS window backed by a CPU
framebuffer (`winit` + `softbuffer`, no Metal). On macOS the windowing event loop
must own the main thread, so under `--gui` the entire VMM — vCPU threads, the serial
console reader, the vsock reactor, and the vmnet RX feeder — runs on spawned threads
while the event loop runs on main. The present path is non-blocking and coalesces to
the latest frame, so a slow or frozen window never backpressures the guest. Closing
the window shuts the guest down; the serial console keeps working alongside the
window.

Without `--gui` (the default), and for `--restore` and `--fuzz`, behavior is
unchanged: no window opens and the vCPU loop runs on the main thread as before.

This is the structural foundation for the 2D GUI bring-up. The `virtio-gpu` device
that actually paints guest pixels into the window is a later milestone; today the
window opens cleared to a solid color.
```

- [ ] **Step 2: Verify the book builds**

Run: `PATH="$HOME/.cargo/bin:$PATH" mdbook build docs 2>&1 | tail -5` (skip if `mdbook` is not installed — then just confirm the Markdown renders by eye).
Expected: build succeeds, no broken-link warnings.

- [ ] **Step 3: Commit**

```bash
git add docs/src/features/devices.md
git commit -m "docs: document the --gui software-display flag"
```

---

## Manual integration verification (after all tasks; needs entitlement + kernel/rootfs)

These are not automated (they need the hypervisor entitlement, a GUI session, and guest assets). Run them by hand and record the outcome in the milestone notes:

1. `target/debug/boot --gui kimage/out/Image kimage/out/rootfs.ext4` — a 1280x800 dark-slate window opens; the guest boots to a shell on the serial console; typing on serial echoes. Confirms vCPUs run off-main and the event loop owns main.
2. **Freeze test:** hold the window (drag/resize-hold, or background it) and confirm the guest keeps making serial progress — proves the present path cannot backpressure vCPUs.
3. **Sandbox coexistence:** run #1 without `--no-sandbox`; the window must open under the default Seatbelt profile. If it is denied, capture the denied operation (Console.app / `sandbox` log) and note whether the windowing path needs an explicit allow (expected: none, since `(allow default)` leaves mach/IOSurface alone).
4. **Regression:** `boot` without `--gui`, `boot --restore <name>`, and `boot --fuzz` open no window and behave exactly as before.

---

## Self-Review Notes

- **Spec coverage:** `DisplaySink`/`NoopSink`/`Frame` in devices (Task 1) ✓; `WindowSink` proxy + coalescing + winit/softbuffer sink in spike (Task 2) ✓; `--gui` gate + main-thread inversion + VMM-thread spawn + shutdown signal (Task 3) ✓; docs (Task 4) ✓; manual window/freeze/sandbox/regression checks ✓. The spec's "trait in devices, real sink in binary," "1280x800 B8G8R8A8," "non-blocking coalescing present," and "all non-GUI paths unchanged" are each realized.
- **Type consistency:** `WindowSink::new() -> (WindowSink, Receiver<Frame>)`, `coalesce(&Receiver<Frame>) -> Option<Frame>`, and `run_event_loop(Receiver<Frame>, Arc<AtomicBool>, u32, u32)` are used identically in Task 2 (definition/tests) and Task 3 (call site). `Frame`/`DirtyRect`/`DisplaySink`/`NoopSink` names match across Tasks 1–2.
- **No placeholders:** every code step shows complete code; the one deferred behavior (blitting real frames) is correctly out of this milestone's scope — clearing the window IS the complete behavior here.
