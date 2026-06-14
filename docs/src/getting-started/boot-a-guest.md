# Boot a Linux guest

The `boot` binary loads an aarch64 kernel + rootfs, runs the vCPU(s), and gives
an interactive 16550 console. **Re-sign after every build** — relinking strips
the hypervisor entitlement.

```console
cargo build -p ignition-spike --bin boot
scripts/sign.sh target/debug/boot

# boot to a shell (log in as root)
target/debug/boot kimage/out/Image kimage/out/rootfs.ext4

target/debug/boot --smp 4 kimage/out/Image kimage/out/rootfs.ext4   # multi-vCPU (SMP)
target/debug/boot --net  kimage/out/Image kimage/out/rootfs.ext4    # vmnet NAT networking
```

Console keys: `Ctrl-A s` = snapshot, `Ctrl-A x` = quit, `Ctrl-A b` = balloon.

Snapshot and restore are covered in [The clone primitive](../concepts/clone-primitive.md) and [Snapshot & restore](../features/snapshot-restore.md).
