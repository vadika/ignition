# Firecracker → macOS/HVF Porting Map

> **Status (2026-06): historical analysis — kept as the FC↔HVF reference.** The KVM→HVF
> mapping (§3), run-loop/ESR decode (§4), threading inversion (§5), and interrupt-path (§6)
> remain accurate. But §1/§7's premise that in-kernel `hv_gic` has "no state get/set API"
> (opaque, lossy snapshots) is **disproven** — `hv_gic_state_*` gives lossless GIC
> save/restore (`crates/hvf/src/gic.rs`), so the userspace-GIC tradeoff did not arise.
> Phases 1–2 and HVF snapshot/restore have shipped; dirty-tracking/diff snapshots and the
> REST API remain open.

Derived from a side-by-side reading of `containers/libkrun` (HVF backend, originally a Firecracker
fork) and `firecracker-microvm/firecracker` (current main), June 2026. libkrun is Apache-2.0, same
license as Firecracker — its HVF code can be lifted nearly verbatim.

## 1. Size of the problem

| Component | libkrun (HVF/macOS) | Firecracker (KVM) | Notes |
|---|---|---|---|
| Hypervisor wrapper | `src/hvf/src/lib.rs` — 731 loc | `kvm-ioctls`/`kvm-bindings` crates (external) | + 4,712 loc bindgen output from `Hypervisor.h` (mechanical) |
| vCPU/VM state machine | `src/vmm/src/macos/vstate.rs` — 731 loc | `src/vmm/src/vstate/{vcpu,vm,kvm}.rs` + `linux/vstate.rs` equivalent ~2,055 loc | macOS side is *smaller* |
| GIC (in-kernel) | `hvfgicv3.rs` — 183 loc | `arch/aarch64/gic/` — ~1,800 loc incl. full register save/restore | FC's bulk is snapshot support |
| GIC (userspace fallback) | `gicv3.rs` + `legacy/vcpu.rs` ICC trap handling | n/a (KVM always in-kernel) | needed pre-macOS 15; possibly needed again for snapshots (§7) |
| Sysreg trap table | `arch/src/aarch64/macos/sysreg.rs` — 146 loc, 38 registers | n/a (KVM handles in-kernel) | |
| Arch boot/FDT | `arch/src/aarch64/` — ~720 loc | `arch/aarch64/{mod,fdt,regs,cache_info}.rs` — ~2,580 loc | FC's `cache_info.rs` (775 loc) parses Linux sysfs — no macOS equivalent, synthesize or omit |
| MMIO device manager | `device_manager/hvf/mmio.rs` — 569 loc | `device_manager/mmio.rs` (KVM-coupled via irqfd) | |

Total hand-written macOS-specific code in libkrun: **~2,400 lines.**

## 2. KVM coupling seam in Firecracker

Files using `kvm_ioctls`/`kvm_bindings` outside `arch/{aarch64,x86_64}` (the surface to abstract or fork):

```
src/vmm/src/vstate/kvm.rs          — hypervisor handle           → replace with HvfVm
src/vmm/src/vstate/vm.rs           — VM + memory regions          → hv_vm_create / hv_vm_map
src/vmm/src/vstate/vcpu.rs         — vCPU threads + run loop      → biggest rewrite (§3, §5)
src/vmm/src/vstate/memory.rs       — GuestMemoryMmap + dirty log  → mmap ports as-is; dirty log: no HVF API (§7)
src/vmm/src/vstate/interrupts.rs   — irqfd                        → direct GIC injection (§6)
src/vmm/src/device_manager/mmio.rs — irqfd registration           → sweep
src/vmm/src/device_manager/acpi.rs — x86 only                     → drop (aarch64-only port)
src/vmm/src/gdb/*                  — KVM debug regs               → drop initially
```

Firecracker upstream tenets are explicitly KVM-only — plan for a permanently diverged fork.

## 3. API mapping: KVM → HVF

