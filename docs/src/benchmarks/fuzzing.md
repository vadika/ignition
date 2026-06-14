# Snapshot-fuzzing benchmark

The in-VMM snapshot fuzzer resets the guest to a parse-entry snapshot every
iteration using `hv_vm_protect` dirty-page tracking, without leaving the VMM.
This page reports the M3 throughput and reset numbers for that machinery on real
hardware. For how the fuzzer works step by step, see
[How snapshot fuzzing works](../fuzzing/overview.md).

Host: Apple Silicon (M3), macOS 26.5. Guest: aarch64, 128 MiB, single vCPU,
16 KiB page granule. Target: libpng 1.6.43 + zlib 1.3.1.

## Results

| Metric | Value |
|--------|------:|
| Steady-state execs/sec (dirty reset) | 1309 |
| Steady-state execs/sec (full-copy reset) | 271 |
| Dirty vs full-copy speedup | 4.8x |
| Reset latency p50 / p99 | 36 / 60 us |
| page-copy p50 | 35 us |
| register-restore p50 | 1 us |
| Dirty-set size p50 / p99 / max (16 KiB pages) | 44 / 50 / 50 |
| Distinct edges (coverage) | 144 |
| Time-to-rediscover planted CVE (synthetic, ASan) | 0.002 s |

Dirty reset runs at 4.8x the throughput of a full-copy reset on the same target.
The reset cost is dominated by the page copy (about 35 us); register restore is
about 1 us. The dirty set the reset copies back is 44 to 50 pages per iteration.
Coverage reached 144 distinct edges, and the planted heap-overflow CVE was
rediscovered deterministically in 0.002 s from the seed corpus.

## Methodology

- SanCov-only libpng build, no AddressSanitizer. The throughput, reset, and
  dirty-set numbers come from a coverage-only build so the snapshot machinery is
  isolated. ASan shadow (1/8 of the working set) would join the dirty set and
  inflate the reset, so the deterministic bug-finding number uses a separate ASan
  build.
- Single core, steady state. execs/sec is measured over a fixed wall-clock window
  after warm-up.
- The Linux/KVM cross-check was dropped from scope. These are ignition's own
  dirty-reset vs full-copy numbers only.

Reproduce:

```console
M3_DURATION=60 python3 scripts/fuzz_m3_bench.py
```
