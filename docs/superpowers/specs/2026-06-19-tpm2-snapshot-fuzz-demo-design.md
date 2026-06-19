# TPM 2.0 snapshot-fuzz demo — design

**Date:** 2026-06-19
**Status:** in implementation
**Track:** Demonstrator — fuzzing payoff (firmware/TEE harnesses)

## Addendum (2026-06-19, during implementation): crypto backend → OpenSSL

The original spec chose a **trimmed, crypto-stubbed** TPM. Implementation found
the ceiling the spec's risk note anticipated: `crypt/` is 21 interdependent files
(`CryptUtil`, `CryptHash`, `CryptRand`, `CryptSym`, `CryptRsa`, `CryptEcc`,
`CryptSelfTest`, …) and `TPM_Manufacture` + `TPM2_Startup` run self-tests across
all of hash/sym/rsa/ecc. Stubbing that believably ≈ reimplementing the crypto
engine — fragile and multi-day.

**Decision (user-approved): use upstream's supported OpenSSL backend.** Build the
real `libtpm.a` + `libplatform.a` from a pinned ms-tpm-20-ref commit, instrumented
with ASan + SanCov. This is more robust, gives full command coverage, and unlocks
the real-CVE stretch. Build notes that differ from the body below:

- **Toolchain:** alpine 3.16 in the arm64 build container — it still ships OpenSSL
  1.1.1 (this TPM commit pokes 1.1 `BIGNUM` internals and `#error`s on 3.x) AND a
  working aarch64 ASan runtime (3.15 had neither pairing; 3.19 has OpenSSL 3).
- **Determinism:** only `Entropy.c` is overridden (deterministic counter via
  `tpm2_det_entropy.c`, `ar`-swapped into `libplatform.a`). NV stays upstream's
  static `s_NV` RAM array — already dirty-tracked and rolled back each iteration,
  so no custom platform needed.
- **No vendored TPM source in-repo:** the build case clones ms-tpm-20-ref at the
  pinned commit (`ee21db0a941decd3cac67925ea3310873af60ab3`) and builds it; the
  repo carries only `target_tpm2.c`, `tpm2_det_entropy.c`, and the build-script
  case. (Supersedes the `kimage/build/fuzz-harness/tpm2/` vendored-subset plan.)
- **Verified on HVF:** 1359 execs/sec, 153 coverage edges, clean GetCapability.

The rest of the spec (snapshot point, planted bug, gates, benchmark, seeds) stands
unchanged. Where the body says "no crypto backend / stubbed crypto," read OpenSSL.

## Goal

Snapshot-fuzz the Microsoft ms-tpm-20-ref TPM 2.0 command processor on Apple
Silicon, reusing the existing in-VMM snapshot fuzzer. The payoff over the
libpng/synthetic targets: TPM has large, mutable global state (NV image,
sessions, objects), so each iteration's dirty-page reset restores a big stateful
secure-world-style workload in microseconds — where a `fork()`-based fuzzer
either pays TPM re-init cost per run or risks state bleed across runs.

Honest framing: the target runs as **aarch64 Linux userspace** (this VMM gives a
single EL1 Linux guest — no EL3, no secure world). The claim is "fast, clean
reset of large stateful firmware state on Apple Silicon," not "impossible to fuzz
elsewhere." It is a stepping stone toward targets that genuinely need a platform
a host `fork()` can't provide (firmware at EL3, if nested-virt/EL2 ever lands).

## Non-goals

- No secure-world / EL3 / OP-TEE execution.
- No real crypto backend (OpenSSL/wolfSSL cross-compile) in the spine. Crypto is
  stubbed; real crypto is only needed for the real-CVE stretch (below).
- No VMM, device, boot-harness, controller, coverage, or reset changes. The
  fuzzer is already target-agnostic; this is a new target + build case + launcher
  only.
- No multi-command stateful sequences per iteration. One iteration = one command
  against the post-Startup snapshot (keeps determinism + the clean reset story).

## What gets built

### 1. Guest target — `kimage/build/fuzz-harness/target_tpm2.c` + vendored TPM subset

A trimmed, vendored copy of ms-tpm-20-ref containing:

