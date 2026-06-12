//! Synchronous virtio-blk request processing (virtio 1.0 §5.2).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};

use super::guest_ram::GuestRam;
use super::queue::{Desc, DescChain};

const SECTOR_SIZE: u64 = 512;

const VIRTIO_BLK_T_IN: u32 = 0;
const VIRTIO_BLK_T_OUT: u32 = 1;
const VIRTIO_BLK_T_FLUSH: u32 = 4;
const VIRTIO_BLK_T_GET_ID: u32 = 8;

const VIRTIO_BLK_S_OK: u8 = 0;
const VIRTIO_BLK_S_IOERR: u8 = 1;
const VIRTIO_BLK_S_UNSUPP: u8 = 2;

const DEVICE_ID: &[u8] = b"ignition-vda";

pub struct VirtioBlk {
    file: File,
    capacity_sectors: u64,
}

impl VirtioBlk {
    pub fn new(file: File) -> std::io::Result<Self> {
        let len = file.metadata()?.len();
        Ok(Self { file, capacity_sectors: len / SECTOR_SIZE })
    }

    pub fn capacity_sectors(&self) -> u64 {
        self.capacity_sectors
    }

    /// Process one request chain. Returns the number of bytes written into
    /// guest-writable buffers (used as the used-ring `len`). The chain is
    /// `[header(read, 16B), data..(read|write), status(write, 1B)]`.
    pub fn process(&mut self, chain: &DescChain, mem: &GuestRam) -> u32 {
        let descs = &chain.descriptors;
        if descs.len() < 2 {
            return 0;
        }
        let header = &descs[0];
        let status_desc = &descs[descs.len() - 1];
        let data = &descs[1..descs.len() - 1];

        let mut hdr = [0u8; 16];
        if header.len < 16 || !mem.read_slice(header.addr, &mut hdr) {
            self.set_status(mem, status_desc.addr, VIRTIO_BLK_S_IOERR);
            return 1;
        }
        let req_type = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
        let sector = u64::from_le_bytes(hdr[8..16].try_into().unwrap());

        let (status, written) = match req_type {
            VIRTIO_BLK_T_IN => self.read_to_guest(mem, data, sector),
            VIRTIO_BLK_T_OUT => (self.write_from_guest(mem, data, sector), 0),
            VIRTIO_BLK_T_FLUSH => {
                let s = if self.file.flush().is_ok() { VIRTIO_BLK_S_OK } else { VIRTIO_BLK_S_IOERR };
                (s, 0)
            }
            VIRTIO_BLK_T_GET_ID => self.get_id(mem, data),
            _ => (VIRTIO_BLK_S_UNSUPP, 0),
        };
        self.set_status(mem, status_desc.addr, status);
        written + 1 // include the status byte
    }

    fn read_to_guest(&mut self, mem: &GuestRam, data: &[Desc], sector: u64) -> (u8, u32) {
        let mut written = 0u32;
        let mut off = sector * SECTOR_SIZE;
        for d in data {
            let mut buf = vec![0u8; d.len as usize];
            if self.file.seek(SeekFrom::Start(off)).is_err() || self.file.read_exact(&mut buf).is_err() {
                return (VIRTIO_BLK_S_IOERR, written);
            }
            if !mem.write_slice(d.addr, &buf) {
                return (VIRTIO_BLK_S_IOERR, written);
            }
            written += d.len;
            off += u64::from(d.len);
        }
        (VIRTIO_BLK_S_OK, written)
    }

    fn write_from_guest(&mut self, mem: &GuestRam, data: &[Desc], sector: u64) -> u8 {
        let mut off = sector * SECTOR_SIZE;
        for d in data {
            let mut buf = vec![0u8; d.len as usize];
            if !mem.read_slice(d.addr, &mut buf) {
                return VIRTIO_BLK_S_IOERR;
            }
            if self.file.seek(SeekFrom::Start(off)).is_err() || self.file.write_all(&buf).is_err() {
                return VIRTIO_BLK_S_IOERR;
            }
            off += u64::from(d.len);
        }
        VIRTIO_BLK_S_OK
    }

