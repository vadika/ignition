# SMP (multi-vCPU) — design

Date: 2026-06-12. Milestone: boot a real aarch64 Linux with N vCPUs on
ignition/HVF, all cores brought online via PSCI `CPU_ON`, cross-core work
scheduled, and a clean PSCI `SYSTEM_OFF` stopping every vCPU.

## Goal

`boot --smp N` boots Linux with N processors: dmesg shows `SMP: Total of N
processors activated`, `nproc` == N, `/proc/cpuinfo` lists N cores; a parallel
workload schedules across cores; `poweroff` stops all vCPU threads and the process
exits 0.

## What already exists (no work needed)

- **PSCI `CPU_ON` is decoded.** `crates/hvf/src/lib.rs` `handle_psci_request`
  returns `VcpuExit::CpuOn(mpidr, entry, context_id)` for fn id `0xc400_0003`.
  `SYSTEM_OFF` (`0x8400_0008`) returns `VcpuExit::Shutdown`.
- **GIC sizes for N.** `HvfGicV3::new(vcpu_count, gic_top)` already lays out one
  contiguous redistributor region of `per_cpu_size × vcpu_count`. In-kernel
  `hv_gic` delivers SGIs/IPIs and per-cpu vtimers natively — no userspace IRQ
  routing. The `Vcpus` trait stays `NoIrqVcpus`.
- **FDT lists N cpus.** `FdtConfig.cpu_mpidrs: Vec<u64>` emits one cpu node per
  entry (`create_cpu_nodes`).
- **vCPU MPIDR is set at create.** `HvfVcpu::new(mpidr, nested)` writes
  `MPIDR_EL1`. vCPUs are thread-affine (create + run on the same thread).
- **Shutdown primitive exists.** `hvf::vcpu_request_exit(vcpuid)` wraps
  `hv_vcpus_exit` to break a sibling out of `hv_vcpu_run`.

## What is missing (this milestone)

Nothing handles `VcpuExit::CpuOn` (the vmm run loop has an `other =>` debug arm);
there is no vCPU registry, no secondary-thread spawn, no shutdown broadcast, and
the boot harness hardcodes 1 vCPU with MPIDR 0.

## Approach

**Lazy spawn on `CPU_ON`** (over pre-spawn-and-park, which needs a park/wake
protocol with no benefit here, and over single-thread multiplexing, which violates
HVF thread-affinity). The primary boots alone; each secondary thread is created
only when the guest issues `CPU_ON` for it. Matches libkrun.

## MPIDR scheme

Linear: `mpidr_for(index) = index as u64` (Aff0 = cpu index). Valid for ≤256
cores; a microVM never exceeds. FDT `cpu_mpidrs = (0..N).collect()`. PSCI `CPU_ON`
passes the target MPIDR in X1, which must equal one of these. This resolves the
deferred `phase1-followups.md` mpidr item — re-validate the FDT `& 0x7F_FFFF` mask
holds for `index` values (it does for index < 2^23).

## Components

### `crates/hvf/src/lib.rs` — secondary entry setter

Add `set_secondary_state(&self, entry: u64, context_id: u64) -> Result<(), Error>`:
the same initial register/CPSR setup as `set_initial_state` but `X0 = context_id`
(not the FDT address) and `PC = entry`. (Implement by factoring the shared
register setup, or a thin second method — keep `set_initial_state` behavior
identical for the primary.)

### `crates/vmm/src/vstate/vcpu_manager.rs` — new

```rust
pub struct VcpuManager {
    bus: Arc<Bus>,
    mpidrs: Vec<u64>,                 // configured set, mpidr_for(0..N)
    running: Mutex<HashSet<u64>>,     // live mpidrs (dup/unknown guard)
    vcpuids: Mutex<Vec<u64>>,         // live hvf ids, for shutdown broadcast
    threads: Mutex<Vec<JoinHandle<Result<(), hvf::Error>>>>,
    shutdown: Arc<AtomicBool>,
}
```

- `VcpuManager::new(vcpu_count, bus) -> Arc<Self>` — fills `mpidrs` with
  `mpidr_for(0..vcpu_count)`.
- `run(self: &Arc<Self>, entry, fdt_addr) -> Result<(), hvf::Error>` — spawns the
  primary (mpidr 0, `set_initial_state(entry, fdt_addr)`), then `join_all`.
