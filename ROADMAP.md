# ignition — Roadmap & Progress

A research microVM for macOS / Apple Silicon on Hypervisor.framework, architecturally
modeled on AWS Firecracker. This file tracks what is built, what is next, and the
research questions that motivate the project. It is the living index; per-feature
detail lives in `docs/superpowers/specs/` (design) and the documentation book under `docs/src/` (outcomes).

_Last updated: 2026-06-18._

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
- [x] **Networking via socket_vmnet (no sudo)** — `--net` connects to the socket_vmnet
  daemon (Homebrew, root LaunchDaemon); the VMM is an unprivileged unix-socket client
  (4-byte-BE frame protocol, VMM-generated MAC). `scripts/install-socket-vmnet.sh`,
  `docs/superpowers/specs/2026-06-18-sudo-free-net-socket-vmnet-design.md`. **Verified
  live**: `--net` boot gets a DHCP lease + pings out with no sudo; two concurrent guests
  get distinct IPs (192.168.105.3/.4). The original in-process vmnet shim was **removed**
  (phase 2 = delete, not harden — socket_vmnet is the sole backend; no zero-install sudo
  fallback).
- [x] virtio-rng — `getentropy`-backed
- [x] virtio-balloon — on-demand reclaim (`Ctrl-A b`). `docs/superpowers/specs/2026-06-13-virtio-balloon-design.md`
- [x] virtio-vsock **E1** (guest→host streams over a host UDS). `docs/superpowers/specs/2026-06-13-virtio-vsock-e1-design.md`
- [x] PL031 RTC + boot-timer. `docs/superpowers/specs/2026-06-13-rtc-pl031-design.md`