    fn get_id(&self, mem: &GuestRam, data: &[Desc]) -> (u8, u32) {
        if let Some(d) = data.first() {
            let mut buf = vec![0u8; d.len as usize];
            let n = (d.len as usize).min(DEVICE_ID.len());
            buf[..n].copy_from_slice(&DEVICE_ID[..n]);
            if mem.write_slice(d.addr, &buf) {
                return (VIRTIO_BLK_S_OK, d.len);
            }
        }
        (VIRTIO_BLK_S_IOERR, 0)
    }

    fn set_status(&self, mem: &GuestRam, addr: u64, status: u8) {
        mem.write_slice(addr, &[status]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    use crate::virtio::guest_ram::GuestRam;
    use crate::virtio::queue::{Desc, DescChain};

    const BASE: u64 = 0x4000_0000;
    const HDR: u64 = BASE + 0x100;
    const DATA: u64 = BASE + 0x200;
    const STATUS: u64 = BASE + 0x800;

    fn mem(backing: &mut Vec<u8>) -> GuestRam {
        GuestRam::new(backing.as_mut_ptr(), backing.len(), BASE)
    }

    /// A two-sector file: sector 0 all 0xAA, sector 1 all 0xBB.
    fn disk() -> File {
        let mut f = tempfile::tempfile().unwrap();
        f.write_all(&[0xAAu8; 512]).unwrap();
        f.write_all(&[0xBBu8; 512]).unwrap();
        f
    }

    fn header(m: &GuestRam, req_type: u32, sector: u64) {
        let mut h = [0u8; 16];
        h[0..4].copy_from_slice(&req_type.to_le_bytes());
        h[8..16].copy_from_slice(&sector.to_le_bytes());
        m.write_slice(HDR, &h);
    }

    fn chain(data_len: u32, data_writable: bool) -> DescChain {
        DescChain {
            head: 0,
            descriptors: vec![
                Desc { addr: HDR, len: 16, writable: false },
                Desc { addr: DATA, len: data_len, writable: data_writable },
                Desc { addr: STATUS, len: 1, writable: true },
            ],
        }
    }

    #[test]
    fn read_copies_sector_into_guest() {
        let mut backing = vec![0u8; 0x1000];
        let m = mem(&mut backing);
        header(&m, VIRTIO_BLK_T_IN, 1); // sector 1 = 0xBB
        let mut blk = VirtioBlk::new(disk()).unwrap();
        let written = blk.process(&chain(512, true), &m);
        assert_eq!(written, 513); // 512 data + 1 status
        let mut out = [0u8; 512];
        m.read_slice(DATA, &mut out);
        assert!(out.iter().all(|&b| b == 0xBB));
        assert_eq!(m.read_u16(STATUS).unwrap() & 0xff, VIRTIO_BLK_S_OK as u16);
    }

    #[test]
    fn write_persists_guest_buffer_to_disk() {
        let mut backing = vec![0u8; 0x1000];
        let m = mem(&mut backing);
        header(&m, VIRTIO_BLK_T_OUT, 0);
        m.write_slice(DATA, &[0xCDu8; 512]);
        let mut blk = VirtioBlk::new(disk()).unwrap();
        blk.process(&chain(512, false), &m);
        // Read sector 0 back out of the file.
        let mut buf = [0u8; 512];
        blk.file.seek(SeekFrom::Start(0)).unwrap();
        blk.file.read_exact(&mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0xCD));
    }

    #[test]
    fn unknown_type_is_unsupported() {
        let mut backing = vec![0u8; 0x1000];
        let m = mem(&mut backing);
        header(&m, 0x99, 0);
        let mut blk = VirtioBlk::new(disk()).unwrap();
        blk.process(&chain(16, true), &m);
        let mut s = [0u8; 1];
        m.read_slice(STATUS, &mut s);
        assert_eq!(s[0], VIRTIO_BLK_S_UNSUPP);
    }

    #[test]
    fn capacity_is_file_len_over_512() {
        let blk = VirtioBlk::new(disk()).unwrap();
        assert_eq!(blk.capacity_sectors(), 2);
    }
}
