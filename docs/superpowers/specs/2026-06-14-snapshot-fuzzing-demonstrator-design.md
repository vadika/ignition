# Snapshot-Fuzzing Demonstrator — Design

_Status: draft · 2026-06-14 · depends on: in-loop `reset()`, `hv_vm_protect` dirty tracking (roadmap near-term)_

## 1. Why

The clone-from-warm primitive is shipped, but "works" and "provably fast & correct" are
different claims. A snapshot fuzzer closes that gap with one artifact that serves two ends
at once:

1. **Benchmark.** Throughput is `1 / (reset_time + run_time)`. With `run_time` driven to a
   single parser call (microseconds), execs/sec becomes a near-direct readout of reset
   latency — the exact number that says "the snapshot path is fast."
2. **Correctness stress test.** A fuzz loop performs millions of restores. Any state the
   snapshot fails to capture or reset — a stray register, a virtio queue index, a missed
   dirty page — surfaces within seconds as nondeterministic crashes or coverage that never
   stabilizes. This is the harshest possible regression test for the load-bearing code.

This is a demonstrator, not a product. It is deliberately scoped to prove the primitive and
produce numbers, then to reach one domain target (firmware/TEE) that nothing else can fuzz
comfortably.

## 2. Goals / non-goals

**Goals**
- Drive a guest-resident parser through millions of reset→inject→run→observe iterations.
- Rediscover a *planted, known* bug as the correctness gate (no false "it works").
- Produce execs/sec, reset-latency distribution, and dirty-set-size distribution.
- Reuse existing ignition primitives; minimize net-new surface.

**Non-goals (this spec)**
- General-purpose fuzzing UX, web dashboards, crash triage/minimization beyond raw capture.
- Multi-tenant or untrusted-host hardening (gated on the Seatbelt sandbox; see roadmap).
- Multi-vCPU fuzzing, network/disk-in-the-loop targets, in-kernel targets. (Later.)
- Hardware coverage (ARM ETM). Source instrumentation only.

## 3. Architecture

```
   host (VMM + fuzzer brain)                 guest (single vCPU, initramfs)
   ─────────────────────────                 ──────────────────────────────
   libAFL: corpus, mutators,                 harness (PID 1 or init):
   scheduler, feedback                         one-time setup
        │                                       loop {
        │ mutate                                  doorbell SNAPSHOT_ME  ← snapshot/reset PC
        ▼                                          len = SHARED.len
   ignition Executor                               parse(SHARED.input, len)  ← the target
     ├─ write input  ─────────►  SHARED WINDOW  ──► (read by harness)
     ├─ zero coverage ────────►  (hv_vm_map'd host RAM, known GPA)
     ├─ resume vCPU ──────────►  guest runs parser; SanCov writes counters ►┐
     │                                                                       │
     ◄── DONE / CRASH doorbell ◄── doorbell (trap-MMIO register) ◄───────────┘
     ├─ read coverage  ◄──────  SHARED WINDOW
     ├─ classify (new cov? crash?)
     └─ reset(dirty pages + regs) ─► guest back at snapshot PC
```

Two channels between host and guest, both reusing patterns already in the tree:

- **Doorbell** — a trap-MMIO register on a new `ignition-fuzz` device. A guest store traps
  to the VMM exactly like the existing **boot-timer** pseudo-device. Carries the control
  protocol (§5).
- **Shared window** — a RAM region `hv_vm_map`'d into guest physical space at a known GPA,
  exposed to the guest as the device's memory region (advertised in the FDT, mmap'd by the
  harness). Backs the input buffer and coverage bitmap. The host reads/writes it directly
  through its own VA — *no virtio, no syscall, no I/O in the loop* (which is also why this
  does not depend on vsock-E2).

Input is injected **below the target's narrowest interface** (a buffer in memory), so there
is no external interface to model or reset. This is the libpng `..._from_memory` choice
generalized.

## 4. The `ignition-fuzz` MMIO device

A single `MmioDevice` (reuses `DeviceManager` MMIO/SPI allocation + FDT emission +
`DeviceRecord` snapshot hooks). Two regions:

**Control registers (trap-MMIO, 4 KiB):**

| Offset | Name | Access | Meaning |
|---|---|---|---|
| `0x00` | `DOORBELL` | W | guest writes a command code (§5); traps to VMM |
| `0x04` | `INPUT_LEN` | RW | length of the current input in the shared window |
| `0x08` | `CRASH_CODE` | W | ASan/abort reason class on a CRASH doorbell |
| `0x0c` | `STATUS` | R | VMM→guest handshake (READY / GO), optional |

