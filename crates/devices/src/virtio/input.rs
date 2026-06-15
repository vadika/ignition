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
#[allow(dead_code)] // used in later tasks
const CFG_EV_BITS: u8 = 0x11;
#[allow(dead_code)] // used in later tasks
const CFG_ABS_INFO: u8 = 0x12;

#[allow(dead_code)] // used in later tasks
const EV_SYN: u16 = 0;
#[allow(dead_code)] // used in later tasks
const EV_KEY: u16 = 1;
#[allow(dead_code)] // used in later tasks
const EV_ABS: u16 = 3;
#[allow(dead_code)] // emitted by the host translator (spike), documents the protocol
const SYN_REPORT: u16 = 0;
#[allow(dead_code)] // used in later tasks
const ABS_X: u16 = 0;
#[allow(dead_code)] // used in later tasks
const ABS_Y: u16 = 1;
#[allow(dead_code)] // used in later tasks
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
    #[allow(dead_code)] // fields read in later tasks (config_read ABS_INFO path)
    Tablet { w: u32, h: u32 },
}

/// virtio-input device (keyboard or absolute tablet).
pub struct VirtioInput {
    #[allow(dead_code)] // read in later tasks (config_read dispatches on flavor)
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
        for b in data.iter_mut() {
            *b = 0;
        }
        let _ = offset;
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
}
