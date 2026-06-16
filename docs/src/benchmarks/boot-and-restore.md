# Boot & restore latency

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

```console
cargo build -p ignition-spike --bin boot && scripts/sign.sh target/debug/boot
python3 scripts/benchmark.py 6      # both fresh-boot methods + restore
# component scripts:
python3 scripts/boot_vs_restore_timing.py   # launch -> running, phased
python3 scripts/restore_test.py             # snapshot -> restore, CPU% + responsive
```

## Disposable browser latency

Date: 2026-06-16. Host: Apple Silicon, macOS 26. Guest: the browser rootfs
(`rootfs-browser.ext4`) — overlay root, Firefox ESR under cage, **2 vCPUs, 1 GiB**,
`--gui --net --track-dirty`. Three operations, all in **ms**, `n = 3`
(`scripts/disposable_browser_bench.py` for the first two; hot restore measured by hand
— serial input does not reach the escape FSM under `--gui`, so the in-place reset is
driven from the GUI window with `Ctrl+Alt+R`).

> **Config note:** figures taken with `--track-dirty` at 1 GiB (the browser now runs at
> 2 GiB — see the OOM note in [the disposable browser page](../features/disposable-browser.md)).
> A full-RAM copy without `--track-dirty` has lower *warm* hot-restore latency but reverts
> live virtio-gpu/input state out from under the running window and **wedges the display**,
> so dirty-only is retained for the GUI.

| Operation | What it is | mean | range |
|-----------|------------|-----:|------:|
| **Cold boot** → `BROWSER_READY` | full kernel boot + overlay `switch_root` + Firefox cold start, to a painted homepage (wall) | **7774 ms** | 7618–8084 |
| Cold boot — `Guest-boot-time` | kernel + early init only (guest time domain) | 599 ms | 536–724 |
| **Cold restore** — `Restore-time` | a fresh `boot --restore browser-base` process: clonefile + `mmap(MAP_SHARED)` + GIC/device/vCPU state restore, before the guest runs | **130 ms** | 127–131 |
| **Hot restore** — `Reset-time` | in-place `Ctrl+Alt+R` rollback of a *running* clone (dirty-page revert + device restore + repaint), after browsing to a real page | **100–1220 ms** | (working-set dependent) |

One cold-restore tail breakdown (132 ms total): `dev:93ms` (recreating the virtio
set — gpu/net/blk/input — dominates) + `stdin:39ms`; everything else is sub-ms.

### Interpretation

- **Cold restore is ~60× faster than cold-booting to a usable browser** (130 ms vs
  ~7.8 s). That gap *is* the disposable-browser value proposition: the warm snapshot
  skips Firefox's ~7 s cold start. Cold restore is also remarkably flat (127–131 ms)
  because `clonefile` + `mmap(MAP_SHARED)` does no large up-front read — the working
  set faults in lazily as the restored browser runs.

- **Hot restore (in-place reset) cost scaled with the dirtied working set under
  `--track-dirty`** — the dirty-only rollback synchronously copies the pages dirtied
  since the checkpoint, page-by-page, plus a full re-protect. Right after loading a
  heavy page the first reset was 1220 ms; subsequent resets fell to 207 → 100 ms.

- **Full-copy is faster *warm* but not viable for the GUI.** A no-`--track-dirty` reset
  is a single sequential full-RAM copy — lower warm latency than the scattered dirty-only
  copy — but it reverts the live virtio-gpu/input ring + fence state out from under the
  still-running window (the GUI host threads are not quiesced during the rollback), which
  **wedges the display** (cage blocks in `virtio_gpu_queue_fenced_ctrl_buffer`). Dirty-only
  reverts a smaller, more consistent subset and keeps the window alive, so it is retained.

- **In-place reset of a live, actively-rendering GUI is fundamentally fragile on HVF.**
  Rolling back guest RAM + vCPU state while the GIC and the virtio devices stay live leaves
  in-flight interrupt state unreconciled — surfacing in turn as net (`not a head`), vtimer
  (RCU stall), and GPU-fence (cage hang) wedges, because `hv_gic_set_state` cannot be
  re-applied mid-run. A relaunch-on-reset (tear down + a fresh `--restore`, i.e. the flat
  130 ms cold-restore path) sidesteps all of it and is the robust alternative when an
  in-place snap-back proves unreliable under load.

- **Known cosmetic warning (non-fatal):** after an in-place reset under active traffic
  the guest may log `virtio_net … not a head`. Root-caused via instrumentation: the
  rollback is *complete* (each reset reverted 600 MB–1 GB of dirtied pages, no malformed
  heads), so this is not corruption. The warm-base snapshot froze the net RX queue
  mid-flight (the device had completed RX into descriptors and advanced `used.idx`
  before the guest drained them); the in-place reset replays those completions on resume
  — and one descriptor is no longer a chain head in the rolled-back state, so the guest
  *drops* it (the warning) before the carrier-bounce rebind re-inits the NIC and
  re-DHCPs. A cold restore never hits it because the guest rebinds first. It self-heals;
  net works after. (A net-idle warm-base snapshot would remove it at the source.)

## Related

- [Snapshot & restore](../features/snapshot-restore.md) — the feature these numbers measure.
- [Disposable browser](../features/disposable-browser.md) — the workload behind the latency table above.
- [Snapshot-fuzzing benchmark](fuzzing.md) — execs/sec as a direct readout of reset latency.
