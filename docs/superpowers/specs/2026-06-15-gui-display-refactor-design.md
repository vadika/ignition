# GUI display refactor — main-thread inversion + DisplaySink seam (M2 structural) — Design

Date: 2026-06-15. Status: approved design, ready for an implementation plan.

This is the first unit of work in the 2D GUI bring-up (`virtio-gpu` + `virtio-input`,
software-rendered, snapshot-safe). The full five-milestone plan lives in
`docs/superpowers/specs/2026-06-15-gui-bringup-plan.md` (adopted from the original
brainstorm). Execution order was reordered from the doc: **this M2 structural refactor
goes first** (de-risk the threading change while the tree is simple), then M1 (the
virtio-gpu device), then M3 input, M4 compositor, M5 snapshot/clone.

## Context

The macOS `winit` event loop **must** run on the main thread. Today `boot`'s `main()`
ends by calling `manager.run(entry, fdt_addr)` on the main thread. That call is *not*
itself the vCPU — `VcpuManager::run` (`crates/vmm/src/vstate/vcpu_manager.rs:194`)
already `thread::spawn`s the primary and secondary vCPUs and then blocks the caller in a
**join loop** (`:577`). The serial console reader is likewise already on its own thread
(`spawn_stdin_reader`), and so are the vsock reactor and vmnet RX feeder.

So the only thing pinning the main thread is the join loop. The refactor: move the
join-blocking `run*` call onto a spawned **VMM thread** and let the `winit` event loop own
the main thread. This is gated behind a new `--gui` flag, so every existing path (boot
without `--gui`, restore, fuzz, CI) keeps its current main-thread-runs-`run*` behavior and
opens no window.

This milestone deliberately splits M2 into its structural half only. **No virtio-gpu
device exists yet** (that is M1), so nothing produces real frames. The window opens and
clears to a solid color; the `DisplaySink` seam is established and wired but idle. M1 plus
the M2 pixel-wiring follow-up will make the GPU device feed real frames through it.

### Decisions locked (from the plan doc + brainstorming)

- **Host windowing stack:** `winit` (window + event loop) + `softbuffer` (CPU framebuffer
  blit). No Metal. Upgrade path to `CAMetalLayer`/`IOSurface` is deferred.
- **`DisplaySink` trait + `NoopSink` live in `crates/devices`** (next to the
  `IrqLine`/`NoopIrq` pattern); the real `winit`/`softbuffer` sink lives in the `spike`
  binary. The device crate gains **no** GUI dependency.
- **Window:** one fixed `1280x800` scanout, `B8G8R8A8` (4 bytes/pixel), cleared to a solid
  color in this milestone.
- **`--gui` gate:** off by default. Off → unchanged behavior, no window, no event loop.
- **Present path is non-blocking and coalescing:** a slow or frozen window must never
  backpressure the vCPUs.

## Goal

`boot --gui <kernel> <rootfs>` opens a 1280x800 macOS window cleared to a solid color and
boots the guest to a shell over the serial console, with the vCPUs and all VMM threads
running off the main thread and the `winit` event loop owning main. Freezing or closing the
window does not stall the guest (closing it triggers a clean shutdown). `boot` without
`--gui`, `--restore`, and `--fuzz` behave exactly as before (no window, `run*` on main).

Non-goals (this milestone): the virtio-gpu device, any real guest→host pixel transfer,
`virtio-input`, a compositor, snapshot of display state. Those are M1/M3/M4/M5.

## Architecture

### New seam — `crates/devices/src/display.rs`

```rust
/// A rectangle of the scanout that changed, in pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirtyRect { pub x: u32, pub y: u32, pub w: u32, pub h: u32 }

/// One presentable frame: a handle to the scanout's host pixel buffer plus the
/// geometry needed to blit it. `pixels` is shared (not copied) so FLUSH hands over
/// a handle, not a memcpy. Format is fixed B8G8R8A8 for v1.
#[derive(Clone)]
pub struct Frame {
    pub scanout_id: u32,
    pub width: u32,
    pub height: u32,
    pub stride: u32,            // bytes per row
    pub dirty: DirtyRect,
    pub pixels: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
}

/// Host display sink. Implementations must be `Send + Sync` and `present` must be
/// non-blocking (drop/coalesce rather than block a vCPU).
pub trait DisplaySink: Send + Sync {
    fn present(&self, frame: Frame);
}

/// Default sink for the VMM core and tests: discards frames.
pub struct NoopSink;
impl DisplaySink for NoopSink {
    fn present(&self, _frame: Frame) {}
}
```

