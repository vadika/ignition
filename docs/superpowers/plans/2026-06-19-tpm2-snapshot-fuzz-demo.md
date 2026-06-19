# TPM 2.0 Snapshot-Fuzz Demo Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add ms-tpm-20-ref's TPM 2.0 command processor as an aarch64-userspace target on the existing in-VMM snapshot fuzzer, with a planted-bug correctness gate and a stateful-reset benchmark — no VMM/device/controller changes.

**Architecture:** A new guest target (`target_tpm2.c` + a trimmed, crypto-stubbed vendored TPM subset) plugs into the unchanged `target_parse(data,len)` ABI. The harness gains one additive `target_init()` hook so the TPM's one-time manufacture/init/startup runs *before* the snapshot point; each iteration injects one command into `ExecuteCommand` against the identical post-Startup snapshot, and the dirty-page reset rolls back the (large) TPM globals. New build case + two launcher scripts mirror the proven M1/M3 pattern.

**Tech Stack:** C (clang, AddressSanitizer + SanCov trace-pc, musl/alpine arm64 in Docker), ms-tpm-20-ref, Rust VMM (reused as-is), Python launchers.

---

## Background the executor must know

- The fuzzer is target-agnostic. A target is one C TU exposing `void target_parse(const uint8_t *data, unsigned long len)`, built with `-fsanitize=address -fsanitize-coverage=trace-pc -O1 -g`, linked with the shared `kimage/build/fuzz-harness/harness.c` (which is the PID-1 init, maps the device, and runs the doorbell loop). Read `kimage/build/fuzz-harness/harness.c` and `kimage/build/fuzz-harness/target.c` before starting.
- `harness.c` rings `CMD_SNAPSHOT_ME` once after its mmap setup, then loops: read `INPUT_LEN`, call `target_parse(g_win, len)`, ring `CMD_DONE`. The snapshot PC lands right after `SNAPSHOT_ME`. **Anything that must be identical every iteration has to run before that doorbell.** Today no target needs pre-snapshot work; TPM does (manufacture + startup).
- The build script `kimage/build/build-fuzz-initramfs.sh [synthetic|libpng]` runs a Docker `--platform linux/arm64 alpine:3.19` container, compiles in-container, and writes `fuzz-initramfs[-X].cpio` to `~/kbuild/out/` (or `~/kbuild/`). It does **not** use the host toolchain.
- `boot --fuzz` flags (from `scripts/fuzz_m1_test.py` / `fuzz_m3_bench.py`): `--mem <MiB> --initramfs <cpio> --solutions <dir> --seed <file> [--reset dirty|full] [--metrics <file>] [--replay <crashfile>] <kernel-Image>`. Crashes land as `crash-*.bin` in the solutions dir. Metrics are written to `--metrics` on SIGINT shutdown; keys include `execs_per_sec`, `coverage_final`, `reset_us_p50/p99`, `dirty_pages_p50/p99/max`, `time_to_crash_s`.
- Building the rootfs/initramfs needs a Linux Docker host. Per repo convention `ssh artemis2` is the arm64 Docker build host (build-only, no HVF). The `boot --fuzz` runs happen on the Mac (HVF). Plan steps that say "build the initramfs" run on the Docker host; steps that "run the fuzzer" run on the Mac.
- Spec: `docs/superpowers/specs/2026-06-19-tpm2-snapshot-fuzz-demo-design.md`.

## File structure

New:
- `kimage/build/fuzz-harness/target_tpm2.c` — the wrapper: `target_init()` (manufacture/init/startup) + `target_parse()` (one `ExecuteCommand`).
- `kimage/build/fuzz-harness/tpm2/` — vendored trimmed ms-tpm-20-ref subset + `crypto_stub.c` + `platform_stub.c`.
- `kimage/build/fuzz-harness/seeds/tpm2/` — one valid command per curated handler.
- `scripts/fuzz_tpm2_test.py` — correctness gate (clone of `fuzz_m1_test.py`).
- `scripts/fuzz_tpm2_bench.py` — benchmark gate (clone of `fuzz_m3_bench.py`).

Modified:
- `kimage/build/fuzz-harness/harness.c` — add the weak `target_init()` hook call.
- `kimage/build/build-fuzz-initramfs.sh` — add the `tpm2)` build case.
- `docs/src/benchmarks/fuzzing.md` — add the TPM section (after the benchmark exists).
- `ROADMAP.md` — tick the firmware/TEE fuzzing payoff item.

Unchanged (reused): everything under `crates/devices/src/fuzz/`, `crates/vmm/src/fuzz/`, `spike/src/bin/boot.rs` fuzz path, the doorbell protocol, the GPA layout.

---

## Task 1: Additive `target_init` harness hook (must not regress synthetic/libpng)

**Files:**
- Modify: `kimage/build/fuzz-harness/harness.c`