### GUI display (software-rendered, snapshot-safe) — beyond FC parity
- [x] **M2 structural** — `--gui` inverts the main thread to a `winit`+`softbuffer` event loop; VMM runs off-main; non-blocking coalescing `DisplaySink` seam. `docs/superpowers/specs/2026-06-15-gui-display-refactor-design.md`
- [x] **M1 virtio-gpu 2D** — device id 16, controlq + cursorq, SG-correct `TRANSFER_TO_HOST_2D`, `RESOURCE_FLUSH`→present; the guest framebuffer console renders in the `--gui` window. No 3D/VIRGL; GPU-state snapshot is M5. `docs/superpowers/specs/2026-06-15-virtio-gpu-m1-design.md`, umbrella `docs/superpowers/specs/2026-06-15-gui-bringup-plan.md`
- [x] **M3 virtio-input** — keyboard + absolute tablet (device id 18); winit key/pointer/click events injected into the guest eventq; typing logs in, pointer tracks 1:1. `docs/superpowers/specs/2026-06-15-virtio-input-m3-design.md`
- [x] **M4 compositor/app** — cage (wlroots, pixman software renderer) + foot terminal, interactive over virtio-input, on a separate `rootfs-gui.ext4`. Surfaced the virtio-gpu fence-signal fix (wlroots page-flips are fenced). `docs/superpowers/specs/2026-06-15-gui-compositor-m4-design.md`
- [x] **M5 snapshot/clone with the GUI live** — final GUI milestone. virtio-gpu resource-table/scanout + virtio-input config state snapshot; `boot --gui --restore <name>` reopens the window and repaints the resumed desktop (scanout re-read from restored backing, no pixel bytes stored); headless `--restore` falls back to the serial console. One warm base fans out into N independent desktops via `scripts/fanout-gui.sh N <base>` (per-pid CoW instance, own window/MAC). `docs/superpowers/specs/2026-06-16-gui-snapshot-m5-design.md`
- [x] **M6 resizable framebuffer** — the `--gui` window is resizable; a debounced drag drives a virtio-gpu connector-cycle (config-change `EVENT_DISPLAY` → guest `GET_DISPLAY_INFO` → disconnect/reconnect) so cage re-modesets and the desktop reflows to the new size. Pointer decoupled from resolution (fixed tablet `ABS` range). Needs cage ≥ 0.2.0 (browser rootfs → alpine 3.21); cage 0.1.5 terminates on sole-output destroy. `docs/superpowers/specs/2026-06-21-resizable-framebuffer-design.md`

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
- [x] **vmid — per-clone CRNG reseed on restore** — on every `--restore` the host pushes a
  fresh 32-byte seed over the existing vsock control channel; the guest (`socat VSOCK-LISTEN`
  → `/usr/bin/vmid-reseed`) force-reseeds via `RNDADDENTROPY` + `RNDRESEEDCRNG`. Pure
  userspace — no ACPI/`vmgenid` driver (this VMM emits FDT). Correctness insurance for the
  MCP agent-sandbox track. Verified live (`scripts/vmid_live_proof.py`): mechanism works
  (push delivered, reseeded clones diverge). **Finding:** the shared-CRNG bug does not
  reproduce observably on HVF aarch64 — no arch RNG and interrupt-timing entropy reseeds the
  CRNG sub-millisecond after resume, so the window vmid closes is tiny on this platform.
  `docs/src/features/vmid.md`, `docs/superpowers/specs/2026-06-17-vmid-design.md`

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
- [x] **Domain payoff — firmware / TEE harnesses** — TPM 2.0 first. The real
  ms-tpm-20-ref command processor (OpenSSL backend) runs as an aarch64-userspace
  snapshot-fuzz target: manufactured + started in `target_init` (pre-snapshot), one
  command per iteration through `ExecuteCommand`, the large TPM state dirty-page-reset
  each iteration. **Verified on HVF:** dirty **1443** vs full **140** execs/sec (10.3×
  — bigger than libpng's 4.8×, since the TPM's larger working set makes full-copy reset
  much slower), **419** coverage edges, reset p50 38 µs; planted NV_Write OOB rediscovered
  in 0.018 s, deterministic replay. `docs/superpowers/specs/2026-06-19-tpm2-snapshot-fuzz-demo-design.md`,
  `docs/src/benchmarks/fuzzing.md`, `scripts/fuzz_tpm2_{test,bench}.py`.
  Honest framing: userspace (no EL3/secure-world here), so the win is fast large-state
  reset on Apple Silicon, not "impossible elsewhere."
  - [ ] **Stretch — real-CVE rediscovery** — swap the planted bug for a vulnerable
    ms-tpm-20-ref pin (e.g. CVE-2023-1017 OOB in `CryptParameterDecryption`); needs a
    crafted seed corpus + session-setup reachability. The OpenSSL backend already
    in place unblocks it.
  - [ ] **OP-TEE TA / firmware** — the genuinely-needs-a-platform targets, if
    nested-virt (EL2) ever lands.

Honest threat-model note: the Seatbelt sandbox v1 (below) confines egress/exec/writes/secrets,
but full read+mach confinement (deny-default) and the uid drop are still v2, so the
fuzzing/sandbox framing stays *"your own / your agent's code on your own machine,"* not secure
multi-tenant.

---

## Adoption track — integration as go-to-market

A VMM nobody can call from existing tools is a demo; one that drops into infrastructure
people already run gets adopted. Integrate where the clone primitive is the visible win,
and impersonate an interface that already has consumers, so adoption cost ≈ 0.

**Discipline:** one faithful seam at a time. A 70%-compatible API is worse than none — it
fails in surprising ways inside tools you don't control. Ship one bridge, prove adoption,
then add the next.

- [x] **MCP server for agent sandboxes** *(first — closest to strengths, hottest demand)* —
  a persistent-session tool that clones a warm snapshot per session. Any MCP-capable agent
  (Claude Code, Codex, Gemini CLI) gets fast disposable sandboxes with zero ignition-specific
  code. Where clone-from-warm is *most visibly* better than cold-boot competitors, and where
  the honest "your agent's code on your machine" threat model fits.
  Shipped: `ignition-mcp` crate (open\_session / run / write\_file / reset / close), persistent
  sessions, Python `ign-exec` guest agent, `build-rootfs-tools.sh` + `make-tools-base.sh`
  tools-base rootfs. **Verified live on HVF** (`scripts/mcp_live_test.py`): open → run
  (`python3` → 4) → filesystem persists across runs → write_file → reset clears state → close,
  all green. `docs/src/features/mcp-server.md`
