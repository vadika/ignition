//! Snapshot directory I/O: a JSON state file plus raw memory/gic/disk artifacts.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use hvf::VcpuState;

pub const SNAP_MAGIC: &str = "ignition-snapshot-v2";
pub const SNAP_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmConfig {
    pub mem_size: u64,
    pub vcpu_count: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VmSnapshot {
    pub magic: String,
    pub version: u32,
    pub config: VmConfig,
    pub vcpu: VcpuState,
    pub devices: Vec<crate::device_manager::DeviceRecord>,
}

impl VmSnapshot {
    pub fn new(
        config: VmConfig,
        vcpu: VcpuState,
        devices: Vec<crate::device_manager::DeviceRecord>,
    ) -> Self {
        Self {
            magic: SNAP_MAGIC.to_string(),
            version: SNAP_VERSION,
            config,
            vcpu,
            devices,
        }
    }
}

/// Reject snapshots this binary can't restore.
pub fn check_version(s: &VmSnapshot) -> io::Result<()> {
    if s.magic != SNAP_MAGIC || s.version != SNAP_VERSION {
        return Err(io::Error::other(format!(
            "incompatible snapshot: magic={:?} version={} (want {:?} v{})",
            s.magic, s.version, SNAP_MAGIC, SNAP_VERSION
        )));
    }
    Ok(())
}

#[derive(Debug)]
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
    // Write into a temp dir, then atomically rename into place, so an interrupted
    // write never leaves a half-written snapshot that --restore would read.
    let tmp = dir.with_extension("tmp");
    let _ = fs::remove_dir_all(&tmp); // clear any prior aborted attempt
    fs::create_dir_all(&tmp)?;
    let p = paths(&tmp);
    fs::File::create(&p.memory)?.write_all(ram)?;
    fs::File::create(&p.gic)?.write_all(gic_blob)?;
    fs::copy(disk_src, &p.disk)?;
    let json = serde_json::to_vec_pretty(snap).map_err(io::Error::other)?;
    fs::write(&p.state, json)?;
    let _ = fs::remove_dir_all(dir); // replace any existing snapshot
    fs::rename(&tmp, dir)?;
    Ok(())
}

/// Read + validate a snapshot's metadata (the raw artifacts are loaded by the
/// caller, which owns the mmap/disk lifetimes).
pub fn read_snapshot(dir: &Path) -> io::Result<(VmSnapshot, Vec<u8>, Paths)> {
    let p = paths(dir);
    let snap: VmSnapshot =
        serde_json::from_slice(&fs::read(&p.state)?).map_err(io::Error::other)?;
    check_version(&snap)?;
    let gic = fs::read(&p.gic)?;
    Ok((snap, gic, p))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_vcpu() -> VcpuState {
        VcpuState {
            gp: (0..33).collect(),
            sysregs: vec![(1, 2)],
            vtimer_mask: false,
            vtimer_offset: 0,
            simd: vec![0u128; 32],
            fpcr: 0,
            fpsr: 0,
            icc: vec![(1, 2)],
            host_counter: 0,
        }
    }

    #[test]
    fn snapshot_roundtrips_with_device_records() {
        use crate::device_manager::DeviceRecord;
        use devices::device::FdtKind;
        let snap = VmSnapshot::new(
            VmConfig { mem_size: 0x2000_0000, vcpu_count: 1 },
            sample_vcpu(),
            vec![DeviceRecord {
                id: "serial".into(),
                base: 0x900_0000,
                size: 0x1000,
                spi: 0,
                fdt_kind: FdtKind::Ns16550a,
                state: serde_json::json!({"scratch": 7}),
            }],
        );
        let json = serde_json::to_string(&snap).unwrap();
        let back: VmSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.version, SNAP_VERSION);
        assert_eq!(back.magic, SNAP_MAGIC);
        assert_eq!(back.devices.len(), 1);
        assert_eq!(back.devices[0].id, "serial");
    }

    #[test]
    fn check_version_rejects_old() {
        let bad = serde_json::json!({
            "magic": SNAP_MAGIC, "version": 0,
            "config": {"mem_size": 1, "vcpu_count": 1},
            "vcpu": serde_json::to_value(sample_vcpu()).unwrap(), "devices": []
        });
        let parsed: VmSnapshot = serde_json::from_value(bad).unwrap();
        assert!(check_version(&parsed).is_err());
    }

    #[test]
    fn write_then_read_validates_magic() {
        let dir = tempfile::tempdir().unwrap();
        let disk = dir.path().join("src.img");
        std::fs::write(&disk, b"DISK").unwrap();
        let snap = VmSnapshot::new(
            VmConfig { mem_size: 0x2000_0000, vcpu_count: 1 },
            sample_vcpu(),
            vec![],
        );
        write_snapshot(dir.path(), &snap, &[0u8; 16], &[1u8, 2, 3], &disk).unwrap();
        let (back, gic, p) = read_snapshot(dir.path()).unwrap();
        assert_eq!(back.magic, SNAP_MAGIC);
        assert_eq!(back.version, SNAP_VERSION);
        assert_eq!(gic, vec![1, 2, 3]);
        assert_eq!(std::fs::read(&p.memory).unwrap(), vec![0u8; 16]);
        assert_eq!(std::fs::read(&p.disk).unwrap(), b"DISK");
    }

    #[test]
    fn read_snapshot_rejects_bad_magic() {
        let dir = tempfile::tempdir().unwrap();
        let disk = dir.path().join("src.img");
        std::fs::write(&disk, b"D").unwrap();
        let snap = VmSnapshot::new(
            VmConfig { mem_size: 0x2000_0000, vcpu_count: 1 },
            sample_vcpu(),
            vec![],
        );
        write_snapshot(dir.path(), &snap, &[0u8; 8], &[0u8], &disk).unwrap();
        let p = paths(dir.path());
        // Corrupt the magic
        let mut bad: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&p.state).unwrap()).unwrap();
        bad["magic"] = serde_json::json!("wrong-magic");
        std::fs::write(&p.state, serde_json::to_vec(&bad).unwrap()).unwrap();
        let err = read_snapshot(dir.path()).unwrap_err();
        assert!(err.to_string().contains("magic"), "error should mention magic: {err}");
    }
}
