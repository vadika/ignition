# Running the fuzzer

This chapter covers the build, the three gate scripts, the benchmark driver, and
every `boot --fuzz` flag. For the design and the measured numbers see
[How snapshot fuzzing works](overview.md). For the kernel image and the two fuzz
initramfs images (synthetic ASan target and the libpng target) see
[Building guest assets](../getting-started/guest-assets.md).

## Build and sign

The fuzzer lives in the `boot` binary of the `ignition-spike` crate. Build it,
then code-sign with the Hypervisor.framework entitlement (macOS will not let an
unsigned binary call `hv_vm_create`):

```console
$ cargo build -p ignition-spike --bin boot
$ scripts/sign.sh target/debug/boot
```

Every command below assumes the signed `target/debug/boot`, a kernel at
`kimage/out/Image`, and the matching initramfs image already present.

## Gates

Three Python drivers run the binary end to end. They locate the artifacts via
`BOOT_BIN`, `FUZZ_KERNEL`, and the `FUZZ_INITRAMFS*` environment variables and
fall back to the default paths above.

### M1: rediscover the planted bug, deterministically

```console
$ python3 scripts/fuzz_m1_test.py
```

Boots the fuzzer with a near-boundary seed (a valid `FUZ` chunk, length 16) and
checks that blind havoc bumps the length field past the buffer, trips the
sanitizer, and writes a solution file. Then it replays the saved crash input
verbatim and confirms it re-crashes. This is the correctness anchor.

### M2: coverage feedback plus dirty-page reset

```console
$ python3 scripts/fuzz_m2_test.py
```

Asserts that coverage grows above its first reading and the corpus expands past
the single seed, that the planted bug is still found through the dirty reset and
replays deterministically, and that dirty-reset execs/sec beats full-copy
execs/sec on equal wall-clock.

### M3: the benchmark

```console
$ M3_DURATION=60 python3 scripts/fuzz_m3_bench.py
```

Runs three fixed-wall-clock passes against real libpng (dirty reset, then
full-copy reset for the speedup ratio) and the synthetic ASan target (for
time-to-rediscover), parses the metrics file, and gates that the machinery
produced usable numbers. `M3_DURATION` (seconds) and `M3_MEM` (guest MiB) tune
the run.

## Driving `boot --fuzz` directly

The gate scripts wrap this invocation. A representative direct run:

```console
$ target/debug/boot --fuzz \
    --initramfs kimage/out/fuzz-initramfs-libpng.cpio \
    --reset dirty \
    --seed corpus/seed.png \
    --metrics /tmp/fuzz-metrics.txt \
    kimage/out/Image
```

`SIGINT` (Ctrl-C) stops the loop and flushes the metrics file cleanly.

### `--reset dirty|full`

How the guest RAM is rolled back between iterations.

- `dirty` (default): per-iteration dirty-page rollback. `hv_vm_protect`
  write-protects guest RAM; the first write to each 16 KiB page traps, marks the
  page dirty, and re-grants write access. The reset copies back only that dirty
  set, then restores the vCPU registers.
- `full`: the full-RAM-copy baseline. Every iteration copies the entire guest
  RAM from the snapshot regardless of what changed. Correct and simple, and the
  reference point the dirty reset is measured against.

### `--metrics <path>`

On clean shutdown the controller writes a metrics file at `<path>` containing:

- `execs/sec`: steady-state throughput.
- reset-latency p50 and p99, split into the page-copy cost and the
  register-restore cost (the page copy dominates; register restore is about
  1 us).
- the dirty-set-size distribution (pages dirtied per iteration, p50/p99/max).
- the coverage curve, emitted as a series of `covsample` lines (timestamp,
  distinct edges) so the coverage-over-time growth is plottable.
- time-to-rediscover the planted bug, the deterministic correctness number.

### Other flags

- `--initramfs <path>`: the guest root image, which selects the target. Use the
  synthetic ASan image for bug-finding and correctness, or the libpng image for
  the throughput benchmark; see
  [Building guest assets](../getting-started/guest-assets.md).
- `--seed <path>`: a seed corpus input the fuzzer starts from.
- `--solutions <dir>`: where crash inputs are written.
- `--replay <path>`: replay a saved input once instead of fuzzing, to confirm a
  crash reproduces deterministically.
- `--mem <MiB>`: guest RAM size.
