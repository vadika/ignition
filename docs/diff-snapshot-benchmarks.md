# Diff-snapshot timing — measured on real HVF hardware

Date: 2026-06-13. Host: Apple Silicon, macOS 26.5. Guest: aarch64 Linux, busybox
rootfs, single vCPU, **512 MiB RAM**, 16 KiB guest pages. Numbers are the **median
of 3 runs** unless noted, with min/max in parentheses. Harness:
`scripts/diff_snapshot_bench.py` (pty console driving, `time.monotonic()` clocks).

> **Snapshot-write times were re-measured (2026-06-13) with an in-process VMM timer**
> (`Snapshot-write-time`), correcting an earlier console-poll artifact. See §2 — a
> Diff (17–58 ms) is much faster to write than a Full (147 ms); the old ~317–372 ms
> band was rendezvous + console + poll overhead, not the write. The AC-comparison
> write rows further down are the old (superseded) numbers, kept only for the
> power-sensitivity point and flagged as such.

> **Power state: the headline tables below were measured on battery.** A full
> re-measurement on **AC power** (`pmset -g batt` = "AC Power"; `pmset -g therm`
> reported no thermal/performance warnings) reproduced the same medians within
> run-to-run noise — every metric inside ±~10 %, and the prime suspects (dd write
> throughput, dd fault tax) did **not** improve on AC. So on this host the
> power source did **not** materially affect these metrics. See
> ["Re-measured on AC power"](#re-measured-on-ac-power) at the end for the
> side-by-side. The numbers in the tables stand as-is.

> **Debug build.** All headline numbers are an **unoptimized `target/debug/boot`**
> (`cargo build` with no `--release`). A release data point is included at the end —
> and, perhaps surprisingly, **release is within noise of debug** for these metrics:
> they are I/O- and guest-bound, not VMM-CPU-bound. Still treat absolute
> milliseconds as figures from this host, not portable production latency.

This doc is a focused follow-up to `docs/benchmarks.md` (which measured plain
boot vs restore and predates the diff-snapshot feature). It quantifies the
**cost and benefit of diff/incremental snapshots** specifically.

## What each timer brackets

| Timer | Brackets |
|-------|----------|
| `Guest-boot-time` (boot-timer device) | VM start → guest init pokes the boot-timer MMIO byte. Kernel + early init, in the guest time domain. |
| boot wall | host `spawn()` of `boot` → `login:` bytes on the console. End-to-end to an interactive shell. |
| dd MB/s | busybox `dd` writing 64 MiB to `/dev/shm` (RAM tmpfs); dd's own reported rate. The write-protect fault tax shows here. |
| snapshot write | `Ctrl-A s` written to the pty → the handler prints `[snapshot] full\|diff '<name>' … written`. |
| `Restore-time` (host log) | `boot --restore` entry → restored guest handed to the run loop (base mmap + chain overlay + GIC/device/vCPU state). |
| restore wall | host `spawn()` of `boot --restore` → first non-empty console byte after we poke Enter. |

## 1. Dirty-tracking runtime overhead

### 1a. Boot time — without `--track-dirty` vs with

| Metric | Untracked | `--track-dirty` |
|--------|----------:|----------------:|
| `Guest-boot-time` (boot-timer) | **202 ms** (190–221, n=3) | **214 ms** (211–584, n=3) |
| boot wall → `login:` | **1241 ms** (1225–1264, n=3) | **1256 ms** (1254–1624, n=3) |

**Tracking adds little to boot.** Both medians move ~10–15 ms — within run-to-run
noise. The tracked column has one cold outlier each (584 ms / 1624 ms on the first
tracked run); the two steady-state runs are ~211–214 ms / ~1254–1256 ms, on top of
untracked ~202 ms / ~1241 ms. The write-protect arming happens once around boot and
the guest faults pages in lazily, so boot-to-login does not pay a big up-front
tracking tax here.

### 1b. In-guest write throughput — without vs with tracking

dd `if=/dev/zero of=/dev/shm/blob bs=1M count=64` (64 MiB into a RAM-backed tmpfs;
the rootfs ext4 is 100 % full so a disk write is impossible, and tmpfs is the right
target to expose the RAM write-protect fault).

| | Untracked | `--track-dirty` |
|--|----------:|----------------:|
| dd throughput | **2100 MB/s** (2100–2200, n=3) | **1500 MB/s** (1500–3600, n=3) |

**The write-protect fault tax is real but noisy.** Median throughput drops ~28 %
(2100 → 1500 MB/s) under tracking, because each first write to a clean page traps
out of write-protect before the store completes. But the spread is wide — one
tracked run measured 3600 MB/s (higher than untracked), so the signal is partly
swamped by tmpfs/host scheduling noise on a single 64 MiB pass. The tax is a
**per-page, first-touch** cost; on a workload that re-writes already-dirty pages it
disappears. Read this as "tracking can cost roughly a quarter of first-touch write
bandwidth," not a precise constant.

## 2. Snapshot write time — Full vs Diff

Measured by an **internal VMM timer** (`Snapshot-write-time = N ms`, logged by
`write_named_snapshot` / `write_named_diff`) that brackets exactly the write work:
`write_snapshot`/`write_diff_snapshot` (memory + GIC + `vmstate.json` + `disk.img`
clonefile) plus the manifest. Full is a fresh-boot root (whole 512 MiB RAM); Diffs are
taken after dirtying a bounded region, against a kept golden root.

| Snapshot | dirtied | dirty pages | write time |
|----------|--------:|------------:|-----------:|
| **Full root** (512 MiB RAM) | — | (whole RAM) | **147 ms** (124–195, n=5) |
| **Diff** | 8 MiB | ~903 | **17 ms** (14–36, n=5) |
| **Diff** | 64 MiB | ~4552 | **58 ms** (30–64, n=5) |

**A Diff is much faster to write than a Full** — the write cost is proportional to
bytes written, exactly as expected. The Full path streams the whole 512 MiB
(`write_all`) in ~147 ms; the Diff path packs only the dirtied 16 KiB pages — ~15 MB
at 8 MiB dirtied → ~17 ms (≈ 8.6× faster), ~75 MB at 64 MiB dirtied → ~58 ms
(≈ 2.5× faster). Roughly linear in packed pages: (58 − 17) ms over (4552 − 903) pages
≈ **~11 µs per packed 16 KiB page**, consistent with bulk memcpy + sequential write.

> **Measurement correction.** An earlier revision reported all three writes in a tight
> ~317–372 ms band and concluded a Diff "is NOT meaningfully faster to write." That was
> a **harness artifact**: the old timer bracketed `Ctrl-A s` keystroke → console line
> using a 300 ms drain-poll, so it folded in the vCPU stop-the-world rendezvous, console
> latency, and up to 300 ms of poll quantization — none of which is the write. With the
> in-process timer the write itself is 17–147 ms and clearly bytes-proportional. The
> ~300 ms a human sees after pressing `Ctrl-A s` is real, but it is rendezvous + console,
> **not** the snapshot write.

So the diff payoff is **both disk footprint and write latency** (plus chain semantics).

## 3. Restore latency — by chain depth

Restoring a Full-only base (1 layer), golden + 1 diff, and golden + 3 diffs. Each
diff layer adds a `read_diff_pages` + `apply_diff` memcpy overlay before vCPUs run.

| Restore target | layers | `Restore-time` (internal) | restore wall (→ first output) |
|----------------|-------:|--------------------------:|------------------------------:|
| **Full only** (golden) | 1 | **245 ms** (240–247, n=3) | **257 ms** (254–257, n=3) |
| **golden + 1 diff** (d1) | 2 | **243 ms** (237–245, n=3) | **258 ms** (254–259, n=3) |
| **golden + 3 diffs** (d3) | 4 | **242 ms** (242–244, n=3) | **257 ms** (256–258, n=3) |

**Restore latency is flat across chain depth.** 1 layer and 4 layers restore in the
same ~242–245 ms internal / ~257 ms wall — the per-layer overlay is lost in the
noise. Reason: each diff here is only ~900 pages (~14 MB), so `apply_diff` is a tiny
memcpy on top of the dominant cost (mapping the 512 MiB base + replaying GIC/device/
vCPU state). The cost *would* grow with very large or very many diffs (each layer's
dirty pages are read + copied), but for shallow chains of small deltas it is
effectively free. Restore also beats fresh boot here (~245 ms vs ~1241 ms boot-to-
shell) because it skips the kernel + init sequence entirely.

## 4. Disk footprint

| Artifact | logical (st_size) | physical (st_blocks×512) |
|----------|------------------:|-------------------------:|
| Full `memory.bin` | 512.0 MiB (536,870,912 B) | 512.0 MiB |
| Diff `memory.bin` (d1, 903 pages) | 14.79 MB | 14.79 MB |
| Diff `memory.bin` (d2, 891 pages) | 14.60 MB | 14.60 MB |
| Diff `memory.bin` (d3, 883 pages) | 14.47 MB | 14.47 MB |

A diff `memory.bin` is **packed, not sparse** — logical == physical == `n_dirty ×
16 KiB`. Each ~8 MiB-dirtied diff is **~14.5 MB, ~35× smaller than the 512 MiB full
RAM image**. (It's >8 MiB because the guest dirties incidental pages — kernel,
shell, page cache — during the interval, not only the blob.)

**Store totals.** The golden + 3-diff chain's total physical store was ~938 MB
(`st_blocks×512` summed over all four layer dirs). That is dominated by each layer's
`disk.img`, not by RAM: `disk.img` is written with APFS `clonefile` (copy-on-write),
so on disk the blocks are largely **shared** between layers even though each file's
`st_blocks` counts them — the summed number overstates true consumption. The RAM side
is the honest delta: **4 full snapshots ≈ 4 × 512 MiB = 2048 MiB of memory images**,
vs a golden + 3 diffs ≈ **512 + 3×~14.5 ≈ 556 MiB** — a ~3.7× saving here, growing
with chain length and shrinking with per-diff dirty-set size.

## Release-build data point

Same host, `target/release/boot`, to show the debug overhead. (Boot + restore only;
n=3, median.)

| Metric | Debug | Release |
|--------|------:|--------:|
| `Guest-boot-time` untracked | 202 ms | **214 ms** (211–237) |
| `Guest-boot-time` tracked | 214 ms | **216 ms** (211–218) |
| boot wall untracked | 1241 ms | **1259 ms** (1255–1644) |
| boot wall tracked | 1256 ms | **1259 ms** (1253–1261) |
| Restore-time (Full) | 245 ms | **243 ms** (241–248) |
| restore wall (Full) | 257 ms | **257 ms** (257–258) |

**Release is not meaningfully faster here** — every metric is within run-to-run
noise of the debug build. These timings are dominated by **guest** kernel/init work
and by **host I/O / HVF** (writing and reading the 512 MiB `memory.bin`, vCPU
exits), not by VMM CPU code the optimizer would speed up. So for *these specific*
boot/restore/snapshot-write metrics the debug-build caveat is largely moot. (A
CPU-bound VMM path — e.g. a huge diff pack or page scan — would still benefit from
`--release`; these workloads just aren't CPU-bound in the VMM.)

## Re-measured on AC power

The headline tables above were taken on **battery**, which can throttle the CPU.
To check whether that biased the numbers, the full suite was re-run with identical
parameters (`--mem 512`, n=3, same throwaway `vmstore-bench/` store) on **AC power**
— `pmset -g batt` → "Now drawing from 'AC Power'", `pmset -g therm` → no thermal or
performance warning recorded. Same debug build, same host, same day.

**Result: within noise of the battery run. Power source did not materially affect
these metrics on this host.** Side-by-side medians (n=3 each):

| Metric | Battery median | AC median | Δ | Δ % | Moved >10 %? |
|--------|---------------:|----------:|--:|----:|:------------:|
| `Guest-boot-time` untracked | 202 ms | 213 ms (208–224) | +11 ms | +5 % | no |
| boot wall untracked | 1241 ms | 1254 ms (1249–1608) | +13 ms | +1 % | no |
| dd 64 MiB untracked | 2100 MB/s | 2000 MB/s (2000–2100) | −100 MB/s | −5 % | no |
| dd 64 MiB `--track-dirty` | 1500 MB/s | 1500 MB/s (1400–1700) | 0 | 0 % | no |
| Full snapshot write ‡ | 317 ms | 350 ms (327–359) | +33 ms | +10 % | borderline† |
| Diff write (8 MiB) ‡ | 339 ms | 339 ms (336–340) | 0 | 0 % | no |
| Diff write (64 MiB) ‡ | 372 ms | 388 ms (356–391) | +16 ms | +4 % | no |
| Restore Full (internal) | 245 ms | 239 ms (238–247) | −6 ms | −2 % | no |
| Restore Full (wall) | 257 ms | 257 ms (256–257) | 0 | 0 % | no |
| Restore golden+1 (internal) | 243 ms | 243 ms (241–245) | 0 | 0 % | no |
| Restore golden+3 (internal) | 242 ms | 244 ms (242–245) | +2 ms | +1 % | no |
| Diff `memory.bin` (d1) | 14.79 MB | 14.88 MB (908 pages) | +0.09 MB | +1 % | no |

† The Full-write +10 % (317 → 350 ms) is at the noise floor, not in AC's favor — AC
was *slower* here, the opposite of a CPU-throttle story. The AC spread (327–359 ms)
overlaps the battery band, so this is run-to-run jitter on the snapshot fixed-cost
floor (quiesce + GIC + device serialize + RAM re-protect), not a power effect.

‡ **Superseded.** These three snapshot-write rows were measured with the old
console-poll harness, which timed `Ctrl-A s` keystroke → console line (300 ms drain
quantization + vCPU rendezvous + console latency), **not** the write. They are kept
only to show that even that conflated number was power-insensitive. The corrected
internal-timer write numbers are in §2 (Full 147 ms, Diff 17–58 ms) — those were
re-measured on battery; the write is bytes/I/O-bound, not CPU-clock-bound, so power
source is immaterial here too (same conclusion as every other row).

**Tracked-boot was excluded from the table** because both runs are dominated by a
cold-start outlier in the first `--track-dirty` boot (write-protect arming), and the
median lands on different samples run-to-run. Battery medians were 214 ms / 1256 ms
(steady samples ~211–214 / ~1254–1256, one 584 / 1624 outlier); AC's steady samples
were ~230 ms / ~1274 ms with two slow 606–608 / 1645–1652 cold samples, so AC's
*median* fell on the outlier (606 / 1645 ms). Comparing **steady-state** tracked
boots (AC ~230/1274 vs battery ~211–214/1254–1256) the gap is ≤20 ms — same noise
regime as the untracked column. This is a cold-cache/arming artifact, not throttling.

**The two metrics most likely to move on AC — dd write throughput and the per-page
fault tax — did not.** dd untracked was if anything *lower* on AC (2000 vs 2100 MB/s,
−5 %, inside the documented wide tmpfs spread), and tracked dd was identical at the
median (1500 MB/s). The first-touch write-protect fault tax is a per-page guest/HVF
cost, not a CPU-clock-bound one, so AC's higher sustained clock buys nothing here.
Boot and restore are guest- and I/O-bound and stayed flat as expected.

Net (power state): **every metric reproduced within noise on AC** — diff ~14.5–14.9
MB / ~35× smaller, ~12 ms tracked-boot tax (steady-state), ~28 % first-touch
write-throughput tax (median; noisy band, reconfirmed), and ~240–245 ms flat restore
across chain depth. The power source changed nothing material on this host.

> **Note on snapshot-write numbers.** The write-time figures in this AC section
> (~317–388 ms) are from the **superseded console-poll harness** and are wrong as
> "write time" — see §2. The corrected internal-timer numbers are Full **147 ms** /
> Diff **17–58 ms**, and a Diff *is* meaningfully faster to write than a Full. Any
> article copy still saying "diffs aren't faster to write (~340 vs ~317 ms)" must be
> revised.

## Methodology & caveats

- **Harness:** `scripts/diff_snapshot_bench.py`, driving `boot` over a pty exactly as
  `scripts/restore_test.py` / `scripts/diff_snapshot_test.py` do — `\x01 s` for the
  snapshot escape, root login with no password, and paced (≤8-byte) keystroke bursts
  because the guest UART RX FIFO is only 16 bytes. Throwaway store `vmstore-bench/`
  (gitignored), removed at start and end.
- **Power state.** Headline tables: **battery**. Full re-run on **AC power** (verified
  via `pmset -g batt`/`pmset -g therm`, no thermal/perf warnings) reproduced every
  metric within noise — see ["Re-measured on AC power"](#re-measured-on-ac-power). On
  this host the power source did not materially change boot, dd throughput, snapshot
  write, or restore.
- **Diff chains are built by restore-then-resnapshot.** A single process cannot diff
  against itself (one `write_name` per process + the same-name-as-parent guard), so
  each diff layer is produced by restoring its parent with `--track-dirty --name <new>`,
  dirtying ~8 MiB in `/dev/shm`, and `Ctrl-A s`. This is the designed diff path.
- **Debug build.** Unoptimized; a release build is faster (see table above). All other
  numbers are debug.
- **Warm page cache, single vCPU, 512 MiB, minimal guest.** Absolute numbers grow with
  RAM size and a fuller guest; the *relative* comparisons (Full vs Diff write,
  restore-by-depth, footprint) are the durable findings.
- **dd-on-tmpfs is a noisy probe of the fault tax** (see §1b) — reported as a median
  with a wide spread, not a clean constant.
- **Clock domains differ.** `Guest-boot-time`/`Restore-time` are stamped inside the
  VMM relative to VM start; the wall timers are host `spawn()` → console, including
  process-spawn overhead. They are complementary, not subtractable.

## Reproduce

```sh
cargo build -p ignition-spike --bin boot && scripts/sign.sh target/debug/boot
python3 scripts/diff_snapshot_bench.py --mem 512        # full debug suite
python3 scripts/diff_snapshot_bench.py --release        # release boot/restore point
```
