// Device models for ignition.
//
// Phase 0: empty. Phase 1 lands the serial/UART MMIO device (first consumer of
// the MMIO-write exit the spike already produces) so a guest kernel reaches a
// serial prompt, then virtio-blk (sync engine — io_uring dropped on macOS).
//
// References to lift from:
//   firecracker/src/vmm/src/devices/legacy/serial.rs   (UART model)
//   libkrun/src/devices/src/legacy/{vcpu.rs,gicv3.rs,hvfgicv3.rs}  (GIC + ICC traps)
//   libkrun/src/vmm/src/device_manager/hvf/mmio.rs     (MMIO bus without irqfd)

pub mod boot_timer;
pub mod bus;
pub mod device;
pub mod fuzz;
pub mod rtc;
pub mod serial;
pub mod virtio;
pub mod display;
