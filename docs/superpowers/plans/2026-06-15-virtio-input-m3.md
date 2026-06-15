# virtio-input (M3) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `boot --gui` interactive — inject host keyboard and absolute-pointer events from the winit window into the guest via two virtio-input devices (keyboard + tablet).

**Architecture:** A `VirtioInput` device (`crates/devices/src/virtio/input.rs`) implements `VirtioDevice` (id 18, eventq=0 + statusq=1) in a keyboard or tablet flavor, with the virtio-input select/subsel config protocol. Events reach the guest via `VirtioMmio::inject_input` (a mirror of `inject_rx`): the winit event loop on the main thread holds an `Arc<Mutex<VirtioMmio>>` per device and writes 8-byte `virtio_input_event` records into the eventq. The event loop translates winit `KeyboardInput`/`CursorMoved`/`MouseInput` into evdev triples (a static keycode map + a pure pointer-scale helper).

**Tech Stack:** Rust (edition 2024), `ignition-devices` (`VirtioDevice`, `Virtqueue`, `GuestRam`), `winit` 0.30 input events in the spike binary.

---

## File Structure

- `crates/devices/src/virtio/input.rs` — **create.** `VirtioInput`, `Flavor`, `InputEvent`, config select/subsel protocol, eventq fill, statusq ack, unit tests.
- `crates/devices/src/virtio/mod.rs` — **modify.** `pub mod input;`.
- `crates/devices/src/virtio/mmio.rs` — **modify.** `inject_input` trait method (defaulted) + `VirtioMmio::inject_input` delegation (mirror of `inject_rx`).
- `spike/src/bin/display_sink.rs` — **modify.** keyboard/tablet handles + guest res on `App`; `map_keycode` + `scale_pos` pure helpers; translate winit input → inject.
- `spike/src/bin/boot.rs` — **modify.** register the two devices under `--gui` (boot), thread handles + resolution into `run_event_loop`.
- `docs/src/features/devices.md`, `ROADMAP.md` — **modify.** docs.

**Shared constants** (Task 1 writes them once in input.rs):

```rust
const VIRTIO_ID_INPUT: u32 = 18;
// config selectors
const CFG_ID_NAME: u8 = 0x01;
const CFG_EV_BITS: u8 = 0x11;
const CFG_ABS_INFO: u8 = 0x12;
// event types / codes
const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_ABS: u16 = 3;
const SYN_REPORT: u16 = 0;
const ABS_X: u16 = 0;
const ABS_Y: u16 = 1;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
```

---

## Task 1: VirtioInput skeleton — identity, queues, statusq ack, config state

**Files:** Create `crates/devices/src/virtio/input.rs`; modify `crates/devices/src/virtio/mod.rs` (add `pub mod input;`).

- [ ] **Step 1: Write the failing tests.** Create `crates/devices/src/virtio/input.rs` with the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const BASE: u64 = 0x4000_0000;
    const DESC: u64 = BASE + 0x1000;
    const AVAIL: u64 = BASE + 0x2000;
    const USED: u64 = BASE + 0x3000;
    const BUF: u64 = BASE + 0x100;

    fn write_desc(m: &GuestRam, i: u64, addr: u64, len: u32, flags: u16, next: u16) {
        let d = DESC + i * 16;
        m.write_slice(d, &addr.to_le_bytes());
        m.write_slice(d + 8, &len.to_le_bytes());
        m.write_slice(d + 12, &flags.to_le_bytes());
        m.write_slice(d + 14, &next.to_le_bytes());
    }

    #[test]
    fn identity() {
        let kbd = VirtioInput::keyboard();
        assert_eq!(kbd.device_id(), 18);
        assert_eq!(kbd.queue_count(), 2);
        assert_eq!(kbd.device_features(0), 0);
        let tab = VirtioInput::tablet(1280, 800);
        assert_eq!(tab.device_id(), 18);
        assert_eq!(tab.queue_count(), 2);
    }

    #[test]
    fn statusq_acks_zero_length() {
        let mut kbd = VirtioInput::keyboard();
        let mut backing = vec![0u8; 0x4000];
        let m = GuestRam::new(backing.as_mut_ptr(), backing.len(), BASE);
        write_desc(&m, 0, BUF, 8, 0, 0); // one readable status buffer
        m.write_u16(AVAIL + 2, 1);
        m.write_u16(AVAIL + 4, 0);
        let mut vq = Virtqueue::new(8, DESC, AVAIL, USED);
        assert!(kbd.handle_notify(1, &mut vq, &m)); // statusq = queue 1
        assert_eq!(m.read_u32(USED + 4), Some(0)); // used id = head 0
        assert_eq!(m.read_u32(USED + 8), Some(0)); // used len = 0
    }

    #[test]
    fn config_write_sets_select_subsel() {
        let mut kbd = VirtioInput::keyboard();
        kbd.config_write(0, &[CFG_ID_NAME]); // select
        kbd.config_write(1, &[0x00]);        // subsel
        assert_eq!(kbd.select, CFG_ID_NAME);
        assert_eq!(kbd.subsel, 0);
    }
}
```

- [ ] **Step 2: Run to verify it fails to compile.** Run: `PATH="$HOME/.cargo/bin:$PATH" cargo test -p ignition-devices input 2>&1 | tail -20`. Expect `cannot find type VirtioInput`.

- [ ] **Step 3: Implement the skeleton.** Prepend above the test module, and add `pub mod input;` to `crates/devices/src/virtio/mod.rs`:

```rust
//! virtio-input (VIRTIO_ID_INPUT = 18). Two flavors: a keyboard (EV_KEY) and an
//! absolute tablet (EV_ABS x/y + buttons). eventq (queue 0) carries device->guest
//! input events (filled via inject); statusq (queue 1) is parsed and ack'd. The
//! config select/subsel protocol advertises the name, EV bits, and ABS axis info.
//! No snapshot of input state (M5). Built only under --gui.