- `spawn(self: &Arc<Self>, mpidr, entry, ctx)` — under `running`: reject if `mpidr`
  not in `mpidrs` or already present (warn, return); else insert, spawn a thread
  that creates `HvfVcpu::new(mpidr)`, registers its vcpuid, calls
  `set_secondary_state(entry, ctx)`, and runs the shared loop.
- `request_shutdown(&self)` — `shutdown.store(true)`; `vcpu_request_exit` every id
  in `vcpuids`.
- The shared per-thread loop (primary + secondary) — the existing run loop plus:
  - `CpuOn(mpidr, entry, ctx)` => `manager.spawn(mpidr, entry, ctx)` then continue.
  - `Shutdown` => `manager.request_shutdown(); return Ok(())`.
  - `Canceled` => `return Ok(())` (woken by a sibling's broadcast).
  - top of loop: `if manager.shutdown.load() { return Ok(()) }`.

The existing single `Vcpu` runner (`crates/vmm/src/vstate/hvf_vcpu.rs`) folds into
this manager (the loop body is shared); `NoIrqVcpus` and the `MAX_PARK` parking are
retained verbatim.

### `spike/src/bin/boot.rs`

- Parse `--smp N` (default 1; validate `1 <= N <= max`, where `max` is
  `hv_vm_get_max_vcpu_count` or a fixed cap of 8).
- `HvfGicV3::new(N, layout::RAM_BASE)`.
- FDT `cpu_mpidrs: (0..N).map(mpidr_for).collect()`.
- Replace the single-`Vcpu` spawn with
  `VcpuManager::new(N, bus).run(entry, fdt_addr)`.

## Data flow (secondary bring-up)

guest PID0 → PSCI `CPU_ON(target_mpidr, entry, ctx)` (HVC) → primary thread's
`hv_vcpu_run` exits `CpuOn` → `manager.spawn` → new thread: `HvfVcpu::new(mpidr)`,
register vcpuid, `set_secondary_state(entry, ctx)`, run → guest
`__secondary_switched` → cpu online. SGIs/IPIs and per-cpu vtimers are delivered
in-kernel by `hv_gic`.

## Concurrency & error handling

- `spawn` takes `&Arc<Self>` so threads spawn siblings and signal shutdown.
- Duplicate/unknown `CPU_ON` target → rejected via the `running` set (warn,
  continue) — never crash on guest misbehavior.
- Shutdown-vs-spawn race: `request_shutdown` sets the flag then broadcasts
  `vcpu_request_exit` to all registered vcpuids; a secondary registers its vcpuid
  before its first `run()` and checks the flag at loop top, so a shutdown landing
  mid-spawn is caught on the first iteration. The tiny window (flag set after a
  secondary registered but before its first flag-check) is bounded by one
  `MAX_PARK` and documented.
- Guest RAM is mapped once into the VM; all vCPUs share it. `Arc<Bus>` devices are
  `Mutex`-guarded, so concurrent MMIO from different cpus serializes per device
  (correct for serial / synchronous virtio-blk).
- `--smp` validated against the HVF max (or cap 8); 0 or over-max rejected with a
  clear message. A secondary that fails `HvfVcpu::new` logs and exits its thread;
  the primary keeps running (that cpu simply never comes online — visible in
  guest dmesg, not a host crash). `join_all` surfaces any per-thread `hvf::Error`.

## Testing

- **Unit (pure, no HVF entitlement):** `mpidr_for(index)` (== index, mask-safe);
  the `CPU_ON`-target validation (in configured set + not already running → accept;
  unknown or duplicate → reject). The validation is factored off the thread path so
  it runs under `cargo test`.
- **Integration (the bar), via the piped-stdin console harness:** `boot --smp 2`
  and `boot --smp 4`:
  - dmesg contains `SMP: Total of N processors activated`;
  - `nproc` prints N; `grep -c ^processor /proc/cpuinfo` == N;
  - cross-core work: `for i in $(seq N); do yes >/dev/null & done; sleep 1; uptime`
    (load climbs), then `kill %1..%N`;
  - `poweroff` (PSCI SYSTEM_OFF) → all vCPU threads stop, process exits 0.

## Out of scope

- **CPU hotplug** (offline/online via `/sys/devices/system/cpu/...`) — bring-up
  only; PSCI `CPU_OFF` is not modeled this milestone.
- **vCPU affinity / NUMA topology** — flat single-cluster topology.
- **CPU feature heterogeneity** — all vCPUs identical.
- The mpidr re-validation is closed by the linear scheme above; multi-redist GIC
  remains moot for HVF (single contiguous region), per `phase1-followups.md`.
