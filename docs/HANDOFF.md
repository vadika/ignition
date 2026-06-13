# Project Handoff: Firecracker → macOS/HVF Port (research project)

> **Status (2026-06): historical planning document — kept for lineage/rationale.**
> Phases 1–2 have largely shipped (boot-to-shell, SMP, virtio-blk/net/rng/balloon/vsock,
> PL031 RTC, snapshot/restore including multi-vCPU and a lazy clonefile+mmap fast-restore).
> One premise below is **disproven**: in-kernel `hv_gic` *does* expose lossless state
> get/set (`hv_gic_state_*`), so the GIC is snapshotted directly (`crates/hvf/src/gic.rs`) —
> the "userspace-GIC-for-snapshottability" tradeoff never materialized. Still open: the REST
> API and dirty-tracking/diff snapshots. See `docs/*-result.md` for what was built.

This document transfers context from a planning conversation. Read it together with
`firecracker-hvf-porting-map.md` (detailed file-by-file analysis) before starting work.

## Goal

Research project: port AWS Firecracker (microVM VMM) to macOS on Apple Silicon, replacing
KVM with Hypervisor.framework (HVF). Permanently diverged fork — Firecracker upstream is
explicitly KVM-only by design tenet, so this needs its own name and repo.

## Why it's feasible — prior art

- **libkrun** (containers/libkrun, Apache-2.0, same license as Firecracker) is itself
  derived from Firecracker's codebase and already runs on HVF/macOS-ARM64. Its entire
  macOS-specific layer is ~2,400 hand-written lines. It is the reference implementation
  for this port; its `hvf` crate and `hvfgicv3.rs` can be lifted nearly verbatim.
- krunkit (libkrun frontend) proves production viability, including GPU (Venus/Vulkan).
- Apple's `container`/`containerization` (Virtualization.framework, closed VMM) is the
  benchmark target, not a building block.

## Where the novelty is (research contributions, in priority order)

1. **Snapshot/restore on HVF** — nobody has this on macOS. Firecracker's killer feature.
   Two hard sub-problems:
   - Dirty page tracking: no KVM_GET_DIRTY_LOG equivalent; implement via `hv_vm_protect`
     write-protection + fault logging.
   - GIC state: in-kernel `hv_gic` has NO state get/set API. Decision required: userspace
     GICv3 (libkrun's legacy gicv3.rs — fully snapshottable, slower, every ICC_* access
     traps) vs in-kernel hv_gic (fast, opaque state, lossy snapshots). This perf-vs-
     snapshottability trade-off is itself a publishable analysis.
2. **Firecracker REST machine-config API on macOS** — lets firecracker-go-sdk and existing
   orchestration target Macs unmodified. This is the differentiator vs just using libkrun.
3. **Benchmarks** vs Apple container (Virtualization.framework) and vs Linux/KVM
   Firecracker: boot time, density, memory overhead, snapshot resume latency. Note: HVF
   has no KVM_IOEVENTFD, so every virtio kick is a full exit→userspace round trip —
   measure this delta explicitly.

## Key technical findings from source reading (June 2026, both repos at main)

- KVM coupling seam in Firecracker: `src/vmm/src/vstate/{kvm,vm,vcpu,memory,interrupts}.rs`,
  `device_manager/mmio.rs`, gdb target. That's what gets replaced/forked.
- HVF exit model: raw ESR_EL2 syndrome decoding in userspace. libkrun handles exactly 6
  exception classes (DATAABORT→MMIO, SYSTEMREGISTERTRAP, WFX, HVC, SMC, BKPT).
