//! Accumulated VM config (from the PUT routes) and its mapping to `boot` flags.
use crate::model::{BootSource, Drive, MachineConfig, NetworkInterface};

#[derive(Debug, Default)]
pub struct VmConfig {
    pub vcpu_count: Option<u64>,
    pub mem_size_mib: Option<u64>,
    pub track_dirty_pages: bool,
    pub kernel_image_path: Option<String>,
    pub boot_args: Option<String>,
    pub root_drive_path: Option<String>,
    pub has_root: bool,
    pub net: bool,
}

impl VmConfig {
    pub fn set_machine(&mut self, m: MachineConfig) {
        self.vcpu_count = Some(m.vcpu_count);
        self.mem_size_mib = Some(m.mem_size_mib);
        self.track_dirty_pages = m.track_dirty_pages;
    }
    pub fn set_boot_source(&mut self, b: BootSource) {
        self.kernel_image_path = Some(b.kernel_image_path);
        self.boot_args = b.boot_args;
    }
    /// Returns Err for a second root device (v1 supports one rootfs positional).
    pub fn set_drive(&mut self, d: Drive) -> Result<(), String> {
        if d.is_root_device {
            if self.has_root {
                return Err("only one root device is supported".to_string());
            }
            self.has_root = true;
            self.root_drive_path = Some(d.path_on_host);
        }
        Ok(())
    }
    pub fn set_net(&mut self, _n: NetworkInterface) {
        // socket_vmnet backend; host_dev_name / guest_mac are accepted but ignored.
        self.net = true;
    }

    /// Map to a `boot` argv tail: [flags...] <kernel> <rootfs>. Caller prepends
    /// the boot binary path and appends --control-sock/--store/etc.
    pub fn to_boot_flags(&self) -> Result<Vec<String>, String> {
        let kernel = self.kernel_image_path.clone()
            .ok_or("no boot-source configured")?;
        let rootfs = self.root_drive_path.clone()
            .ok_or("no root drive configured")?;
        let mut v = Vec::new();
        if let Some(n) = self.vcpu_count { v.push("--smp".into()); v.push(n.to_string()); }
        if let Some(m) = self.mem_size_mib { v.push("--mem".into()); v.push(m.to_string()); }
        if self.track_dirty_pages { v.push("--track-dirty".into()); }
        if let Some(a) = &self.boot_args { v.push("--append".into()); v.push(a.clone()); }
        if self.net { v.push("--net".into()); }
        v.push(kernel);
        v.push(rootfs);
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;
    fn full() -> VmConfig {
        let mut c = VmConfig::default();
        c.set_machine(MachineConfig { vcpu_count: 2, mem_size_mib: 1024, track_dirty_pages: true });
        c.set_boot_source(BootSource { kernel_image_path: "/k/Image".into(), boot_args: Some("ro".into()) });
        c.set_drive(Drive { drive_id: "rootfs".into(), path_on_host: "/r.ext4".into(), is_root_device: true, is_read_only: false }).unwrap();
        c.set_net(NetworkInterface { iface_id: "eth0".into(), host_dev_name: None, guest_mac: None });
        c
    }
    #[test]
    fn maps_full_config_to_flags() {
        let v = full().to_boot_flags().unwrap();
        assert_eq!(v, vec![
            "--smp","2","--mem","1024","--track-dirty","--append","ro","--net","/k/Image","/r.ext4",
        ].into_iter().map(String::from).collect::<Vec<_>>());
    }
    #[test]
    fn missing_kernel_is_err() {
        let mut c = full();
        c.kernel_image_path = None;
        assert!(c.to_boot_flags().unwrap_err().contains("boot-source"));
    }
    #[test]
    fn missing_root_is_err() {
        let mut c = full();
        c.root_drive_path = None;
        assert!(c.to_boot_flags().unwrap_err().contains("root drive"));
    }
    #[test]
    fn second_root_device_rejected() {
        let mut c = full();
        let err = c.set_drive(Drive { drive_id: "d2".into(), path_on_host: "/2".into(), is_root_device: true, is_read_only: false });
        assert!(err.is_err());
    }
}
