# ignition — Roadmap & Progress

A research microVM for macOS / Apple Silicon on Hypervisor.framework, architecturally
modeled on AWS Firecracker. This file tracks what is built, what is next, and the
research questions that motivate the project. It is the living index; per-feature
detail lives in `docs/superpowers/specs/` (design) and the documentation book under `docs/src/` (outcomes).

_Last updated: 2026-06-14._

**Legend:** `[x]` shipped · `[~]` in progress · `[ ]` planned · `[-]` deferred / out of scope

---

## Thesis — what ignition is *for*

Firecracker parity is the foundation, not the point. The macOS microVM space is already
contested by Virtualization.framework-based tools (Apple `container`, Shuru, CodeRunner,
Docker Sandboxes), so "isolated Linux microVM on a Mac" is not a differentiator.

The differentiator is the **fast snapshot + clone-from-warm-base primitive on bare HVF** —
`clonefile` + `MAP_SHARED` against an immutable base, where a clone idles at ~0% CPU and
touches only its own dirtied pages. This is the Firecracker/E2B production pattern, and the
VZ-based tools cannot expose it cleanly because they sit on a closed whole-VM
checkpoint API. ignition is on raw HVF, so it can.

Positioning follows from that: ignition is a **substrate other tools are built on**, not an
end-user product. Its "customers" are tool-builders (agent-sandbox authors, fuzzing
harnesses, CI backends), not Mac users at large. Everything below is organized around
making the clone primitive (a) provably fast and correct, and (b) reachable from
infrastructure developers already run.

Two tracks carry the thesis beyond parity:
- **Demonstrator track** — fuzzing. The cleanest proof the clone primitive does real work:
  a single benchmark number (execs/sec) that is a direct function of reset latency, and
  simultaneously the most brutal correctness test the snapshot path will ever face.
- **Adoption track** — integration. Impersonate interfaces that already have consumers
  (MCP, Firecracker REST, OCI) so adoption cost is ~zero. One faithful seam at a time.

---

## Shipped

### Core VMM
- [x] Boot aarch64 Linux to a shell — kernel + FDT load, boot regs, run loop (ESR decode, MMIO, WFI/WFE idle, PSCI). `docs/src/getting-started/boot-a-guest.md`, `docs/src/internals/validation-spike.md`
- [x] In-kernel GICv3 (`hv_gic_*`), SPI/PPI delivery. `docs/superpowers/specs/2026-06-12-phase1-gic-design.md`
- [x] Interactive 16550 console — TX + RX. `docs/src/features/devices.md`
- [x] Uniform device model — `DeviceManager` + `MmioDevice` trait (MMIO/SPI alloc, bus, FDT, snapshot). `docs/superpowers/specs/2026-06-13-device-model-framework-design.md`
- [x] SMP via PSCI `CPU_ON` (`--smp N`). `docs/src/features/devices.md`
- [x] Parametrized guest RAM (`--mem <MiB>`).

### Devices (full Firecracker aarch64 set)
- [x] virtio-blk — rootfs from a disk image
- [x] virtio-net — vmnet NAT backend (`--net`; needs `sudo`/entitlement). `docs/src/features/devices.md`
- [x] virtio-rng — `getentropy`-backed
- [x] virtio-balloon — on-demand reclaim (`Ctrl-A b`). `docs/superpowers/specs/2026-06-13-virtio-balloon-design.md`
- [x] virtio-vsock **E1** (guest→host streams over a host UDS). `docs/superpowers/specs/2026-06-13-virtio-vsock-e1-design.md`
- [x] PL031 RTC + boot-timer. `docs/superpowers/specs/2026-06-13-rtc-pl031-design.md`

