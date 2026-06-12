# Milestone 2d result: real Linux kernel booted on ignition/HVF

Date: 2026-06-12. Host: macOS 26.5.1, Apple Silicon.
Guest: Linux 6.1.0 aarch64 (Firecracker `microvm-kernel-ci-aarch64-6.1.config`),
built via `kimage/build/build-kernel.sh`. Booted with:

```
cargo build -p hvf-spike --bin boot
scripts/sign.sh target/debug/boot
target/debug/boot kimage/out/Image          # 2>diag  1>guest-console
```

## Outcome: exceeded the milestone bar (earlycon → full kernel boot)

The success criterion was earlycon output. The kernel went much further — it
booted to the init/rootfs handoff (214 lines of console), then panicked only
because no root filesystem was provided (expected: no `root=`, no virtio-blk yet).

Harness diagnostics:
```
kernel : 16923136 bytes, entry=0x40000000
dtb    : 1326 bytes @ 0x5fe00000
gic    : dist=[0x3ffd0000, 0x10000] redist=[0x3ffe0000, 0x20000]
cmdline: console=ttyS0 earlycon=uart8250,mmio,0x9000000 reboot=k panic=1
```

Key proofs that every prior milestone composed correctly:
- `Machine model: linux,dummy-virt` — the FDT root node (2a).
- `earlycon: uart8250 at MMIO 0x0000000009000000` + 200+ console lines — the 16550
  serial over the MMIO bus (UART-echo milestone) and `default_cmdline` (2c).
- `NUMA: Faking a node at [mem 0x40000000-0x5fffffff]` — the RAM layout (2c).
- `psci: PSCIv0.2 detected in firmware` — the FDT psci node + HVC conduit; PSCI
  `SYSTEM_OFF` at the end was handled by the run loop → clean exit.
- `GICv3: 988 SPIs implemented`, `CPU0: found redistributor 0 region 0:0x3ffe0000`
  — the in-kernel `hv_gic` (2b), at exactly the redistributor address `HvfGicV3`
  computed.
- `arch_timer: cp15 timer(s) running at 24.00MHz (virt)`, clocksource +
  `sched_clock` registered, BogoMIPS calibrated — the virtual timer worked; the
  run loop's bounded WFI/`WaitForEventTimeout` parking + vtimer masking (2d) was
  sufficient.

Final lines:
```
[    0.046760] VFS: Cannot open root device "(null)" or unknown-block(0,0): error -6
[    0.046965] Kernel panic - not syncing: VFS: Unable to mount root fs on unknown-block(0,0)
[    0.048841] Rebooting in 1 seconds..
== guest requested shutdown (PSCI SYSTEM_OFF) -> [vcpu exited cleanly]
```

## Next (2e): rootfs → shell

`kimage/out/rootfs.ext4` (alpine arm64, busybox, ttyS0 console, passwordless
root) is built and waiting. Reaching a shell needs a **virtio-mmio block device**
to back it (or an initramfs path), `root=/dev/vda` on the cmdline, plus the
virtio-mmio FDT node + MMIO dispatch. That is milestone 2e.
