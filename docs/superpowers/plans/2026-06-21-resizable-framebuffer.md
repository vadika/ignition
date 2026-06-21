# Resizable Framebuffer Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the macOS host window resizable and propagate the new size to the guest so the framebuffer reconfigures to match.

**Architecture:** Reuse the virtio-gpu config-change path. On a debounced window resize the GUI thread sets the gpu device's advertised mode and raises a config-change IRQ; the guest's `virtio_gpu` DRM driver re-queries `GET_DISPLAY_INFO`, cage (wlroots) modesets, firefox reflows. The absolute tablet axis range is decoupled from resolution (fixed `0..32767`) so pointer mapping needs no per-resize update.

**Tech Stack:** Rust, virtio-mmio, winit + softbuffer (host window), Alpine guest with cage/wlroots/firefox-esr.

**Spec:** `docs/superpowers/specs/2026-06-21-resizable-framebuffer-design.md`

**Test commands:**
- Devices crate: `cargo test -p ignition-devices <name>`
- Host binary: `cargo test -p ignition-spike --bin display_sink <name>`

---

## File map

- `crates/devices/src/virtio/mmio.rs` — add `set_display_mode` default to the `VirtioDevice` trait; add `VirtioMmio::display_set_mode` wrapper.
- `crates/devices/src/virtio/gpu.rs` — `events_read` field, `config_read`/`config_write` for the events registers, `set_display_mode` override.
- `crates/devices/src/virtio/input.rs` — fixed `TABLET_ABS_MAX` absinfo range.
- `spike/src/bin/display_sink.rs` — `target_dims` helper; window made resizable; `gpu` handle + debounced resize; pointer scaled to the fixed tablet range.
- `spike/src/bin/boot.rs` — pass the gpu transport handle into `run_event_loop` (both fresh-boot and restore paths); min-size constants.

---

## Task 1: Device-side resize (virtio-gpu events + mode setter)

**Files:**
- Modify: `crates/devices/src/virtio/mmio.rs` (trait, ~line 26-41)
- Modify: `crates/devices/src/virtio/gpu.rs` (const block ~16-38; struct ~53-58; `new` ~62-64; `config_read` ~449-459; add `config_write` + `set_display_mode`)
- Test: `crates/devices/src/virtio/gpu.rs` (`#[cfg(test)] mod tests`)

- [ ] **Step 1: Add the trait method default**

In `crates/devices/src/virtio/mmio.rs`, inside `pub trait VirtioDevice`, next to the existing `config_write` default (line 38), add:

```rust
    /// Push a new display mode (width/height) to a display device. Returns true if
    /// this device is a display and handled it. Default: not a display.
    fn set_display_mode(&mut self, _w: u32, _h: u32) -> bool {
        false
    }
```

- [ ] **Step 2: Write the failing gpu tests**

In `crates/devices/src/virtio/gpu.rs`, inside `mod tests`, add:

```rust
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
    fn config_write_events_clear_clears_event() {
        let mut gpu = new_gpu();
        gpu.set_display_mode(800, 600);
        // Guest acks by writing the bit to events_clear (config offset 4).
        gpu.config_write(4, &VIRTIO_GPU_EVENT_DISPLAY.to_le_bytes());
        let mut cfg = [0u8; 4];
        gpu.config_read(0, &mut cfg);
        assert_eq!(u32::from_le_bytes(cfg), 0);
    }
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p ignition-devices set_display_mode_updates_dims_and_raises_event config_write_events_clear_clears_event`
Expected: FAIL — `set_display_mode` resolves to the trait default (returns false / no event), `config_write` is a no-op, `VIRTIO_GPU_EVENT_DISPLAY` is undefined (compile error).

- [ ] **Step 4: Implement the device changes**

In `crates/devices/src/virtio/gpu.rs`:

Add the event constant near the other `const`s (after line ~31):

```rust
/// config events_read bit: scanout topology / mode changed; guest re-queries
/// GET_DISPLAY_INFO. Matches VIRTIO_GPU_EVENT_DISPLAY.
const VIRTIO_GPU_EVENT_DISPLAY: u32 = 0x0001;
```

Add a field to `struct VirtioGpu`:

```rust
    events_read: u32,
```

Initialise it in `new`:

```rust
    pub fn new(width: u32, height: u32, sink: Box<dyn DisplaySink>) -> Self {
        VirtioGpu { width, height, resources: HashMap::new(), scanout_res: 0, sink, events_read: 0 }
    }
```

Replace `config_read` so offset 0 serves `events_read`:

```rust
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
```

Add `config_write` and `set_display_mode` to `impl VirtioDevice for VirtioGpu` (next to `config_read`):

