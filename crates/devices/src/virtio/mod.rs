//! Synchronous, exit-driven virtio-mmio block device.

pub mod balloon;
pub mod blk;
pub mod guest_ram;
pub mod mmio;
pub mod net;
pub mod queue;
pub mod rng;

/// A device interrupt line. Implemented by the boot harness over the GIC.
pub trait IrqLine: Send + Sync {
    /// Assert (`true`) or deassert (`false`) the device's interrupt.
    fn set_spi(&self, level: bool);
}

/// An `IrqLine` that drops all assertions — for irq-less construction (tests,
/// and the manager before a real GIC line is attached).
pub struct NoopIrq;
impl IrqLine for NoopIrq {
    fn set_spi(&self, _level: bool) {}
}