**Shared window (RAM-backed, `hv_vm_map`, size configurable, default 2 MiB):**

| Sub-region | Purpose | Written by |
|---|---|---|
| `input[]` | bytes handed to the parser | host (each iteration) |
| `coverage[]` | SanitizerCoverage 8-bit counters | guest (during run) |

The window is RAM the VMM owns, so guest VA→PA gymnastics are avoided: the harness mmaps
the device region (known GPA from the FDT), and the host reads/writes the same bytes via its
backing pointer. Snapshot: the device's `DeviceRecord` records the window size + GPA; the
window contents themselves are *host-managed* and excluded from snapshot/reset (see §6).

## 5. Control protocol

Doorbell command codes (guest → VMM):

- `SNAPSHOT_ME` (0x1) — "one-time setup complete, I am parked at the parse site." On the
  **first** receipt the VMM advances PC past the store, captures the snapshot (regs +
  memory baseline), and hands control to the host fuzzer loop. Subsequent iterations never
  re-execute this store, because reset lands PC *after* it.
- `DONE` (0x2) — "input processed cleanly." VMM reads coverage, classifies, resets.
- `CRASH` (0x3) — emitted from the target's ASan death callback (and a `SIGABRT`/`SIGSEGV`
  handler as backstop). VMM records the input + `CRASH_CODE` + register snapshot, then resets.

Per-iteration sequence (host-driven; vCPU is stopped at each ◆):

```
◆ (post-snapshot, vCPU stopped)
  host: input = mutator.next();  copy into SHARED.input;  INPUT_LEN = len;  zero SHARED.coverage
  host: resume vCPU
  guest: len = INPUT_LEN;  parse(SHARED.input, len)        // SanCov fills SHARED.coverage
  guest: doorbell DONE      (or CRASH from death callback)
◆ VMM trap on doorbell:
  host: read SHARED.coverage   // MUST read before reset
  host: feedback.is_interesting(coverage)? -> corpus.add(input)
  host: if CRASH -> solutions.add(input, CRASH_CODE, regs)
  host: reset(dirty_pages -> base; regs -> snapshot regs)  // PC lands post-SNAPSHOT_ME
  loop
```

Ordering invariant: **coverage is read on the doorbell exit, before reset**, because the
coverage page is guest-written and would otherwise be rolled back.

## 6. Reset semantics (the one genuinely new piece)

Reset rolls the guest back to the snapshot point in a *live* VMM, per iteration, without
re-`clonefile`ing. Two halves:

- **Registers** — restore the snapshot's vCPU register file (incl. PC, SP). Single-vCPU, so
  this is a direct `hv_vcpu_set_reg` sweep; the multi-vCPU rendezvous is not needed here.
- **Memory** — restore *only guest-dirtied pages* to the base. Dirty set discovered via
  `hv_vm_protect` write-protect + fault logging (the near-term roadmap item; this is its
  second consumer alongside diff snapshots). At the snapshot point all guest pages are
  write-protected; the first guest write to a page faults, the VMM logs it and unprotects;
  reset copies the logged pages back from base and re-protects.

**Host-managed pages are excluded from the dirty-reset.** The shared window (input +
coverage) is written by the *host* (whose writes don't fault) and is overwritten/zeroed each
iteration anyway, so base-rollback would be redundant and racy. The VMM marks the window GPA
range as reset-exempt.

**Phasing to de-risk:**
- **v0 reset = full-memory copy.** Boot a small guest (`--mem 96`), `memcpy` the whole guest
  RAM from base each iteration. ~10–15 ms/iter, ~70 execs/sec — slow but *correct*, and
  removes the dependency on dirty tracking so the loop, injection, coverage, and crash paths
  can be proven first.
- **v1 reset = dirty-pages only.** Swap in the `hv_vm_protect` dirty set. This is the step
  that turns reset latency into the throughput story; everything else stays identical.

## 7. Determinism requirements

Same snapshot + same input ⇒ same execution, or coverage never stabilizes and crashes don't
reproduce. The whole-machine reset does most of the work (allocator state, guest RNG state,
and the frozen RTC all roll back to identical values, so `malloc` returns identical
pointers). Remaining rules for the fuzz config:

- `--smp 1` — no scheduling nondeterminism.
- **No virtio-rng** in the loop (host entropy is a variance source). Omit the device or seed
  deterministically.
- **No virtio-net / vsock** — nothing in the loop needs them; absent devices = less state.
- **initramfs, no rootfs writes** — harness + target + libs in initramfs; no virtio-blk in
  the loop, so no block dirtying. (blk may load the initramfs pre-snapshot.)
- All target one-time init runs **before** `SNAPSHOT_ME`, so per-iteration work is just the
  parse.
- Frozen time: the guest clock is stuck at snapshot time by construction (RTC state rolls
  back); targets must not depend on wall-clock progress within a run.

## 8. Target build

- Compile target + harness with `-fsanitize=address` (catches non-crashing corruption and
  turns it into a deterministic abort) and `-fsanitize-coverage=inline-8bit-counters`.
- Place the SanCov counter section in the shared window via
  `__sanitizer_cov_8bit_counters_init` pointing at the mmap'd device region, so the host
  reads counters directly.
- `__asan_set_death_callback` → write `CRASH` to the doorbell with a reason class in
  `CRASH_CODE`; backstop with `SIGSEGV`/`SIGABRT` handlers that do the same.
- Harness reads exactly `INPUT_LEN` bytes; never reads the uninitialized tail of the window.

## 9. Host fuzzer brain — libAFL Executor

Don't reimplement corpus/mutation/scheduling. Integrate **libAFL** (Rust, matches the
codebase) and implement ignition as a custom `Executor`:

- `Executor::run_target(input)` → inject into shared window, resume vCPU, await doorbell.
- Observer over the coverage window (8-bit counters) → `MaxMapFeedback`.
- `CrashFeedback` fed by the CRASH doorbell → solutions corpus.
- Standard `HavocMutator` + `QueueScheduler` to start.

Bonus: "ignition is a libAFL executor" is itself an adoption seam (roadmap), not just demo
plumbing.

## 10. Milestones

- [ ] **M0 — loop skeleton (v0 reset).** `ignition-fuzz` device (doorbell + window), harness,
  full-memory reset, blind random mutation, CRASH capture. No coverage yet. Gate: a
  hand-injected malformed input is captured as a crash.
- [ ] **M1 — correctness gate.** Target = **libpng, known-CVE build** (e.g. CVE-2015-8126).
  Gate: fuzzer rediscovers the planted crash from an empty/seed corpus, deterministically
  reproducible from the saved input.
- [x] **M2 — coverage + dirty-page reset (v1).** Add SanCov window + libAFL feedback; swap
  reset to `hv_vm_protect` dirty set. Gate: coverage curve stabilizes; execs/sec jumps.
- [x] **M3 — benchmark.** Target = **libpng current**. Capture execs/sec, reset-latency
  p50/p99, dirty-set-size distribution. Output: `docs/fuzzing-demonstrator-result.md` +
  numbers into `docs/benchmarks.md`.
- [ ] **M4 — stateful targets.** `freetype` / `libxml2`: larger dirty sets, more bug surface;
  stresses reset harder.
- [ ] **M5 — domain payoff.** TPM 2.0 command-handler or OP-TEE TA harness, parked at the
  command-parse entry, input injected into the command buffer. The target nothing else can
  fuzz comfortably; the novel/publishable landing.

## 11. Metrics (M3 deliverable)

- Throughput: execs/sec (steady-state, single core).
- Reset latency: p50 / p99, decomposed into register-restore vs page-copy.
- Dirty-set size: pages dirtied per iteration (distribution) — explains reset latency and
  feeds the diff-snapshot work.
- Time-to-rediscover the planted CVE (M1) — a deterministic correctness number.
- Coverage growth curve.

## 12. Risks & open questions

- **Shadow-memory cost.** ASan shadow is 1/8 of the working set; touched shadow pages join
  the dirty set and inflate reset. Measure; if dominant, consider a coverage-only build for
  the benchmark and an ASan build for bug-finding as separate runs.
- **Window exemption correctness.** Mis-marking the shared window (reset-exempt vs tracked)
  silently corrupts determinism. Add an assertion: after reset, a canary in tracked memory
  matches base; the window does not.
- **Snapshot-point drift.** If any per-iteration setup leaks before `SNAPSHOT_ME`, run_time
  inflates. Verify the snapshot PC sits immediately before the parse call.
- **Doorbell vs PC advance.** On `DONE`/`CRASH` the VMM must *not* advance PC then resume; it
  resets PC from the snapshot. On the one-time `SNAPSHOT_ME` it *does* advance, so the
  captured PC is post-store. Get this asymmetry right.
- **libAFL executor latency.** Per-iteration host work (mutation, feedback, classify) must
  stay well under reset+run or it becomes the bottleneck; keep the Observer reading a fixed
  counter region, not scanning.

## 13. Out of scope (revisit later)

Multi-vCPU fuzzing; network/disk/syscall-path targets; in-kernel and hypervisor targets;
crash minimization/dedup; persistent corpus sync across hosts; hardware-assisted coverage.