Rationale: mirrors `IrqLine`/`NoopIrq` exactly — a trait the device crate owns, a no-op
default for the manager/tests, and a real implementation supplied by the binary. The
device (M1) will hold a `Box<dyn DisplaySink>`; in `--gui` mode that box is a lightweight
**proxy** (an `mpsc::Sender<Frame>`), never the non-`Send` `winit`/`softbuffer` objects.

### Real sink + event loop — `spike` binary (new module `spike/src/bin/display_sink.rs`)

```rust
/// Send+Sync proxy handed to the VMM/device side. `present` forwards to the UI
/// thread over an mpsc channel and never blocks.
pub struct WindowSink { tx: std::sync::mpsc::Sender<Frame> }
impl DisplaySink for WindowSink {
    fn present(&self, frame: Frame) { let _ = self.tx.send(frame); }
}
```

- `WindowSink` is constructed in `main()` when `--gui` is set, paired with the
  `Receiver<Frame>` that the event loop drains.
- The window + `softbuffer` surface are created and owned **only on the main thread**
  inside the event-loop callback (they are not `Send`). On each redraw the loop drains the
  receiver, **coalesces to the most recent `Frame`** (so a backlog collapses to one blit),
  and either blits that frame's dirty rect or — in this milestone, since no frames arrive —
  clears the surface to the solid color.
- Window-close event → initiate clean shutdown (see Error handling).

### Main-thread inversion in `spike/src/bin/boot.rs`

Today (tail of `main()`): `spawn_stdin_reader(...)`, `apply_or_exit(...)`,
`manager.run(entry, fdt_addr)` — all on main, the last one blocking in the join loop.

After:

- Parse a new `--gui` flag (default `false`).
- **`--gui` off (default):** unchanged. `manager.run(...)` stays on the main thread.
- **`--gui` on:**
  1. Build the `winit` `EventLoop` and the `WindowSink`/`Receiver` pair on the main thread
     **before** spawning the VMM.
  2. Move the existing tail work — `spawn_stdin_reader`, `apply_or_exit` (sandbox), and
     `manager.run(entry, fdt_addr)` — into a spawned **VMM thread**. The sandbox is still
     applied late, just on that thread before `run`; it is process-wide, so the main-thread
     event loop runs under the same profile (the profile already leaves the mach/XPC and
     windowing surface alone — `winit`/CoreAnimation use mach + IOSurface, which `(allow
     default)` permits; verify on the test host, see Testing).
  3. Run the `winit` event loop on the main thread. The loop owns the window, drains the
     present channel, and clears to the solid color.
  4. When the VMM thread's `run` returns (guest exited / vCPU error), it signals the event
     loop to exit; when the window closes, the event loop signals shutdown.

The restore (`run_restore`) and fuzz (`run_fuzz_mode`) paths are **not** wired to `--gui`
in this milestone (they keep running `run_restored` / `run_fuzz` on the main thread, no
window). GUI-on-restore is M5; fuzz never has a window.

## Data flow (this milestone)

```
main thread                         VMM thread                    vCPU threads
-----------                         ----------                    ------------
build EventLoop + (tx,rx)
spawn VMM thread  ───────────────►  spawn_stdin_reader
                                    apply_or_exit (sandbox)
                                    manager.run() ─────────────►  primary + secondaries
run event loop:                        (blocks in join loop)         (HVF guest exec)
  drain rx, coalesce → blit
  (no frames yet → clear color)
  on window close → shutdown
  on VMM-done signal → exit loop  ◄── run() returns, signal
```

Serial console keeps flowing on its own threads (stdin reader + `FlushWriter` stdout)
exactly as today; the window and the serial console coexist.

## Error handling / shutdown

- **Window close:** post a shutdown to the VMM. Simplest correct version that matches the
  existing model: the event loop calls `process::exit(0)` after restoring the terminal
  (reuse the `TermiosGuard`/`restore_termios` path the Ctrl-A x handler already uses), since
  the guest is disposable and the existing Ctrl-A x quit already exits the process. A graceful
  vCPU stop is not required for v1.
