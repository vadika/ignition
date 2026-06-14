//! The `ignition-fuzz` MMIO device and host/guest control protocol (M0).

pub mod device;
pub mod protocol;

pub use device::FuzzDevice;
