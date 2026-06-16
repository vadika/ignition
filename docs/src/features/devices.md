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

## virtio-vsock

virtio-vsock carries stream connections between host and guest over the virtio
transport. E1 (guest→host) exposes per-port host listeners at `{uds}_{port}`: a guest
process connecting to a vsock port surfaces on the host as a connection to the matching
Unix socket path.

### vsock host→guest (E2)

A host process opens a connection *into* a listening guest over the same control
socket, using Firecracker's hybrid protocol:

1. The host connects to `{uds}` (the base path of `--vsock-uds`) and sends
   `CONNECT <guest_port>\n`.
2. ignition allocates an ephemeral host port, signals the guest (`REQUEST`), and the
   guest's listener accepts (`RESPONSE`).
3. ignition replies `OK <host_port>\n` to the host; raw bytes then flow both ways on
   that same connection. If no guest process is listening, the connection is closed.

```console
# guest init runs e.g.:  socat VSOCK-LISTEN:5000,fork EXEC:cat
socat - UNIX-CONNECT:/tmp/ignition-vsock-e2 <<<'CONNECT 5000'
```

Guest→host (E1) and host→guest (E2) coexist; per-port paths `{uds}_{port}` remain the
E1 guest→host listeners.

For a full worked example with `socat` servers and clients on both ends, see the
[vsock round-trip use case](https://github.com/vadika/ignition/blob/main/examples/vsock-roundtrip.md).

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

## GUI display (software-rendered)

`boot --gui <kernel> <rootfs>` opens a 1280x800 macOS window backed by a CPU
framebuffer (`winit` + `softbuffer`, no Metal). On macOS the windowing event loop
must own the main thread, so under `--gui` the entire VMM — vCPU threads, the serial
console reader, the vsock reactor, and the vmnet RX feeder — runs on spawned threads
while the event loop runs on main. The present path is non-blocking and coalesces to
the latest frame, so a slow or frozen window never backpressures the guest. Closing
the window ends the session (the process exits, tearing the disposable guest down);
the serial console keeps working alongside the window.

Without `--gui` (the default), and for `--restore` and `--fuzz`, behavior is
unchanged: no window opens and the vCPU loop runs on the main thread as before.

A **virtio-gpu** device (2D only, device id 16) is added under `--gui`: the Linux
`virtio_gpu` driver binds it, `/dev/dri/card0` and `/dev/fb0` appear, and the kernel
framebuffer console renders live in the macOS window. `RESOURCE_FLUSH` from the guest
presents the scanned-out resource through the display sink; `TRANSFER_TO_HOST_2D`
copies guest pixels (scatter-gather correct) into a host buffer. No 3D/VIRGL/Venus, no
display resize or hotplug, and snapshot of GPU state is a later milestone.

The guest kernel must be built with `CONFIG_DRM`, `CONFIG_DRM_VIRTIO_GPU`,
`CONFIG_DRM_FBDEV_EMULATION`, `CONFIG_FB`, and `CONFIG_FRAMEBUFFER_CONSOLE`.

Under `--gui`, two **virtio-input** devices (device id 18) make the window interactive:
a keyboard (`EV_KEY`) and an absolute tablet (`EV_ABS` x/y + buttons). The winit event
loop translates host key/pointer/click events into Linux evdev events and injects them
into the guest's eventq (the `inject_rx`-style path), so typing logs in at the console
and the pointer tracks the macOS cursor 1:1 over the 1280x800 scanout. The guest kernel
needs `CONFIG_VIRTIO_INPUT=y` and `CONFIG_INPUT_EVDEV=y`.

### Wayland compositor (cage + foot)

With the GUI rootfs (`rootfs-gui.ext4`, built by `kimage/build/build-rootfs-gui.sh`),
`--gui` runs a **cage** Wayland kiosk (wlroots **pixman** software renderer — no GL,
matching the 2D-only virtio-gpu) hosting a **foot** terminal: an interactive
software-rendered Linux desktop in the macOS window, driven by the virtio-input keyboard
+ pointer. The compositor path exercises fenced virtio-gpu commands — page-flips set
`VIRTIO_GPU_FLAG_FENCE`, and the device signals the fence in its response so wlroots's
render loop keeps producing frames (without it the compositor renders one frame then
stalls). The minimal base rootfs has no compositor and uses the framebuffer console.

The GUI guest also snapshots and restores: the virtio-gpu resource table + scanout
binding and the virtio-input config state survive a snapshot, and `boot --gui --restore
<name>` reopens the window and repaints the resumed desktop before the guest runs (the
device re-reads the scanout from the restored backing — no pixel bytes are stored). A
headless `--restore` (no `--gui`) restores the same guest to the serial console with
frames discarded. Because each restore gets its own copy-on-write instance, one
warm-base snapshot fans out into N independent desktops — see
`scripts/fanout-gui.sh N <base>`. With `--net` (under `sudo`) each clone also gets its
own MAC and DHCP lease, since the GUI rootfs runs the `netwatch` carrier-poller that
rebinds virtio-net on restore.

## Related

- [Device model](../concepts/device-model.md) — the trait these devices implement.
- [Snapshot & restore](snapshot-restore.md) — how device state survives a snapshot.
