# ignition benchmarks — boot & restore latency

> **Status note (2026-06): numbers predate the fast-restore work.** Restore latency here was
> measured with eager `read(memory.bin)`; restore now uses clonefile + `mmap(MAP_SHARED)`
> (lazy, immutable base) and is materially faster — these figures are pre-fast-restore.
> The `--store`/`--name` store convention, multi-vCPU snapshot, and re-snapshot post-date this
> doc.

Date: 2026-06-13. Host: Apple Silicon, macOS 26.5. Guest: aarch64 Linux 6.1
(Firecracker CI microvm config + virtio-balloon/vsock/devmem), Alpine 3.19 busybox
rootfs, single vCPU, 512 MiB RAM. Warm page cache. `n = 6` (`scripts/benchmark.py 6`).

These are **ignition-internal** numbers (fresh boot vs snapshot restore). Cross-VMM
comparison (KVM Firecracker, Apple Virtualization.framework) is future work.

## Two measurement methods (and what each captures)

Boot/restore latency depends on *where you start and stop the clock*. We use two
complementary methods rather than one:

1. **`Guest-boot-time` — the boot-timer device** (`Ctrl`-less, automatic). The
   guest's init pokes a magic byte to a fixed MMIO address at the end of early boot;
   the VMM timestamps it relative to VM start. This measures **kernel + early-init
   readiness from inside the guest's time domain** — it excludes host-side process
   spawn and is independent of how long the rest of userspace (getty, login) takes.
   This is Firecracker's own boot-time metric, ported.

2. **`launch → login:` — the host harness** (`scripts/benchmark.py`). Wall-clock
   from `exec(boot)` until the `login:` prompt bytes appear on the console. This
   measures **time to an interactive shell** end-to-end: host process spawn, kernel
   load into guest RAM, FDT generation, HVF setup, the full kernel boot, *and* all of
   openrc init through to getty.

For restore there is a third clock:

3. **`Restore-time` — host-side** (logged in `run_restore`). Wall-clock from
   `boot --restore` entry until the restored guest is handed to the run loop:
   `mmap` + load `memory.bin` (512 MiB) + GIC/device/vCPU state restore. The
   boot-timer device *cannot* measure restore — the guest's init does not re-run on
   restore — so this host-side clock is the restore analog of `Guest-boot-time`.

## Results (n = 6)

| Phase | Metric | mean | min | max |
|-------|--------|-----:|----:|----:|
| **Fresh boot** | Guest-boot-time (boot-timer, VM-start → init ready) | **204 ms** | 193 | 214 |
| | launch → `login:` (host wall, to interactive shell) | **1.24 s** | 1.23 | 1.25 |
| **Restore**    | Restore-time (host-side, RAM load + state restore) | **115 ms** | 93 | 148 |
| | launch → restored prompt (host wall) | **0.53 s** | 0.50 | 0.55 |

## Interpretation

- **Kernel readiness vs shell.** The two fresh-boot numbers differ by ~6× (204 ms
  vs 1.24 s) precisely because they measure different things: the kernel + early init
  reach the boot-timer poke in ~200 ms, but reaching a usable `login:` prompt (host
  process spawn + the rest of openrc init + getty) takes ~1.24 s wall. Reporting only
  one would mislead — "boots in 200 ms" (kernel) and "1.2 s to a shell" (end-to-end)
  are both true and answer different questions.

- **Restore beats fresh boot.** Bringing a fully-booted guest back to a *running*
  state costs ~115 ms (host-side) — about **1.8× faster than the 204 ms kernel boot**
  and **~11× faster than the 1.24 s boot-to-shell**, because restore skips the entire
  kernel boot + init sequence and only replays memory + device/vCPU state.

- **Restore cost is RAM-load-bound and flat.** The 115 ms is dominated by copying the
  512 MiB `memory.bin` into the guest mapping; it scales with RAM size, not with how
  much the guest does at startup. A heavier guest (more services, larger init) inflates
  the *fresh-boot* numbers but leaves restore roughly unchanged — so the restore
  advantage **widens** for real workloads beyond this minimal Alpine rootfs.

## Caveats

- **Warm page cache.** `memory.bin` and the kernel image are in the host page cache;
  a cold restore adds disk-read time to the 115 ms.
- **`launch → restored prompt` (0.53 s) is harness-quantized.** The restored guest is
  *running* at ~115 ms (`Restore-time`); the 0.53 s is the benchmark nudging the
  getty to redraw its prompt on a 0.5 s cadence, not a true readiness cost. Use
  `Restore-time` as the restore latency; treat `→ prompt` as an upper bound.
- **Clock domains differ.** `Guest-boot-time` is stamped inside the VMM relative to
  VM start (≈ vCPU creation); `launch → login:` is host wall from `exec`, including
  ~tens of ms of process spawn before the VM exists. They are complementary, not
  subtractable.
- **Minimal guest, single vCPU, 512 MiB.** Small kernel + busybox init → unusually
  fast boot; absolute numbers will grow with a fuller guest. Multi-vCPU and
  incremental/dirty-page snapshots are not measured (out of scope).

## Reproduce

```sh
cargo build -p ignition-spike --bin boot && scripts/sign.sh target/debug/boot
python3 scripts/benchmark.py 6      # both fresh-boot methods + restore
# component scripts:
python3 scripts/boot_vs_restore_timing.py   # launch -> running, phased
python3 scripts/restore_test.py             # snapshot -> restore, CPU% + responsive
```

---

## Snapshot fuzzer (M3) — libpng, single core

In-VMM snapshot fuzzer (`boot --fuzz`), `hv_vm_protect` dirty-page reset, target
= libpng 1.6.43 (SanCov, no ASan). Full write-up and methodology:
`docs/fuzzing-demonstrator-result.md`.

| Metric | Value |
|--------|------:|
| Steady-state execs/sec (dirty reset) | 1309 |
| Steady-state execs/sec (full-copy reset) | 271 |
| Reset latency p50 / p99 | 36 / 60 us |
| page-copy p50 / register-restore p50 | 35 / 1 us |
| Dirty-set size p50 / p99 / max (16 KiB pages) | 44 / 50 / 50 |
| Distinct edges (coverage) | 144 |
| Time-to-rediscover planted CVE (synthetic, ASan) | 0.002 s |

Reproduce: `M3_DURATION=60 python3 scripts/fuzz_m3_bench.py`.