| KVM | HVF | Divergence |
|---|---|---|
| `KVM_CREATE_VM` | `hv_vm_create(config)` | one VM per process on HVF |
| `KVM_SET_USER_MEMORY_REGION` | `hv_vm_map(uva, gpa, size, RWX)` | near 1:1; `hv_vm_unmap` for ballooning |
| `KVM_CREATE_VCPU` (fd, movable) | `hv_vcpu_create` (thread-bound) | **must be called on the thread that runs it** (§5) |
| `KVM_RUN` → typed `VcpuExit` | `hv_vcpu_run` → raw `hv_vcpu_exit_t` (reason + ESR syndrome) | you decode ESR_EL2 yourself (§4) |
| `KVM_SET_ONE_REG` | `hv_vcpu_set_reg` / `hv_vcpu_set_sys_reg` | different reg ID encodings (KVM u64 ids vs HVF enums) |
| in-kernel GIC via `KVM_CREATE_DEVICE` | `hv_gic_create(config)` (macOS 15+) | no state get/set API (§7) |
| irqfd (`KVM_IRQFD`) | none — `hv_gic_set_spi(line, level)` synchronous call | every device interrupt path changes (§6) |
| signal-based vCPU kick | `hv_vcpu_request_exit` → exit reason `CANCELED` | replaces FC's `KVM_KICK_SIGNAL` machinery |
| in-kernel PSCI | none — you are the PSCI firmware | (§4.4) |
| WFI blocks in kernel | `EC_WFX_TRAP` exits to userspace | you implement the idle loop (§4.3) |
| `KVM_GET_DIRTY_LOG` | **nothing** | dirty tracking via `hv_vm_protect` write faults — research item |
| vtimer handled in-kernel | `HV_EXIT_REASON_VTIMER_ACTIVATED` + `hv_vcpu_set_vtimer_mask` | manual mask/unmask sync each exit |

Bindings note: libkrun loads `hv_gic_*` via `dlopen`/`libloading` from
`/System/Library/Frameworks/Hypervisor.framework` so the binary still runs on macOS < 15.
Targeting macOS 15/26-only allows direct linking.

## 4. The run loop (libkrun `hvf/src/lib.rs::run`, the Rosetta stone)

HVF exit reasons: `CANCELED` (kicked), `EXCEPTION` (the real one), `VTIMER_ACTIVATED`.
For `EXCEPTION`, decode `(syndrome >> 26) & 0x3f` (EC field). libkrun handles exactly six classes:

### 4.1 EC_DATAABORT (0x24) → MMIO
Manual ISS decode: `isv` (bit 24), `iswrite` (bit 6), `sas` (bits 23:22, len = 1<<sas),
`srt` (bits 20:16, register number; 31 = xzr). Faulting GPA from `exception.physical_address`.

**Deferred-read gotcha:** HVF cannot complete the read in the handler. libkrun stashes
`pending_mmio_read {addr, len, srt}` plus `pending_advance_pc = true`, returns
`VcpuExit::MmioRead(pa, &mut buf)`; the *next* `run()` entry writes the bus result into Xn
and advances PC by 4 before re-entering the guest. Writes are simpler: read Xsrt, hand bytes
to the bus, advance PC. KVM hides all of this.

### 4.2 EC_SYSTEMREGISTERTRAP (0x18)
Decode `isread` (bit 0), `rt` (bits 9:5), reg = syndrome & SYSREG_MASK (op0/op1/op2/CRn/CRm
packed — see `macos/sysreg.rs` encoding macro, 38 registers). Dispatch to
`Vcpus::handle_sysreg_read/write` — used by the userspace GIC for `ICC_*` registers
(`ICC_IAR1_EL1`, `ICC_SGI1R_EL1` for SGIs/IPIs, `ICC_EOIR1_EL1`, priority regs), plus
debug regs (`MDCCINT_EL1`, `OSLAR/OSDLR`) as ignore-writes. With in-kernel `hv_gic` this
class nearly disappears.

### 4.3 EC_WFX_TRAP (0x1) — the userspace idle loop
Read `CNTV_CTL_EL0`: if timer disabled or masked → park indefinitely (`WaitForEvent`).
Else read `CNTV_CVAL_EL0`, compare against `mach_absolute_time()`; if already expired,
re-enter; else compute `Duration` from `cntfrq` and park with timeout
(`WaitForEventTimeout`). Parking = blocking on a per-vCPU crossbeam channel
(`recv_timeout`); device IRQ injection sends on the channel to wake. This **is** the vCPU
idle loop, in userspace.