### Snapshot / restore — *the load-bearing feature*
- [x] Snapshot/restore to a shell — resume from saved PC, idles ~0% CPU, responsive. `docs/src/features/snapshot-restore.md`
- [x] Self-describing v2 format — `DeviceRecord` list, version guard.
- [x] **Multi-vCPU** snapshot — stop-the-world rendezvous; every core saves itself + resumes at its PC. `docs/superpowers/specs/2026-06-13-multi-vcpu-snapshot-design.md`
- [x] **virtio-net** snapshot/restore — `--smp` + `--net` + sudo; link-bounce + carrier-watch re-DHCP; clones get distinct MAC/IP.
- [x] In-kernel GIC state captured losslessly via `hv_gic_state_*` (disproved the "GIC is opaque/unsnapshottable" premise).
- [x] **Fast restore** — `clonefile` + `mmap(MAP_SHARED)`: lazy page fault-in, immutable base. `docs/superpowers/specs/2026-06-13-fast-restore-clonefile-mmap-design.md`
- [x] **Snapshot store** — `--store`/`--name`, `snapshots/<name>/` bases + `instances/<name>-<pid>/` CoW clones, `manifest.json`, auto-generated names, re-snapshot (+ same-name `--force` guard).
- [x] **Dirty-page tracking on HVF** — `--track-dirty` arms `hv_vm_protect` write-protect; first write to each 16 KiB page traps (Data-Abort translation fault), marks dirty, re-grants. The genuinely novel platform bit — no `KVM_GET_DIRTY_LOG` equivalent. Shared foundation for diff snapshots and the (planned) in-loop reset.
- [x] **Diff / incremental snapshots** — a restored armed guest writes a Diff layer (only changed pages, `parent` = the restored-from leaf) as an immutable delta chain; restore reassembles root + diffs transparently. `docs/superpowers/specs/2026-06-13-diff-snapshots-design.md`, `docs/src/features/diff-snapshots.md`
- [x] **Restore instrumentation + cost attribution** — per-stage `Restore-breakdown` / `Restore-tail` timers; bench parses + records them. The ~245 ms restore cost is **host RAM page-in** (cache-state dependent), not the HVF-object/overlay stages (~3 ms). Lazy stage-2 demand-paging (`--lazy-restore`) was prototyped (correct single-vCPU + SMP) and **shelved**: `clonefile`+`MAP_SHARED` already demand-pages host-side, so the win could not be demonstrated without a clean cold-base A/B (`sudo purge`). `docs/src/benchmarks/diff-snapshots.md` §3

### Snapshot fuzzer — demonstrator (M0–M3)
- [x] In-VMM snapshot fuzzer: `ignition-fuzz` trap-MMIO doorbell + `MAP_SHARED` input/coverage window, guest harness as PID 1, blind+coverage-guided mutation, CRASH capture, `--replay`.
- [x] In-loop `reset()` — per-iteration dirty-page rollback + register restore in the live VMM (reset p50 36 µs).
- [x] SanCov `trace-pc` coverage feedback into a reset-exempt window + host virgin-bits map.
- [x] Benchmarked on libpng 1.6.43: **1309 execs/sec** (dirty) vs 271 (full), 4.8×. `docs/src/benchmarks/fuzzing.md`

The clone primitive (immutable base + lazy CoW clones + dirty tracking + diff chains) is
**shipped**, and the fuzzing demonstrator (M0–M3) proves it does real work. The next tracks
turn it from "works" into "provably fast, correct, and reachable."

---

## Near-term (next)

Ordered so the clone primitive gets proven and hardened before it gets dressed up.

- [x] **In-loop `reset()` primitive** — per-iteration rollback of *only the dirtied pages*
  to the base, in a **live, running** VMM, without re-`clonefile`ing, plus vCPU register
  restore. Stays in-memory, no disk/format/versioning. Shipped in the fuzzer (M2): reset p50
  **36 µs** (page-copy ~35 µs + register restore ~1 µs), 44–50 dirty pages/iter on libpng.
  Built on the dirty-tracking substrate. `docs/src/benchmarks/fuzzing.md`
- [ ] **Resume-latency benchmark vs Linux/KVM** — per-stage restore attribution is **done**
  (`docs/src/benchmarks/diff-snapshots.md` §3); remaining is the cross-platform comparison
  (ignition fast-restore vs Linux/KVM Firecracker) and a clean cold-base eager-vs-lazy A/B
  if a cold-start workload shows the page-in is on the critical path.
- [x] **virtio-vsock E2** — host→guest connections via Firecracker's hybrid control
  protocol (`CONNECT <port>` → `OK <host_port>`); host control socket `{uds}`, guest
  RESPONSE establishes the conn, bidirectional streaming reuses E1's `Connection`.
  `docs/superpowers/specs/2026-06-15-virtio-vsock-e2-design.md`, `scripts/vsock_e2_test.py`.

---

## Demonstrator track — snapshot fuzzing