- The command dispatcher — `ExecuteCommand(cmdSize, cmd, respSize, resp)`.
- The generated marshaling layer (`Unmarshal_*` / `Marshal_*`) — where the
  CVE-2023-1017-family of TPM parser bugs actually live.
- A **curated handler set** (small on purpose): `TPM2_Startup`, `TPM2_Shutdown`,
  `TPM2_GetCapability`, `TPM2_NV_DefineSpace`, `TPM2_NV_Write`, `TPM2_NV_Read`,
  `TPM2_ContextSave`, `TPM2_ContextLoad`. These exercise the command parser and
  NV/state paths with minimal crypto.
- Crypto calls reachable from the curated handlers are stubbed or compiled out;
  the platform RNG is made deterministic.

Target entry, matching the harness ABI `void target_parse(const uint8_t*, unsigned long)`:

```c
void target_parse(const uint8_t *data, unsigned long len) {
    unsigned char rsp[4096];
    uint32_t       rsp_len = sizeof rsp;
    unsigned char *rp      = rsp;
    ExecuteCommand((uint32_t)len, (unsigned char *)data, &rsp_len, &rp);
}
```

Build flags: target + vendored TPM sources `-fsanitize=address
-fsanitize-coverage=trace-pc -O1 -g`; `harness.c` keeps its existing
`-fsanitize=address -O1 -g` (no coverage; it defines the trace-pc callback). The
only change to `harness.c` is one additive, no-op-for-existing-targets
`target_init` hook (see §2); its fuzz loop is otherwise untouched.

### 2. One-time init before the snapshot (determinism + the reset story)

The harness's pre-`SNAPSHOT_ME` setup runs the full TPM bring-up once:

```
_plat__NVEnable(NULL) → TPM_Manufacture(1) → _TPM_Init() → TPM2_Startup(TPM_SU_CLEAR)
```

Only then does it ring `SNAPSHOT_ME`. The snapshot PC lands at the top of the
parse loop, so every iteration starts from the **identical post-Startup TPM
state**. The per-iteration dirty-page reset rolls back the TPM globals (NV image,
session/object slots) that the previous command mutated. This is the headline:
expect a dirty-set per iteration **notably larger than libpng's 44–50 pages**,
which is exactly the "large stateful reset" metric the demo exists to show.

Setup lives in the harness, which currently hardcodes a single `target_parse`.
The target exposes an optional `void target_init(void)` that the harness calls
before `SNAPSHOT_ME` if present (weak symbol or a `#define`-guarded call), so the
synthetic/libpng targets stay no-op. This is the one small harness touch; if a
weak symbol is awkward on the toolchain, fall back to a per-target compile macro.

### 3. Planted bug — G1 correctness gate

A deliberate out-of-bounds write in one curated handler, behind a recognizable
command shape — e.g. `TPM2_NV_Write` with an attacker-controlled size field
copied into a fixed-size stack/heap buffer with no bound check. This is the same
length-field shape the M1 gate already proved on the CVE-2015-8126 pattern. ASan
detects it → `__asan_set_death_callback` → `CRASH` doorbell → host records the
crashing input. The plant is documented in the target source as deliberate.

### 4. Host side — launcher + build case only

- `scripts/fuzz_tpm2_bench.py` — clone of `scripts/fuzz_m3_bench.py`. Seed = bytes
  of one minimal valid command (e.g. `TPM2_Startup` or `TPM2_GetCapability`).
  Runs `boot --fuzz --initramfs kimage/out/fuzz-initramfs-tpm2.cpio --seed <file>`
  in both `--reset dirty` and `--reset full`, with `--metrics`. Asserts the
  benchmark gates.
- `scripts/fuzz_tpm2_test.py` — clone of `scripts/fuzz_m1_test.py`. Asserts the
  planted bug is rediscovered from a small seed corpus within a few seconds and
  that the crashing input replays deterministically (`--replay`).
- `kimage/build/build-fuzz-initramfs.sh` — new `tpm2)` case: compile the vendored
  TPM sources + `target_tpm2.c` (ASan+SanCov) + `harness.c` (ASan), link, bundle
  musl loader + `/dev/mem`, cpio-pack to `fuzz-initramfs-tpm2.cpio`.

