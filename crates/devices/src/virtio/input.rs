//! virtio-input (VIRTIO_ID_INPUT = 18). Two flavors: a keyboard (EV_KEY) and an
//! absolute tablet (EV_ABS x/y + buttons). eventq (queue 0) carries device->guest
//! input events (filled via inject); statusq (queue 1) is parsed and ack'd. The
//! config select/subsel protocol advertises the name, EV bits, and ABS axis info.
//! Snapshot (M5): select/subsel are saved/restored; flavor is construction-time and
//! rebuilt by device wiring. Built only under --gui.

use super::guest_ram::GuestRam;
use super::mmio::VirtioDevice;
use super::queue::Virtqueue;

const VIRTIO_ID_INPUT: u32 = 18;

const CFG_ID_NAME: u8 = 0x01;
const CFG_EV_BITS: u8 = 0x11;
const CFG_ABS_INFO: u8 = 0x12;

#[allow(dead_code)] // translator-only
const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_REL: u16 = 2;
const EV_ABS: u16 = 3;
const REL_WHEEL: u16 = 8;
#[allow(dead_code)] // emitted by the host translator (spike), documents the protocol
const SYN_REPORT: u16 = 0;
const ABS_X: u16 = 0;
const ABS_Y: u16 = 1;
/// Absolute tablet axis range max (QEMU virtio-tablet convention). libinput maps
/// this fixed range onto the current guest output extent, so the pointer stays
/// correct at any resolution — absinfo is probed once at boot and never changes.
pub const TABLET_ABS_MAX: u32 = 32767;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;

/// Set bit `code` in a little-endian bitmap slice (no-op if out of range).
fn set_bit(bitmap: &mut [u8], code: u16) {
    let (byte, bit) = ((code / 8) as usize, code % 8);
    if byte < bitmap.len() {
        bitmap[byte] |= 1 << bit;
    }
}

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
    Tablet,
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
    pub fn tablet(_width: u32, _height: u32) -> Self {
        // ponytail: dims no longer affect the device; signature kept so call sites in boot.rs don't churn
        VirtioInput { flavor: Flavor::Tablet, select: 0, subsel: 0 }
    }
}

impl VirtioInput {
    /// Write one `InputEvent` per available eventq buffer; push each used (len 8).
    /// Returns true if at least one event was delivered (caller raises the IRQ).
    pub fn fill_events(&mut self, eventq: &mut Virtqueue, mem: &GuestRam, events: &[InputEvent]) -> bool {
        let mut any = false;
        for ev in events {
            let Some(chain) = eventq.pop_avail(mem) else { break };
            let target = chain.descriptors.iter().find(|d| d.writable && d.len >= 8);
            if let Some(d) = target {
                mem.write_slice(d.addr, &ev.to_le_bytes());
                eventq.push_used(mem, chain.head, 8);
                any = true;
            } else {
                eventq.push_used(mem, chain.head, 0);
            }
        }
        any
    }

    /// Build the 136-byte config window for the current (flavor, select, subsel):
    /// [select:1][subsel:1][size:1][reserved:5][union:128].
    fn config_image(&self) -> [u8; 136] {
        let mut c = [0u8; 136];
        c[0] = self.select;
        c[1] = self.subsel;
        let u = &mut c[8..136];
        let size: usize = match self.select {
            CFG_ID_NAME => {
                let name: &[u8] = match self.flavor {
                    Flavor::Keyboard => b"ignition-keyboard",
                    Flavor::Tablet => b"ignition-tablet",
                };
                u[..name.len()].copy_from_slice(name);
                name.len()
            }
            CFG_EV_BITS => match (self.subsel as u16, &self.flavor) {
                (EV_KEY, Flavor::Keyboard) => {
                    for code in 1..=127u16 {
                        set_bit(u, code);
                    }
                    16
                }
                (EV_KEY, Flavor::Tablet) => {
                    set_bit(u, BTN_LEFT);
                    set_bit(u, BTN_RIGHT);
                    set_bit(u, BTN_MIDDLE);
                    35
                }
                (EV_ABS, Flavor::Tablet) => {
                    set_bit(u, ABS_X);
                    set_bit(u, ABS_Y);
                    1
                }
                (EV_REL, Flavor::Tablet) => {
                    set_bit(u, REL_WHEEL);
                    2
                }
                _ => 0,
            },
            CFG_ABS_INFO => match &self.flavor {
                Flavor::Tablet => {
                    // Only the advertised axes (ABS_X/ABS_Y) have absinfo; any other
                    // axis returns size 0 so the guest doesn't see a phantom [0,0] axis.
                    // Range is fixed (resolution-independent); libinput maps it onto the
                    // current output extent at runtime.
                    let max = match self.subsel as u16 {
                        ABS_X | ABS_Y => Some(TABLET_ABS_MAX),
                        _ => None,
                    };
                    match max {
                        Some(max) => {
                            u[0..4].copy_from_slice(&0u32.to_le_bytes());
                            u[4..8].copy_from_slice(&max.to_le_bytes());
                            20
                        }
                        None => 0,
                    }
                }
                Flavor::Keyboard => 0,
            },
            _ => 0,
        };
        c[2] = size as u8;
        c
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
        let img = self.config_image();
        for (i, b) in data.iter_mut().enumerate() {
            let idx = (offset as usize).saturating_add(i);
            *b = if idx < img.len() { img[idx] } else { 0 };
        }
    }