The proof that the clone primitive does real work. Dual purpose, both load-bearing:
1. **Benchmark** — execs/sec is a direct function of reset latency, so a working fuzzer is a
   single defensible number that says "the snapshot path is fast." Shipped (M3): 1309
   execs/sec on libpng, reset p50 36 µs. (Cross-VMM comparison vs a Linux/KVM snapshot fuzzer
   was dropped from scope; ignition reports its own dirty-vs-full numbers.)
2. **Correctness stress test** — a fuzz loop does millions of restores; any uncaptured
   register, stale queue index, or missed dirty page surfaces immediately as
   nondeterministic crashes or coverage that won't stabilize. Dogfoods the load-bearing code
   under the harshest possible workload.

Architecture (all reuse existing primitives): input injected **directly into a known guest
page** via `MAP_SHARED` (no virtio, no syscall, no I/O in the loop — dodges the vsock-E2
dependency); guest→host control via the **boot-timer magic-MMIO pattern**; coverage via a
shared bitmap page; reset via the in-loop `reset()` above. Inject below the target's
narrowest interface (a buffer in memory), so there is no external interface to reset.

- [x] **Guest harness + injection channel** (M0/M1) — parked-at-call-site loop; `ignition-fuzz`
  trap-MMIO doorbell (`SNAPSHOT_ME` / `DONE` / `CRASH`); shared input + coverage window. Blind
  mutation brain + CRASH capture + verbatim `--replay`. `scripts/fuzz_m1_test.py`
- [x] **Correctness gate** (M1) — fuzzer rediscovers a planted length-field heap overflow
  (the CVE-2015-8126 shape) in **0.002 s** from a seed corpus, deterministically replayable.
  ASan death-callback → CRASH doorbell. Pure-compute, no I/O; reset = dirty-pages + registers.
- [x] **Coverage + dirty-page reset** (M2) — SanCov `trace-pc` into a reset-exempt window +
  host virgin-bits map; reset swapped from full-RAM copy to the dirty-set. Coverage curve
  grows, corpus expands, execs/sec jumps (~3.5× on the synthetic target).
- [x] **libpng-current + benchmark** (M3) — real libpng 1.6.43 (SanCov, no ASan): **1309
  execs/sec** dirty-reset vs 271 full-copy (**4.8×**), reset p50 36 µs, dirty-set 44–50 pages,
  144 edges. `docs/src/benchmarks/fuzzing.md`, `scripts/fuzz_m3_bench.py`.
  (Linux/KVM cross-check dropped from scope; ignition-own numbers only.)
- [ ] **Stateful targets** *(next)* — `freetype` / `libxml2`: still single-threaded compute, larger
  dirty-page sets, more bug surface; stresses the reset path harder.
- [ ] **Domain payoff — firmware / TEE harnesses** — TPM 2.0 command-handler or OP-TEE TA,
  parked at the command-parse entry, input injected into the command buffer. *The target
  nobody can fuzz comfortably elsewhere* — it assumes a platform/secure-world a host
  `fork()` can't provide but a microVM snapshot can. Turns "snapshot fuzzing reimplemented
  on a Mac" into "snapshot-fuzz firmware/TEE parsers on Apple Silicon" — novel and
  publishable, squarely in ignition's wheelhouse (vtpmd, fTPM, OP-TEE).

Honest threat-model note: until the Seatbelt sandbox lands (below), the fuzzing/sandbox
framing is *"your own / your agent's code on your own machine,"* not secure multi-tenant.

---

## Adoption track — integration as go-to-market

A VMM nobody can call from existing tools is a demo; one that drops into infrastructure
people already run gets adopted. Integrate where the clone primitive is the visible win,
and impersonate an interface that already has consumers, so adoption cost ≈ 0.

**Discipline:** one faithful seam at a time. A 70%-compatible API is worse than none — it
fails in surprising ways inside tools you don't control. Ship one bridge, prove adoption,
then add the next.

- [ ] **MCP server for agent sandboxes** *(first — closest to strengths, hottest demand)* —
  an `execute`-style tool that clones a warm snapshot per call. Any MCP-capable agent
  (Claude Code, Codex, Gemini CLI) gets fast disposable sandboxes with zero ignition-specific
  code. Where clone-from-warm is *most visibly* better than cold-boot competitors, and where
  the honest "your agent's code on your machine" threat model fits.