use super::guest_ram::GuestRam;
use super::mmio::VirtioDevice;
use super::queue::Virtqueue;

const VIRTIO_ID_INPUT: u32 = 18;

const CFG_ID_NAME: u8 = 0x01;
const CFG_EV_BITS: u8 = 0x11;
const CFG_ABS_INFO: u8 = 0x12;

const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_ABS: u16 = 3;
#[allow(dead_code)] // emitted by the host translator (spike), documents the protocol
const SYN_REPORT: u16 = 0;
const ABS_X: u16 = 0;
const ABS_Y: u16 = 1;
const BTN_LEFT: u16 = 0x110;
#[allow(dead_code)] // advertised in the EV_KEY bitmap; emitted by the translator
const BTN_RIGHT: u16 = 0x111;
#[allow(dead_code)]
const BTN_MIDDLE: u16 = 0x112;

/// One virtio-input event on the wire (no timestamp): type, code, value — 8 LE bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputEvent {
    pub etype: u16,
    pub code: u16,
    pub value: u32,
}

impl InputEvent {
    fn to_le_bytes(self) -> [u8; 8] {
        let mut b = [0u8; 8];
        b[0..2].copy_from_slice(&self.etype.to_le_bytes());
        b[2..4].copy_from_slice(&self.code.to_le_bytes());
        b[4..8].copy_from_slice(&self.value.to_le_bytes());
        b
    }
}

enum Flavor {
    Keyboard,
    Tablet { w: u32, h: u32 },
}

/// virtio-input device (keyboard or absolute tablet).
pub struct VirtioInput {
    flavor: Flavor,
    select: u8,
    subsel: u8,
}

impl VirtioInput {
    pub fn keyboard() -> Self {
        VirtioInput { flavor: Flavor::Keyboard, select: 0, subsel: 0 }
    }
    pub fn tablet(width: u32, height: u32) -> Self {
        VirtioInput { flavor: Flavor::Tablet { w: width, h: height }, select: 0, subsel: 0 }
    }
}

impl VirtioDevice for VirtioInput {
    fn device_id(&self) -> u32 {
        VIRTIO_ID_INPUT
    }

    fn device_features(&self, _sel: u32) -> u32 {
        0
    }

    fn config_read(&self, offset: u64, data: &mut [u8]) {
        // Built up in Task 2; for now serve zeros (config image not yet assembled).
        for b in data.iter_mut() {
            *b = 0;
        }
        let _ = offset;
    }

    fn config_write(&mut self, offset: u64, data: &[u8]) {
        // The guest selects what config_read returns by writing select@0 / subsel@1.
        for (i, &byte) in data.iter().enumerate() {
            match offset as usize + i {
                0 => self.select = byte,
                1 => self.subsel = byte,
                _ => {}
            }
        }
    }

    fn queue_count(&self) -> usize {
        2
    }

    fn handle_notify(&mut self, queue_idx: usize, vq: &mut Virtqueue, mem: &GuestRam) -> bool {
        // eventq (0) is device->guest, filled by inject (Task 3); a bare notify just
        // means the guest posted buffers — nothing to do here. statusq (1): ack.
        let mut serviced = false;
        while let Some(chain) = vq.pop_avail(mem) {
            if queue_idx == 1 {
                vq.push_used(mem, chain.head, 0);
            } else {
                // eventq notify: buffers are consumed lazily by inject; release this
                // descriptor with zero length so a stray kick doesn't wedge the ring.
                vq.push_used(mem, chain.head, 0);
            }
            serviced = true;
        }
        serviced
    }
}
```

> NOTE on the eventq notify arm: the guest normally posts eventq buffers and does NOT
> kick (the device fills them asynchronously). The loop above only runs if the guest
> *does* kick; releasing with len 0 is harmless. The real fill happens in `inject`
> (Task 3), which pops avail buffers directly. Do not write events here.

- [ ] **Step 4: Run to verify it passes.** Run: `PATH="$HOME/.cargo/bin:$PATH" cargo test -p ignition-devices input 2>&1 | tail -10`. Expect `3 passed`.

- [ ] **Step 5: Clippy.** Run: `PATH="$HOME/.cargo/bin:$PATH" cargo clippy -p ignition-devices --all-targets 2>&1 | tail -10`. Unused consts/fields used in later tasks may warn — add `#[allow(dead_code)]` with a "used in later tasks" comment only if they are hard errors; warnings are acceptable.

