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

## TPM 2.0 command processor (ms-tpm-20-ref)

The fuzzing payoff target: Microsoft's reference TPM 2.0 stack
(`ms-tpm-20-ref`, real OpenSSL crypto backend) compiled as an aarch64 Linux
userspace target on the same snapshot fuzzer. `target_init` runs the one-time
manufacture / power-on / `TPM2_Startup` **before** the snapshot doorbell, so the
snapshot captures a fully-started TPM; each iteration runs one command through
`ExecuteCommand` and the TPM's mutable global state (NV image, sessions, objects)
is rolled back by the per-iteration dirty-page reset.

Honest framing: this runs in the normal-world Linux guest (no EL3 / secure world
on HVF), so the win is **fast reset of a large, stateful firmware workload on
Apple Silicon**, not "impossible to fuzz elsewhere." It is the stepping stone to
targets that genuinely need a platform a host `fork()` can't provide.

Measured on HVF (`scripts/fuzz_tpm2_bench.py`, GetCapability seed, single core,
`--mem 256`, 30 s windows):

| metric | dirty reset | full reset |
|---|---|---|
| execs/sec | **1443** | 140 |
| reset latency p50 | 38 µs | — |
| coverage (edges) | 419 | — |
| dirty-set p50 / max | 39 / 55 pages | — |

The dirty-vs-full speedup is **10.3×** — larger than libpng's 4.8×, because the
TPM's bigger working set makes a full-RAM-copy reset much slower (140 vs libpng's
271 execs/sec) while the dirty-set stays small (~39 pages). That gap *is* the
stateful-reset story: rolling back only what one command touched, in tens of
microseconds, instead of re-initialising or re-copying the whole TPM each run.

Correctness gate (`scripts/fuzz_tpm2_test.py`): a planted length-field OOB in the
`TPM2_NV_Write` path (ASan-instrumented) is rediscovered from the near-boundary
seed in **0.018 s** and replays deterministically. Real-CVE rediscovery (a
vulnerable upstream pin) is the tracked stretch; the OpenSSL backend that makes it
possible is already in place.

Reproduce:

```console
TPM2_DURATION=60 python3 scripts/fuzz_tpm2_bench.py   # benchmark
python3 scripts/fuzz_tpm2_test.py                     # correctness gate
```

The initramfs is built (on an arm64 Docker host) with
`kimage/build/build-fuzz-initramfs.sh tpm2` — it clones ms-tpm-20-ref at a pinned
commit and builds the TPM core + platform with ASan + SanCov against OpenSSL 1.1.
