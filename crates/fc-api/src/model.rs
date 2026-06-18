//! Firecracker REST request/response bodies. Field names match the Firecracker
//! API so an unmodified firecracker-go-sdk / flintlock client serializes to them.
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Deserialize)]
pub struct MachineConfig {
    pub vcpu_count: u64,
    pub mem_size_mib: u64,
    #[serde(default)]
    pub track_dirty_pages: bool,
}

#[derive(Debug, Default, Deserialize)]
pub struct BootSource {
    pub kernel_image_path: String,
    #[serde(default)]
    pub boot_args: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Drive {
    pub drive_id: String,
    pub path_on_host: String,
    #[serde(default)]
    pub is_root_device: bool,
    #[serde(default)]
    pub is_read_only: bool,
}

#[derive(Debug, Default, Deserialize)]
pub struct NetworkInterface {
    pub iface_id: String,
    #[serde(default)]
    pub host_dev_name: Option<String>,
    #[serde(default)]
    pub guest_mac: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Action {
    pub action_type: String, // "InstanceStart" | "SendCtrlAltDel" | "FlushMetrics"
}

#[derive(Debug, Deserialize)]
pub struct VmUpdate {
    pub state: String, // "Paused" | "Resumed"
}

#[derive(Debug, Deserialize)]
pub struct SnapshotCreate {
    pub snapshot_path: String,
    #[serde(default)]
    pub mem_file_path: Option<String>,
    #[serde(default)]
    pub snapshot_type: Option<String>, // accepted, ignored (boot decides Full/Diff)
}

#[derive(Debug, Deserialize)]
pub struct SnapshotLoad {
    pub snapshot_path: String,
    #[serde(default)]
    pub mem_file_path: Option<String>,
    #[serde(default = "default_true")]
    pub resume_vm: bool,
    #[serde(default)]
    pub enable_diff_snapshots: bool, // accepted, ignored
}
fn default_true() -> bool { true }

#[derive(Debug, Serialize)]
pub struct InstanceInfo {
    pub id: String,
    pub state: String, // "Not started" | "Running" | "Paused"
    pub vmm_version: String,
    pub app_name: String,
}

#[derive(Debug, Serialize)]
pub struct Fault {
    pub fault_message: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_real_go_sdk_machine_config() {
        // Body shape firecracker-go-sdk PUT /machine-config emits.
        let j = r#"{"vcpu_count":2,"mem_size_mib":1024,"smt":false,"track_dirty_pages":true}"#;
        let mc: MachineConfig = serde_json::from_str(j).unwrap();
        assert_eq!(mc.vcpu_count, 2);
        assert_eq!(mc.mem_size_mib, 1024);
        assert!(mc.track_dirty_pages); // unknown fields like smt are ignored
    }
    #[test]
    fn parses_drive_and_snapshot_create() {
        let d: Drive = serde_json::from_str(
            r#"{"drive_id":"rootfs","path_on_host":"/x/rootfs.ext4","is_root_device":true,"is_read_only":false}"#,
        ).unwrap();
        assert!(d.is_root_device);
        let s: SnapshotCreate = serde_json::from_str(
            r#"{"snapshot_path":"/s/snap","mem_file_path":"/s/mem","snapshot_type":"Full"}"#,
        ).unwrap();
        assert_eq!(s.snapshot_path, "/s/snap");
    }
    #[test]
    fn snapshot_load_defaults_resume_true() {
        let l: SnapshotLoad = serde_json::from_str(r#"{"snapshot_path":"/s/snap"}"#).unwrap();
        assert!(l.resume_vm);
    }
}
