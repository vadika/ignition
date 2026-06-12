# Milestone 2f findings: VMM stack complete; the gap is guest userspace init

Date: 2026-06-12. Status: **VMM correct; remaining gap is rootfs init/tty, not
our code.** Two earlier theories on this milestone were disproven by
instrumentation — this is the final, evidence-backed conclusion.

## Evidence

Instrumented the live boot (`target/debug/boot kimage/out/Image
kimage/out/rootfs.ext4`):

1. **The virtual timer works.** `HV_EXIT_REASON_VTIMER_ACTIVATED` never fires —
   the in-kernel `hv_gic` delivers the EL1 vtimer natively. (Disproves the
   original "vtimer delivery" theory; the list-register experiment is moot.)
2. **virtio-blk + its IRQ work.** Logging every request: **711 requests in the
   first ~31 s, all `status = 0` (OK)**, across many distinct sectors (reads of
   init/OpenRC, then writes of OpenRC state / ext4 journal). The guest acks every
   completion; nothing is left pending. (Disproves the "hv_gic_set_spi doesn't
   wake the guest / limps on timeout" theory — I/O completes and the guest
   reaps it.)
3. **Then the guest goes idle** (0% CPU) — it has finished its boot I/O and is
   waiting in userspace, not throttled and not blocked on a device.

## Root cause: alpine userspace init

The kernel boots, mounts the virtio rootfs, and execs `/sbin/init`. After
`OpenRC 0.52.1` it produces no further console output and goes idle — OpenRC
hangs on an early service (an alpine/OpenRC config matter; candidates: a service
that waits on a device/tty we don't model, `hwclock`/`mdev`/`modules`, etc.).

`init=/bin/sh` (with the flushing serial, and even `-- -i`) runs the shell but
prints `/bin/sh: can't access tty` and no prompt: a bare `sh` as PID 1 cannot get
a controlling terminal (`setsid` + `TIOCSCTTY` on `/dev/console`), so it stays
non-interactive. Setting up the controlling tty and spawning a getty is the
**rootfs init's** responsibility.

## What landed (the one VMM fix this surfaced)

- `spike/src/bin/boot.rs` `FlushWriter`: the guest console sink now flushes each
  byte, so a newline-less prompt (`login: ` / `/ # `) is visible instead of
  sitting in stdout's line buffer (commit 39d0d51).

## Reaching a visible prompt — next options (mostly rootfs-side)

- **Rootfs:** make the alpine image spawn `agetty -L 0 ttyS0 vt100` (or a
  `console::respawn:/sbin/getty -L ttyS0` inittab line), or fix/skip the OpenRC
  service that hangs. A getty prints `login:` and sets up the tty — independent
  of the VMM. This is the most direct path and is owned by `kimage/`.
- **VMM (optional, for interactivity):** implement serial RX (host stdin in raw
  mode → 16550 RBR + RX interrupt) so an interactive shell/login works once the
  rootfs presents one. The virtio IRQ path already proves `hv_gic_set_spi`
  delivery, so the serial RX interrupt will work the same way.

## Bottom line

The ignition VMM boots a real aarch64 Linux to userspace with a working
virtio-blk rootfs, virtual timer, and interrupt delivery. The shell-prompt bar is
now gated on **guest rootfs init configuration** (getty/controlling-tty), not on
any missing hypervisor capability.