The harness must call an optional one-time `target_init()` before `SNAPSHOT_ME`. Existing targets (`target.c`, `target_png.c`) define no `target_init`, so it must default to a no-op. Use a weak symbol so a target that omits it links cleanly to the no-op, and one that defines it overrides.

- [ ] **Step 1: Add the weak hook declaration and call**

In `kimage/build/fuzz-harness/harness.c`, add near the `target_parse` declaration:

```c
/* Optional one-time per-target setup, run BEFORE the snapshot doorbell so its
 * effects are baked into the snapshot and identical every iteration. Targets
 * that need no setup (synthetic, libpng) omit it and get this weak no-op. */
__attribute__((weak)) void target_init(void) {}
```

Then, in `main()`, immediately before the `doorbell(CMD_SNAPSHOT_ME);` line, add:

```c
    target_init();               /* one-time, pre-snapshot (weak no-op by default) */
    /* One-time setup is complete; park at the parse site. */
    doorbell(CMD_SNAPSHOT_ME);   /* <-- snapshot/reset PC lands just after here */
```

(Replace the existing comment+doorbell pair so the comment isn't duplicated.)

- [ ] **Step 2: Rebuild the synthetic initramfs (Docker host)**

Run (on the Docker build host, repo root):

```bash
kimage/build/build-fuzz-initramfs.sh synthetic
```

Expected: ends with `wrote .../fuzz-initramfs.cpio`. Copy it to the Mac at `kimage/out/fuzz-initramfs.cpio` (or set `FUZZ_INITRAMFS`).

- [ ] **Step 3: Verify the M1 gate still passes (Mac)**

Run:

```bash
python3 scripts/fuzz_m1_test.py
```

Expected: `PASS: M1 gate`. This proves the weak hook did not change existing-target behavior (the snapshot point still sits after a no-op init).

- [ ] **Step 4: Commit**

```bash
git add kimage/build/fuzz-harness/harness.c
git commit -m "fuzz(harness): add weak target_init() hook before snapshot doorbell"
```

---

## Task 2: Vendor + build a minimal TPM that runs ExecuteCommand (the integration spike)

**Files:**
- Create: `kimage/build/fuzz-harness/tpm2/` (vendored subset + stubs)
- Create: `kimage/build/fuzz-harness/target_tpm2.c`
- Modify: `kimage/build/build-fuzz-initramfs.sh`

This is the load-bearing, iterative task. The goal state is a tiny aarch64 `/init` that manufactures + starts a TPM and successfully runs `ExecuteCommand` on a `TPM2_GetCapability` buffer (returns `TPM_RC_SUCCESS`, response header well-formed). The curated handlers avoid crypto, but the **boot path** (`TPM_Manufacture` + `TPM2_Startup`) calls crypto self-tests and RNG — those get stubbed to deterministic success.

> **Method note (no fabricated upstream source):** the exact set of upstream `.c` files that must be vendored is discovered by the link-error loop in Step 4 (compile → unresolved symbol → add the file that defines it or stub it → repeat). The steps below give the starting subset, the stub skeletons (exact code), and the done-condition. Do not paste thousands of lines of upstream into this plan; vendor real files from the pinned commit.

- [ ] **Step 1: Pin and fetch ms-tpm-20-ref**

Pin upstream `https://github.com/microsoft/ms-tpm-20-ref` at a fixed commit (record the SHA in `tpm2/VENDOR.md`). Its tree of interest:
- `TPMCmd/tpm/include/` — headers (`Tpm.h`, `TpmTypes.h`, generated `*_fp.h`).
- `TPMCmd/tpm/src/main/` — `ExecuteCommand.c`, `CommandDispatcher.c`, `SessionProcess.c`.
- `TPMCmd/tpm/src/command/` — per-group handlers; we keep `Startup/`, `Capability/`, `NVStorage/`, `Context/`.
- `TPMCmd/tpm/src/support/` — `Marshal.c` (generated marshal/unmarshal), `Manufacture.c`, helpers.
- `TPMCmd/tpm/src/subsystem/` — `NvDynamic.c`, `NvReserved.c`, `Object.c`, `Session.c`, `PP.c`, `Time.c`, etc. (add as link errors demand).
- `TPMCmd/tpm/src/crypt/` — **do NOT vendor**; replaced by `crypto_stub.c`.
- `TPMCmd/Platform/src/` — **do NOT vendor**; replaced by `platform_stub.c`.

- [ ] **Step 2: Write the crypto stub**

Create `kimage/build/fuzz-harness/tpm2/crypto_stub.c`. It must satisfy the `Crypt*` / RNG symbols the boot path and curated handlers reference, returning deterministic success. Start from this skeleton and extend per link errors (signatures come from the matching `*_fp.h`):

```c
/* Deterministic crypto stub for the no-crypto TPM fuzz build. Self-tests pass,
 * RNG is a fixed counter, hashes/HMAC are length-correct fillers. This is NOT a
 * real TPM crypto backend — it exists only to bring the command-parse + NV/
 * capability paths up under the snapshot fuzzer (spec: non-crypto handlers). */
#include "Tpm.h"

/* --- self-test: always good --- */
TPM_RC CryptSelfTest(int fullTest) { (void)fullTest; return TPM_RC_SUCCESS; }
void   CryptInitUnits(void) {}
BOOL   CryptInit(void) { return TRUE; }
void   CryptStartup(STARTUP_TYPE type) { (void)type; }

/* --- RNG: deterministic counter so runs are reproducible --- */
static uint32_t g_rng;
INT32 CryptRandomGenerate(INT32 n, BYTE *out) {
    for (INT32 i = 0; i < n; i++) out[i] = (BYTE)(g_rng++);
    return n;
}
void  CryptRandStartup(void) { g_rng = 0; }
/* DRBG entry points used by the platform/RNG bridge, if referenced: */
UINT16 _cpri__GenerateRandom(INT32 n, BYTE *out) { return (UINT16)CryptRandomGenerate(n, out); }

/* --- hashes/HMAC: fill with a length-correct deterministic pattern --- */
/* Provide CryptHashStart/Data/End, CryptHmac*, CryptGetHashDigestSize, etc. as
 * the link loop demands; each returns a fixed digest of the requested size. */
```

Implement the remaining referenced functions the same way (deterministic, length-correct). Keep a comment block listing every stubbed symbol so reviewers see the crypto boundary.

- [ ] **Step 3: Write the platform stub**

Create `kimage/build/fuzz-harness/tpm2/platform_stub.c`. The TPM platform layer (`_plat__*`) backs NV with a RAM buffer (no file), gives a frozen clock, and no physical-presence/cancel. Skeleton (extend per link errors; signatures from `Platform_fp.h`):

```c
/* RAM-backed, deterministic platform layer for the TPM fuzz build. NV lives in a
 * static buffer (rolled back by the snapshot dirty-page reset each iteration),
 * the clock is frozen, PP/cancel are off. */
#include "Tpm.h"
#include "Platform_fp.h"

#define NV_SIZE (64 * 1024)
static unsigned char g_nv[NV_SIZE];

int  _plat__NVEnable(void *p)            { (void)p; return 0; }
void _plat__NVDisable(int delete)        { (void)delete; }
int  _plat__IsNvAvailable(void)          { return 0; /* available */ }
int  _plat__NvMemoryRead(unsigned int off, unsigned int sz, void *d) {
    if (off + sz > NV_SIZE) return 1; memcpy(d, g_nv + off, sz); return 0;
}
int  _plat__NvMemoryWrite(unsigned int off, unsigned int sz, void *d) {
    if (off + sz > NV_SIZE) return 1; memcpy(g_nv + off, d, sz); return 0;
}
int  _plat__NvMemoryMove(unsigned int src, unsigned int dst, unsigned int sz) {
    if (src + sz > NV_SIZE || dst + sz > NV_SIZE) return 1;
    memmove(g_nv + dst, g_nv + src, sz); return 0;
}
int  _plat__NvCommit(void)               { return 0; }
void _plat__SetNvAvail(void)             {}
/* Frozen clock: */
uint64_t _plat__RealTime(void)           { return 0; }
int  _plat__TimerWasReset(void)          { return 1; }
int  _plat__TimerWasStopped(void)        { return 1; }
uint64_t _plat__TimerRead(void)          { return 0; }
/* PP / cancel / locality: */
int  _plat__PhysicalPresenceAsserted(void) { return 0; }
int  _plat__IsCanceled(void)             { return 0; }
unsigned char _plat__LocalityGet(void)   { return 0; }
```

- [ ] **Step 4: Write `target_tpm2.c` (init + parse wrapper)**

Create `kimage/build/fuzz-harness/target_tpm2.c`:

```c
/* TPM 2.0 fuzz target: one-time manufacture/init/startup in target_init (runs
 * pre-snapshot), then each iteration runs one command through ExecuteCommand.
 * The injected bytes are a raw TPM command stream (header + payload). TPM global
 * state mutated by a command is rolled back by the snapshot dirty-page reset. */
#include <stdint.h>
#include <string.h>
#include "Tpm.h"
#include "ExecCommand_fp.h"     /* declares ExecuteCommand (path per pinned tree) */
#include "Manufacture_fp.h"     /* declares TPM_Manufacture */

void target_init(void) {
    _plat__NVEnable(NULL);
    TPM_Manufacture(1);         /* first-time manufacture into the RAM NV */
    _TPM_Init();                /* power-on init */
    /* Startup(CLEAR) via the command interface so the snapshot sits at a fully
     * started TPM. Build the 12-byte command inline: tag=TPM_ST_NO_SESSIONS,
     * size=12, cc=TPM2_CC_Startup, param=TPM_SU_CLEAR. */
    unsigned char cmd[12] = {0x80,0x01, 0,0,0,12, 0,0,0x01,0x44, 0,0};
    unsigned char rsp[64]; uint32_t rlen = sizeof rsp; unsigned char *rp = rsp;
    ExecuteCommand(sizeof cmd, cmd, &rlen, &rp);
}

void target_parse(const uint8_t *data, unsigned long len) {
    unsigned char rsp[4096]; uint32_t rlen = sizeof rsp; unsigned char *rp = rsp;
    ExecuteCommand((uint32_t)len, (unsigned char *)data, &rlen, &rp);
}
```

(Adjust the `#include` paths and the `TPM2_CC_Startup` opcode bytes to the pinned tree's headers; `0x144` is `TPM_CC_Startup`, `0x0000` is `TPM_SU_CLEAR`.)

- [ ] **Step 5: Add the `tpm2)` build case (link-error loop lives here)**

In `kimage/build/build-fuzz-initramfs.sh`, extend the target selector:

```bash
  tpm2)      OUT_NAME="fuzz-initramfs-tpm2.cpio" ;;
```

and add a third `if/elif` branch (mirroring the `libpng` branch's container shape) that:
1. Mounts `$HERE/fuzz-harness:/src:ro` (already contains `tpm2/`).
2. `apk add --no-cache clang compiler-rt musl-dev`.
3. Compiles every vendored TPM `.c` + `crypto_stub.c` + `platform_stub.c` + `target_tpm2.c` with `-fsanitize=address -fsanitize-coverage=trace-pc -O1 -g -I/src/tpm2/include -I/src` and `harness.c` with `-fsanitize=address -O1 -g -I/src` (no coverage — it holds the trace-pc callback).
4. Links all objects with `clang -fsanitize=address` into `/out/root/init`.
5. Bundles `ld-musl-aarch64.so.1` + `libgcc_s.so.1`, makes the `/dev` nodes, cpio-packs (copy these lines verbatim from the synthetic branch).

The vendored `.c` list is built iteratively: start with `ExecuteCommand.c CommandDispatcher.c Marshal.c Manufacture.c` + the four command groups + `crypto_stub.c platform_stub.c`, compile, and for each `undefined symbol: X` add the upstream file that defines `X` (or stub it in `crypto_stub.c`/`platform_stub.c`). Repeat until it links. Keep the file list in a `TPM_SRCS=` shell array in the script so it's explicit.

- [ ] **Step 6: Build it (Docker host)**

```bash
kimage/build/build-fuzz-initramfs.sh tpm2
```

Expected: ends with `wrote .../fuzz-initramfs-tpm2.cpio`. Iterate Step 5 until this succeeds. Copy the cpio to the Mac at `kimage/out/fuzz-initramfs-tpm2.cpio`.

- [ ] **Step 7: Smoke-test that the TPM actually runs (Mac)**

Create a one-shot seed = a valid `TPM2_GetCapability` command and run a single short fuzz session; the run must NOT immediately crash (which would mean manufacture/startup faulted) and must produce iterations. Seed bytes (TPM2_GetCapability, cap=TPM_CAP_TPM_PROPERTIES, property=TPM_PT_MANUFACTURER, count=1):

```bash
python3 - <<'PY'
import struct
# tag=TPM_ST_NO_SESSIONS(0x8001), cc=TPM2_CC_GetCapability(0x17a),
# capability=TPM_CAP_TPM_PROPERTIES(0x6), property=0x100, propertyCount=1
body = struct.pack(">IIII", 0x17a, 0x6, 0x100, 1)  # cc + 3 args
cmd  = struct.pack(">HI", 0x8001, 10 + len(body)) + body
open("/tmp/tpm_getcap.seed","wb").write(cmd)
print("wrote", len(cmd), "bytes")
PY
mkdir -p /tmp/tpm_smoke
target/debug/boot --fuzz --mem 256 \
  --initramfs kimage/out/fuzz-initramfs-tpm2.cpio \
  --solutions /tmp/tpm_smoke --seed /tmp/tpm_getcap.seed \
  --reset dirty --metrics /tmp/tpm_smoke.metrics kimage/out/Image &
BPID=$!; sleep 20; kill -INT $BPID; wait $BPID 2>/dev/null
grep -E 'execs_per_sec|coverage_final' /tmp/tpm_smoke.metrics
ls /tmp/tpm_smoke   # no crash-*.bin expected for a clean GetCapability seed
```

Expected: `execs_per_sec` > 0, `coverage_final` > 0, no `crash-*.bin`. If it crashes immediately on a valid GetCapability, manufacture/startup is faulting — fix the stubs (Steps 2–3) before proceeding.

- [ ] **Step 8: Commit**

```bash
git add kimage/build/fuzz-harness/tpm2 kimage/build/fuzz-harness/target_tpm2.c kimage/build/build-fuzz-initramfs.sh
git commit -m "fuzz(tpm2): vendor trimmed crypto-stubbed TPM, run ExecuteCommand under the snapshot fuzzer"
```

---

## Task 3: Plant the bug in a curated handler

**Files:**
- Modify: a vendored NV handler under `kimage/build/fuzz-harness/tpm2/` (the `TPM2_NV_Write` path)

Plant a deterministic, ASan-catchable out-of-bounds write reachable from a well-formed `TPM2_NV_Write`, mirroring the M1 length-field shape. The plant must sit AFTER unmarshaling (so a valid command header reaches it) and be clearly marked.

- [ ] **Step 1: Add the planted overflow**

In the vendored `TPM2_NV_Write` implementation, just before the legitimate NV write, insert a fixed-size scratch copy keyed off the attacker-controlled `data.size`:

```c
    /* PLANTED BUG (fuzz demo, see spec): copy the write payload into a fixed
     * 32-byte scratch with no bound check -- a length-field OOB in the CVE
     * shape. ASan traps it; the harness rings CRASH. Remove for a real build. */
    {
        volatile unsigned char scratch[32];
        for (UINT16 k = 0; k < in->data.t.size; k++)
            scratch[k] = in->data.t.buffer[k];   /* OOB when size > 32 */
    }
```

(Use the pinned tree's actual field names for the `TPM2B` write payload; `in->data.t.size` / `in->data.t.buffer` is the typical shape.)

- [ ] **Step 2: Rebuild the tpm2 initramfs (Docker host)**

```bash
kimage/build/build-fuzz-initramfs.sh tpm2
```

Copy the cpio to `kimage/out/fuzz-initramfs-tpm2.cpio` on the Mac.

- [ ] **Step 3: Confirm a too-large NV_Write crashes (Mac)**

Build a seed that does `NV_DefineSpace` then `NV_Write` with a 64-byte payload (> 32). Because each iteration resets, the crash must be reachable within a single command where possible; for the demo, plant so that a single `NV_Write` to a predefined index overflows (define the index in `target_init` so it exists at snapshot time). Run a 30s session and expect a `crash-*.bin`:

```bash
mkdir -p /tmp/tpm_plant
# seed: NV_Write to index 0x01000000, 64 bytes of payload (built in Step ... of the seed task)
target/debug/boot --fuzz --mem 256 --initramfs kimage/out/fuzz-initramfs-tpm2.cpio \
  --solutions /tmp/tpm_plant --seed kimage/build/fuzz-harness/seeds/tpm2/nv_write_big.seed \
  --reset dirty kimage/out/Image &
BPID=$!; sleep 30; kill -INT $BPID; wait $BPID 2>/dev/null
ls /tmp/tpm_plant/crash-*.bin && echo "PLANTED BUG REACHED"
```

Expected: at least one `crash-*.bin`, `PLANTED BUG REACHED`. (Define the NV index in `target_init` so the write target exists in the snapshot — add `TPM2_NV_DefineSpace` for index `0x01000000`, 64 bytes, after Startup in `target_tpm2.c`.)

- [ ] **Step 4: Commit**

```bash
git add kimage/build/fuzz-harness/tpm2 kimage/build/fuzz-harness/target_tpm2.c
git commit -m "fuzz(tpm2): plant a length-field OOB in NV_Write for the correctness gate"
```

---

## Task 4: Seed corpus (one valid command per curated handler)

**Files:**
- Create: `kimage/build/fuzz-harness/seeds/tpm2/*.seed`
- Create: `kimage/build/fuzz-harness/seeds/tpm2/gen_seeds.py`

A reproducible generator emits one minimal valid command per curated handler so the mutator has a foothold in each parse path, plus the near-boundary `nv_write_big.seed` used by Task 3.

- [ ] **Step 1: Write the seed generator**

Create `kimage/build/fuzz-harness/seeds/tpm2/gen_seeds.py`:

```python
#!/usr/bin/env python3
"""Generate minimal valid TPM 2.0 command seeds for the curated handler set.
Each command is a raw TPM command stream: header(tag,size,cc) + params."""
import struct, os

ST_NO_SESSIONS = 0x8001
CC = dict(Startup=0x144, GetCapability=0x17a,
          NV_DefineSpace=0x12a, NV_Write=0x137, NV_Read=0x14e,
          ContextSave=0x162, ContextLoad=0x161)

def cmd(cc, body=b""):
    return struct.pack(">HI", ST_NO_SESSIONS, 10 + len(body)) + struct.pack(">I", cc) + body

here = os.path.dirname(__file__)
def w(name, data): open(os.path.join(here, name), "wb").write(data)

# Startup(CLEAR)
w("startup.seed", cmd(CC["Startup"], struct.pack(">H", 0x0000)))
# GetCapability(TPM_PROPERTIES, PT 0x100, count 1)
w("getcap.seed", cmd(CC["GetCapability"], struct.pack(">III", 0x6, 0x100, 1)))
# NV_Read of a defined index (define created in target_init) -- authHandle+nvIndex
# (no sessions here; the demo handlers accept platform auth at locality 0)
w("nv_read.seed", cmd(CC["NV_Read"], struct.pack(">IIHH", 0x4000000C, 0x01000000, 16, 0)))
# NV_Write small (valid, <=32): authHandle, nvIndex, size, data, offset
w("nv_write.seed", cmd(CC["NV_Write"],
    struct.pack(">II", 0x4000000C, 0x01000000) + struct.pack(">H", 8) + b"\x01"*8 + struct.pack(">H", 0)))
# NV_Write big (>32 -> overflow): the Task 3 crasher
w("nv_write_big.seed", cmd(CC["NV_Write"],
    struct.pack(">II", 0x4000000C, 0x01000000) + struct.pack(">H", 64) + b"\x02"*64 + struct.pack(">H", 0)))
print("wrote seeds to", here)
```

- [ ] **Step 2: Generate the seeds**

```bash
python3 kimage/build/fuzz-harness/seeds/tpm2/gen_seeds.py
ls kimage/build/fuzz-harness/seeds/tpm2/*.seed
```

Expected: `startup.seed getcap.seed nv_read.seed nv_write.seed nv_write_big.seed`.

- [ ] **Step 3: Sanity-check the small NV_Write seed does NOT crash (Mac)**

```bash
mkdir -p /tmp/tpm_seedcheck
target/debug/boot --fuzz --mem 256 --initramfs kimage/out/fuzz-initramfs-tpm2.cpio \
  --solutions /tmp/tpm_seedcheck --seed kimage/build/fuzz-harness/seeds/tpm2/nv_write.seed \
  --reset dirty kimage/out/Image &
BPID=$!; sleep 8; kill -INT $BPID; wait $BPID 2>/dev/null
# The exact seed (size 8) must not itself crash; only mutation past 32 should.
ls /tmp/tpm_seedcheck/crash-*.bin 2>/dev/null && echo "UNEXPECTED: seed itself crashes" || echo "ok: valid seed is clean"
```

Expected: `ok: valid seed is clean` (mutation, not the seed, finds the bug).

- [ ] **Step 4: Commit**

```bash
git add kimage/build/fuzz-harness/seeds/tpm2
git commit -m "fuzz(tpm2): reproducible seed corpus for the curated handler set"
```

---

## Task 5: Correctness gate — `fuzz_tpm2_test.py`

**Files:**
- Create: `scripts/fuzz_tpm2_test.py`

Mirror `scripts/fuzz_m1_test.py`: blind rediscovery of the planted NV_Write OOB from the small NV_Write seed, then deterministic replay.

- [ ] **Step 1: Write the gate script**

Create `scripts/fuzz_tpm2_test.py`:

```python
#!/usr/bin/env python3
"""TPM2 correctness gate: rediscover the planted NV_Write length-field OOB and
replay it deterministically. Mirrors fuzz_m1_test.py for the TPM target."""
import glob, os, signal, subprocess, sys, tempfile, time

BOOT = os.environ.get("BOOT_BIN", "target/debug/boot")
KERNEL = os.environ.get("FUZZ_KERNEL", "kimage/out/Image")
INITRAMFS = os.environ.get("FUZZ_INITRAMFS_TPM2", "kimage/out/fuzz-initramfs-tpm2.cpio")
SEED = os.environ.get("TPM2_SEED", "kimage/build/fuzz-harness/seeds/tpm2/nv_write.seed")
MEM = os.environ.get("TPM2_MEM", "256")

def run(cmd, sol, timeout):
    p = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    deadline = time.time() + timeout
    found = None
    while time.time() < deadline:
        hits = glob.glob(os.path.join(sol, "crash-*.bin"))
        if hits:
            found = sorted(hits)[0]; break
        if p.poll() is not None: break
        time.sleep(0.5)
    try:
        p.send_signal(signal.SIGINT); p.wait(timeout=5)
    except Exception:
        p.kill()
    out = p.stdout.read().decode(errors="replace") if p.stdout else ""
    return found, out

def main():
    for x in (BOOT, KERNEL, INITRAMFS, SEED):
        if not os.path.exists(x):
            print(f"missing artifact: {x}", file=sys.stderr); sys.exit(2)
    d = tempfile.mkdtemp(prefix="fuzz-tpm2-")
    sol1 = os.path.join(d, "sol1")
    found, out = run([BOOT, "--fuzz", "--mem", MEM, "--initramfs", INITRAMFS,
                      "--solutions", sol1, "--seed", SEED, "--reset", "dirty", KERNEL], sol1, 120)
    if not found:
        print(out); print("FAIL: planted NV_Write OOB not rediscovered", file=sys.stderr); sys.exit(1)
    print("PASS(a): rediscovered planted OOB ->", found)
    sol2 = os.path.join(d, "sol2")
    found2, out2 = run([BOOT, "--fuzz", "--mem", MEM, "--initramfs", INITRAMFS,
                        "--solutions", sol2, "--replay", found, KERNEL], sol2, 30)
    if not found2:
        print(out2); print("FAIL: replayed crash did not reproduce", file=sys.stderr); sys.exit(1)
    print("PASS(b): replayed crash reproduced ->", found2)
    print("PASS: TPM2 correctness gate")

if __name__ == "__main__":
    main()
```

- [ ] **Step 2: Run the gate (Mac)**

```bash
python3 scripts/fuzz_tpm2_test.py
```

Expected: `PASS(a)`, `PASS(b)`, `PASS: TPM2 correctness gate`. If (a) times out, widen the timeout or check the mutator reaches `size > 32` from the seed (the seed's size field is one mutable byte).

- [ ] **Step 3: Commit**

```bash
git add scripts/fuzz_tpm2_test.py
git commit -m "fuzz(tpm2): correctness gate - rediscover + replay the planted OOB"
```

---

## Task 6: Benchmark gate + docs + roadmap

**Files:**
- Create: `scripts/fuzz_tpm2_bench.py`
- Modify: `docs/src/benchmarks/fuzzing.md`
- Modify: `ROADMAP.md`

Mirror `scripts/fuzz_m3_bench.py`: dirty-vs-full execs/sec, reset p50, dirty-set distribution (the stateful-reset headline), coverage curve — on a clean (non-crashing) seed so the run benchmarks throughput rather than stopping at the bug.

- [ ] **Step 1: Write the benchmark script**

Create `scripts/fuzz_tpm2_bench.py`:

```python
#!/usr/bin/env python3
"""TPM2 benchmark: dirty-vs-full execs/sec, reset latency, dirty-set size, and
coverage on the TPM command processor. Mirrors fuzz_m3_bench.py. Uses the clean
GetCapability seed so the run measures throughput (no early crash-stop)."""
import os, re, signal, subprocess, sys, tempfile, time

BOOT = os.environ.get("BOOT_BIN", "target/debug/boot")
KERNEL = os.environ.get("FUZZ_KERNEL", "kimage/out/Image")
INITRAMFS = os.environ.get("FUZZ_INITRAMFS_TPM2", "kimage/out/fuzz-initramfs-tpm2.cpio")
SEED = os.environ.get("TPM2_BENCH_SEED", "kimage/build/fuzz-harness/seeds/tpm2/getcap.seed")
DURATION = float(os.environ.get("TPM2_DURATION", "60"))
MEM = os.environ.get("TPM2_MEM", "256")
METRIC = re.compile(r"^metric (.+)$", re.M)

def parse_metrics(path):
    d = {}
    if not os.path.exists(path): return d
    for line in METRIC.findall(open(path).read()):
        for tok in line.split():
            if "=" in tok:
                k, v = tok.split("=", 1); d[k] = v
    return d

def run(reset, metrics_path, sols):
    logf = open(sols + ".log", "w+b")
    cmd = [BOOT, "--fuzz", "--mem", MEM, "--initramfs", INITRAMFS, "--solutions", sols,
           "--reset", reset, "--seed", SEED, "--metrics", metrics_path, KERNEL]
    p = subprocess.Popen(cmd, stdout=logf, stderr=subprocess.STDOUT)
    deadline = time.time() + DURATION
    while time.time() < deadline and p.poll() is None:
        time.sleep(0.5)
    try:
        p.send_signal(signal.SIGINT); p.wait(timeout=10)
    except Exception:
        p.kill()
    logf.close()
    return parse_metrics(metrics_path)

def num(m, k, d=0.0):
    try: return float(m.get(k, d))
    except ValueError: return d

def main():
    for x in (BOOT, KERNEL, INITRAMFS, SEED):
        if not os.path.exists(x):
            print(f"missing artifact: {x}", file=sys.stderr); sys.exit(2)
    d = tempfile.mkdtemp(prefix="fuzz-tpm2-bench-")
    print(f"[1/2] tpm2 / dirty reset ({DURATION:.0f}s) ...")
    md = run("dirty", os.path.join(d, "dirty.txt"), os.path.join(d, "dirty"))
    print(f"[2/2] tpm2 / full reset ({DURATION:.0f}s) ...")
    mf = run("full", os.path.join(d, "full.txt"), os.path.join(d, "full"))

    eps_d, eps_f = num(md, "execs_per_sec"), num(mf, "execs_per_sec")
    cov = num(md, "coverage_final")
    rp50, rp99 = num(md, "reset_us_p50"), num(md, "reset_us_p99")
    dp50, dp99, dmax = num(md, "dirty_pages_p50"), num(md, "dirty_pages_p99"), num(md, "dirty_pages_max")

    print("\n=== TPM2 benchmark ===")
    print(f"tpm2 dirty: {eps_d:.0f} execs/sec | coverage={cov:.0f} edges")
    print(f"tpm2 full : {eps_f:.0f} execs/sec")
    print(f"reset latency (dirty): p50={rp50:.0f}us p99={rp99:.0f}us")
    print(f"dirty-set size: p50={dp50:.0f} p99={dp99:.0f} max={dmax:.0f} pages (16 KiB each)")

    fail = []
    if not (eps_d > 0): fail.append(f"dirty execs/sec not positive ({eps_d})")
    if not (cov > 0): fail.append(f"coverage did not register ({cov})")
    if "reset_us_p50" not in md: fail.append("reset latency p50 missing")
    if "dirty_pages_p50" not in md: fail.append("dirty-set distribution missing")
    if not (eps_d > eps_f > 0): fail.append(f"dirty not faster than full ({eps_d:.0f} vs {eps_f:.0f})")
    if fail:
        for f in fail: print("FAIL:", f, file=sys.stderr)
        sys.exit(1)
    print("PASS: TPM2 benchmark gate")

if __name__ == "__main__":
    main()
```

- [ ] **Step 2: Run the benchmark (Mac)**

```bash
python3 scripts/fuzz_tpm2_bench.py
```

Expected: prints the table and `PASS: TPM2 benchmark gate`. Record the dirty-set `p50`/`max` — the demo's headline is that it is materially larger than libpng's 44–50 pages (TPM mutates more state per command).

- [ ] **Step 3: Write the benchmark up in the docs**

Append a `## TPM 2.0 command processor` section to `docs/src/benchmarks/fuzzing.md` with: the target description (ms-tpm-20-ref, trimmed, crypto-stubbed, userspace), the honest framing (no secure world; the win is fast large-state reset), and a results table filled from Step 2 (`execs/sec dirty vs full`, `reset p50/p99`, `dirty-set p50/p99/max`, `coverage edges`, `time-to-rediscover` from Task 5). Note the planted-bug nature and that real-CVE rediscovery is a tracked follow-up.

- [ ] **Step 4: Tick the roadmap**

In `ROADMAP.md`, change the firmware/TEE fuzzing payoff bullet from `- [ ]` to `- [x]` and append: `Shipped: ms-tpm-20-ref command processor as a userspace snapshot-fuzz target (trimmed, crypto-stubbed); planted-OOB correctness gate + stateful-reset benchmark. docs/superpowers/specs/2026-06-19-tpm2-snapshot-fuzz-demo-design.md`. Leave the real-CVE stretch as a new unchecked sub-bullet.

- [ ] **Step 5: Commit**

```bash
git add scripts/fuzz_tpm2_bench.py docs/src/benchmarks/fuzzing.md ROADMAP.md
git commit -m "fuzz(tpm2): benchmark gate + docs + roadmap tick"
```

---

## Self-review notes (for the executor)

- **Determinism:** if `time_to_crash`/replay is flaky, confirm the crypto RNG stub is a fixed counter (Task 2 Step 2) and that `target_init` ran the NV_DefineSpace so the write index exists in the snapshot. Same snapshot + same input must give the same execution.
- **Pre-snapshot rule:** every TPM bring-up call (manufacture, init, startup, define-space) MUST be in `target_init` (Task 2 Step 4), never in `target_parse` — otherwise it re-runs (and re-dirties) every iteration and the snapshot isn't a started TPM.
- **No VMM edits:** if any step tempts you to touch `crates/` or `boot.rs`, stop — the target is supposed to be self-contained. The only non-target/non-script edit in the whole plan is the weak `target_init` hook in `harness.c` (Task 1).
- **Build vs run split:** initramfs builds run on the arm64 Docker host; `boot --fuzz` runs on the Mac (HVF). Keep the cpio artifact path in sync (`kimage/out/fuzz-initramfs-tpm2.cpio` or the `FUZZ_INITRAMFS_TPM2` env override).
- **Ceiling (no-crypto trim):** the central risk is getting `TPM_Manufacture`+`TPM2_Startup` to pass with stubbed self-tests. If the stub surface balloons (Task 2 Step 5 link loop won't converge in ~half a day), the documented fallback is to link a real crypto backend (OpenSSL, the upstream-supported path) instead of stubbing — heavier build, but it unblocks and also enables the real-CVE stretch. Flag this to the user before switching, since it changes the build's weight.
