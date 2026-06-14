//! Device-model vocabulary: the trait + types shared by devices and the
//! DeviceManager. `MmioDevice` extends `BusDevice` so one trait object serves
//! both the bus (upcast) and the manager.

use crate::bus::{BusDevice, BusError};
use serde::{Deserialize, Serialize};

/// Which FDT node shape a device emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FdtKind {
    Ns16550a,
    VirtioMmio,
    Pl031,
    IgnitionFuzz,
}

/// Failures from device placement / restore.
#[derive(Debug)]
pub enum DeviceMgrError {
    WindowExhausted { need: u64, remaining: u64 },
    SpiExhausted,
    BusOverlap(BusError),
    UnknownDeviceId(String),
    StateInvalid { id: String, reason: String },
}

impl std::fmt::Display for DeviceMgrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeviceMgrError::WindowExhausted { need, remaining } => {
                write!(f, "MMIO window exhausted: need {need:#x}, {remaining:#x} left")
            }
            DeviceMgrError::SpiExhausted => write!(f, "SPI range exhausted"),
            DeviceMgrError::BusOverlap(e) => write!(f, "bus overlap: {e}"),
            DeviceMgrError::UnknownDeviceId(id) => write!(f, "no builder for device id {id:?}"),
            DeviceMgrError::StateInvalid { id, reason } => {
                write!(f, "invalid saved state for {id:?}: {reason}")
            }
        }
    }
}

impl std::error::Error for DeviceMgrError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DeviceMgrError::BusOverlap(e) => Some(e),
            _ => None,
        }
    }
}

/// A memory-mapped device the `DeviceManager` can place, describe in the FDT, and
/// snapshot. Extends `BusDevice`: a single `Arc<Mutex<dyn MmioDevice>>` is upcast
/// to `Arc<Mutex<dyn BusDevice>>` for the bus.
pub trait MmioDevice: BusDevice {
    fn fdt_kind(&self) -> FdtKind;
    /// Stable key for the snapshot record, e.g. "serial", "virtio-blk".
    fn snapshot_id(&self) -> &str;
    /// Serialize device state for the snapshot.
    fn save(&self) -> serde_json::Value;
    /// Apply restored state; called after construction, before first run.
    fn restore(&mut self, v: &serde_json::Value) -> Result<(), DeviceMgrError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct Mock {
        id: String,
        last: serde_json::Value,
    }
    impl BusDevice for Mock {}
    impl MmioDevice for Mock {
        fn fdt_kind(&self) -> FdtKind { FdtKind::VirtioMmio }
        fn snapshot_id(&self) -> &str { &self.id }
        fn save(&self) -> serde_json::Value { self.last.clone() }
        fn restore(&mut self, v: &serde_json::Value) -> Result<(), DeviceMgrError> {
            self.last = v.clone();
            Ok(())
        }
    }

    #[test]
    fn mmio_device_upcasts_to_bus_device() {
        let dev: Arc<Mutex<dyn MmioDevice>> =
            Arc::new(Mutex::new(Mock { id: "m".into(), last: serde_json::Value::Null }));
        // Trait upcast (stable since Rust 1.86): MmioDevice -> BusDevice.
        let bus: Arc<Mutex<dyn BusDevice>> = dev.clone();
        bus.lock().unwrap().write(0, 0, &[1]); // default no-op, must not panic
        assert_eq!(dev.lock().unwrap().snapshot_id(), "m");
    }

    #[test]
    fn fdt_kind_serde_roundtrips() {
        let j = serde_json::to_value(FdtKind::Ns16550a).unwrap();
        assert_eq!(serde_json::from_value::<FdtKind>(j).unwrap(), FdtKind::Ns16550a);
    }

    #[test]
    fn save_restore_roundtrips_on_mock() {
        let mut m = Mock { id: "m".into(), last: serde_json::json!({"a": 1}) };
        let saved = m.save();
        m.restore(&serde_json::json!({"a": 2})).unwrap();
        assert_eq!(m.save(), serde_json::json!({"a": 2}));
        assert_eq!(saved, serde_json::json!({"a": 1}));
    }
}