- Gotchas already documented in the porting map:
  - MMIO reads are deferred — complete register writeback + PC advance on NEXT run() entry.
  - HVF vCPUs are thread-affine: hv_vcpu_create must run on the executing thread (inverts
    Firecracker's create-then-move model; kick via hv_vcpu_request_exit, not signals).
  - WFI/WFE traps to userspace — you implement the idle loop (park on channel with
    CNTV_CVAL-derived timeout against mach_absolute_time).
  - You are the PSCI firmware (VERSION, SYSTEM_OFF/RESET, CPU_ON via channel to parked
    secondary vCPU threads; SMC needs manual PC advance, HVC doesn't).
  - MPIDR: write vcpuid to Aff1 or in-kernel GIC redistributor IDs won't match.
  - vtimer: manual mask/unmask sync per exit (hv_vcpu_set_vtimer_mask).
  - Boot regs identical to KVM path: PC=entry, X0=FDT, CPSR=PSTATE_FAULT_BITS_64.
  - No TAP on macOS: virtio-net over unixgram/unixstream to gvproxy/passt (krunkit
    pattern) or vmnet (needs root or restricted entitlement).
  - vsock: Firecracker's is pure userspace — ports as-is. io_uring block engine: drop,
    keep sync. event-manager is epoll: port to kqueue or shim with mio.
  - Jailer/seccomp: no Linux equivalent; stub first, Seatbelt later.
  - Nested virt available (M3+/macOS 15+): EL2 boot path exists in libkrun (HCR_EL2,
    CNTHCTL_EL2, ID_AA64PFR0_EL1 EL2+GIC3 bits, mask SME in ID_AA64PFR1_EL1 or guest
    hangs after MMU enable).
- Targeting macOS 15/26+ only is the sane choice (hv_gic_* APIs are macOS 15+; libkrun
  dlopens them for backward compat — direct linking is fine if we require 15+).
- Entitlement: com.apple.security.hypervisor; ad-hoc codesign suffices for local dev.

## Phased plan

1. **Boot-to-shell** (~weeks): lift libkrun's hvf crate + hvfgicv3; new
   vstate/hvf_{vm,vcpu}.rs mirroring libkrun's macos/vstate.rs; virtio-blk (sync) +
   serial + vsock; kqueue event loop; FDT from FC's fdt.rs minus cache_info.rs (775 loc
   of Linux sysfs parsing — no macOS equivalent). Single vCPU first.
2. **Parity-ish** (~month): SMP via PSCI CPU_ON channels; virtio-net via gvproxy;
   Firecracker REST API; balloon via hv_vm_unmap.
3. **Research core** (~months): snapshot/restore + dirty tracking + GIC decision +
   benchmarks.

## Concrete first task (validation spike)

Scaffold a minimal consumer of the lifted hvf crate that boots a kernel (from libkrunfw
or Apple's containerization kernel config) to a serial prompt on macOS 26 / Apple
Silicon. Goal: confirm the lifted code compiles against the current macOS SDK headers
before committing to fork structure.

## Repos

- https://github.com/firecracker-microvm/firecracker (fork base)
- https://github.com/containers/libkrun (reference; lift src/hvf/, hvfgicv3.rs,
  macos/vstate.rs patterns; Apache-2.0)
- https://github.com/libkrun/krunkit (networking patterns: gvproxy unixgram, vfkit magic)
- https://github.com/apple/containerization (kernel config, benchmark target)

## Reading order (paths verified)

```
libkrun/src/hvf/src/lib.rs                       # hypervisor abstraction, 731 loc
libkrun/src/vmm/src/macos/vstate.rs              # threading, WFE parking, run_emulation
libkrun/src/devices/src/legacy/hvfgicv3.rs       # in-kernel GIC wrapper, 183 loc
libkrun/src/devices/src/legacy/vcpu.rs           # VcpuList: IRQ bookkeeping, ICC traps
libkrun/src/devices/src/legacy/gicv3.rs          # userspace GIC (snapshot-friendly)
libkrun/src/arch/src/aarch64/macos/sysreg.rs     # ESR sysreg encoding macros
libkrun/src/vmm/src/device_manager/hvf/mmio.rs   # MMIO bus without irqfd
firecracker/src/vmm/src/vstate/{kvm,vm,vcpu}.rs  # the seam to cut
firecracker/src/vmm/src/arch/aarch64/            # boot regs, FDT, GIC snapshot code
```

## Environment notes for Claude Code

- Development machine must be Apple Silicon Mac, macOS 15+ (26 preferred).
- Rust toolchain, aarch64-apple-darwin target. bindgen for Hypervisor.h if regenerating
  bindings (or reuse libkrun's checked-in bindings.rs, 4,712 loc).
- codesign with entitlements plist containing com.apple.security.hypervisor after every
  build, or hv_vm_create returns HV_DENIED.
- Guest kernel: libkrunfw bundles one; Apple containerization repo has an optimized
  config + containerized build env. Kata kernel config also works.