### 4.4 EC_AA64_HVC (0x16) / EC_AA64_SMC (0x17) — you are PSCI
libkrun implements: `PSCI_VERSION` (→2), `MIGRATE_INFO_TYPE` (→2), `SYSTEM_OFF`/`SYSTEM_RESET`
(→ Shutdown), `CPU_ON` (read mpidr/entry/ctx from X1–X3, return 0 in X0, then the VMM sends
`entry` over a channel to the parked secondary vCPU thread, which only then calls
`set_initial_state(entry, fdt)` and starts running). SMC additionally needs manual PC advance;
HVC does not. SMCCC features beyond this minimal set (e.g. `PSCI_FEATURES`, `CPU_SUSPEND`)
will be probed by newer kernels — be ready to extend.

### 4.5 EC_AA64_BKPT (0x3c) — debugging hook. 

### 4.6 VTIMER_ACTIVATED
Set vtimer IRQ (PPI) pending in the GIC, mark `vtimer_masked = true`; unmask via
`hv_vcpu_set_vtimer_mask(false)` once the guest EOIs (libkrun syncs in `hvf_sync_vtimer`
on each exit).

## 5. Threading model inversion

- KVM: vCPU fds created up front on the main thread, moved into worker threads, kicked via signals.
- HVF: `hv_vcpu_create` **inside** `Vcpu::run()` after the thread spawns (thread-affine); kicked
  via `hv_vcpu_request_exit(vcpuid)`.
- libkrun MPIDR detail: vcpuid is written to **Aff1** of `MPIDR_EL1` at vCPU creation, otherwise
  redistributor IDs won't match with in-kernel `hv_gic`. (Classic lost-week landmine.)
- Boot regs are bit-identical to Firecracker: PC = kernel entry, X0 = FDT addr,
  CPSR = `PSR_MODE_EL1h | A | F | I | D` — same `PSTATE_FAULT_BITS_64` constant. The arm64 Linux
  boot protocol doesn't care who the hypervisor is. FDT generation ports with plumbing changes only
  (drop `cache_info.rs` sysfs parsing; GIC node fed from `hv_gic_get_{distributor,redistributor}_size`
  + chosen base addresses; GICv3 maint IRQ = PPI 9, compatible = "arm,gic-v3").
- Memory layout: libkrun aarch64 DRAM starts at the same 2 GB (`0x8000_0000`) for kernel boot;
  GIC dist/redist placed just below `MMIO_MEM_START`.

## 6. Device interrupt path (pervasive mechanical sweep)

Firecracker: device → `EventFd` → KVM irqfd → in-kernel injection. Fire and forget.
libkrun: device → `IrqChip::set_irq(Some(line), _)` → `hv_gic_set_spi(line, true)`
(+ wake any parked vCPU via its WFE channel / `hv_vcpu_request_exit` for running ones).
The EventFd parameter survives in the trait signature but is unused on the HVF path.
Every virtio device's `signal_used_queue` path is touched. Also: no `KVM_IOEVENTFD`, so no
fast MMIO doorbells — every virtio kick is a full vmexit→userspace round trip (one of the
measurable perf deltas vs KVM Firecracker; worth benchmarking explicitly).

## 7. Snapshot/restore — the research-grade gap

Firecracker's ~1,800 loc of `gic/gicv3/regs/*` exists solely to serialize GIC state
(dist/redist/ICC regs) via `KVM_DEVICE_ATTR`. The `hv_gic_*` API surface in libkrun's bindings
has **no state get/set** — in-kernel HVF GIC state appears opaque. Consequences:

1. vCPU core state: capturable (`hv_vcpu_get_reg` / `hv_vcpu_get_sys_reg` enumerating the
   register set — FC's `get_all_registers` logic maps over).
2. Guest memory: yours (mmap), trivially serializable. Dirty tracking: no `KVM_GET_DIRTY_LOG`
   equivalent — implement via `hv_vm_protect` write-protect + fault-on-write logging.
   Genuinely novel work on this platform.
3. GIC state: either (a) use the **userspace** GICv3 (libkrun's `gicv3.rs` legacy path) where all
   state lives in your structs — snapshot trivially, pay sysreg-trap overhead; or (b) in-kernel
   `hv_gic` for speed, accept lossy GIC snapshot (re-init + replay pending SPIs) — fine for many
   workloads, wrong in general; or (c) reverse whether newer macOS exposes GIC state APIs.
   This trade-off (perf vs snapshottability) is a paper section in itself.
4. vtimer offsets: `CNTVOFF` handling across save/restore needs care (HVF manages the offset;
   check `hv_vcpu_get/set_vtimer_offset` availability).

## 8. Everything else that changes

- **Event loop:** FC's `event-manager` is epoll. Port to kqueue or shim with `mio`. Tedious, mechanical.
- **Block io_uring engine:** drop; keep sync engine. Optional research-lite: kqueue/POSIX-AIO async engine.
- **Net:** no TAP on macOS. Options: unixgram/unixstream virtio-net backend to gvproxy/passt
  (krunkit's approach, incl. vfkit magic + offload negotiation), or vmnet (root or restricted
  `com.apple.vm.networking` entitlement). Keep FC's virtio-net device, replace the TAP backend.
- **vsock:** FC's virtio-vsock is pure userspace over unix sockets — ports as-is.
- **Jailer/seccomp:** no Linux namespaces/seccomp. Initially stub; later: Seatbelt
  (`sandbox_init`) profile + separate uid. "Defense-in-depth for a Darwin VMM" is an open question.
- **Entitlements/signing:** `com.apple.security.hypervisor` entitlement; ad-hoc codesign suffices
  for local dev.
- **Nested virt (bonus):** HVF on M3+/macOS 15+ exposes EL2. libkrun's path: boot in
  `PSTATE_EL2h`, set `HCR_EL2`/`CNTHCTL_EL2`, enable EL2+GICv3 bits in `ID_AA64PFR0_EL1`, mask SME
  in `ID_AA64PFR1_EL1` (guest hangs after MMU enable otherwise — another documented landmine).
  Enables KVM-inside-the-microVM on a Mac.

## 9. Suggested phases

1. **Boot-to-shell** (weeks): lift `hvf` crate + `hvfgicv3.rs` from libkrun; new
   `vstate/hvf_{vm,vcpu}.rs` mirroring `macos/vstate.rs`; virtio-blk (sync) + serial + vsock;
   kqueue event loop; FDT from FC's `fdt.rs` minus cache_info. Single vCPU first — defer
   PSCI CPU_ON plumbing.
2. **Parity-ish** (month): SMP via CPU_ON channels; virtio-net over gvproxy; Firecracker REST
   machine-config API on top (the differentiator vs libkrun — existing firecracker-go-sdk
   tooling targets Macs unmodified); balloon via hv_vm_unmap.
3. **Research core** (months): snapshot/restore — vCPU state enumeration, userspace-GIC
   snapshot path, dirty tracking via `hv_vm_protect`, diff snapshots; benchmark resume latency
   vs Linux/KVM Firecracker and boot/density vs Apple `container` (Virtualization.framework).

## 10. Files to read, in order

```
libkrun/src/hvf/src/lib.rs                       # the whole hypervisor abstraction, 731 loc
libkrun/src/vmm/src/macos/vstate.rs              # thread model, WFE parking, run_emulation
libkrun/src/devices/src/legacy/hvfgicv3.rs       # in-kernel GIC wrapper
libkrun/src/devices/src/legacy/vcpu.rs           # VcpuList: IRQ bookkeeping + userspace ICC traps
libkrun/src/devices/src/legacy/gicv3.rs          # userspace GIC (snapshot-friendly path)
libkrun/src/arch/src/aarch64/macos/sysreg.rs     # ESR sysreg encoding
libkrun/src/vmm/src/device_manager/hvf/mmio.rs   # MMIO bus without irqfd
--- vs ---
firecracker/src/vmm/src/vstate/{kvm,vm,vcpu}.rs  # the seam to cut
firecracker/src/vmm/src/arch/aarch64/{vcpu,regs,fdt}.rs
firecracker/src/vmm/src/arch/aarch64/gic/        # what snapshotting demands of a GIC
```
