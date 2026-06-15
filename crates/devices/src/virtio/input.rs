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

#[allow(dead_code)] // translator-only
const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_ABS: u16 = 3;
#[allow(dead_code)] // emitted by the host translator (spike), documents the protocol
const SYN_REPORT: u16 = 0;
const ABS_X: u16 = 0;
const ABS_Y: u16 = 1;
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
    #[allow(dead_code)] // used in later tasks (inject path)
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

impl VirtioInput {
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
                    Flavor::Tablet { .. } => b"ignition-tablet",
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
                (EV_KEY, Flavor::Tablet { .. }) => {
                    set_bit(u, BTN_LEFT);
                    set_bit(u, BTN_RIGHT);
                    set_bit(u, BTN_MIDDLE);
                    35
                }
                (EV_ABS, Flavor::Tablet { .. }) => {
                    set_bit(u, ABS_X);
                    set_bit(u, ABS_Y);
                    1
                }
                _ => 0,
            },
            CFG_ABS_INFO => match &self.flavor {
                Flavor::Tablet { w, h } => {
                    // Only the advertised axes (ABS_X/ABS_Y) have absinfo; any other
                    // axis returns size 0 so the guest doesn't see a phantom [0,0] axis.
                    let max = match self.subsel as u16 {
                        ABS_X => Some(w.saturating_sub(1)),
                        ABS_Y => Some(h.saturating_sub(1)),
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

    fn handle_notify(&mut self, _queue_idx: usize, vq: &mut Virtqueue, mem: &GuestRam) -> bool {
        // eventq (0) buffers are consumed by inject (Task 3); a stray kick releases
        // them with len 0. statusq (1) chains are acked with len 0 as well.
        let mut serviced = false;
        while let Some(chain) = vq.pop_avail(mem) {
            vq.push_used(mem, chain.head, 0);
            serviced = true;
        }
        serviced
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
        assert_eq!(u32::from_le_bytes(u[4..8].try_into().unwrap()), 1279);
        let (_size, u) = read_cfg(&tab, CFG_ABS_INFO, ABS_Y as u8);
        assert_eq!(u32::from_le_bytes(u[4..8].try_into().unwrap()), 799);
    }
}