- [x] **Firecracker REST control API** *(second — broadest inherited ecosystem)* —
  a Firecracker-compatible REST server over a unix socket so `firecracker-go-sdk`,
  flintlock, and existing orchestration target Macs **unmodified**. Converts "novel
  research VMM" into "Firecracker on Apple Silicon that snapshots faster." (Also the
  clearest differentiator vs libkrun.)
  Shipped: `ignition-fc-api` crate — the launch + snapshot route subset (machine-config,
  boot-source, drives, network-interfaces, `InstanceStart`, `PATCH /vm` pause/resume,
  `snapshot/create`, `snapshot/load`). Translate-and-spawn: the server accumulates config,
  then maps it to `boot` flags and spawns a headless child driven over a `--control-sock`
  (the same `request_pause`/`resume`/`snapshot` methods as the serial FSM). One VM per API
  socket. **Mock-tested** (`scripts/fc_api_mock_test.py`, CI-safe stub boot) and
  **live-tested on HVF** (`scripts/fc_api_live_test.py`: start → pause → snapshot → resume,
  then clone-from-snapshot in a second server). `docs/src/features/fc-rest-api.md`
- [ ] **OCI / containerd shim** *(heavier, later)* — present as a `runtimeClass`-style
  runtime so `nerdctl` / Buildkit / CI runners get microVM-per-container with no workflow
  change (the path Kata took to adoption).
- [ ] **CI runner executor** *(later)* — clean VM per job from a golden snapshot for
  self-hosted GitHub Actions / GitLab runners on M-series fleets.

---

## Hardening & honesty gates

These gate the claims the adoption track is allowed to make. The HVF *hardware* boundary is
real and strong today; the VMM *process* is not yet jailed.

- [~] **Seatbelt sandbox** — **v1 shipped** (`docs/src/internals/sandbox.md`): self-applied
  `sandbox_init` targeted-deny profile (no IP egress, no exec/fork, writes only to VM-state
  dirs, host secrets denied), on by default, fail-closed, `--no-sandbox` opt-out; HVF + vmnet
  intact. **Remaining (v2):** flip to `(deny default)` for full read+mach confinement, and the
  separate-uid privilege drop. Until v2, lead with "your own code, your own machine."
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

- [-] **Userspace net backend (gvproxy/passt)** — would drop the vmnet `sudo`/restricted-entitlement requirement, but networking stays vmnet-as-is by decision. (vmnet without root needs the restricted, Apple-provisioned `com.apple.vm.networking` entitlement — not grantable by ad-hoc codesign.) (Sudo is now avoidable via socket_vmnet — see Shipped. This item remains only for a fully userspace backend with no daemon.)
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
| REST API | ✅ `ignition-fc-api` (launch + snapshot subset) | ✅ | inherits FC's tool ecosystem |
| Jailer / seccomp | 🟡 Seatbelt v1 (targeted-deny) | ✅ | full deny-default + uid drop = v2 |
| MMDS, rate limiters, CPU templates, metrics | ❌ planned | ✅ | |
| Nested virt (EL2) | ❌ research | n/a | HVF M3+/macOS 15+ bonus |

Parity is the floor. The **demonstrator** (snapshot fuzzing) and **adoption** (MCP / REST /
OCI) tracks are deliberately *beyond* Firecracker parity — they are the reason-to-exist, not
catch-up items. The clone-from-warm primitive they exploit is the thing the
Virtualization.framework-based macOS tools cannot cheaply replicate.

See `docs/src/internals/design-decisions.md` and `docs/src/internals/hvf-firecracker-map.md` for the full FC↔HVF
source analysis (note their dated GIC-snapshot premise, since disproven).