- [ ] **Step 6: Commit.**

```bash
git add crates/devices/src/virtio/input.rs crates/devices/src/virtio/mod.rs
git commit -m "feat(devices): virtio-input skeleton (identity, statusq ack, config state)"
```

---

## Task 2: Config select/subsel protocol — ID_NAME, EV_BITS, ABS_INFO

**Files:** Modify `crates/devices/src/virtio/input.rs`.

- [ ] **Step 1: Write the failing tests** (add to the `tests` module):

```rust
    fn read_cfg(dev: &VirtioInput, select: u8, subsel: u8) -> (u8, Vec<u8>) {
        // emulate the guest: set select/subsel, read size@2 and the union@8..
        let mut d = dev_clone_with_sel(dev, select, subsel);
        let mut size = [0u8; 1];
        d.config_read(2, &mut size);
        let mut u = vec![0u8; 128];
        d.config_read(8, &mut u);
        (size[0], u)
    }
    // helper: build a fresh device of the same flavor with select/subsel applied
    fn dev_clone_with_sel(dev: &VirtioInput, select: u8, subsel: u8) -> VirtioInput {
        let mut d = match dev.flavor {
            Flavor::Keyboard => VirtioInput::keyboard(),
            Flavor::Tablet { w, h } => VirtioInput::tablet(w, h),
        };
        d.config_write(0, &[select]);
        d.config_write(1, &[subsel]);
        d
    }

    #[test]
    fn config_id_name() {
        let kbd = VirtioInput::keyboard();
        let (size, u) = read_cfg(&kbd, CFG_ID_NAME, 0);
        assert_eq!(&u[..size as usize], b"ignition-keyboard");
        let tab = VirtioInput::tablet(1280, 800);
        let (size, u) = read_cfg(&tab, CFG_ID_NAME, 0);
        assert_eq!(&u[..size as usize], b"ignition-tablet");
    }

    #[test]
    fn config_ev_bits_keyboard_has_keys() {
        let kbd = VirtioInput::keyboard();
        let (size, u) = read_cfg(&kbd, CFG_EV_BITS, EV_KEY as u8);
        assert!(size > 0);
        // KEY_A = 30 must be advertised (bit 30 set).
        assert_ne!(u[30 / 8] & (1 << (30 % 8)), 0);
    }

    #[test]
    fn config_ev_bits_tablet_has_abs_and_buttons() {
        let tab = VirtioInput::tablet(1280, 800);
        // EV_ABS bitmap: ABS_X(0), ABS_Y(1) set.
        let (size, u) = read_cfg(&tab, CFG_EV_BITS, EV_ABS as u8);
        assert!(size >= 1);
        assert_ne!(u[0] & 0b11, 0);
        // EV_KEY bitmap: BTN_LEFT (0x110 = 272) set.
        let (_size, u) = read_cfg(&tab, CFG_EV_BITS, EV_KEY as u8);
        assert_ne!(u[272 / 8] & (1 << (272 % 8)), 0);
    }

    #[test]
    fn config_abs_info_ranges() {
        let tab = VirtioInput::tablet(1280, 800);
        let (size, u) = read_cfg(&tab, CFG_ABS_INFO, ABS_X as u8);
        assert_eq!(size, 20);
        assert_eq!(u32::from_le_bytes(u[0..4].try_into().unwrap()), 0);     // min
        assert_eq!(u32::from_le_bytes(u[4..8].try_into().unwrap()), 1279);  // max = w-1
        let (_size, u) = read_cfg(&tab, CFG_ABS_INFO, ABS_Y as u8);
        assert_eq!(u32::from_le_bytes(u[4..8].try_into().unwrap()), 799);   // max = h-1
    }
```

- [ ] **Step 2: Run to verify they fail.** Run: `PATH="$HOME/.cargo/bin:$PATH" cargo test -p ignition-devices input 2>&1 | tail -15`. Expect the 4 new tests FAIL (config_read returns zeros).

- [ ] **Step 3: Implement.** Replace `config_read` and add a `config_image` builder + a `set_bit` helper inside `impl VirtioInput`:

```rust
/// Set bit `code` in a little-endian bitmap slice (no-op if out of range).
fn set_bit(bitmap: &mut [u8], code: u16) {
    let (byte, bit) = ((code / 8) as usize, code % 8);
    if byte < bitmap.len() {
        bitmap[byte] |= 1 << bit;
    }
}
```

```rust
impl VirtioInput {
    /// Build the 136-byte config window for the current (flavor, select, subsel):
    /// [select:1][subsel:1][size:1][reserved:5][union:128].
    fn config_image(&self) -> [u8; 136] {
        let mut c = [0u8; 136];
        c[0] = self.select;
        c[1] = self.subsel;
        let u = &mut c[8..136]; // 128-byte union
        let size: usize = match self.select {
            CFG_ID_NAME => {
                let name: &[u8] = match self.flavor {
                    Flavor::Keyboard => b"ignition-keyboard",
                    Flavor::Tablet { .. } => b"ignition-tablet",
                };
                u[..name.len()].copy_from_slice(name);
                name.len()
            }
            CFG_EV_BITS => match (self.subsel as u16, &self.flavor) {
                // EV_KEY: keyboard advertises codes 1..=127; tablet advertises buttons.
                (EV_KEY, Flavor::Keyboard) => {
                    for code in 1..=127u16 {
                        set_bit(u, code);
                    }
                    16 // bytes covering codes 0..=127
                }
                (EV_KEY, Flavor::Tablet { .. }) => {
                    set_bit(u, BTN_LEFT);
                    set_bit(u, BTN_RIGHT);
                    set_bit(u, BTN_MIDDLE);
                    35 // bytes covering up to code 274 (BTN_MIDDLE)
                }
                // EV_ABS: tablet advertises ABS_X/ABS_Y; keyboard none.
                (EV_ABS, Flavor::Tablet { .. }) => {
                    set_bit(u, ABS_X);
                    set_bit(u, ABS_Y);
                    1
                }
                _ => 0,
            },
            CFG_ABS_INFO => match &self.flavor {
                Flavor::Tablet { w, h } => {
                    let max = match self.subsel as u16 {
                        ABS_X => w.saturating_sub(1),
                        ABS_Y => h.saturating_sub(1),
                        _ => 0,
                    };
                    // virtio_input_absinfo { min, max, fuzz, flat, res } = 5 x u32.
                    u[0..4].copy_from_slice(&0u32.to_le_bytes()); // min
                    u[4..8].copy_from_slice(&max.to_le_bytes()); // max
                    // fuzz/flat/res stay 0.
                    20
                }
                Flavor::Keyboard => 0,
            },
            _ => 0,
        };
        c[2] = size as u8;
        c
    }
}
```

Replace `config_read`:

```rust
    fn config_read(&self, offset: u64, data: &mut [u8]) {
        let img = self.config_image();
        for (i, b) in data.iter_mut().enumerate() {
            let idx = (offset as usize).saturating_add(i);
            *b = if idx < img.len() { img[idx] } else { 0 };
        }
    }
```

- [ ] **Step 4: Run to verify they pass.** Run: `PATH="$HOME/.cargo/bin:$PATH" cargo test -p ignition-devices input 2>&1 | tail -10`. Expect all input tests pass.

- [ ] **Step 5: Commit.**

```bash
git add crates/devices/src/virtio/input.rs
git commit -m "feat(devices): virtio-input config select/subsel (name, EV bits, ABS info)"
```

---

## Task 3: eventq injection + VirtioMmio::inject_input

**Files:** Modify `crates/devices/src/virtio/input.rs` and `crates/devices/src/virtio/mmio.rs`.

- [ ] **Step 1: Write the failing tests** (add to input.rs `tests`):

```rust
    #[test]
    fn inject_writes_evdev_triples() {
        let mut kbd = VirtioInput::keyboard();
        let mut backing = vec![0u8; 0x4000];
        let m = GuestRam::new(backing.as_mut_ptr(), backing.len(), BASE);
        // two writable eventq buffers (8 bytes each) at BUF and BUF+0x40.
        write_desc(&m, 0, BUF, 8, 2, 0);
        write_desc(&m, 1, BUF + 0x40, 8, 2, 0);
        m.write_u16(AVAIL + 2, 2);
        m.write_u16(AVAIL + 4, 0); // ring[0] = 0
        m.write_u16(AVAIL + 6, 1); // ring[1] = 1
        let mut vq = Virtqueue::new(8, DESC, AVAIL, USED);
        let evs = [
            InputEvent { etype: EV_KEY, code: 30, value: 1 }, // KEY_A press
            InputEvent { etype: EV_SYN, code: 0, value: 0 },
        ];
        assert!(kbd.fill_events(&mut vq, &m, &evs));
        // buffer 0 = KEY_A press.
        assert_eq!(m.read_u16(BUF).unwrap(), EV_KEY);
        assert_eq!(m.read_u16(BUF + 2).unwrap(), 30);
        assert_eq!(m.read_u32(BUF + 4).unwrap(), 1);
        // buffer 1 = SYN.
        assert_eq!(m.read_u16(BUF + 0x40).unwrap(), EV_SYN);
        // two used entries of length 8.
        assert_eq!(m.read_u16(USED + 2), Some(2)); // used.idx = 2
    }

    #[test]
    fn inject_with_no_buffers_returns_false() {
        let mut kbd = VirtioInput::keyboard();
        let mut backing = vec![0u8; 0x4000];
        let m = GuestRam::new(backing.as_mut_ptr(), backing.len(), BASE);
        m.write_u16(AVAIL + 2, 0); // no avail buffers
        let mut vq = Virtqueue::new(8, DESC, AVAIL, USED);
        let evs = [InputEvent { etype: EV_KEY, code: 30, value: 1 }];
        assert!(!kbd.fill_events(&mut vq, &m, &evs));
    }
```

