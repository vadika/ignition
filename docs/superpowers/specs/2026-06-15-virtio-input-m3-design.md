# virtio-input (M3) — keyboard + absolute tablet for the GUI — Design

Date: 2026-06-15. Status: approved design, ready for an implementation plan.

Third milestone of the 2D GUI bring-up (umbrella plan:
`docs/superpowers/specs/2026-06-15-gui-bringup-plan.md`). M2 (the `winit`+`softbuffer`
event loop owning the main thread, `DisplaySink` seam) and M1 (virtio-gpu 2D, fbcon renders
in the `--gui` window) are shipped and validated live. This milestone makes the GUI
**interactive**: host keyboard and pointer events from the `winit` window are injected into
the guest as `virtio-input` events.

The guest kernel already has `CONFIG_VIRTIO_INPUT=y` (added in the M1 kernel rebuild), so no
remote kernel rebuild is required.

## Context & decisions locked (brainstorming)

- **Two devices** (not one combined): a **keyboard** (`EV_KEY`) and an **absolute tablet**
  (`EV_ABS` x/y + `BTN_LEFT/RIGHT/MIDDLE`). Matches QEMU virtio-keyboard + virtio-tablet and
  how libinput/Wayland expect to classify devices — important for the M4 compositor.
- **Absolute pointer** (tablet), not relative: the cursor tracks the macOS pointer 1:1. The
  `ABS_X`/`ABS_Y` ranges are `0..=(width-1)` / `0..=(height-1)` of the scanout (1279 / 799),
  so the guest maps directly to the 1280×800 framebuffer.
- **Device id 18**, `queue_count = 2` (eventq = 0, statusq = 1), `device_features = 0`.
- **Injection mirrors `inject_rx`**: the `winit` event loop (main thread) holds an
  `Arc<Mutex<VirtioMmio>>` per device and calls an inject method that fills the eventq and
  pulses the IRQ. No new threading model.
- **`--gui` gates the devices** (boot mode only). Snapshot of input state is **M5** (`save`
  returns `Null`), consistent with virtio-gpu.
- **Keycode map**: a static `winit::keyboard::KeyCode` → Linux evdev-code table covering a
  practical subset (letters, digits, Enter/Space/Tab/Backspace/Escape, arrows, modifiers,
  common punctuation) — enough to log in and drive a shell. Unmapped keys are dropped.
  Extensible by adding rows.

## Goal

Under `boot --gui`, typing in the macOS window reaches the guest (e.g. log in at the fbcon
console), and moving/clicking the pointer drives an absolute guest pointer that tracks the
macOS cursor 1:1 over the 1280×800 scanout. Two `/dev/input/eventN` nodes appear (keyboard +
tablet). All device-protocol and translation logic is unit-tested on the host; the live
type/point check is the milestone's manual acceptance. Non-GUI / restore / fuzz are unchanged.

