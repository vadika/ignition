# Device model

ignition wires every device through one uniform path. A `DeviceManager` owns the
set of devices, and each device implements the `MmioDevice` trait. That single
abstraction handles MMIO-window and SPI allocation, bus dispatch, FDT node
emission, and snapshot enumeration, so adding a device does not mean touching the
boot path, the FDT generator, and the snapshot writer separately.

## DeviceManager and the MmioDevice trait

`DeviceManager` centralizes what would otherwise be scattered per-device
plumbing:

- **MMIO / SPI allocation.** The manager hands each device a slice of the MMIO
  address window and an SPI line, so device placement is decided in one place
  instead of being hard-coded per device.
- **Bus dispatch.** A guest MMIO access (decoded from a Data Abort in the run
  loop) is routed to the device whose window contains the faulting address.
- **FDT node emission.** Each device describes its own FDT node (`reg`, interrupt,
  compatible string). The FDT generator walks the manager rather than hard-listing
  devices, so the device tree the guest sees always matches the devices that are
  actually wired.
- **Snapshot hooks.** Each device emits a `DeviceRecord` at snapshot time and is
  reconstructed from one at restore. The snapshot format is a self-describing list
  of these records rather than a hand-maintained struct of device fields.

Because the same `DeviceManager` describes devices for both a fresh boot and a
restore, there is a single device-wiring site. Boot and restore drive the same
code to allocate windows, register on the bus, and produce or consume device
records, which keeps the two paths from drifting apart.

## The shipped device set

ignition implements the full Firecracker aarch64 device set:

- **virtio-blk** for the root filesystem.
- **virtio-net** over a vmnet NAT backend.
- **virtio-rng** backed by host entropy.
- **virtio-balloon** for on-demand memory reclaim.
- **virtio-vsock** for guest-to-host streams.
- **PL031 RTC** for wall-clock time.
- **boot-timer**, a magic-MMIO probe that reports guest boot time (and that the
  fuzzer reuses as a control-plane doorbell).

For the per-device behavior, the networking model, and the SMP wiring, see
[Devices, SMP & networking](../features/devices.md).

## Related

- [Architecture](architecture.md) — where the device manager sits in the VMM.
- [The clone primitive](clone-primitive.md) — how `DeviceRecord` snapshot hooks are used.
- [VM internal API (MMIO)](vm-internal-api.md) — the guest-facing MMIO contract (boot-timer, fuzz device).