(`m.read_u16` exists on GuestRam.)

- [ ] **Step 2: Run to verify they fail.** Run: `PATH="$HOME/.cargo/bin:$PATH" cargo test -p ignition-devices input::tests::inject 2>&1 | tail -10`. Expect FAIL (`no method fill_events`).

- [ ] **Step 3: Implement `fill_events` on `VirtioInput`:**

```rust
    /// Write one `InputEvent` per available eventq buffer; push each used (len 8).
    /// Returns true if at least one event was delivered (caller raises the IRQ).
    pub fn fill_events(&mut self, eventq: &mut Virtqueue, mem: &GuestRam, events: &[InputEvent]) -> bool {
        let mut any = false;
        for ev in events {
            let Some(chain) = eventq.pop_avail(mem) else { break };
            // Find the first device-writable descriptor (>= 8 bytes) and write there.
            let target = chain.descriptors.iter().find(|d| d.writable && d.len >= 8);
            if let Some(d) = target {
                mem.write_slice(d.addr, &ev.to_le_bytes());
                eventq.push_used(mem, chain.head, 8);
                any = true;
            } else {
                eventq.push_used(mem, chain.head, 0); // unusable buffer; release it
            }
        }
        any
    }
```

- [ ] **Step 4: Add the trait method + transport delegation in `mmio.rs`.** In the `VirtioDevice` trait (next to `inject_rx`), add a defaulted method:

```rust
    /// Fill the eventq (queue 0) with input events. Default: not an input device.
    fn inject_input(
        &mut self,
        _eventq: &mut Virtqueue,
        _mem: &GuestRam,
        _events: &[crate::virtio::input::InputEvent],
    ) -> bool {
        false
    }
```

Implement it on `VirtioInput` (in input.rs, inside `impl VirtioDevice for VirtioInput`), delegating to `fill_events`:

```rust
    fn inject_input(
        &mut self,
        eventq: &mut Virtqueue,
        mem: &GuestRam,
        events: &[InputEvent],
    ) -> bool {
        self.fill_events(eventq, mem, events)
    }
```

Add the transport method on `VirtioMmio` (next to `inject_rx`, mirroring it — eventq is queue 0):

```rust
    /// Inject host input events into the eventq (queue 0) and raise the IRQ if any
    /// were delivered. Mirrors `inject_rx`; called from the GUI event-loop thread.
    pub fn inject_input(&mut self, events: &[crate::virtio::input::InputEvent]) -> bool {
        let Some(q) = self.queues.get_mut(0) else {
            return false;
        };
        if q.ready == 0 {
            return false;
        }
        let Some(vq) = q.vq.as_mut() else {
            return false;
        };
        let used = self.dev.inject_input(vq, &self.mem, events);
        if used {
            self.raise();
        }
        used
    }
```

- [ ] **Step 5: Run tests + clippy.** Run: `PATH="$HOME/.cargo/bin:$PATH" cargo test -p ignition-devices 2>&1 | tail -5` (all pass) and `PATH="$HOME/.cargo/bin:$PATH" cargo clippy -p ignition-devices --all-targets 2>&1 | tail -8` (clean except pre-existing fuzz warnings; remove now-used `#[allow(dead_code)]` on EV_SYN/BTN_RIGHT/BTN_MIDDLE if they are now referenced — EV_SYN/buttons are used by tests/translator, keep allows only if still unused in the crate).

- [ ] **Step 6: Commit.**

```bash
git add crates/devices/src/virtio/input.rs crates/devices/src/virtio/mmio.rs
git commit -m "feat(devices): virtio-input eventq injection + VirtioMmio::inject_input"
```

---

## Task 4: Keycode map + pointer scale (pure helpers in spike)

**Files:** Modify `spike/src/bin/display_sink.rs` (add the helpers + tests; the module is part of the `boot` bin).

- [ ] **Step 1: Write the failing tests** (add to the `tests` module of display_sink.rs):