- [ ] **Firecracker REST control API** *(second — broadest inherited ecosystem)* —
  machine-config API on the vstate/device seam so `firecracker-go-sdk`, flintlock, and
  existing orchestration target Macs **unmodified**. Converts "novel research VMM" into
  "Firecracker on Apple Silicon that snapshots faster." (Also the clearest differentiator
  vs libkrun.)
- [ ] **OCI / containerd shim** *(heavier, later)* — present as a `runtimeClass`-style
  runtime so `nerdctl` / Buildkit / CI runners get microVM-per-container with no workflow
  change (the path Kata took to adoption).
- [ ] **CI runner executor** *(later)* — clean VM per job from a golden snapshot for
  self-hosted GitHub Actions / GitLab runners on M-series fleets.

---

## Hardening & honesty gates

These gate the claims the adoption track is allowed to make. The HVF *hardware* boundary is
real and strong today; the VMM *process* is not yet jailed.

- [ ] **Seatbelt sandbox** — `sandbox_init` profile + separate uid (no Linux jailer/seccomp
  equivalent). **Gates any "untrusted / multi-tenant" positioning.** Until it lands, lead
  with "your own code, your own machine," never "secure multi-tenant hosting."
- [x] **virtio-vsock E2** (host→guest) — shipped; unblocks control-plane integration designs.

---

## Planned (remaining FC-parity infra)

- [ ] **MMDS** — microVM metadata service.
- [ ] **Rate limiters** — token-bucket on blk/net.
- [ ] **CPU templates** — feature masking.
- [ ] **Metrics / structured logging** — beyond the current boot-timer.
- [ ] **Snapshot management layer** — named lineage, diff chains, `list`, GC; chain
  flatten/compaction to bound restore latency on deep chains.

---

## Research track

- [x] **Dirty tracking on HVF** — `hv_vm_protect` write-fault bitmap. Shipped (see above);
  the novel platform bit, foundation for both diff snapshots (size) and the planned fuzzing
  in-loop reset (throughput).
- [ ] **Benchmarks** — resume latency, boot time, density, memory overhead vs Linux/KVM
  Firecracker and vs Apple `container` (Virtualization.framework). Note the no-`KVM_IOEVENTFD`
  cost (every virtio kick = full userspace round trip) — quantify it. (Restore-side
  per-stage attribution already done; see `docs/src/benchmarks/diff-snapshots.md`.)
- [ ] **Snapshot-fuzzing throughput study** — execs/sec vs reset latency as a function of
  dirty-set size and target; ignition-on-HVF vs Linux/KVM snapshot fuzzers. (Demonstrator
  track, written up as research.)
- [ ] **Nested virt (EL2)** — HVF on M3+/macOS 15+ exposes EL2; libkrun has the boot path.
  KVM-inside-the-microVM on a Mac.
- [ ] **Disk dirty-block tracking** — currently a full CoW clone per restore (instant via
  `clonefile`, but no block-level diff).

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
| virtio blk/net/rng/balloon/vsock, RTC | ✅ | ✅ | vsock both directions (E1+E2) |
| Snapshot/restore (multi-vCPU, net) | ✅ | ✅ | |
| Lazy/CoW restore (immutable base) | ✅ `clonefile`+`MAP_SHARED` | ✅ `mmap MAP_PRIVATE` / UFFD | macOS has no `userfaultfd` |
| Diff snapshots (dirty tracking) | ✅ `hv_vm_protect` write-fault | ✅ `KVM_GET_DIRTY_LOG` | no `KVM_GET_DIRTY_LOG` equivalent |
| REST API | ❌ adoption track | ✅ | inherits FC's tool ecosystem |
| Jailer / seccomp | ❌ planned (Seatbelt) | ✅ | gates untrusted-tenant claims |
| MMDS, rate limiters, CPU templates, metrics | ❌ planned | ✅ | |
| Nested virt (EL2) | ❌ research | n/a | HVF M3+/macOS 15+ bonus |

Parity is the floor. The **demonstrator** (snapshot fuzzing) and **adoption** (MCP / REST /
OCI) tracks are deliberately *beyond* Firecracker parity — they are the reason-to-exist, not
catch-up items. The clone-from-warm primitive they exploit is the thing the
Virtualization.framework-based macOS tools cannot cheaply replicate.

See `docs/src/internals/design-decisions.md` and `docs/src/internals/hvf-firecracker-map.md` for the full FC↔HVF
source analysis (note their dated GIC-snapshot premise, since disproven).
