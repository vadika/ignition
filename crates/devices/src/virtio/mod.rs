//! Synchronous, exit-driven virtio-mmio block device.

pub mod blk;
pub mod guest_ram;
pub mod mmio;
pub mod net;
pub mod queue;

/// A device interrupt line. Implemented by the boot harness over the GIC.
pub trait IrqLine: Send + Sync {
    /// Assert (`true`) or deassert (`false`) the device's interrupt.
    fn set_spi(&self, level: bool);
}