```rust
    #[test]
    fn keycode_maps_known_keys() {
        use winit::keyboard::KeyCode;
        assert_eq!(map_keycode(KeyCode::KeyA), Some(30));
        assert_eq!(map_keycode(KeyCode::Enter), Some(28));
        assert_eq!(map_keycode(KeyCode::Space), Some(57));
        assert_eq!(map_keycode(KeyCode::Digit1), Some(2));
        assert_eq!(map_keycode(KeyCode::ArrowUp), Some(103));
        assert_eq!(map_keycode(KeyCode::F13), None); // unmapped
    }

    #[test]
    fn pointer_scale_maps_corners() {
        // origin -> (0,0); far corner -> (gw-1, gh-1).
        assert_eq!(scale_pos(0.0, 0.0, 2560, 1600, 1280, 800), (0, 0));
        assert_eq!(scale_pos(2559.0, 1599.0, 2560, 1600, 1280, 800), (1279, 799));
        // out-of-range clamps.
        assert_eq!(scale_pos(99999.0, 99999.0, 2560, 1600, 1280, 800), (1279, 799));
        assert_eq!(scale_pos(-5.0, -5.0, 2560, 1600, 1280, 800), (0, 0));
    }
```

- [ ] **Step 2: Run to verify they fail.** Run: `PATH="$HOME/.cargo/bin:$PATH" cargo test -p ignition-spike --bin boot keycode 2>&1 | tail -10` and `... pointer_scale ...`. Expect FAIL (functions not found).

- [ ] **Step 3: Implement the helpers** in display_sink.rs (top level):

```rust
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
    let denom_w = surf_w.max(1) as f64;
    let denom_h = surf_h.max(1) as f64;
    let x = px * (gw.saturating_sub(1) as f64) / (denom_w - 1.0).max(1.0);
    let y = py * (gh.saturating_sub(1) as f64) / (denom_h - 1.0).max(1.0);
    (clamp(x, gw - 1), clamp(y, gh - 1))
}
```

- [ ] **Step 4: Run to verify they pass.** Run: `PATH="$HOME/.cargo/bin:$PATH" cargo test -p ignition-spike --bin boot 2>&1 | tail -10`. Expect pass. (If `scale_pos` corner case `2559 -> 1279` is off by the rounding, the test pins the contract; adjust the formula so the far physical pixel maps to `gw-1`. The given formula yields `2559*1279/2559 = 1279`. Good.)

- [ ] **Step 5: Commit.**

```bash
git add spike/src/bin/display_sink.rs
git commit -m "feat(spike): winit->evdev keycode map + pointer scale helpers"
```

---

## Task 5: Translate winit input events → inject (event loop)

**Files:** Modify `spike/src/bin/display_sink.rs`.

- [ ] **Step 1: Add input-device handles + guest res to `App`.** Add fields to `struct App`:

```rust
    /// virtio-input device handles (eventq injection) + guest resolution for pointer
    /// scaling. None when no input devices are wired.
    keyboard: Option<std::sync::Arc<std::sync::Mutex<ignition_devices::virtio::mmio::VirtioMmio>>>,
    tablet: Option<std::sync::Arc<std::sync::Mutex<ignition_devices::virtio::mmio::VirtioMmio>>>,
    gw: u32,
    gh: u32,
```

- [ ] **Step 2: Handle the input events** in `App::window_event` (add arms before the `_ => {}`):

```rust
            WindowEvent::KeyboardInput { event, .. } => {
                use ignition_devices::virtio::input::InputEvent;
                use winit::keyboard::PhysicalKey;
                if let PhysicalKey::Code(kc) = event.physical_key {
                    if let (Some(code), Some(kbd)) = (map_keycode(kc), &self.keyboard) {
                        let value = if event.state.is_pressed() { 1 } else { 0 };
                        let evs = [
                            InputEvent { etype: 1, code, value },          // EV_KEY
                            InputEvent { etype: 0, code: 0, value: 0 },    // EV_SYN/SYN_REPORT
                        ];
                        let _ = kbd.lock().unwrap().inject_input(&evs);
                    }
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                use ignition_devices::virtio::input::InputEvent;
                if let Some(tab) = &self.tablet {
                    let (x, y) = scale_pos(position.x, position.y, self.surf_w, self.surf_h, self.gw, self.gh);
                    let evs = [
                        InputEvent { etype: 3, code: 0, value: x },        // EV_ABS ABS_X
                        InputEvent { etype: 3, code: 1, value: y },        // EV_ABS ABS_Y
                        InputEvent { etype: 0, code: 0, value: 0 },        // EV_SYN
                    ];
                    let _ = tab.lock().unwrap().inject_input(&evs);
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
                        InputEvent { etype: 1, code, value },              // EV_KEY BTN_*
                        InputEvent { etype: 0, code: 0, value: 0 },        // EV_SYN
                    ];
                    let _ = tab.lock().unwrap().inject_input(&evs);
                }
            }
```

