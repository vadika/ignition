# Devices, SMP & networking

ignition wires its devices through a uniform `DeviceManager`: MMIO-window and SPI
allocation, bus registration, FDT-node description, and snapshot enumeration all sit
behind the `MmioDevice` trait.

## Console

A 16550 UART provides a fully bidirectional console. TX drains to stdout; RX buffers
typed input into the UART's RX FIFO, sets the LSR data-ready bit, and raises the RX
interrupt over the same GIC serial line (INTID 32) that TX uses. A reader thread runs
an escape FSM (`Ctrl-A x` quits) and forwards bytes into the device, so a real
interactive root login works: type `root`, get a shell, run commands, detach with
`Ctrl-A x`.

## virtio devices

virtio runs over a generalized virtio-mmio transport: a `VirtioDevice` trait
(`device_id`/`device_features`/`config_read`/`queue_count`/`handle_notify`/`inject_rx`)
with per-queue state, hardened feature-select clamping, and a QueueReady invariant.
Config space (offset >= 0x100) is byte-addressable at any access width, which Linux
needs because it reads multi-byte config fields one byte at a time.

- **virtio-blk** carries the real rootfs over a split virtqueue. The device probes,
  the guest mounts ext4 over the virtqueue, and init runs off the disk. A boot serviced
  roughly 692 virtqueue requests (about 605 reads, 62 writes) through the
  QueueNotify -> walk -> file I/O -> used-ring -> SPI path.
- **virtio-rng, virtio-balloon, and virtio-vsock** round out the block-era device set.

## virtio-net + vmnet

`--net` (opt-in) brings up a virtio-net NIC backed by vmnet.framework in shared/NAT
mode through a C shim. The full data path (TX -> vmnet -> RX -> IRQ on INTID 34 ->
guest) is proven on hardware. The `--net` path needs the vmnet entitlement and must run
under sudo for shared mode; without sudo it fails cleanly with a clear message. The
rootfs auto-brings-up `eth0` and DHCPs on boot, so the guest reaches the internet with
no manual steps.

vmnet survives snapshot/restore: on restore the link is bounced and the guest's
carrier-watch re-runs DHCP. Each clone gets a distinct MAC and IP.

## SMP

`--smp N` (default 1, cap 8) boots a real aarch64 Linux with N vCPUs. Secondaries come
online via PSCI `CPU_ON`, schedule work, and stop on `SYSTEM_OFF`. A `VcpuManager`
owns the linear MPIDR mapping (`mpidr_for(index) = index`) shared by the FDT,
`MPIDR_EL1`, and the `CPU_ON` claim guard; lazy bring-up spawns a thread-affine vCPU
per core. A restored guest reports `nproc == N`. The in-kernel `hv_gic` delivers
SGIs/IPIs and per-cpu vtimers natively, so secondaries need no VMM-side interrupt
plumbing.

```console
target/debug/boot --smp 4 kimage/out/Image kimage/out/rootfs.ext4
# [    0.010315] SMP: Total of 4 processors activated.
# (none):~# nproc
# 4
```

## Clock

A PL031 RTC plus the EL1 virtual timer keep guest time. The vtimer PPI (INTID 27) is
delivered through the in-kernel GIC, and on restore the vtimer offset is set so that
`CNTVCT` resumes continuously across the snapshot rather than jumping forward.