```rust
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
        self.events_read |= VIRTIO_GPU_EVENT_DISPLAY;
        true
    }
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p ignition-devices set_display_mode_updates_dims_and_raises_event config_write_events_clear_clears_event`
Expected: PASS (2 passed)

- [ ] **Step 6: Run the whole gpu/devices suite for regressions**

Run: `cargo test -p ignition-devices`
Expected: PASS (no regressions)

- [ ] **Step 7: Commit**

```bash
git add crates/devices/src/virtio/gpu.rs crates/devices/src/virtio/mmio.rs
git commit -m "virtio-gpu: events_read + set_display_mode for runtime modeset"
```

---

## Task 2: Transport wrapper (display_set_mode raises config-change IRQ)

**Files:**
- Modify: `crates/devices/src/virtio/mmio.rs` (near `net_set_link`, ~line 363)
- Test: `crates/devices/src/virtio/mmio.rs` (`mod tests`)

- [ ] **Step 1: Write the failing test**

In `crates/devices/src/virtio/mmio.rs`, inside `mod tests`, add:

```rust
    #[test]
    fn display_set_mode_signals_config_change() {
        use crate::display::NoopSink;
        use crate::virtio::gpu::VirtioGpu;

        #[derive(Default)]
        struct RecIrq { level: Mutex<Option<bool>> }
        impl crate::virtio::IrqLine for RecIrq {
            fn set_spi(&self, level: bool) { *self.level.lock().unwrap() = Some(level); }
        }

        let backing = Box::leak(vec![0u8; 0x1000].into_boxed_slice());
        let mem = GuestRam::new(backing.as_mut_ptr(), backing.len(), 0x4000_0000);
        let irq = Arc::new(RecIrq::default());
        let mut t = VirtioMmio::new(
            "virtio-gpu",
            Box::new(VirtioGpu::new(1280, 800, Box::new(NoopSink))),
            mem,
            irq.clone(),
        );

        t.display_set_mode(1024, 768);

        let mut b = [0u8; 4];
        t.read(0, 0x060, &mut b); // InterruptStatus
        assert_eq!(u32::from_le_bytes(b) & 0b10, 0b10, "config-change bit set");
        assert_eq!(*irq.level.lock().unwrap(), Some(true), "irq asserted");
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p ignition-devices display_set_mode_signals_config_change`
Expected: FAIL — `display_set_mode` does not exist (compile error).

- [ ] **Step 3: Implement the wrapper**

In `crates/devices/src/virtio/mmio.rs`, next to `net_set_link` (line ~363), add:

```rust
    /// Push a new display mode to a display device and raise a config-change
    /// interrupt so the guest re-reads config events_read and re-queries
    /// GET_DISPLAY_INFO. No-op for non-display devices.
    pub fn display_set_mode(&mut self, w: u32, h: u32) {
        if self.dev.set_display_mode(w, h) {
            self.signal_config_change();
        }
    }
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p ignition-devices display_set_mode_signals_config_change`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/devices/src/virtio/mmio.rs
git commit -m "virtio-mmio: display_set_mode wrapper raises config-change IRQ"
```

---

## Task 3: Decouple tablet absolute range from resolution

**Files:**
- Modify: `crates/devices/src/virtio/input.rs` (const block ~22-27; `CFG_ABS_INFO` arm ~140-156; optionally `Flavor::Tablet` fields)
- Test: `crates/devices/src/virtio/input.rs` (`mod tests`)

- [ ] **Step 1: Write the failing test**

In `crates/devices/src/virtio/input.rs`, inside `mod tests`, add:

```rust
    #[test]
    fn tablet_abs_range_is_resolution_independent() {
        // Range is the fixed normalized max regardless of the constructor dims.
        let tab = VirtioInput::tablet(1400, 880);
        let (size_x, ux) = read_cfg(&tab, CFG_ABS_INFO, ABS_X as u8);
        let (size_y, uy) = read_cfg(&tab, CFG_ABS_INFO, ABS_Y as u8);
        assert_eq!(size_x, 20);
        assert_eq!(size_y, 20);
        assert_eq!(u32::from_le_bytes(ux[4..8].try_into().unwrap()), TABLET_ABS_MAX);
        assert_eq!(u32::from_le_bytes(uy[4..8].try_into().unwrap()), TABLET_ABS_MAX);
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p ignition-devices tablet_abs_range_is_resolution_independent`
Expected: FAIL — `TABLET_ABS_MAX` undefined (compile error); current max is `w-1`/`h-1`.

- [ ] **Step 3: Implement the fixed range**

In `crates/devices/src/virtio/input.rs`, add a public constant near the other `const`s (after line ~27):

```rust
/// Absolute tablet axis range max (QEMU virtio-tablet convention). libinput maps
/// this fixed range onto the current guest output extent, so the pointer stays
/// correct at any resolution — absinfo is probed once at boot and never changes.
pub const TABLET_ABS_MAX: u32 = 32767;
```

In the `CFG_ABS_INFO` match arm, change the per-axis max from resolution-derived to the constant. Replace:

```rust
            CFG_ABS_INFO => match &self.flavor {
                Flavor::Tablet { w, h } => {
                    // Only the advertised axes (ABS_X/ABS_Y) have absinfo; any other
                    // axis returns size 0 so the guest doesn't see a phantom [0,0] axis.
                    let max = match self.subsel as u16 {
                        ABS_X => Some(w.saturating_sub(1)),
                        ABS_Y => Some(h.saturating_sub(1)),
                        _ => None,
                    };
```

with:

```rust
            CFG_ABS_INFO => match &self.flavor {
                Flavor::Tablet { .. } => {
                    // Only the advertised axes (ABS_X/ABS_Y) have absinfo; any other
                    // axis returns size 0 so the guest doesn't see a phantom [0,0] axis.
                    // Fixed normalized range, resolution-independent (see TABLET_ABS_MAX).
                    let max = match self.subsel as u16 {
                        ABS_X | ABS_Y => Some(TABLET_ABS_MAX),
                        _ => None,
                    };
```

The `Flavor::Tablet { w, h }` fields are now unused. To avoid a dead-field warning, simplify the variant: change its definition from `Tablet { w: u32, h: u32 }` to a unit-like `Tablet`, update the constructor `VirtioInput::tablet` to build `Flavor::Tablet` (prefix the now-unused params: `pub fn tablet(_width: u32, _height: u32) -> Self`), and change every other `Flavor::Tablet { .. }` match arm to `Flavor::Tablet`.

`ponytail:` keep the `tablet(_width, _height)` signature so `boot.rs` call sites don't churn; the dims no longer affect the device.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p ignition-devices tablet_abs_range_is_resolution_independent`
Expected: PASS

- [ ] **Step 5: Update the stale absinfo-range test**

The existing `config_abs_info_ranges` test (around line 368) asserts the old `w-1`/`h-1` maxima. Update its expected values to `TABLET_ABS_MAX` for both axes (same assertion shape as Step 1's test).

- [ ] **Step 6: Run the input suite**

Run: `cargo test -p ignition-devices input`
Expected: PASS (no regressions)

- [ ] **Step 7: Commit**

```bash
git add crates/devices/src/virtio/input.rs
git commit -m "virtio-input: fixed tablet ABS range (resolution-independent pointer)"
```

---

## Task 4: Window-size → guest-dims mapping (pure helper)

**Files:**
- Modify: `spike/src/bin/display_sink.rs` (add `target_dims` near `scale_pos`, ~line 163-180)
- Test: `spike/src/bin/display_sink.rs` (`#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing test**

In `spike/src/bin/display_sink.rs`, inside the test module (near the existing `scale_pos` tests ~line 672), add:

```rust
    #[test]
    fn target_dims_clamps_rounds_and_descales() {
        // within bounds, odd physical -> even guest logical
        assert_eq!(target_dims(1401, 881, 1.0, (320, 240), (2000, 2000)), (1400, 880));
        // below min clamps up
        assert_eq!(target_dims(100, 100, 1.0, (320, 240), (2000, 2000)), (320, 240));
        // above max clamps down
        assert_eq!(target_dims(5000, 5000, 1.0, (320, 240), (2000, 2000)), (2000, 2000));
        // HiDPI scale 2.0 -> logical halves (physical 2800x1760 -> 1400x880)
        assert_eq!(target_dims(2800, 1760, 2.0, (320, 240), (2000, 2000)), (1400, 880));
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p ignition-spike --bin display_sink target_dims_clamps_rounds_and_descales`
Expected: FAIL — `target_dims` not defined (compile error).

- [ ] **Step 3: Implement the helper**

In `spike/src/bin/display_sink.rs`, add next to `scale_pos`:

```rust
/// Map a physical window size to clamped, even-rounded guest *logical* dimensions.
/// Logical (physical / `scale`) so a HiDPI window does not double the guest
/// resolution. `min`/`max` are (w, h) bounds; even-rounded since some modesetting
/// paths dislike odd widths (B8G8R8A8 stride is 4-aligned regardless).
pub fn target_dims(
    phys_w: u32,
    phys_h: u32,
    scale: f64,
    min: (u32, u32),
    max: (u32, u32),
) -> (u32, u32) {
    let logical = |p: u32| (p as f64 / scale.max(0.1)).round() as u32;
    let clamp = |v: u32, lo: u32, hi: u32| v.max(lo).min(hi);
    let even = |v: u32| v & !1;
    (
        even(clamp(logical(phys_w), min.0, max.0)),
        even(clamp(logical(phys_h), min.1, max.1)),
    )
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p ignition-spike --bin display_sink target_dims_clamps_rounds_and_descales`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add spike/src/bin/display_sink.rs
git commit -m "display_sink: target_dims helper (window size -> clamped guest dims)"
```

---

## Task 5: Wire resize into the event loop and boot

This task is integration glue around the units tested in Tasks 1-4 (no new pure logic). Verification is a manual GUI run; `target_dims`/`display_set_mode` carry the unit coverage.

**Files:**
- Modify: `spike/src/bin/display_sink.rs` (`App` struct fields; `run_event_loop` signature + `App` init; window builder ~423; `Resized` handler ~448; `about_to_wait` ~548; `CursorMoved` scale_pos call ~494; remove `gw`/`gh`)
- Modify: `spike/src/bin/boot.rs` (min-size consts ~59; both `run_event_loop` call sites ~1450, ~2445)

- [ ] **Step 1: Add the gpu handle + debounce state to `App` and `run_event_loop`**

In `spike/src/bin/display_sink.rs`:

Add to `struct App` (next to the existing `tablet` field, ~line 272):

```rust
    gpu: Option<std::sync::Arc<std::sync::Mutex<ignition_devices::virtio::mmio::VirtioMmio>>>,
    pending_resize: Option<(u32, u32)>,
    last_resize: Option<std::time::Instant>,
    min_dims: (u32, u32),
    max_dims: (u32, u32),
```

Add parameters to `run_event_loop` (after `tablet`, before `gw`); then **remove** the now-dead `gw`/`gh` params (the pointer no longer needs guest resolution — see Step 4). New signature:

```rust
#[allow(clippy::too_many_arguments)]
pub fn run_event_loop(
    rx: Receiver<Frame>,
    done: Arc<AtomicBool>,
    width: u32,
    height: u32,
    keyboard: Option<std::sync::Arc<std::sync::Mutex<ignition_devices::virtio::mmio::VirtioMmio>>>,
    tablet: Option<std::sync::Arc<std::sync::Mutex<ignition_devices::virtio::mmio::VirtioMmio>>>,
    gpu: Option<std::sync::Arc<std::sync::Mutex<ignition_devices::virtio::mmio::VirtioMmio>>>,
    min_dims: (u32, u32),
    max_dims: (u32, u32),
    manager: Option<std::sync::Arc<ignition_vmm::vstate::vcpu_manager::VcpuManager>>,
    visible: bool,
) {
```

In the `App { ... }` initializer inside `run_event_loop`, remove `gw,` and `gh,` and add:

```rust
        gpu,
        pending_resize: None,
        last_resize: None,
        min_dims,
        max_dims,
```

- [ ] **Step 2: Make the window resizable**

In `resumed` (line ~423), change:

```rust
            .with_resizable(false);
```

to:

```rust
            .with_resizable(true);
```

- [ ] **Step 3: Record a pending resize on `Resized`**

Replace the `WindowEvent::Resized(_)` arm (line ~448) with:

```rust
            WindowEvent::Resized(size) => {
                self.resize_surface();
                self.force_paint = true;
                // Debounce: remember the target guest dims and when the drag last
                // moved; about_to_wait pushes the modeset once it settles.
                if self.gpu.is_some() {
                    let scale = self.window.as_ref().map(|w| w.scale_factor()).unwrap_or(1.0);
                    self.pending_resize = Some(target_dims(
                        size.width.max(1),
                        size.height.max(1),
                        scale,
                        self.min_dims,
                        self.max_dims,
                    ));
                    self.last_resize = Some(std::time::Instant::now());
                }
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
```

- [ ] **Step 4: Scale the pointer into the fixed tablet range**

In the `CursorMoved` arm (line ~494), replace:

```rust
                    let (x, y) = scale_pos(position.x, position.y, self.surf_w, self.surf_h, self.gw, self.gh);
```

with:

```rust
                    use ignition_devices::virtio::input::TABLET_ABS_MAX;
                    let (x, y) = scale_pos(
                        position.x,
                        position.y,
                        self.surf_w,
                        self.surf_h,
                        TABLET_ABS_MAX + 1,
                        TABLET_ABS_MAX + 1,
                    );
```

(`scale_pos` is unchanged — it maps the surface onto `0..(arg-1)`, i.e. `0..TABLET_ABS_MAX`. Its existing tests stay valid.)

Then delete the `gw: u32,` and `gh: u32,` fields from `struct App` (they are now unreferenced).

- [ ] **Step 5: Flush the debounced resize in `about_to_wait`**

The event loop already wakes on a ~16ms `WaitUntil` cadence (line ~555), so just check the debounce there. At the top of `about_to_wait` (line ~548), before the existing control-flow set, add:

```rust
        // Debounce window resizes: push one modeset once the drag has settled.
        const RESIZE_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(150);
        if let (Some(dims), Some(t)) = (self.pending_resize, self.last_resize) {
            if t.elapsed() >= RESIZE_DEBOUNCE {
                if let Some(gpu) = &self.gpu {
                    gpu.lock().unwrap_or_else(|p| p.into_inner()).display_set_mode(dims.0, dims.1);
                }
                self.pending_resize = None;
                self.last_resize = None;
            }
        }
```

- [ ] **Step 6: Add min-size constants and update boot call sites**

In `spike/src/bin/boot.rs`, near `GUI_W`/`GUI_H` (line ~59) add:

```rust
const MIN_W: u32 = 320;
const MIN_H: u32 = 240;
```

At the fresh-boot `run_event_loop` call (line ~1450), pass the gpu handle and bounds, and drop the trailing `GUI_W, GUI_H` (former `gw`/`gh`). The `gpu_handle` already exists (`let gpu_handle = ctx.gpu_mmio.clone();`, line ~1215). Result:

```rust
        display_sink::run_event_loop(
            rx,
            done,
            GUI_W,
            GUI_H,
            kbd_handle,
            tab_handle,
            gpu_handle,
            (MIN_W, MIN_H),
            (GUI_W, GUI_H),
            Some(manager.clone()),
            !gui_hidden,
        );
```

`ponytail:` max bound = base `(GUI_W, GUI_H)` for now — the base image is built at that size and the host window already fits it to the work area; raising the ceiling is a one-line change when a larger base lands.

At the restore-path `run_event_loop` call (line ~2445), make the identical change. The restore path already has `let gpu_handle = ctx.gpu_mmio.clone();` (line ~2135) — note it is also `.clone()`d for `present_scanout` at line ~2360/2427, so pass `gpu_handle.clone()` if the borrow checker requires it.

- [ ] **Step 7: Build the workspace**

Run: `cargo build -p ignition-spike --bin boot --bin display_sink`
Expected: builds clean — no unused `gw`/`gh`, no missing args.

- [ ] **Step 8: Run the host-binary tests**

Run: `cargo test -p ignition-spike --bin display_sink`
Expected: PASS (scale_pos + target_dims tests green)

- [ ] **Step 9: Re-sign and manual GUI verification**

The base browser rootfs must be rebuilt once for the new tablet absinfo range (probed at boot; see [[virtio-input-evbits-probe-at-boot]]). After rebuilding the base and re-signing the boot binary (relink strips the signature; see [[resign-boot-after-cargo-build]]):

1. Launch a `--gui` browser session.
2. Confirm cursor tracking is correct at the initial size (validates the fixed tablet range).
3. Drag a window corner to a new size; release. Within ~150ms the guest should re-modeset: cage output resizes, firefox reflows, frames fill the new window sharply (soft only during the drag).
4. Confirm cursor tracking is still correct at the new size.

If the guest does not reflow: check `dmesg` in the guest for a `virtio_gpu` hotplug uevent and verify cage is not pinned to a fixed output mode (spec "Constraints / risks").

- [ ] **Step 10: Commit**

```bash
git add spike/src/bin/display_sink.rs spike/src/bin/boot.rs
git commit -m "display_sink: resizable window -> debounced guest modeset"
```

---

## Self-review notes (coverage)

- Spec "device-side resize" → Task 1. "transport wrapper / config-change IRQ" → Task 2. "input decoupling (fixed 0..32767)" → Task 3. "host pointer" → Task 5 Step 4 (uses Task 3's `TABLET_ABS_MAX`). "host window + debounce" → Tasks 4 + 5. "wiring boot.rs" → Task 5 Step 6. "snapshot/restore at base dims, no format change" → satisfied by not touching `gpu.save()`/`restore()` (width/height reconstructed from `VirtioGpu::new`); restore call site updated in Step 6.
- Method/type names consistent across tasks: `set_display_mode` (trait + gpu override + transport), `display_set_mode` (transport public), `TABLET_ABS_MAX` (input → display_sink), `target_dims` (display_sink).
- No placeholders; every code step shows full code.