- [ ] **Step 3: Thread the new fields through `run_event_loop`.** Change the signature and `App` construction:

```rust
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
        keyboard,
        tablet,
        gw,
        gh,
        window: None,
        surface: None,
    };
    if let Err(e) = event_loop.run_app(&mut app) {
        eprintln!("[gui] event loop exited with error: {e}");
    }
}
```

- [ ] **Step 4: Build.** Run: `PATH="$HOME/.cargo/bin:$PATH" cargo build -p ignition-spike --bin boot 2>&1 | tail -20`. Fix compile errors. Likely: `ElementState::is_pressed()` exists in winit 0.30; if not, match `ElementState::Pressed`. `event.physical_key`/`event.state` are fields of `KeyEvent`. The `run_event_loop` call site in boot.rs will now fail to compile (wrong arity) — that is fixed in Task 6; for THIS task's build check, temporarily pass the new args at the boot.rs call site as `None, None, 1280, 800` so it compiles (Task 6 wires the real handles).

- [ ] **Step 5: Run tests + clippy.** `PATH="$HOME/.cargo/bin:$PATH" cargo test -p ignition-spike --bin boot 2>&1 | tail -10` and `... clippy ...`. Expect pass + clean.

- [ ] **Step 6: Commit.**

```bash
git add spike/src/bin/display_sink.rs spike/src/bin/boot.rs
git commit -m "feat(spike): translate winit keyboard/pointer events into inject_input"
```

---

## Task 6: Register the two input devices under --gui (boot)

**Files:** Modify `spike/src/bin/boot.rs`.

- [ ] **Step 1: Add handle fields to `DeviceContext`** (after `display_sink`):

```rust
    /// virtio-input device handles (Some only in --gui boot), kept for the event loop.
    keyboard_mmio: Option<Arc<Mutex<VirtioMmio>>>,
    tablet_mmio: Option<Arc<Mutex<VirtioMmio>>>,
```

Initialize them `None` in BOTH `DeviceContext` literals (boot ~762 and restore ~1604).

- [ ] **Step 2: Register the devices alongside the gpu in `setup_devices`** (input exists only with a display). Replace the existing gpu `if let (Mode::Boot, Some(sink)) = (&mode, ctx.display_sink.take()) { ... }` block with this expanded version that, after the gpu, also registers the keyboard + tablet and stashes their handles:

```rust
    if let (Mode::Boot, Some(sink)) = (&mode, ctx.display_sink.take()) {
        let mem = ctx.guest_ram();
        place::<VirtioMmio, _>(mgr, &mode, "virtio-gpu", layout::MMIO_WINDOW,
            move |irq| VirtioMmio::new(
                "virtio-gpu",
                Box::new(ignition_devices::virtio::gpu::VirtioGpu::new(1280, 800, sink)),
                mem,
                irq,
            ))?;
        let mem_kbd = ctx.guest_ram();
        if let Some(h) = place::<VirtioMmio, _>(mgr, &mode, "virtio-keyboard", layout::MMIO_WINDOW,
            move |irq| VirtioMmio::new(
                "virtio-keyboard",
                Box::new(ignition_devices::virtio::input::VirtioInput::keyboard()),
                mem_kbd, irq))? {
            ctx.keyboard_mmio = Some(h);
        }
        let mem_tab = ctx.guest_ram();
        if let Some(h) = place::<VirtioMmio, _>(mgr, &mode, "virtio-tablet", layout::MMIO_WINDOW,
            move |irq| VirtioMmio::new(
                "virtio-tablet",
                Box::new(ignition_devices::virtio::input::VirtioInput::tablet(1280, 800)),
                mem_tab, irq))? {
            ctx.tablet_mmio = Some(h);
        }
    }
```

- [ ] **Step 3: Pass the handles into `run_event_loop`.** Before the `--gui` branch builds its threads, capture the handles (they were set during `setup_devices`):

```rust
    let kbd_handle = ctx.keyboard_mmio.clone();
    let tab_handle = ctx.tablet_mmio.clone();
```

(Place this where `ctx` is still in scope — right after `setup_devices(&mut mgr, &mut ctx, Mode::Boot)`, alongside the existing `let serial = ctx.serial.clone()...` lines.)

Then change the `--gui` branch's `run_event_loop` call (the Task-5 temporary `None, None, 1280, 800`) to:

```rust
        display_sink::run_event_loop(rx, done, 1280, 800, kbd_handle, tab_handle, 1280, 800);
```

