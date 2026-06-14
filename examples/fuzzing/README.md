# Snapshot-fuzzing demo — the userspace twin of ignition M0/M1

A tiny, **runnable** model of the snapshot-fuzzing loop from
`docs/superpowers/specs/2026-06-14-snapshot-fuzzing-demonstrator-design.md`.

It runs **anywhere with gcc** (Linux, or macOS with the gcc shim), so you can watch the
loop find a planted bug *today*, before the ignition `ignition-fuzz` device and in-loop
`reset()` exist. The only thing it swaps out is the reset mechanism: this uses `fork()`
where ignition will use VM snapshot/restore. Everything else — the harness shape,
inject-into-shared-memory, coverage feedback, crash detection, corpus — is the same.

## What it is

A coverage-guided fork-server fuzzer (~110 lines) against a target parser (~30 lines)
with a **planted, realistic bug**: a length-field heap overflow (`memcpy` into a
16-byte buffer trusting an attacker-controlled length — the archetypal image/font CVE shape).

## Run it

    ./run.sh            # build, fuzz, reproduce
    ./run.sh 200000     # cap the exec budget

Expected output (crash typically in <1s, ~100-200 execs from the boundary seed):

    ==> fuzz (coverage-guided, fork-per-input reset)
    seeded corpus with 1 boundary input (27 bytes)
      corpus=4    execs=10
    *** CRASH after 139 execs (0.10s, 1425 execs/sec), 4 corpus entries
        crashing input (26 bytes): 46 55 5a 01 43 11 00 ...   # FUZ v1, 'C' chunk len=0x11=17
    ==> reproduce the discovered crash (determinism gate)
    AddressSanitizer: heap-buffer-overflow ... WRITE of size 17 ... in parse target.c:28

The fuzzer discovers the magic ("FUZ"), reaches the chunk parser, and bumps a length
field from 16 to 17 — past the buffer size — and ASan catches the overflow.

## Files

- `target.c`   — the parser with the planted bug. Built **with**
  `-fsanitize-coverage=trace-pc` + ASan, at `-O0` (so the dead-store memcpy isn't elided).
- `harness.c`  — coverage callback + fuzzer brain (mutation, corpus, crash detection).
  Built **without** coverage instrumentation (else the callback recurses infinitely).
- `repro.c`    — replays a saved crash input against the target with the ASan report visible.
- `run.sh`     — build + fuzz + reproduce.

## Map to ignition (what each piece becomes)

| this demo                              | ignition M0/M1                                    |
|----------------------------------------|---------------------------------------------------|
| `fork()` per input                     | snapshot + in-loop `reset()` (dirty pages + regs) |
| parent process (never mutated)         | the immutable snapshot base                       |
| `MAP_SHARED` cov/input regions         | the `hv_vm_map`'d shared window on `ignition-fuzz` |
| `__sanitizer_cov_trace_pc` -> `cov[]`  | SanCov 8-bit counters in the shared window        |
| child `_exit(0)` vs SIGABRT (waitpid)  | `DONE` vs `CRASH` doorbell (magic-MMIO)           |
| `corpus[]` + `virgin[]` map            | libAFL corpus + `MaxMapFeedback`                  |

## Gotchas this demo encodes (you will hit all of these in ignition)

1. **Instrument the target, not the harness.** The coverage callback must live in a
   non-instrumented TU or it recurses into itself. (In ignition: the harness/VMM side
   is never SanCov-built; only the guest target is.)
2. **The callback fires before `main`.** Global constructors trip coverage before the
   shared map exists — guard with a null check. (In ignition: the window must be mapped
   before the first guest instruction at the snapshot point.)
3. **`-O0` or make the buffer observable.** At `-O1+` the optimizer deletes a write-only
   buffer, so the bug vanishes. Real targets read their output, so this is a demo artifact —
   but worth knowing when you build minimal harnesses.
4. **ASan must abort, not exit.** `ASAN_OPTIONS=abort_on_error=1` turns a finding into a
   signal the parent can see. (In ignition: `__asan_set_death_callback` -> CRASH doorbell.)
5. **Read coverage before reset.** Here the parent reads `cov[]` after `waitpid`, before
   the next fork zeroes it. (In ignition: read the coverage window on the DONE exit,
   before rolling back dirty pages.)
6. **Seed at the boundary.** A seed sitting one mutation from the bug makes the demo fast
   and deterministic — exactly how real seed corpora are built.

## What this demo does NOT show (because fork() != VM snapshot)

- Whole-machine state reset (kernel, devices, registers) — fork only snapshots one process.
- Dirty-page tracking via `hv_vm_protect` — fork's CoW is the kernel's; ignition hand-rolls it.
- Reset latency as the throughput ceiling — fork cost here is fixed; ignition's reset is the
  thing being optimized and benchmarked.

Those three are exactly the parts ignition adds, and exactly what the M3 benchmark measures.
