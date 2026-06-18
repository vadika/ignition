//! Snapshot directory I/O: a JSON state file plus raw memory/gic/disk artifacts.

use std::ffi::CString;
use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use ignition_hvf::VcpuState;

// macOS APFS copy-on-write clone. `<sys/clonefile.h>`; flags are clonefile_flags_t (u32).
unsafe extern "C" {
    fn clonefile(src: *const libc::c_char, dst: *const libc::c_char, flags: u32) -> libc::c_int;
}

pub const SNAP_MAGIC: &str = "ignition-snapshot-v3";
pub const SNAP_VERSION: u32 = 3;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmConfig {
    pub mem_size: u64,
    pub vcpu_count: u64,
}

/// Whether a snapshot is self-contained (`Full`) or stores only the delta from a
/// parent (`Diff`). Diff layers form an immutable chain via `SnapshotManifest::parent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SnapshotType {
    Full,
    Diff,
}

/// Human/management metadata for a base snapshot, written as `manifest.json`
/// alongside the machine state. Distinct from `vmstate.json` (the machine state).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotManifest {
    pub name: String,
    pub mem_size: u64,
    pub vcpu_count: u64,
    pub snapshot_type: SnapshotType,
    pub parent: Option<String>,
}

impl SnapshotManifest {
    /// A self-contained base snapshot with no parent.
    pub fn new_full(name: String, mem_size: u64, vcpu_count: u64) -> Self {
        Self {
            name,
            mem_size,
            vcpu_count,
            snapshot_type: SnapshotType::Full,
            parent: None,
        }
    }

    /// A diff layer recording only the delta from `parent`.
    pub fn new_diff(name: String, parent: String, mem_size: u64, vcpu_count: u64) -> Self {
        Self {
            name,
            mem_size,
            vcpu_count,
            snapshot_type: SnapshotType::Diff,
            parent: Some(parent),
        }
    }
}

/// One vCPU's saved state plus the MPIDR identifying which core it is. A
/// multi-vCPU snapshot carries one of these per online core.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VcpuCheckpoint {
    pub mpidr: u64,
    pub state: VcpuState,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VmSnapshot {
    pub magic: String,
    pub version: u32,
    pub config: VmConfig,
    pub vcpus: Vec<VcpuCheckpoint>,
    pub devices: Vec<crate::device_manager::DeviceRecord>,
}

impl VmSnapshot {
    pub fn new(
        config: VmConfig,
        vcpus: Vec<VcpuCheckpoint>,
        devices: Vec<crate::device_manager::DeviceRecord>,
    ) -> Self {
        Self {
            magic: SNAP_MAGIC.to_string(),
            version: SNAP_VERSION,
            config,
            vcpus,
            devices,
        }
    }
}

