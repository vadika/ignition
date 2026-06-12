# SMP (multi-vCPU) milestone — DONE

Date: 2026-06-12. Status: **complete, verified.** ignition/HVF boots a real aarch64
Linux with N vCPUs; all cores come online via PSCI `CPU_ON`, schedule work, and
stop cleanly on PSCI `SYSTEM_OFF`.

## Verified

`target/debug/boot --smp 4 kimage/out/Image kimage/out/rootfs.ext4`:

```
[    0.010315] SMP: Total of 4 processors activated.
(none):~# nproc
4
(none):~# grep -c ^processor /proc/cpuinfo
4
(none):~# poweroff
... reboot: Power down   (process exits 0)
```

`--smp 2` → `SMP: Total of 2 processors activated`. `--smp 1` regression → boots to
login. No `CPU_ON for ... ignored` warnings (MPIDR keys match end to end). Guest
kernel confirmed built with `CONFIG_SMP` + PSCI.

## What landed

- **`crates/hvf/src/lib.rs`** — `set_secondary_state(entry, context_id)` (PC=entry,
  X0=context_id), factored from `set_initial_state` via a shared `setup_registers`
  (primary behavior byte-for-byte unchanged). PSCI `CPU_ON` already decoded to
  `VcpuExit::CpuOn`.
- **`crates/vmm/src/vstate/vcpu_manager.rs`** (new) — `VcpuManager`:
  - `mpidr_for(index) = index` (linear Aff0 = cpu index) — the single MPIDR source
    of truth shared by the FDT, `MPIDR_EL1`, and the `CPU_ON` claim guard.
  - `claim()` — atomic check-and-insert guarding against unknown / duplicate
    `CPU_ON` targets (unit-tested).
  - Lazy bring-up: on `VcpuExit::CpuOn` the manager spawns a thread that creates the
    thread-affine `HvfVcpu`, `set_secondary_state`, and runs the shared loop.
  - Shutdown: `SYSTEM_OFF` → monotonic flag (`Release`) + `vcpu_request_exit`
    broadcast; every `run_loop` re-checks the flag (`Acquire`) at the top, so
    termination is guaranteed by the loop check (the broadcast only interrupts an
    already-blocked vcpu). `join_all` drains the thread registry, joining
    mid-shutdown spawns. vcpuids registered before first `run()` so no broadcast is
    missed.
- **`spike/src/bin/boot.rs`** — `--smp N` (default 1, cap 8, unknown-flag guard);
  `HvfGicV3::new(N, …)`, FDT `cpu_mpidrs = (0..N).map(mpidr_for)`,
  `VcpuManager::new(N, bus).run(...)`. The interactive console (raw-mode reader +
  Ctrl-A x) coexists; `Canceled` exits cleanly.

## Why no userspace IRQ routing

In-kernel `hv_gic` delivers SGIs/IPIs and per-cpu vtimers natively and sizes one
contiguous redistributor region for N cpus, so the `Vcpus` trait stays
`NoIrqVcpus` — secondaries need no VMM-side interrupt plumbing.

## Concurrency (final review)

Single-writer-per-thread: each thread owns its `HvfVcpu`; shared state is three
mutexes + one atomic, each lock held briefly with no two-lock path. The
shutdown-vs-late-spawn window is self-healing (monotonic flag + bounded
`vcpu.run()` loop-check); `join_all` provably loses no handle (a spawner is still
being joined when its child's handle is pushed). Reviewed clean.

## Tests

3 vmm unit tests (`mpidr_for`, claim accept-once, claim reject-unknown) + 2 hvf
mmio tests; workspace builds; 0 clippy warnings. Runtime SMP behavior verified by
the `--smp 2`/`4` integration boots above.

## Follow-ups opened

- **Unknown PSCI fn panics the vCPU** (`handle_psci_request` `_ => panic!`): SMP
  widened the guest surface (a secondary could try `CPU_OFF`/`AFFINITY_INFO`).
  Should return `NOT_SUPPORTED`. Tracked in `phase1-followups.md`.
- CPU hotplug (`CPU_OFF`, sysfs online/offline) is out of scope — bring-up only.
- `NoIrqVcpus` is duplicated between `vcpu_manager.rs` and `hvf_vcpu.rs`; a later
  cleanup can hoist it into the `hvf` crate.
