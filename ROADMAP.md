# ignition — Roadmap & Progress

A research microVM for macOS / Apple Silicon on Hypervisor.framework, architecturally
modeled on AWS Firecracker. This file tracks what is built, what is next, and the
research questions that motivate the project. It is the living index; per-feature
detail lives in `docs/superpowers/specs/` (design) and `docs/*-result.md` (outcomes).

_Last updated: 2026-06-13._

**Legend:** `[x]` shipped · `[~]` in progress · `[ ]` planned · `[-]` deferred / out of scope

---

## Shipped

### Core VMM
- [x] Boot aarch64 Linux to a shell — kernel + FDT load, boot regs, run loop (ESR decode, MMIO, WFI/WFE idle, PSCI). `docs/2d-boot-result.md`, `docs/SPIKE_RESULTS.md`
- [x] In-kernel GICv3 (`hv_gic_*`), SPI/PPI delivery. `docs/superpowers/specs/2026-06-12-phase1-gic-design.md`
- [x] Interactive 16550 console — TX + RX. `docs/serial-rx-result.md`
- [x] Uniform device model — `DeviceManager` + `MmioDevice` trait (MMIO/SPI alloc, bus, FDT, snapshot). `docs/superpowers/specs/2026-06-13-device-model-framework-design.md`
- [x] SMP via PSCI `CPU_ON` (`--smp N`). `docs/smp-result.md`
- [x] Parametrized guest RAM (`--mem <MiB>`).

### Devices (full Firecracker aarch64 set)
- [x] virtio-blk — rootfs from a disk image
- [x] virtio-net — vmnet NAT backend (`--net`; needs `sudo`/entitlement). `docs/virtio-net-result.md`
- [x] virtio-rng — `getentropy`-backed
- [x] virtio-balloon — on-demand reclaim (`Ctrl-A b`). `docs/superpowers/specs/2026-06-13-virtio-balloon-design.md`
- [x] virtio-vsock **E1** (guest→host streams over a host UDS). `docs/superpowers/specs/2026-06-13-virtio-vsock-e1-design.md`
- [x] PL031 RTC + boot-timer. `docs/superpowers/specs/2026-06-13-rtc-pl031-design.md`

### Snapshot / restore
- [x] Snapshot/restore to a shell — resume from saved PC, idles ~0% CPU, responsive. `docs/snapshot-restore-result.md`
- [x] Self-describing v2 format — `DeviceRecord` list, version guard.
- [x] **Multi-vCPU** snapshot — stop-the-world rendezvous; every core saves itself + resumes at its PC. `docs/superpowers/specs/2026-06-13-multi-vcpu-snapshot-design.md`
- [x] **virtio-net** snapshot/restore — `--smp` + `--net` + sudo; link-bounce + carrier-watch re-DHCP; clones get distinct MAC/IP.
- [x] In-kernel GIC state captured losslessly via `hv_gic_state_*` (disproved the "GIC is opaque/unsnapshottable" premise).
- [x] **Fast restore** — `clonefile` + `mmap(MAP_SHARED)`: lazy page fault-in, immutable base. `docs/superpowers/specs/2026-06-13-fast-restore-clonefile-mmap-design.md`
- [x] **Snapshot store** — `--store`/`--name`, `snapshots/<name>/` bases + `instances/<name>-<pid>/` CoW clones, `manifest.json`, auto-generated names, re-snapshot (+ same-name `--force` guard).

---

## Next

- [ ] **Diff / incremental snapshots** (snapshot *size*) — dirty-page tracking via `hv_vm_protect` write-protect + fault logging; base + delta layers. The research-core item; pairs with a snapshot-management layer (diff chains, `list`, GC). _No KVM_GET_DIRTY_LOG equivalent — genuinely novel on this platform._
- [ ] **Resume-latency benchmark** — quantify fast-restore vs the old eager-read path, and vs Linux/KVM Firecracker. `docs/benchmarks.md` (current numbers predate fast-restore).
- [ ] **virtio-vsock E2** — host→guest connections (E1 is guest→host only).

---

## Planned

- [ ] **REST control API** — Firecracker machine-config API on top of the vstate/device seam, so `firecracker-go-sdk` + existing orchestration target Macs unmodified. _The clearest differentiator vs libkrun._
- [ ] **Seatbelt sandbox** — `sandbox_init` profile + separate uid (no Linux jailer/seccomp equivalent). Defense-in-depth for a Darwin VMM.
- [ ] **MMDS** — microVM metadata service.
- [ ] **Rate limiters** — token-bucket on blk/net.
- [ ] **CPU templates** — feature masking.
- [ ] **Metrics / structured logging** — beyond the current boot-timer.
- [ ] **Snapshot management layer** — named lineage, diff chains, `list`, GC (lands with diff snapshots).

---

## Research track

- [ ] **Dirty tracking on HVF** — `hv_vm_protect` write-fault bitmap (enables diff snapshots + density). No native API; the novel bit.
- [ ] **Benchmarks** — resume latency, boot time, density, memory overhead vs Linux/KVM Firecracker and vs Apple `container` (Virtualization.framework). Note the no-`KVM_IOEVENTFD` cost (every virtio kick = full userspace round trip).
- [ ] **Nested virt (EL2)** — HVF on M3+/macOS 15+ exposes EL2; libkrun has the boot path. KVM-inside-the-microVM on a Mac.
- [ ] **Disk dirty-block tracking** — currently a full CoW clone per restore (instant via `clonefile`, but no block-level diff).

---

## Deferred / out of scope

- [-] **Userspace net backend (gvproxy/passt)** — would drop the vmnet `sudo`/restricted-entitlement requirement, but networking stays vmnet-as-is by decision. (vmnet without root needs the restricted, Apple-provisioned `com.apple.vm.networking` entitlement — not grantable by ad-hoc codesign.)
- [-] **CPU hotplug** (`CPU_OFF`, sysfs online/offline) — out of scope; SMP is fixed at boot.
- [-] **io_uring block engine** — dropped by design; sync engine only.
- [-] **x86_64 / ACPI** — aarch64-only port.

---

## Parity vs Firecracker (at a glance)

| Area | ignition | Firecracker | Notes |
|---|---|---|---|
| Boot, GIC, SMP, console | ✅ | ✅ | HVF-equivalent |
| virtio blk/net/rng/balloon/vsock, RTC | ✅ | ✅ | vsock host→guest (E2) pending |
| Snapshot/restore (multi-vCPU, net) | ✅ | ✅ | |
| Lazy/CoW restore (immutable base) | ✅ `clonefile`+`MAP_SHARED` | ✅ `mmap MAP_PRIVATE` / UFFD | macOS has no `userfaultfd` |
| Diff snapshots (dirty tracking) | ❌ planned | ✅ `KVM_GET_DIRTY_LOG` | `hv_vm_protect` — no native API |
| REST API | ❌ planned | ✅ | the libkrun differentiator |
| Jailer / seccomp | ❌ planned (Seatbelt) | ✅ | no Linux equivalent |
| MMDS, rate limiters, CPU templates, metrics | ❌ planned | ✅ | |
| Nested virt (EL2) | ❌ research | n/a | HVF M3+/macOS 15+ bonus |

See `docs/HANDOFF.md` and `docs/firecracker-hvf-porting-map.md` for the full FC↔HVF
source analysis (note their dated GIC-snapshot premise, since disproven).