Non-goals (M3): relative pointer / scroll wheel / multitouch; key-repeat handling (the guest
repeats); LED/status feedback (statusq is parsed and ack'd, contents ignored); snapshot of
input state (M5); a full keycode table (a documented subset ships, extensible).

## Architecture — new module `crates/devices/src/virtio/input.rs`

```rust
/// One virtio-input event on the wire (no timestamp): 8 bytes, little-endian.
#[derive(Clone, Copy)]
pub struct InputEvent {
    pub etype: u16, // EV_KEY=1, EV_ABS=3, EV_SYN=0
    pub code: u16,
    pub value: u32,
}

/// Which evdev capabilities this instance advertises.
enum Flavor {
    Keyboard,             // EV_KEY over a key-code set
    Tablet { w: u32, h: u32 }, // EV_KEY (buttons) + EV_ABS (x:0..w-1, y:0..h-1)
}

pub struct VirtioInput {
    flavor: Flavor,
    select: u8,    // VIRTIO_INPUT_CFG_* (set by config_write)
    subsel: u8,    // sub-selector (set by config_write)
}

impl VirtioInput {
    pub fn keyboard() -> Self;
    pub fn tablet(width: u32, height: u32) -> Self;
}
```

`VirtioInput` implements `VirtioDevice`: `device_id() = 18`, `queue_count() = 2`,
`device_features(_) = 0`.

### Config space — select/subsel protocol

virtio-input config is a 136-byte window: `select:u8 @0`, `subsel:u8 @1`, `size:u8 @2`,
`reserved[5] @3`, `union u[128] @8`. The guest writes `select`/`subsel`, then reads `size` +
`u`. `config_write` stores `select`/`subsel`; `config_read(offset, data)` builds the current
136-byte image from `(flavor, select, subsel)` and serves the requested slice (any width,
mirroring the other devices).

Selectors handled (others → `size = 0`, zeroed union):
- `VIRTIO_INPUT_CFG_ID_NAME` (0x01): `u` = ASCII name ("ignition-keyboard" / "ignition-tablet"),
  `size` = name length.
- `VIRTIO_INPUT_CFG_EV_BITS` (0x11):
  - `subsel = EV_KEY` (0x01): `u` = a key bitmap with a bit set per emitted code; `size` =
    bitmap byte length (enough bytes to cover the highest code). Keyboard: the mapped
    evdev key codes. Tablet: `BTN_LEFT`/`RIGHT`/`MIDDLE` (0x110–0x112).
  - `subsel = EV_ABS` (0x03), tablet only: `u` = ABS bitmap with `ABS_X`(0) and `ABS_Y`(1)
    set; `size` = 1.
  - keyboard `subsel = EV_ABS` → `size = 0` (no abs); tablet still reports EV_KEY bitmap for
    its buttons.
- `VIRTIO_INPUT_CFG_ABS_INFO` (0x12), tablet only, `subsel = axis`: `u` =
  `virtio_input_absinfo { min:u32=0, max:u32=(w-1 or h-1), fuzz:0, flat:0, res:0 }` (20 bytes),
  `size = 20`. `ABS_X`(subsel 0) → max = w-1; `ABS_Y`(subsel 1) → max = h-1.

A `bitmap_set(code)` helper sets bit `code` in a little-endian byte array. Getting the
EV_BITS and ABS_INFO right is what lets the guest bind the device and place the pointer
correctly (a wrong ABS max clamps the pointer to a corner).

### eventq (queue 0) — device → guest

The guest posts device-writable buffers (typically 8 bytes each) and kicks. On a notify the
device records nothing special (buffers are consumed lazily on inject). When host input
arrives, `inject(events)` writes one `virtio_input_event` (8 LE bytes) per available buffer,
`push_used(head, 8)`, and after the batch pulses the IRQ once. If the guest has posted no
buffers, events for that batch are dropped (consistent with how an over-fast producer behaves;
fbcon/login typing is far slower than buffer replenishment).

```rust
// On VirtioInput (called via VirtioMmio::inject_input):
fn fill_events(&mut self, eventq: &mut Virtqueue, mem: &GuestRam, events: &[InputEvent]) -> bool {
    let mut any = false;
    for ev in events {
        let Some(chain) = eventq.pop_avail(mem) else { break };
        // write 8 bytes (etype, code, value) into the first writable descriptor
        // push_used(chain.head, 8); any = true;
    }
    any // caller pulses IRQ if true
}
```

### statusq (queue 1) — guest → device

Parse the chain, ignore the payload, `push_used(head, 0)`. (Mirrors the virtio-gpu cursorq.)

### Injection entry point — `crates/devices/src/virtio/mmio.rs`

Mirror `inject_rx` (mmio.rs:256). Add to the `VirtioDevice` trait a defaulted
`fn inject_input(&mut self, _eventq, _mem, _events) -> bool { false }`, and on `VirtioMmio` a
`pub fn inject_input(&mut self, events: &[InputEvent]) -> bool` that locates queue 0, calls the
device, and pulses the IRQ on `true` — exactly as `inject_rx` does for the RX queue. (virtio-net
uses queue index conventions; eventq is queue 0 for virtio-input.)

## Host-side translation — `spike/src/bin/display_sink.rs`

The event loop already runs on the main thread (M2). Add to `App`:
- `keyboard: Option<Arc<Mutex<VirtioMmio>>>`, `tablet: Option<Arc<Mutex<VirtioMmio>>>` (the
  device handles), plus the guest resolution (`gw`, `gh` = 1280, 800) for pointer scaling.
- `WindowEvent::KeyboardInput { event, .. }`: map `event.physical_key` (a
  `winit::keyboard::PhysicalKey::Code(KeyCode)`) through the keycode table; on a hit, build
  `[InputEvent{EV_KEY, code, value: pressed?1:0}, InputEvent{EV_SYN, SYN_REPORT, 0}]` and
  `keyboard.lock().inject_input(&evs)`. Unmapped keys dropped.
- `WindowEvent::CursorMoved { position, .. }`: `position` is physical pixels in the window;
  scale to the guest axis: `x = position.x * (gw-1) / max(surf_w-1,1)` (clamped to
  `0..=gw-1`), likewise y. Emit `[EV_ABS ABS_X x, EV_ABS ABS_Y y, EV_SYN SYN_REPORT 0]` to the
  tablet.
- `WindowEvent::MouseInput { state, button, .. }`: map `Left/Right/Middle` →
  `BTN_LEFT/RIGHT/MIDDLE`, emit `[EV_KEY btn value, EV_SYN]` to the tablet.

A `keycode.rs` (or inline `fn map_keycode(KeyCode) -> Option<u16>`) holds the static table:

```
KeyA..KeyZ -> KEY_A(30).. (evdev letter codes)
Digit1..Digit0 -> KEY_1(2)..KEY_0(11)
Enter->28 Space->57 Backspace->14 Tab->15 Escape->1
ArrowUp/Down/Left/Right->103/108/105/106
ShiftLeft->42 ShiftRight->54 ControlLeft->29 ControlRight->97
AltLeft->56 AltRight->100 SuperLeft->125 SuperRight->126
Minus->12 Equal->13 BracketLeft->26 BracketRight->27 Backslash->43
Semicolon->39 Quote->40 Backquote->41 Comma->51 Period->52 Slash->53
CapsLock->58
```

(The exact rows are enumerated in the implementation plan; this list is the committed subset.)

## Integration — `spike/src/bin/boot.rs`

- Under `--gui` + `Mode::Boot`, after the virtio-gpu registration, register two input devices:
  `place(... "virtio-keyboard" ... VirtioInput::keyboard())` and `place(... "virtio-tablet" ...
  VirtioInput::tablet(1280, 800))`. Keep both `Arc<Mutex<VirtioMmio>>` handles.
- Pass the two handles (and the guest resolution) into `run_event_loop` so the event loop can
  inject. Threading: the handles are shared with the VMM; the event loop briefly locks each on
  input (mirrors how the snapshot handler and net feeder share device locks).
- Restore / non-gui: no input devices (GUI-restore is M5).
- The MMIO ids "virtio-keyboard"/"virtio-tablet" are distinct so the device-record enumeration
  and FDT nodes stay unambiguous.

## Error handling

- Config reads for unknown selectors/subsels → `size = 0`, zeroed union (never panic).
- `config_write` with a short/odd slice updates only the bytes present (`select`@0, `subsel`@1).
- Injection when no guest buffer is available → drop the event batch (no block, no panic).
- Unmapped winit keys → dropped silently.
- Pointer scaling clamps to the axis range (no out-of-range ABS values).
- All inject/config paths are panic-free on any host or guest input.

## Testing

Unit (`crates/devices`, crafted chains over `GuestRam`+`Virtqueue`, no kernel; mirror rng/gpu):
1. **Identity** — keyboard and tablet both report `device_id()=18`, `queue_count()=2`,
   `device_features(_)=0`.
2. **Config ID_NAME** — select=ID_NAME returns the device name and the right size.
3. **Config EV_BITS** — keyboard: EV_KEY bitmap has bits for mapped keys (e.g. KEY_A=30);
   tablet: EV_KEY has BTN_LEFT (0x110), EV_ABS has ABS_X(0)/ABS_Y(1).
4. **Config ABS_INFO** — tablet ABS_X max = 1279, ABS_Y max = 799; keyboard ABS_INFO size 0.
5. **Inject writes evdev triples** — post N writable buffers on eventq, `inject_input` a batch
   (`EV_KEY KEY_A 1`, `EV_SYN`), assert the 8-byte records land in the buffers and N used
   entries are published.
6. **Inject with no buffers** — `inject_input` returns false, no panic, nothing used.
7. **statusq ack** — a status buffer is used with zero length.

Unit (`spike`, pure): **keycode map** — `map_keycode(KeyCode::KeyA) == Some(30)`,
`Enter == Some(28)`, an unmapped key → `None`; **pointer scale** — a position at the surface's
far corner maps to `(gw-1, gh-1)`, origin maps to `(0,0)` (factor the scale into a pure
`fn scale_pos(px, py, surf_w, surf_h, gw, gh) -> (u32, u32)`).

Integration / manual (macOS, entitlement + the GUI kernel; the milestone's acceptance):
- `boot --gui`: `/dev/input/event*` appear; `dmesg` shows the two virtio-input devices; typing
  in the window logs in at the fbcon console; the pointer tracks the macOS cursor 1:1; clicks
  register.

## File structure

- Create `crates/devices/src/virtio/input.rs` — `VirtioInput`, `Flavor`, `InputEvent`, config
  protocol, eventq fill, statusq ack, unit tests.
- Modify `crates/devices/src/virtio/mod.rs` — `pub mod input;`.
- Modify `crates/devices/src/virtio/mmio.rs` — `InputEvent` re-export or shared type location;
  `inject_input` trait method (defaulted) + `VirtioMmio::inject_input` delegation + IRQ pulse.
- Modify `spike/src/bin/display_sink.rs` — keyboard/tablet handles + guest res on `App`;
  translate `KeyboardInput`/`CursorMoved`/`MouseInput`; the keycode map + `scale_pos` helper
  (pure, tested).
- Modify `spike/src/bin/boot.rs` — register the two devices under `--gui` (boot), thread the
  handles + resolution into `run_event_loop`.
- Modify `docs/src/features/devices.md` (virtio-input section), `ROADMAP.md` (M3 line).

## End state

`boot --gui` is interactive: keyboard and an absolute pointer drive the guest through two
virtio-input devices, events injected from the `winit` event loop via the `inject_rx`-style
path. Host unit tests cover the config protocol, event injection, the keycode map, and pointer
scaling. This is the last piece before a real compositor (M4) can run a clickable app, and
before snapshot-of-GUI (M5).
