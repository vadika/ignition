//! Snapshot directory I/O: a JSON state file plus raw memory/gic/disk artifacts.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use devices::serial::SerialSnapshot;
use devices::virtio::mmio::VirtioMmioState;
use hvf::VcpuState;

pub const SNAP_MAGIC: &str = "ignition-snapshot-v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MmioWindow {
    pub base: u64,
    pub size: u64,
    pub spi: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmConfig {
    pub mem_size: u64,
    pub vcpu_count: u64,
    pub serial: MmioWindow,
    pub blk: MmioWindow,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceState {
    pub blk: VirtioMmioState,
    pub serial: SerialSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmSnapshot {
    pub magic: String,
    pub config: VmConfig,
    pub vcpu: VcpuState,
    pub devices: DeviceState,
}

impl VmSnapshot {
    pub fn new(config: VmConfig, vcpu: VcpuState, devices: DeviceState) -> Self {
        Self {
            magic: SNAP_MAGIC.to_string(),
            config,
            vcpu,
            devices,
        }
    }
}

pub struct Paths {
    pub memory: PathBuf,
    pub gic: PathBuf,
    pub disk: PathBuf,
    pub state: PathBuf,
}

pub fn paths(dir: &Path) -> Paths {
    Paths {
        memory: dir.join("memory.bin"),
        gic: dir.join("gic.bin"),
        disk: dir.join("disk.img"),
        state: dir.join("vmstate.json"),
    }
}

/// Write the full snapshot. `ram` is the guest RAM slice; `gic_blob` the GIC state;
/// `disk_src` the live rootfs path (copied into the snapshot).
pub fn write_snapshot(
    dir: &Path,
    snap: &VmSnapshot,
    ram: &[u8],
    gic_blob: &[u8],
    disk_src: &Path,
) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let p = paths(dir);
    fs::File::create(&p.memory)?.write_all(ram)?;
    fs::File::create(&p.gic)?.write_all(gic_blob)?;
    fs::copy(disk_src, &p.disk)?;
    let json = serde_json::to_vec_pretty(snap).map_err(io::Error::other)?;
    fs::write(&p.state, json)?;
    Ok(())
}

/// Read + validate a snapshot's metadata (the raw artifacts are loaded by the
/// caller, which owns the mmap/disk lifetimes).
pub fn read_snapshot(dir: &Path) -> io::Result<(VmSnapshot, Vec<u8>, Paths)> {
    let p = paths(dir);
    let snap: VmSnapshot =
        serde_json::from_slice(&fs::read(&p.state)?).map_err(io::Error::other)?;
    if snap.magic != SNAP_MAGIC {
        return Err(io::Error::other(format!("bad snapshot magic: {}", snap.magic)));
    }
    let gic = fs::read(&p.gic)?;
    Ok((snap, gic, p))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> VmSnapshot {
        VmSnapshot::new(
            VmConfig {
                mem_size: 0x2000_0000,
                vcpu_count: 1,
                serial: MmioWindow { base: 0x0900_0000, size: 0x1000, spi: 0 },
                blk: MmioWindow { base: 0x0a00_0000, size: 0x200, spi: 1 },
            },
            VcpuState {
                gp: (0..33).collect(),
                sysregs: vec![(1, 2)],
                vtimer_mask: false,
                vtimer_offset: 0,
                simd: vec![0u128; 32],
                fpcr: 0,
                fpsr: 0,
            },
            DeviceState {
                blk: VirtioMmioState {
                    status: 0xf,
                    queue_sel: 0,
                    device_features_sel: 0,
                    interrupt_status: 0,
                    queues: vec![],
                },
                serial: SerialSnapshot {
                    baud_divisor_low: 1,
                    baud_divisor_high: 0,
                    interrupt_enable: 0xf,
                    interrupt_identification: 1,
                    line_control: 3,
                    line_status: 0x60,
                    modem_control: 0,
                    modem_status: 0,
                    scratch: 0,
                },
            },
        )
    }

    #[test]
    fn snapshot_json_round_trips() {
        let s = sample();
        let json = serde_json::to_string(&s).unwrap();
        let back: VmSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn write_then_read_validates_magic() {
        let dir = tempfile::tempdir().unwrap();
        let disk = dir.path().join("src.img");
        std::fs::write(&disk, b"DISK").unwrap();
        write_snapshot(dir.path(), &sample(), &[0u8; 16], &[1u8, 2, 3], &disk).unwrap();
        let (snap, gic, p) = read_snapshot(dir.path()).unwrap();
        assert_eq!(snap, sample());
        assert_eq!(gic, vec![1, 2, 3]);
        assert_eq!(std::fs::read(&p.memory).unwrap(), vec![0u8; 16]);
        assert_eq!(std::fs::read(&p.disk).unwrap(), b"DISK");
    }
}