- [ ] **Step 4: Build + test + clippy + sign.** Run:
`PATH="$HOME/.cargo/bin:$PATH" cargo build -p ignition-spike --bin boot 2>&1 | tail -15`
`PATH="$HOME/.cargo/bin:$PATH" cargo test -p ignition-spike --bin boot 2>&1 | tail -8`
`PATH="$HOME/.cargo/bin:$PATH" cargo clippy -p ignition-spike --all-targets 2>&1 | tail -10`
`./scripts/sign.sh target/debug/boot 2>&1 | tail -2`
Tests pass; clippy clean except pre-existing fuzz warnings; signing OK. Confirm both restore + non-gui still compile (DeviceContext literals have the two new `None` fields).

- [ ] **Step 5: Commit.**

```bash
git add spike/src/bin/boot.rs
git commit -m "feat(spike): register virtio-keyboard + virtio-tablet under --gui (boot)"
```

---

## Task 7: Documentation

**Files:** Modify `docs/src/features/devices.md`, `ROADMAP.md`.

- [ ] **Step 1: Add a virtio-input note to the GUI section of `docs/src/features/devices.md`** (after the virtio-gpu paragraph):

```markdown
Under `--gui`, two **virtio-input** devices (device id 18) make the window interactive:
a keyboard (`EV_KEY`) and an absolute tablet (`EV_ABS` x/y + buttons). The winit event
loop translates host key/pointer/click events into Linux evdev events and injects them
into the guest's eventq (the `inject_rx`-style path), so typing logs in at the console
and the pointer tracks the macOS cursor 1:1 over the 1280x800 scanout. The guest kernel
needs `CONFIG_VIRTIO_INPUT=y` and `CONFIG_INPUT_EVDEV=y`.
```

- [ ] **Step 2: Update `ROADMAP.md`** — change the GUI M3 line from planned to shipped:

Replace the `- [ ] **M3 virtio-input**, **M4 compositor/app**, **M5 ...**` line with:

```markdown
- [x] **M3 virtio-input** — keyboard + absolute tablet (device id 18); winit key/pointer/click events injected into the guest eventq; typing logs in, pointer tracks 1:1. `docs/superpowers/specs/2026-06-15-virtio-input-m3-design.md`
- [ ] **M4 compositor/app**, **M5 snapshot/clone with the GUI live** — remaining GUI milestones.
```

- [ ] **Step 3: Verify the book builds.** Run: `PATH="$HOME/.cargo/bin:$PATH" mdbook build docs 2>&1 | tail -5`. Expect success.

- [ ] **Step 4: Commit.**

```bash
git add docs/src/features/devices.md ROADMAP.md
git commit -m "docs: virtio-input (M3) device + roadmap"
```

---

## Manual integration verification (after all tasks; needs entitlement + the GUI kernel)

1. `target/debug/boot --gui kimage/out/Image kimage/out/rootfs.ext4` — in the window, the guest
   `dmesg` shows two `virtio_input` devices and `/dev/input/event*` nodes appear.
2. Type the login (`root`) in the window — characters reach the fbcon console; you can log in
   and run a shell command, all from the window (serial unused).
3. Move the pointer — an absolute guest pointer tracks the macOS cursor; clicks register
   (verify with `evtest /dev/input/eventN` or a TUI that reads the mouse).
4. Regression: `boot` without `--gui`, `--restore`, `--fuzz` add no input devices and behave
   as before.

---

## Self-Review Notes

- **Spec coverage:** two devices id 18 / 2 queues (Task 1) ✓; config select/subsel ID_NAME / EV_BITS / ABS_INFO with ABS max = res-1 (Task 2) ✓; eventq inject 8-byte records + no-buffer drop + `VirtioMmio::inject_input` mirror of `inject_rx` (Task 3) ✓; statusq ack (Task 1) ✓; keycode map + pointer scale pure+tested (Task 4) ✓; winit KeyboardInput/CursorMoved/MouseInput translation (Task 5) ✓; `--gui` boot-only registration + handles threaded (Task 6) ✓; docs + ROADMAP (Task 7) ✓. Snapshot deferred to M5 (devices boot-only; `save` defaults to Null — VirtioInput does not override it).
- **Type consistency:** `InputEvent { etype:u16, code:u16, value:u32 }` defined in input.rs, used by `fill_events`, the trait method, `VirtioMmio::inject_input`, and the spike translator identically. `VirtioInput::keyboard()` / `::tablet(u32,u32)`; `fill_events(&mut Virtqueue,&GuestRam,&[InputEvent])->bool`; `map_keycode(KeyCode)->Option<u16>`; `scale_pos(f64,f64,u32,u32,u32,u32)->(u32,u32)`; `run_event_loop(... , Option<Arc<Mutex<VirtioMmio>>>, Option<...>, u32, u32)` consistent across Tasks 3/5/6.
- **No placeholders:** every code step is complete. EV_KEY type literal `1`, EV_ABS `3`, EV_SYN `0` are used directly in the spike translator (it does not import the devices-crate constants) — these are the stable evdev type numbers, commented at each use.
