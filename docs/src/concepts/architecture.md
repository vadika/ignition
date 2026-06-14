# Architecture

ignition is a research microVM for macOS on Apple Silicon, built on Apple's
Hypervisor.framework (HVF). It is architecturally modeled on AWS Firecracker (the
microVM model, the vstate seam, the device set) but it is not a port: it shares
roughly zero lines of Firecracker source. The lineage is the design plus the
rust-vmm building blocks Firecracker also uses (`vm-superio`, `vm-fdt`). The one
genuinely lifted piece is the HVF backend, taken from libkrun and then
substantially reworked.

## Crates

The workspace splits cleanly along the seam between architecture-neutral VMM
logic and the macOS/HVF hypervisor backend.

```text
crates/
  arch/      ignition-arch     aarch64 sysreg tables, boot regs, FDT helpers
  hvf/       ignition-hvf      Hypervisor.framework backend (lifted from libkrun, reworked)
  devices/   ignition-devices  serial / virtio / GIC device implementations
  vmm/       ignition-vmm      the vstate seam: HVF replacement for FC kvm/vm/vcpu
spike/       ignition-spike    the `boot` binary (interactive microVM)
```

Crate library names are `ignition_*`. Because the `hvf` crate was lifted from
libkrun and then reworked (direct `hv_gic_*`, SMP, snapshot/restore), its imports
were updated to match the ignition tree.

## The vstate seam

Firecracker isolates everything KVM-specific behind a small set of files:
`vstate/{kvm,vm,vcpu,memory,interrupts}.rs` plus the MMIO device manager. That is
the surface a VMM has to replace to move off KVM. ignition cuts at the same seam
and substitutes HVF for KVM there:

- `KVM_CREATE_VM` becomes `hv_vm_create`; `KVM_SET_USER_MEMORY_REGION` becomes
  `hv_vm_map`. There is one VM per process on HVF.
- `KVM_CREATE_VCPU` becomes `hv_vcpu_create`, which on HVF must run on the thread
  that will execute the vCPU (the vCPU is thread-affine). This inverts
  Firecracker's create-then-move model: ignition spawns the thread first and
  creates the vCPU inside it.
- The in-kernel GIC is created with `hv_gic_create` instead of `KVM_CREATE_DEVICE`,
  and its state is captured losslessly through `hv_gic_state_*`.
- Interrupt injection has no irqfd. A device interrupt is a synchronous
  `hv_gic_set_spi(line, level)` call plus a wake of any parked vCPU. There is also
  no `KVM_IOEVENTFD`, so every virtio kick is a full exit to userspace.

`ignition-vmm` owns this seam; `ignition-hvf` provides the raw HVF wrappers it
drives.

## The run loop

`KVM_RUN` returns a typed exit. HVF returns a raw `hv_vcpu_exit_t` (a reason plus
the ESR_EL2 syndrome), so ignition decodes the exception itself. The run loop
reads the exit reason (`CANCELED`, `EXCEPTION`, `VTIMER_ACTIVATED`) and, for an
exception, the EC field `(syndrome >> 26) & 0x3f`, then dispatches:

- **MMIO** (Data Abort, EC `0x24`): decode the ISS (access size, source register,
  read/write) and the faulting guest physical address. HVF cannot complete a read
  inside the handler, so ignition stashes the pending read and completes the
  register writeback plus the PC advance on the next run loop entry.
- **System-register trap** (EC `0x18`): decode the packed sysreg id and dispatch
  to a read/write handler. With the in-kernel GIC this class nearly disappears.
- **WFI/WFE idle** (EC `0x1`): this is the idle loop, in userspace. If the virtual
  timer is disabled or masked the vCPU parks indefinitely; otherwise it parks with
  a timeout derived from `CNTV_CVAL_EL0` against `mach_absolute_time()`. A device
  IRQ wakes the parked vCPU over a per-vCPU channel.
- **PSCI** (HVC `0x16` / SMC `0x17`): ignition is the PSCI firmware. It implements
  `PSCI_VERSION`, `SYSTEM_OFF`/`SYSTEM_RESET`, and `CPU_ON` (the SMP path that
  hands an entry point to a parked secondary vCPU thread). SMC needs a manual PC
  advance; HVC does not.

For the source-level mapping of every KVM construct to its HVF replacement, see
[HVF and Firecracker map](../internals/hvf-firecracker-map.md).

## Related

- [Device model](device-model.md) — how devices plug into this architecture.
- [The clone primitive](clone-primitive.md) — the snapshot/restore feature built on it.
