# Milestone 2e result: virtio-blk root mounts, init runs (shell gated on IRQ delivery)

Date: 2026-06-12. Host: macOS 26.5.1, Apple Silicon. Guest: Linux 6.1 aarch64 +
alpine arm64 rootfs (`kimage/out/{Image,rootfs.ext4}`).

```
target/debug/boot kimage/out/Image kimage/out/rootfs.ext4   # after scripts/sign.sh
```

## What works (verified end to end)

The synchronous virtio-mmio block device carries the real alpine rootfs:

- `virtio_blk virtio0: [vda] 196608 512-byte logical blocks (96.0 MiB)` — our
  device probed; capacity correct.
- `EXT4-fs (vda): recovery complete` / `mounted filesystem` /
  `VFS: Mounted root (ext4 filesystem) on device 254:0` — root mounted over our
  virtqueue.
- `Run /sbin/init as init process` → `OpenRC 0.52.1` — init runs off the disk.
- Instrumentation showed ~692 virtqueue requests serviced during boot (605 reads,
  62 writes, varying heads/sizes) — the device handles real, varied block I/O
  through the QueueNotify → walk → file-I/O → used-ring → set_spi path.

So milestones 2a–2e all compose: FDT + in-kernel GIC + kernel load + boot regs +
serial + the virtio-blk rootfs.

Two integration fixes were needed and landed on this branch:
1. **`/chosen/rng-seed`** (re-added; dropped in 2a) — without it the kernel CRNG
   never initializes and early userspace `getrandom()` blocks. After the fix:
   `random: crng init done`.

## Where it stops (and why)

OpenRC prints its banner and then the guest goes **0.0% CPU — fully idle, parked
in WFI** waiting for an interrupt that never arrives. It is not stuck on virtio
(I/O completes) and not busy-spinning; it is blocked on **interrupt delivery**:

- **Timer:** `NoIrqVcpus::set_vtimer_irq` is a no-op, and there is no `hv_gic`
  PPI-injection API, so the EL1 virtual-timer PPI never reaches the guest. The
  kernel boots (busy-wait) but userspace `sleep`/service-timeouts hang. This is
  exactly the "vtimer with in-kernel GIC" path flagged as unproven in milestone
  2b.
- **Async virtio / general IRQ wakeup:** the run loop's WFI handling is the
  earlycon-grade bounded sleep (no channel parking that wakes on an injected
  IRQ). A device interrupt raised while the vCPU is parked does not promptly wake
  it.

The shell prompt therefore needs the **next milestone (2f): interrupt delivery** —
real vtimer PPI injection on the in-kernel GIC, plus channel-based WFI parking
that wakes on `set_spi`. (Serial RX for interactivity rides along there.)

## Status

The 2e code (GuestRam, split virtqueue, virtio-blk, virtio-mmio transport, FDT
node, harness wiring) is complete, unit-tested (14 device tests + the arch FDT
tests), reviewed, and demonstrably mounts the rootfs and runs init. The literal
"shell prompt on stdout" bar is blocked by the separately-scoped IRQ-delivery
work, not by the block device.
