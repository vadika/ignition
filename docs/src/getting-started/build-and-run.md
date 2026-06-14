# Build & run

The runnable artifact is `boot`; it needs the hypervisor entitlement, which
relinking strips, so re-sign after every build.

```console
cargo build
# the runnable artifact is `boot`; it needs the hypervisor entitlement before it
# can call hv_vm_create — re-sign after every build (relinking strips it):
cargo build -p ignition-spike --bin boot
scripts/sign.sh target/debug/boot
# usage (kernel + rootfs) is in "Boot a Linux guest" below.
```

## Requirements

Apple Silicon Mac, macOS 15+ (26 preferred), Rust 1.96+ (edition 2024).