- **VMM thread finishes** (guest powered off / vCPU error): it sets a shared `AtomicBool`
  (or sends on a channel) the event loop polls each iteration; the loop then exits and
  `main` returns, matching today's "[vcpus exited cleanly]" / "[vcpu error]" reporting
  (moved to the VMM thread's log).
- **`present` never blocks:** `WindowSink::present` is a non-blocking `Sender::send`; if the
  receiver is somehow gone the frame is dropped. The UI thread coalesces, so a slow blit
  drops intermediate frames rather than stalling producers.
- **Sandbox apply failure** on the VMM thread is still fail-closed (`process::exit(1)`),
  unchanged.

## Testing

Unit (`crates/devices`, no GUI deps, run in CI):
1. **`NoopSink` is a no-op `DisplaySink`** — `present` on a constructed `Frame` returns and
   touches nothing (compile + trait-object construction test; mirrors `NoopIrq` tests).
2. **`Frame`/`DirtyRect` construct and clone** — a `Frame` over an `Arc<Mutex<Vec<u8>>>`
   sized `1280*800*4` clones cheaply (same `Arc`), `DirtyRect` equality holds.

Unit (`spike`, gated to the binary's module):
3. **`WindowSink::present` forwards and never blocks** — construct a `(WindowSink, Receiver)`
   pair, call `present(frame)`, assert the frame arrives on the receiver; call `present`
   after dropping the receiver and assert it returns (no panic, no block).
4. **Coalescing** — send three frames, run the drain-and-coalesce helper, assert it yields
   exactly the last frame. (Factor the coalesce step into a pure function the event loop
   calls, so it is testable without a window.)

Integration / manual (macOS, needs the hypervisor entitlement + kernel/rootfs; documented
in the milestone notes):
- `boot --gui <kernel> <rootfs>`: a 1280x800 window opens cleared to the solid color; the
  guest boots to a shell over the serial console; typing on serial works. Confirms vCPUs run
  off-main and the event loop owns main.
- **Freeze test:** stop draining (or drag/resize-hold the window) and confirm the guest keeps
  making progress on serial — proves the present path cannot backpressure vCPUs.
- **Sandbox coexistence:** the window opens and renders under the default Seatbelt profile
  (no `--no-sandbox`); if it does not, capture the denied operation and record whether the
  windowing path needs an explicit allow (expected: none, since `(allow default)` leaves
  mach/IOSurface alone). Re-sign after relinking (`scripts/sign.sh`).
- `boot` **without** `--gui`, `boot --restore`, and `boot --fuzz` are unchanged (no window).

## File structure

- Create `crates/devices/src/display.rs` — `DirtyRect`, `Frame`, `DisplaySink`, `NoopSink`
  + unit tests. Add `pub mod display;` to `crates/devices/src/lib.rs`.
- Create `spike/src/bin/display_sink.rs` — `WindowSink`, the event-loop runner, the pure
  coalesce helper + unit tests. Declared `mod display_sink;` in `boot.rs`.
- Modify `spike/Cargo.toml` — add `winit` and `softbuffer` dependencies.
- Modify `spike/src/bin/boot.rs` — `--gui` flag; on `--gui`, move
  `spawn_stdin_reader`/`apply_or_exit`/`manager.run` onto a spawned VMM thread and run the
  event loop on main; shared shutdown signal.
- Modify `docs/src/...` (a short `gui` page under SUMMARY, or a section in
  `features/devices.md`) — document the `--gui` flag and the software-display approach,
  noting that pixels are not wired until M1.
- Save the umbrella five-milestone plan to
  `docs/superpowers/specs/2026-06-15-gui-bringup-plan.md` (verbatim from the brainstorm) so
  later milestones reference one source of truth.

## End state

`boot --gui` opens a blank software-rendered window and boots a guest with the `winit`
event loop owning the main thread and the entire VMM (vCPUs, console, reactors) on spawned
threads, under the unchanged Seatbelt profile, with a non-blocking coalescing present seam
ready for the virtio-gpu device. All non-GUI paths are byte-for-byte unchanged. This
de-risks the one structural change in the GUI plan before any device-protocol work begins.