    fn config_write(&mut self, offset: u64, data: &[u8]) {
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
        // statusq (1): guest->device LED/repeat writes — ack and drop.
        // eventq (0): device->guest. The guest posts writable buffers here for the
        // device to FILL on input; a kick must NOT consume them. `inject` (fill_events)
        // pops them when real input arrives. Releasing them here with len 0 makes the
        // guest re-post in a tight loop (IRQ storm -> guest soft-lockup), so do nothing.
        if queue_idx != 1 {
            return false;
        }
        let mut serviced = false;
        while let Some(chain) = vq.pop_avail(mem) {
            vq.push_used(mem, chain.head, 0);
            serviced = true;
        }
        serviced
    }

    fn inject_input(
        &mut self,
        eventq: &mut Virtqueue,
        mem: &GuestRam,
        events: &[InputEvent],
    ) -> bool {
        self.fill_events(eventq, mem, events)
    }

    fn save(&self) -> serde_json::Value {
        // flavor is construction-time (rebuilt by setup_devices); only the config
        // protocol cursor is dynamic.
        serde_json::json!({ "select": self.select, "subsel": self.subsel })
    }

    fn restore(&mut self, v: &serde_json::Value) -> Result<(), String> {
        self.select = v.get("select").and_then(|x| x.as_u64()).ok_or("input: missing select")? as u8;
        self.subsel = v.get("subsel").and_then(|x| x.as_u64()).ok_or("input: missing subsel")? as u8;
        Ok(())
    }
}

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
        write_desc(&m, 0, BUF, 8, 0, 0);
        m.write_u16(AVAIL + 2, 1);
        m.write_u16(AVAIL + 4, 0);
        let mut vq = Virtqueue::new(8, DESC, AVAIL, USED);
        assert!(kbd.handle_notify(1, &mut vq, &m));
        assert_eq!(m.read_u32(USED + 4), Some(0));
        assert_eq!(m.read_u32(USED + 8), Some(0));
    }

    #[test]
    fn eventq_kick_does_not_consume_buffers() {
        // A notify on the eventq (queue 0) must leave the guest's posted buffers in
        // the avail ring for `inject` to fill — consuming them here storms the guest.
        let mut kbd = VirtioInput::keyboard();
        let mut backing = vec![0u8; 0x4000];
        let m = GuestRam::new(backing.as_mut_ptr(), backing.len(), BASE);
        write_desc(&m, 0, BUF, 8, 2, 0); // a writable eventq buffer
        m.write_u16(AVAIL + 2, 1);
        m.write_u16(AVAIL + 4, 0);
        let mut vq = Virtqueue::new(8, DESC, AVAIL, USED);
        assert!(!kbd.handle_notify(0, &mut vq, &m)); // no-op
        assert_eq!(m.read_u16(USED + 2), Some(0)); // used.idx still 0 (nothing consumed)
    }

    #[test]
    fn config_write_sets_select_subsel() {
        let mut kbd = VirtioInput::keyboard();
        kbd.config_write(0, &[CFG_ID_NAME]);
        kbd.config_write(1, &[0x00]);
        assert_eq!(kbd.select, CFG_ID_NAME);
        assert_eq!(kbd.subsel, 0);
    }

    fn read_cfg(dev: &VirtioInput, select: u8, subsel: u8) -> (u8, Vec<u8>) {
        let d = dev_clone_with_sel(dev, select, subsel);
        let mut size = [0u8; 1];
        d.config_read(2, &mut size);
        let mut u = vec![0u8; 128];
        d.config_read(8, &mut u);
        (size[0], u)
    }
    fn dev_clone_with_sel(dev: &VirtioInput, select: u8, subsel: u8) -> VirtioInput {
        let mut d = match dev.flavor {
            Flavor::Keyboard => VirtioInput::keyboard(),
            Flavor::Tablet => VirtioInput::tablet(0, 0),
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
        assert_ne!(u[30 / 8] & (1 << (30 % 8)), 0); // KEY_A = 30
    }

    #[test]
    fn config_ev_bits_tablet_has_abs_and_buttons() {
        let tab = VirtioInput::tablet(1280, 800);
        let (size, u) = read_cfg(&tab, CFG_EV_BITS, EV_ABS as u8);
        assert!(size >= 1);
        assert_ne!(u[0] & 0b11, 0); // ABS_X(0), ABS_Y(1)
        let (_size, u) = read_cfg(&tab, CFG_EV_BITS, EV_KEY as u8);
        assert_ne!(u[272 / 8] & (1 << (272 % 8)), 0); // BTN_LEFT = 272
    }

    #[test]
    fn config_ev_bits_tablet_has_rel_wheel() {
        let tab = VirtioInput::tablet(1280, 800);
        let (size, u) = read_cfg(&tab, CFG_EV_BITS, EV_REL as u8);
        assert!(size >= 2);
        assert_ne!(u[REL_WHEEL as usize / 8] & (1 << (REL_WHEEL % 8)), 0);
        // Keyboard advertises no relative axes.
        let kbd = VirtioInput::keyboard();
        let (size, _u) = read_cfg(&kbd, CFG_EV_BITS, EV_REL as u8);
        assert_eq!(size, 0);
    }

    #[test]
    fn config_abs_info_unknown_axis_is_empty() {
        let tab = VirtioInput::tablet(1280, 800);
        let (size, _u) = read_cfg(&tab, CFG_ABS_INFO, 2); // ABS_Z: not advertised
        assert_eq!(size, 0);
    }

    #[test]
    fn config_abs_info_ranges() {
        let tab = VirtioInput::tablet(1280, 800);
        let (size, u) = read_cfg(&tab, CFG_ABS_INFO, ABS_X as u8);
        assert_eq!(size, 20);
        assert_eq!(u32::from_le_bytes(u[0..4].try_into().unwrap()), 0);
        assert_eq!(u32::from_le_bytes(u[4..8].try_into().unwrap()), TABLET_ABS_MAX);
        let (_size, u) = read_cfg(&tab, CFG_ABS_INFO, ABS_Y as u8);
        assert_eq!(u32::from_le_bytes(u[4..8].try_into().unwrap()), TABLET_ABS_MAX);
    }

    #[test]
    fn inject_writes_evdev_triples() {
        let mut kbd = VirtioInput::keyboard();
        let mut backing = vec![0u8; 0x4000];
        let m = GuestRam::new(backing.as_mut_ptr(), backing.len(), BASE);
        write_desc(&m, 0, BUF, 8, 2, 0);
        write_desc(&m, 1, BUF + 0x40, 8, 2, 0);
        m.write_u16(AVAIL + 2, 2);
        m.write_u16(AVAIL + 4, 0);
        m.write_u16(AVAIL + 6, 1);
        let mut vq = Virtqueue::new(8, DESC, AVAIL, USED);
        let evs = [
            InputEvent { etype: EV_KEY, code: 30, value: 1 },
            InputEvent { etype: EV_SYN, code: 0, value: 0 },
        ];
        assert!(kbd.fill_events(&mut vq, &m, &evs));
        assert_eq!(m.read_u16(BUF).unwrap(), EV_KEY);
        assert_eq!(m.read_u16(BUF + 2).unwrap(), 30);
        assert_eq!(m.read_u32(BUF + 4).unwrap(), 1);
        assert_eq!(m.read_u16(BUF + 0x40).unwrap(), EV_SYN);
        assert_eq!(m.read_u16(USED + 2), Some(2));
    }

    #[test]
    fn save_restore_roundtrips_select_subsel() {
        let mut kbd = VirtioInput::keyboard();
        kbd.config_write(0, &[CFG_EV_BITS]); // select
        kbd.config_write(1, &[EV_KEY as u8]); // subsel
        let saved = kbd.save();

        let mut kbd2 = VirtioInput::keyboard();
        kbd2.restore(&saved).expect("restore ok");
        assert_eq!(kbd2.select, CFG_EV_BITS);
        assert_eq!(kbd2.subsel, EV_KEY as u8);
    }

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

    #[test]
    fn inject_with_no_buffers_returns_false() {
        let mut kbd = VirtioInput::keyboard();
        let mut backing = vec![0u8; 0x4000];
        let m = GuestRam::new(backing.as_mut_ptr(), backing.len(), BASE);
        m.write_u16(AVAIL + 2, 0);
        let mut vq = Virtqueue::new(8, DESC, AVAIL, USED);
        let evs = [InputEvent { etype: EV_KEY, code: 30, value: 1 }];
        assert!(!kbd.fill_events(&mut vq, &m, &evs));
    }
}