No changes to `crates/devices/src/fuzz/*`, `crates/vmm/src/fuzz/*`,
`spike/src/bin/boot.rs` `run_fuzz_mode`, the doorbell protocol, or the GPA layout.

### 5. Seed corpus

A small set, one minimal valid command per curated handler (Startup,
GetCapability, NV_DefineSpace, NV_Write, NV_Read, ContextSave, ContextLoad), so
the mutator has a reachable entry into each handler's parse path rather than
discovering valid command headers from scratch.

## Data flow (unchanged from the existing fuzzer)

1. Host zeroes the coverage map, mutates a corpus entry into the shared input
   window, sets `INPUT_LEN`, resumes the vCPU.
2. Guest `target_parse` calls `ExecuteCommand` over the injected bytes; SanCov
   trace-pc increments edge counters in the shared coverage window.
3. Clean return → `DONE` doorbell → host folds coverage into the virgin-bits map
   (new edge → push input to corpus), then dirty-page resets the TPM state and
   restores vCPU registers from the snapshot.
4. ASan abort → `CRASH` doorbell → host records the crashing input + code, resets.

## Deliverables / gates

Mirror the proven M1 + M3 structure:

- **G1 — correctness:** `fuzz_tpm2_test.py` rediscovers the planted bug from the
  seed corpus in < a few seconds, deterministic replay. CI-gateable.
- **G2 — benchmark:** `fuzz_tpm2_bench.py` reports execs/sec dirty-vs-full, reset
  p50, dirty-set page distribution (expected ≫ libpng), and the coverage curve
  over the curated handler set. Written up as a new section in
  `docs/src/benchmarks/fuzzing.md`.
- **Docs:** this spec + the benchmark write-up; ROADMAP item ticked.

## Stretch (separate follow-up, out of this spec's scope)

Replace the planted bug with a real-CVE pin — build a vulnerable ms-tpm-20-ref
commit (e.g. CVE-2023-1017, OOB write in `CryptParameterDecryption`) and
rediscover it from a crafted seed corpus. Requires the real crypto backend and
session-setup reachability — both deliberately excluded from the spine. Tracked
as a follow-up once the spine passes.

## Risks & mitigations

- **Multi-handler reachability from one seed** → ship the one-valid-command
  per-handler seed corpus (§5) rather than relying on the mutator to synthesize
  valid command headers.
- **Determinism** → keep curated paths free of wall-clock/RNG variance: RNG made
  deterministic, RTC already frozen at snapshot, all setup runs before
  `SNAPSHOT_ME`. Same snapshot + same input ⇒ same execution.
- **Build scope creep** → keep the curated handler set small; resist pulling in a
  crypto backend (that's the stretch). The vendored subset must compile without
  OpenSSL/wolfSSL.
- **License** → ms-tpm-20-ref ships under a BSD-style Microsoft reference
  license; vendor only the trimmed subset needed and carry its license header.
- **`target_init` harness touch** → the single non-target change; keep it a
  weak-symbol/`#define`-guarded no-op so the synthetic and libpng targets are
  byte-for-byte unaffected.

## Files

New:
- `kimage/build/fuzz-harness/target_tpm2.c`
- `kimage/build/fuzz-harness/tpm2/` — vendored trimmed ms-tpm-20-ref subset
- `scripts/fuzz_tpm2_bench.py`
- `scripts/fuzz_tpm2_test.py`
- `kimage/build/fuzz-harness/seeds/tpm2/` — seed corpus
- `docs/src/benchmarks/fuzzing.md` — new TPM section (edit)

Edited:
- `kimage/build/build-fuzz-initramfs.sh` — `tpm2)` case
- `kimage/build/fuzz-harness/harness.c` — optional `target_init` hook (no-op for
  existing targets)
- `ROADMAP.md` — tick the firmware/TEE fuzzing payoff item

Unchanged (reused): `crates/devices/src/fuzz/*`, `crates/vmm/src/fuzz/*`,
`spike/src/bin/boot.rs` fuzz path, doorbell protocol, GPA layout.