/// Reject snapshots this binary can't restore.
pub fn check_version(s: &VmSnapshot) -> io::Result<()> {
    if s.magic != SNAP_MAGIC || s.version != SNAP_VERSION {
        return Err(io::Error::other(format!(
            "unsupported snapshot {} v{} (need {} v{})",
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
    pub manifest: PathBuf,
}

pub fn paths(dir: &Path) -> Paths {
    Paths {
        memory: dir.join("memory.bin"),
        gic: dir.join("gic.bin"),
        disk: dir.join("disk.img"),
        state: dir.join("vmstate.json"),
        manifest: dir.join("manifest.json"),
    }
}

/// `<store>/snapshots/<name>` — the immutable base directory for a named snapshot.
pub fn base_dir(store: &Path, name: &str) -> PathBuf {
    store.join("snapshots").join(name)
}

/// `<store>/instances/<name>-<pid>` — the ephemeral CoW instance directory.
pub fn instance_dir(store: &Path, name: &str, pid: u32) -> PathBuf {
    store.join("instances").join(format!("{name}-{pid}"))
}

/// Write `manifest.json` into an existing base directory.
pub fn write_manifest(dir: &Path, manifest: &SnapshotManifest) -> io::Result<()> {
    let json = serde_json::to_vec_pretty(manifest).map_err(io::Error::other)?;
    fs::write(paths(dir).manifest, json)
}

/// Read `manifest.json` from a base directory.
pub fn read_manifest(dir: &Path) -> io::Result<SnapshotManifest> {
    let bytes = fs::read(paths(dir).manifest)?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

/// Walk a diff chain from `leaf` to its Full root, returning the manifests
/// ordered root..leaf. Rejects a missing parent layer and a cycle.
pub fn resolve_chain(store: &Path, leaf: &str) -> io::Result<Vec<SnapshotManifest>> {
    let mut chain = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut cur = leaf.to_string();
    loop {
        if !seen.insert(cur.clone()) {
            return Err(io::Error::other(format!("snapshot chain cycle at {cur}")));
        }
        let m = read_manifest(&base_dir(store, &cur))
            .map_err(|e| io::Error::other(format!("missing layer {cur}: {e}")))?;
        let parent = m.parent.clone();
        chain.push(m);
        match parent {
            Some(p) => cur = p,
            None => break, // reached Full root
        }
    }
    chain.reverse(); // root..leaf
    Ok(chain)
}

/// Copy `src` to `dst` using APFS `clonefile(2)` (O(1), copy-on-write) when
/// possible, falling back to a byte copy on filesystems that don't support it.
/// `dst` must not already exist. The result is always an independent file: writing
/// to it never mutates `src`.
pub fn clonefile_or_copy(src: &Path, dst: &Path) -> io::Result<()> {
    let csrc = CString::new(src.as_os_str().as_bytes()).map_err(io::Error::other)?;
    let cdst = CString::new(dst.as_os_str().as_bytes()).map_err(io::Error::other)?;
    let rc = unsafe { clonefile(csrc.as_ptr(), cdst.as_ptr(), 0) };
    if rc == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        // Not APFS, or src and dst are on different filesystems: fall back.
        Some(libc::ENOTSUP) | Some(libc::EXDEV) | Some(libc::ENOSYS) => {
            log::warn!(
                "clonefile unsupported ({err}); falling back to byte copy: {} -> {}",
                src.display(),
                dst.display()
            );
            fs::copy(src, dst)?;
            Ok(())
        }
        _ => Err(err),
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
    clonefile_or_copy(disk_src, &p.disk)?;
    let json = serde_json::to_vec_pretty(snap).map_err(io::Error::other)?;
    fs::write(&p.state, json)?;
    let _ = fs::remove_dir_all(dir); // replace any existing snapshot
    fs::rename(&tmp, dir)?;
    Ok(())
}

/// Write a Diff layer atomically. Same staging-then-rename discipline as
/// [`write_snapshot`], but `memory.bin` holds only the `dirty` pages packed
/// back-to-back (plus a `dirty.idx` sidecar) instead of the whole RAM. GIC,
/// `vmstate.json`, and `disk.img` (clonefile of `disk_src`) are written in full.
pub fn write_diff_snapshot(
    dir: &Path,
    snap: &VmSnapshot,
    dirty: &[u64],
    ram: &[u8],
    gic_blob: &[u8],
    disk_src: &Path,
) -> io::Result<()> {
    let tmp = dir.with_extension("tmp");
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp)?;
    let p = paths(&tmp);
    fs::File::create(&p.gic)?.write_all(gic_blob)?;
    clonefile_or_copy(disk_src, &p.disk)?;
    let json = serde_json::to_vec_pretty(snap).map_err(io::Error::other)?;
    fs::write(&p.state, json)?;
    write_diff_pages(&tmp, dirty, ram)?; // memory.bin (packed) + dirty.idx
    let _ = fs::remove_dir_all(dir);
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

/// Write a diff layer: the dirty pages packed back-to-back into `memory.bin`
/// (ascending page-index order) and the sorted page indices into `dirty.idx`
/// as little-endian u64. `dirty` must be sorted page indices into `ram`.
///
/// Invariant: guest RAM (`mem_size`) is always a multiple of [`crate::dirty::PAGE`]
/// because it derives from `--mem` in MiB (1 MiB = 64 × 16 KiB), so every dirty
/// page index maps to a full in-bounds page of `ram`. The bounds guard below
/// exists purely as defense against a future non-aligned size: it turns the only
/// slice-out-of-bounds panic path into a clean `Err` and never fires for
/// MiB-sized RAM.
pub fn write_diff_pages(dir: &Path, dirty: &[u64], ram: &[u8]) -> io::Result<()> {
    let page = crate::dirty::PAGE;
    let mut mem = fs::File::create(dir.join("memory.bin"))?;
    for &p in dirty {
        let o = p as usize * page;
        if o + page > ram.len() {
            return Err(io::Error::other(format!(
                "dirty page {p} out of RAM bounds ({} pages of {page}B, ram {}B)",
                dirty.len(),
                ram.len()
            )));
        }
        mem.write_all(&ram[o..o + page])?;
    }
    let mut idx = fs::File::create(dir.join("dirty.idx"))?;
    for &p in dirty {
        idx.write_all(&p.to_le_bytes())?;
    }
    Ok(())
}

/// Read a diff layer written by [`write_diff_pages`]: returns the page indices
/// (from `dirty.idx`) and the packed dirty-page bytes (from `memory.bin`).
pub fn read_diff_pages(dir: &Path) -> io::Result<(Vec<u64>, Vec<u8>)> {
    let raw = fs::read(dir.join("dirty.idx"))?;
    let idx: Vec<u64> = raw
        .chunks_exact(8)
        .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
        .collect();
    let packed = fs::read(dir.join("memory.bin"))?;
    Ok((idx, packed))
}

/// Overlay a diff layer's packed pages onto `target` (the mmap'd guest RAM).
/// `idx[i]` is the destination page index for `packed[i*PAGE..(i+1)*PAGE]`.
pub fn apply_diff(target: &mut [u8], idx: &[u64], packed: &[u8]) -> io::Result<()> {
    let page = crate::dirty::PAGE;
    if packed.len() != idx.len() * page {
        return Err(io::Error::other("diff packed size mismatch"));
    }
    for (i, &p) in idx.iter().enumerate() {
        let o = p as usize * page;
        if o + page > target.len() {
            return Err(io::Error::other("diff page out of range"));
        }
        target[o..o + page].copy_from_slice(&packed[i * page..(i + 1) * page]);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_pack_apply_roundtrip() {
        let page = crate::dirty::PAGE;
        let mut ram = vec![0u8; 4 * page];
        for (i, b) in ram.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        let dirty = vec![1u64, 3];
        let dir = tempfile::tempdir().unwrap();
        write_diff_pages(dir.path(), &dirty, &ram).unwrap();
        let (idx, packed) = read_diff_pages(dir.path()).unwrap();
        assert_eq!(idx, dirty);
        assert_eq!(packed.len(), 2 * page);
        let mut target = vec![0u8; 4 * page];
        apply_diff(&mut target, &idx, &packed).unwrap();
        for &p in &dirty {
            let o = p as usize * page;
            assert_eq!(&target[o..o + page], &ram[o..o + page]);
        }
        assert!(target[0..page].iter().all(|&b| b == 0)); // page 0 untouched
        assert!(target[2 * page..3 * page].iter().all(|&b| b == 0)); // page 2 untouched
    }

    #[test]
    fn write_diff_pages_rejects_out_of_bounds_index() {
        let page = crate::dirty::PAGE;
        let ram = vec![0u8; 2 * page]; // 2 pages
        let dir = tempfile::tempdir().unwrap();
        // index 2 is out of bounds for a 2-page ram
        assert!(write_diff_pages(dir.path(), &[2u64], &ram).is_err());
    }

    #[test]
    fn apply_diff_rejects_out_of_range() {
        let page = crate::dirty::PAGE;
        let mut target = vec![0u8; page]; // 1 page
        let packed = vec![7u8; page];
        assert!(apply_diff(&mut target, &[5u64], &packed).is_err());
    }

    #[test]
    fn apply_diff_rejects_size_mismatch() {
        let page = crate::dirty::PAGE;
        let mut target = vec![0u8; 2 * page];
        let packed = vec![0u8; page - 1]; // not a multiple
        assert!(apply_diff(&mut target, &[0u64], &packed).is_err());
    }

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
        use ignition_devices::device::FdtKind;
        let snap = VmSnapshot::new(
            VmConfig { mem_size: 0x2000_0000, vcpu_count: 1 },
            vec![VcpuCheckpoint { mpidr: 0, state: sample_vcpu() }],
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
            "vcpus": [{"mpidr": 0, "state": serde_json::to_value(sample_vcpu()).unwrap()}], "devices": []
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
            vec![VcpuCheckpoint { mpidr: 0, state: sample_vcpu() }],
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
            vec![VcpuCheckpoint { mpidr: 0, state: sample_vcpu() }],
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

    #[test]
    fn manifest_roundtrip_with_type_and_parent() {
        let m = SnapshotManifest::new_diff("child".into(), "root".into(), 1 << 20, 1);
        let j = serde_json::to_vec(&m).unwrap();
        let back: SnapshotManifest = serde_json::from_slice(&j).unwrap();
        assert_eq!(back, m);
        assert_eq!(back.snapshot_type, SnapshotType::Diff);
        assert_eq!(back.parent.as_deref(), Some("root"));
        assert_eq!(SnapshotManifest::new_full("root".into(), 1 << 20, 1).parent, None);
    }

    #[test]
    fn v2_snapshot_rejected() {
        let s = VmSnapshot {
            magic: "ignition-snapshot-v2".to_string(),
            version: 2,
            config: VmConfig { mem_size: 0, vcpu_count: 0 },
            vcpus: vec![],
            devices: vec![],
        };
        assert!(check_version(&s).is_err());
    }

    #[test]
    fn manifest_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let m = SnapshotManifest::new_full("brave-hopper".to_string(), 1 << 30, 4);
        write_manifest(dir.path(), &m).unwrap();
        let back = read_manifest(dir.path()).unwrap();
        assert_eq!(back, m);
        assert_eq!(back.mem_size, 1 << 30);
        assert_eq!(back.vcpu_count, 4);
    }

    #[test]
    fn store_paths_are_well_formed() {
        let store = Path::new("/tmp/vmstore");
        assert_eq!(base_dir(store, "foo"), Path::new("/tmp/vmstore/snapshots/foo"));
        assert_eq!(
            instance_dir(store, "foo", 1234),
            Path::new("/tmp/vmstore/instances/foo-1234")
        );
    }

    #[test]
    fn clonefile_or_copy_duplicates_and_isolates() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.bin");
        let dst = dir.path().join("dst.bin");
        fs::write(&src, b"hello world").unwrap();

        clonefile_or_copy(&src, &dst).unwrap();
        assert_eq!(fs::read(&dst).unwrap(), b"hello world");

        // Editing the clone must NOT change the source (CoW / copy isolation).
        fs::write(&dst, b"CHANGED!!!!").unwrap();
        assert_eq!(fs::read(&src).unwrap(), b"hello world");
    }

    #[test]
    fn snapshot_roundtrips_multiple_vcpus() {
        let snap = VmSnapshot::new(
            VmConfig { mem_size: 0x2000_0000, vcpu_count: 4 },
            vec![
                VcpuCheckpoint { mpidr: 0, state: sample_vcpu() },
                VcpuCheckpoint { mpidr: 1, state: sample_vcpu() },
                VcpuCheckpoint { mpidr: 2, state: sample_vcpu() },
                VcpuCheckpoint { mpidr: 3, state: sample_vcpu() },
            ],
            vec![],
        );
        let json = serde_json::to_string(&snap).unwrap();
        let back: VmSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.config.vcpu_count, 4);
        assert_eq!(back.vcpus.len(), 4);
        let mpidrs: Vec<u64> = back.vcpus.iter().map(|c| c.mpidr).collect();
        assert_eq!(mpidrs, vec![0, 1, 2, 3]);
    }

    fn put_manifest(store: &Path, manifest: &SnapshotManifest) {
        let d = base_dir(store, &manifest.name);
        fs::create_dir_all(&d).unwrap();
        write_manifest(&d, manifest).unwrap();
    }

    #[test]
    fn resolve_chain_root_to_leaf() {
        let store = tempfile::tempdir().unwrap();
        put_manifest(store.path(), &SnapshotManifest::new_full("root".into(), 4096, 1));
        put_manifest(
            store.path(),
            &SnapshotManifest::new_diff("mid".into(), "root".into(), 4096, 1),
        );
        put_manifest(
            store.path(),
            &SnapshotManifest::new_diff("leaf".into(), "mid".into(), 4096, 1),
        );
        let names: Vec<_> = resolve_chain(store.path(), "leaf")
            .unwrap()
            .into_iter()
            .map(|m| m.name)
            .collect();
        assert_eq!(names, vec!["root", "mid", "leaf"]);
    }

    #[test]
    fn resolve_chain_missing_parent_errors() {
        let store = tempfile::tempdir().unwrap();
        put_manifest(
            store.path(),
            &SnapshotManifest::new_diff("orphan".into(), "ghost".into(), 4096, 1),
        );
        assert!(resolve_chain(store.path(), "orphan").is_err());
    }

    #[test]
    fn resolve_chain_cycle_errors() {
        let store = tempfile::tempdir().unwrap();
        put_manifest(
            store.path(),
            &SnapshotManifest::new_diff("a".into(), "b".into(), 4096, 1),
        );
        put_manifest(
            store.path(),
            &SnapshotManifest::new_diff("b".into(), "a".into(), 4096, 1),
        );
        assert!(resolve_chain(store.path(), "a").is_err());
    }
}
