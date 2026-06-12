# Milestone 2f: interrupt delivery → login prompt — SOLVED

Date: 2026-06-12. Status: **DONE.** A real aarch64 Linux boots on ignition/HVF to
an Alpine `(none) login:` prompt on host stdout. The root cause was the **serial
TX-empty interrupt**, a VMM-side fix — not the vtimer and not virtio, both of
which were already correct. Three theories preceded the right one; the evidence
trail is kept below so the dead ends aren't re-walked.

## The fix (what actually unblocked the boot)

The kernel's interrupt-driven 8250 tty blocks after the 16-byte TX FIFO fills,
waiting for the THRE (TX-holding-register-empty) interrupt. Our 16550
(`vm_superio::Serial`) was wired with a no-op `Trigger`, so that interrupt was
never raised: OpenRC's first service write filled the FIFO and hung, which looked
like a dead boot. printk's console path *polls* THRE, so the kernel banner and
dmesg printed fine — masking the gap until userspace used the tty layer.

Wiring the serial's `Trigger` to pulse the GIC's serial SPI (INTID 32, the same
`hv_gic_set_spi` edge-pulse mechanism virtio already used) unblocked it. OpenRC
then ran every sysinit service to `[ ok ]`, printed `/etc/issue`, and getty
emitted the login prompt.

- `crates/devices/src/serial.rs`: `SerialIrq` enum `{Noop, Gic(Arc<dyn IrqLine>)}`
  impl `vm_superio::Trigger`; the `Gic` variant asserts then deasserts the SPI
  (edge-rising; the GIC latches the edge). `Serial::with_irq(out, irq)` selects it;
  `Serial::new(out)` keeps the `Noop` line for the output-only smoke harnesses.
- `spike/src/bin/boot.rs`: `GicIrq { gic, intid }` now carries the absolute INTID;
  the serial is wired with `intid = SERIAL_SPI + 32` (= 32), virtio with
  `VIRTIO_SPI + 32` (= 33).

Reproduce: `target/debug/boot kimage/out/Image kimage/out/rootfs.ext4` reaches
`(none) login:` (~236 console lines) in ~30 s. NB: re-sign after any rebuild —
`cargo build --workspace` relinks `boot` and strips the hypervisor entitlement
(`hv_vm_create` then fails with `VmCreate`); `scripts/sign.sh target/debug/boot`.

## Evidence trail (theories disproven before the right one)

1. **vtimer delivery — WRONG.** `HV_EXIT_REASON_VTIMER_ACTIVATED` never fires; the
   in-kernel `hv_gic` delivers the EL1 vtimer natively. The list-register injection
   experiment was moot and was reverted.
2. **virtio completion-IRQ — WRONG.** Logging every block request: 711 requests in
   ~31 s, all `status = 0`, across distinct sectors — the guest acks every
   completion. virtio + `hv_gic_set_spi` delivery were already correct.
3. **rootfs init / controlling-tty — WRONG (this doc's earlier conclusion).** The
   boot *looked* gated on OpenRC/getty config because output stopped mid-banner.
   `init=/sbin/getty` then printed exactly ~16 chars (`Welcome to Alpin`) before
   stopping — exactly the TX FIFO size — which finally fingered the serial TX
   interrupt as the real, VMM-side cause.

## What also landed earlier on this milestone

- `spike/src/bin/boot.rs` `FlushWriter`: the console sink flushes each byte, so a
  newline-less prompt (`login: `) is visible instead of sitting in stdout's line
  buffer (commit 39d0d51). Necessary but not sufficient — the serial IRQ was the
  real blocker.

## Remaining (next milestone, optional)

- **Serial RX** for interactivity: host stdin in raw mode → 16550 RBR + RX
  interrupt, so typing at the login prompt works. Same `hv_gic_set_spi` path the
  TX interrupt now proves.

## Bottom line

The ignition VMM boots a real aarch64 Linux to a userspace login prompt with a
working virtio-blk rootfs, native virtual timer, and full interrupt delivery
(virtio completion + serial TX). The shell-prompt bar is met; only interactive
input (serial RX) remains.
