# Snapshot-Fuzzer M1 (Correctness Gate: realistic bug + sanitizer) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** M1 of the snapshot fuzzer (`docs/superpowers/specs/2026-06-14-snapshot-fuzzing-demonstrator-design.md`): swap the M0 raw-signal stub for a realistic parser with a planted **length-field heap overflow**, detected by a sanitizer in-guest, wired to the `CRASH` doorbell via `__asan_set_death_callback`. Add a verbatim **replay** mode and a determinism gate. Coverage feedback + libAFL + libpng remain deferred to M2 (scope decision: M1 stays blind-mutation, honest to the spec's milestone ordering).

**Architecture:** Reuses the entire M0 in-VMM loop (device, `run_fuzz`/`fuzz_loop`, full-RAM v0 reset, blind mutator, boot wiring) unchanged. M1 changes are guest-side plus two small host additions. The guest target is a chunk-format parser (`"FUZ"` magic | version | `type|len|data` chunks; a `'C'` chunk `memcpy`s `len` bytes into a 16-byte heap buffer with no bound check — the archetypal image/font CVE shape). Built with AddressSanitizer so a 17-byte write into the 16-byte buffer is caught deterministically (a 1-byte heap overflow does not reliably segfault). ASan's death callback rings the `CRASH` doorbell; the M0 signal handlers stay as a backstop. A near-boundary seed (`len=16`, valid) lets blind havoc bump `len` past 16 and find the bug without coverage.

**Tech Stack:** Same as M0. Plus a sanitizer-capable aarch64 toolchain in the artemis2 build container. **Toolchain risk:** static-musl + ASan is historically fragile; Task 1 is a spike that resolves the working linkage (static-musl ASan, or dynamic-glibc with runtime libs bundled into the initramfs) and records it. If ASan-in-guest proves intractable within the spike's bounded effort, fall back to a **guard-page allocator** (place the 16-byte buffer abutting an unmapped page so the overflow faults as SIGSEGV into the existing handler) — same deterministic-detection guarantee, no ASan runtime. Either outcome satisfies the M1 gate.

---

## What M0 already provides (reuse, do not touch)

- `ignition-fuzz` device, protocol, `run_fuzz`/`fuzz_loop`, full-RAM reset, blind mutator (`crates/vmm/src/fuzz/controller.rs`), `--fuzz` boot mode (`spike/src/bin/boot.rs`).
- The guest harness shell `kimage/build/fuzz-harness/harness.c` (the doorbell loop + `main`) and the container build `kimage/build/build-fuzz-initramfs.sh` (artemis2 workflow, `REBUILD-GUEST-ASSETS.md`).
- The M0 gate `scripts/fuzz_m0_test.py` stays as a regression check.

## File Structure (M1 changes)

- `kimage/build/fuzz-harness/target.c` (new) — the chunk parser with the planted length-field heap overflow. Compiled with the sanitizer/guard chosen in Task 1.
- `kimage/build/fuzz-harness/harness.c` (modify) — call `target_parse` from `target.c`; install `__asan_set_death_callback` + `__asan_default_options`; keep signal backstop.
- `kimage/build/build-fuzz-initramfs.sh` (modify) — compile target+harness with the sanitizer; bundle runtime libs into the cpio if the chosen linkage is dynamic.
- `crates/vmm/src/fuzz/controller.rs` (modify) — a replay mode: feed a fixed input verbatim (no mutation).
- `spike/src/bin/boot.rs` (modify) — `--replay <file>` flag in `--fuzz` mode.
- `scripts/fuzz_m1_test.py` (new) — the M1 gate: blind rediscovery from a boundary seed + verbatim-replay determinism check.

---

### Task 1: Sanitizer-in-guest toolchain spike (de-risk)

**Files:**
- Create: `kimage/build/fuzz-harness/asan_spike.c` (throwaway; delete or keep as a smoke test)
- Scratch work on artemis2.

**This is a spike. Its only deliverable is a confirmed, recorded recipe for getting a sanitizer-instrumented aarch64 binary to run as `/init` in the fuzz initramfs and deterministically report a heap overflow inside the microVM.** Do not build the real target/harness yet.

- [ ] **Step 1: Write the spike program**

`kimage/build/fuzz-harness/asan_spike.c`:

```c
/* Throwaway: prove a sanitizer-instrumented binary runs in the guest and
 * detects a heap overflow. Writes a result byte to the boot-timer-style console
 * via stdout (the kernel console) so we can see it on the serial log. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

int main(void) {
    printf("ASAN_SPIKE: start\n");
    fflush(stdout);
    char *buf = malloc(16);
    /* 17-byte write into a 16-byte buffer: ASan (or a guard page) must catch it. */
    memset(buf, 0xAA, 17);
    printf("ASAN_SPIKE: no-detection (BAD) sink=%d\n", buf[16]);
    fflush(stdout);
    free(buf);
    return 0;
}
```

- [ ] **Step 2: Find a working linkage on artemis2 (try in order, stop at first success)**

On artemis2, in an `--platform linux/arm64` container, attempt each and note which links AND runs (the binary must execute under `qemu-user` in the container as a quick check, then for real in the microVM):

1. **alpine static-musl + ASan**: `apk add gcc musl-dev compiler-rt clang`; `clang -fsanitize=address -static-pie asan_spike.c -o init` (or gcc `-fsanitize=address -static-libasan`). If it links and a container run reports `heap-buffer-overflow`, prefer this (smallest initramfs).
2. **debian/ubuntu arm64 dynamic glibc + ASan**: `ubuntu:22.04` + `gcc`; `gcc -fsanitize=address asan_spike.c -o init`. Then `ldd init` to list the runtime: `ld-linux-aarch64.so.1`, `libc.so.6`, `libasan.so.*`, `libgcc_s.so.1`, `libm.so.6`, `libpthread`/`libdl` as needed. These get bundled into the initramfs at their `ldd` paths.
3. **guard-page fallback (no ASan)**: if neither sanitizer linkage runs in-guest within the spike's effort, abandon ASan. Record that Task 2 will use a guard-page allocator (mmap 2 pages, `mprotect` the second `PROT_NONE`, return a pointer 16 bytes before the guard) so the overflow faults as SIGSEGV. Build stays static-musl (the M0 toolchain).

- [ ] **Step 3: Run the spike in the microVM**

Pack a minimal initramfs with the spike binary as `/init` (plus `/dev/{mem,console,null}` nodes and any bundled libs at their `ldd` paths), pull it back, and boot it:
```bash
# build on artemis2 (mirror build-fuzz-initramfs.sh), pull to kimage/out/asan-spike.cpio
cargo build -p ignition-spike && scripts/sign.sh target/debug/boot
target/debug/boot --fuzz --mem 96 --initramfs kimage/out/asan-spike.cpio kimage/out/Image
# or a plain boot if simpler; watch the guest console (stdout) for ASAN_SPIKE lines.
```
Gate: the guest console shows an AddressSanitizer `heap-buffer-overflow` report (linkage 1/2) OR the process dies on SIGSEGV at the overflow (linkage 3). The `no-detection (BAD)` line must NOT appear.

- [ ] **Step 4: Record the recipe**

Write the working recipe (compiler invocation, packages, bundled libs + paths, `ASAN_OPTIONS` needed) as a comment block at the top of `kimage/build/build-fuzz-initramfs.sh` (you will implement it in Task 3). Commit the spike + the recorded recipe:

```bash
git add kimage/build/fuzz-harness/asan_spike.c kimage/build/build-fuzz-initramfs.sh
git commit -m "fuzz(m1): sanitizer-in-guest toolchain spike + recorded recipe"
```

- [ ] **Step 5: Report** the chosen linkage (1, 2, or 3) to the controller — Tasks 2/3 depend on it. If linkage 3 (guard page), Tasks 2/3 use the guard allocator and the SIGSEGV path instead of ASan APIs; the plan notes the divergence inline.

---

### Task 2: Realistic target with a planted length-field heap overflow

**Files:**
- Create: `kimage/build/fuzz-harness/target.c`
- Modify: `kimage/build/fuzz-harness/harness.c` (call into `target.c`; remove the M0 stub `target_parse`)

- [ ] **Step 1: Write the target**

`kimage/build/fuzz-harness/target.c` (the chunk parser; identical bug shape to `docs/examples/fuzzing/target.c`, adapted for the guest). If Task 1 chose the guard-page fallback, replace `malloc(16)` with the guard allocator described in Task 1 Step 2.3 (a helper at the top of this file); otherwise use plain `malloc` and let ASan catch it.

```c
/* target.c — a chunk-format parser with a PLANTED length-field heap overflow.
 * Format: "FUZ" magic | version(1) | chunks; chunk = type(1) | len(2 LE) | data[len].
 * Type 'C' copies `len` bytes into a 16-byte heap buffer with NO bound check
 * (the archetypal image/font CVE shape). Built with the sanitizer chosen in M1
 * Task 1 so the overflow is caught deterministically. */
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

volatile uint8_t g_sink;  /* observable side-effect so the copy isn't elided */

void target_parse(const uint8_t *d, unsigned long n) {
    if (n < 4) return;
    if (d[0] != 'F' || d[1] != 'U' || d[2] != 'Z' || d[3] != 1) return;
    unsigned long i = 4;
    while (i + 3 <= n) {
        uint8_t  type = d[i];
        uint16_t len  = (uint16_t)(d[i + 1] | (d[i + 2] << 8));
        const uint8_t *data = d + i + 3;
        if (i + 3 + (unsigned long)len > n) return;     /* chunk truncated */
        if (type == 'C') {
            uint8_t *buf = malloc(16);
            memcpy(buf, data, len);                     /* BUG: len may exceed 16 */
            for (uint16_t k = 0; k < len; k++) g_sink ^= buf[k];  /* read -> live */
            free(buf);
        }
        i += 3 + (unsigned long)len;
    }
}
```

- [ ] **Step 2: Point the harness at the real target**

In `kimage/build/fuzz-harness/harness.c`: delete the M0 stub `target_parse` (lines defining it), and add a prototype near the top:

```c
/* The fuzz target lives in target.c (instrumented with the sanitizer). */
void target_parse(const uint8_t *data, unsigned long len);
```

The harness loop already calls `target_parse((const uint8_t *)g_win, len)`. Change its `len` argument type to `unsigned long` to match (the loop variable is `uint32_t`; widen at the call: `target_parse((const uint8_t *)g_win, (unsigned long)len)`).

- [ ] **Step 3: Build is deferred to Task 3** (the build script change compiles both TUs). For now, syntax-check `target.c` locally if a compiler is handy (`gcc -fsyntax-only -c target.c`), then commit:

```bash
git add kimage/build/fuzz-harness/target.c kimage/build/fuzz-harness/harness.c
git commit -m "fuzz(m1): chunk-parser target with planted length-field heap overflow"
```

---

### Task 3: ASan death-callback → CRASH doorbell + build wiring

**Files:**
- Modify: `kimage/build/fuzz-harness/harness.c` (death callback + options)
- Modify: `kimage/build/build-fuzz-initramfs.sh` (compile target+harness with the sanitizer; bundle libs if dynamic)

**If Task 1 chose the guard-page fallback (linkage 3):** skip the ASan API parts of Step 1; the overflow faults as SIGSEGV and the existing `crash_handler` already rings `CMD_CRASH` with `CRASH_CODE = SIGSEGV`. You still do Step 2 (build target+harness together) — just without `-fsanitize=address`. Then go to Task 4.

- [ ] **Step 1: Install the ASan death callback (linkage 1/2 only)**

In `harness.c`, add (the harness TU must NOT be ASan-instrumented in a way that recurses; building the harness without `-fsanitize=address` while linking the ASan runtime is the standard split — confirm in Task 1):

```c
/* ASan calls this just before it aborts on a finding. We ring the CRASH
 * doorbell (the VMM records the input + resets us) instead of letting ASan
 * exit. CRASH_CODE carries a fixed ASan class; the signal handlers remain a
 * backstop for faults ASan does not intercept. */
extern void __asan_set_death_callback(void (*cb)(void));

#define CRASH_CODE_ASAN 0x5a  /* arbitrary nonzero ASan class marker */

static void asan_on_death(void) {
    reg_write(REG_CRASH_CODE, CRASH_CODE_ASAN);
    doorbell(CMD_CRASH);
    for (;;) { }
}

/* Force ASan to abort (so the death callback fires) and keep it quiet/fast. */
const char *__asan_default_options(void) {
    return "abort_on_error=1:halt_on_error=1:detect_leaks=0";
}
```

Call `__asan_set_death_callback(asan_on_death);` in `main` right after the `signal(...)` installs, before the `SNAPSHOT_ME` doorbell.

- [ ] **Step 2: Build target + harness in the container with the sanitizer**

Update `build-fuzz-initramfs.sh` per the Task 1 recipe. Sketch (linkage-dependent — use the recorded recipe):

```sh
# inside the arm64 container, in /src:
#   target.c  -> instrumented WITH the sanitizer
#   harness.c -> compiled WITHOUT -fsanitize=address, linked against the ASan runtime
# Example (ubuntu/glibc dynamic, linkage 2):
gcc -O1 -g -fsanitize=address -c target.c  -o target.o
gcc -O1 -g                     -c harness.c -o harness.o   # not instrumented
gcc -fsanitize=address target.o harness.o -o /out/root/init
# then bundle the ldd-listed runtime libs into /out/root at their paths:
#   mkdir -p /out/root/lib && cp <each lib from `ldd init`> /out/root/lib/...
# (static-musl linkage 1: a single `-static-pie -fsanitize=address` link, no bundling.)
```
Keep the `/dev/{mem,console,null}` node creation and the `cpio -o -H newc` packing from M0. `-O1` (not `-O2`) and the `g_sink` read keep the bug from being optimized away.

- [ ] **Step 3: Build on artemis2, pull back, verify**

Per `REBUILD-GUEST-ASSETS.md`:
```bash
ssh artemis2 'mkdir -p ~/kbuild/fuzz-harness'
scp kimage/build/fuzz-harness/*.c kimage/build/fuzz-harness/*.h artemis2:~/kbuild/fuzz-harness/
scp kimage/build/build-fuzz-initramfs.sh artemis2:~/kbuild/
ssh artemis2 'cd ~/kbuild && chmod +x build-fuzz-initramfs.sh && ./build-fuzz-initramfs.sh'
scp artemis2:'~/kbuild/out/fuzz-initramfs.cpio' kimage/out/fuzz-initramfs.cpio 2>/dev/null \
  || scp artemis2:'~/kbuild/fuzz-initramfs.cpio' kimage/out/fuzz-initramfs.cpio
head -c 6 kimage/out/fuzz-initramfs.cpio   # 070701
```

- [ ] **Step 4: Commit**

```bash
git add kimage/build/fuzz-harness/harness.c kimage/build/build-fuzz-initramfs.sh
git commit -m "fuzz(m1): ASan death-callback -> CRASH doorbell + sanitized initramfs build"
```

---

### Task 4: Verbatim replay mode (host) for the determinism gate

**Files:**
- Modify: `crates/vmm/src/fuzz/controller.rs` (a replay flag that disables mutation)
- Modify: `spike/src/bin/boot.rs` (`--replay <file>` in fuzz mode)

The determinism gate must feed a saved crash input **verbatim** and confirm it re-crashes. The blind mutator would perturb it, so add a replay path: the controller injects the fixed bytes every iteration with no mutation.

- [ ] **Step 1: Write the failing test (controller)**

In `crates/vmm/src/fuzz/controller.rs` tests, add:

```rust
#[test]
fn replay_input_is_used_verbatim() {
    // A controller in replay mode must expose exactly the replay bytes, unmutated.
    let win = vec![0u8; 64];
    // Use the pure helper directly: replay returns the fixed input length and the
    // window holds the fixed bytes.
    let fixed = vec![0xAB, 0xCD, 0xEF];
    let n = super::replay_into(&fixed, &mut { win.clone() }[..]);
    assert_eq!(n, 3);
}
```

(Adjust to match the helper you implement; the point is a non-mutating injection.)

- [ ] **Step 2: Implement the replay helper + controller flag**

Add a pure helper and a `replay: Option<Vec<u8>>` field to `FuzzController`. When `replay` is set, `prepare_next_input` copies the fixed bytes (clamped to the window) instead of calling `mutate`:

```rust
/// Copy a fixed input verbatim into the window (replay/determinism mode).
pub fn replay_into(input: &[u8], window: &mut [u8]) -> u32 {
    let n = input.len().min(window.len());
    window[..n].copy_from_slice(&input[..n]);
    n as u32
}
```

In `FuzzController`: add `replay: Option<Vec<u8>>` (set via a new `new` parameter or a `set_replay`), and in `prepare_next_input`:

```rust
    fn prepare_next_input(&mut self) -> u32 {
        if let Some(fixed) = &self.replay {
            let fixed = fixed.clone();
            return replay_into(&fixed, self.window());
        }
        // ... existing blind-mutation path ...
    }
```

Thread the new field through `FuzzController::new` (add a `replay: Option<Vec<u8>>` arg; M0 callers pass `None`). Update the M0 boot call site accordingly.

- [ ] **Step 3: Add `--replay <file>` to fuzz mode**

In `spike/src/bin/boot.rs` arg parsing, add `--replay <path>` (fuzz mode only). When set, read the file and pass `Some(bytes)` as the controller's replay input (and it implies a tiny run — the gate stops as soon as a crash is captured). Document it in the usage string.

- [ ] **Step 4: Test + build**

Run `cargo test -p ignition-vmm fuzz::controller` (replay test + existing pass), `cargo build -p ignition-spike`, `cargo clippy -p ignition-vmm -p ignition-spike`.

- [ ] **Step 5: Commit**

```bash
git add crates/vmm/src/fuzz/controller.rs spike/src/bin/boot.rs
git commit -m "fuzz(m1): verbatim replay mode (--replay) for the determinism gate"
```

---

### Task 5: M1 gate test

**Files:**
- Create: `scripts/fuzz_m1_test.py`

Two-part gate: (a) blind rediscovery of the planted overflow from a near-boundary seed; (b) the saved crash input, replayed verbatim, re-crashes (determinism, spec §7).

- [ ] **Step 1: Write the test**

`scripts/fuzz_m1_test.py` (model on `scripts/fuzz_m0_test.py`; remember `scripts/sign.sh target/debug/boot` after building):

```python
#!/usr/bin/env python3
"""M1 gate: rediscover a planted length-field heap overflow + replay determinism.

(a) Boot the fuzzer with a near-boundary seed (valid 'FUZ' chunk, len=16). Blind
    havoc must bump len past 16 and trigger the sanitizer -> CRASH doorbell ->
    solution file. (b) Replay the saved crash input verbatim and confirm it
    re-crashes (deterministic, reproducible).
"""
import glob, os, signal, subprocess, sys, tempfile, time

BOOT = os.environ.get("BOOT_BIN", "target/debug/boot")
KERNEL = os.environ.get("FUZZ_KERNEL", "kimage/out/Image")
INITRAMFS = os.environ.get("FUZZ_INITRAMFS", "kimage/out/fuzz-initramfs.cpio")

# Near-boundary seed: "FUZ" v1, one 'C' chunk with len=16 (== buffer size, valid),
# followed by >=16 data bytes. A single byte bump of the len field -> overflow.
SEED = bytes([ord('F'), ord('U'), ord('Z'), 1, ord('C'), 16, 0] + list(range(1, 21)))

def run(cmd, sol, timeout):
    p = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    deadline = time.time() + timeout
    found = None
    while time.time() < deadline:
        hits = glob.glob(os.path.join(sol, "crash-*.bin"))
        if hits:
            found = sorted(hits)[0]
            break
        if p.poll() is not None:
            break
        time.sleep(0.5)
    try:
        p.send_signal(signal.SIGINT); p.wait(timeout=5)
    except Exception:
        p.kill()
    out = p.stdout.read().decode(errors="replace") if p.stdout else ""
    return found, out

def main():
    for x in (BOOT, KERNEL, INITRAMFS):
        if not os.path.exists(x):
            print(f"missing artifact: {x}", file=sys.stderr); sys.exit(2)
    d = tempfile.mkdtemp(prefix="fuzz-m1-")
    sol1 = os.path.join(d, "sol1"); seed = os.path.join(d, "seed.bin")
    open(seed, "wb").write(SEED)
    # (a) blind rediscovery
    found, out = run([BOOT, "--fuzz", "--mem", "96", "--initramfs", INITRAMFS,
                      "--solutions", sol1, "--seed", seed, KERNEL], sol1, 90)
    if not found:
        print(out); print("FAIL: planted overflow not rediscovered", file=sys.stderr); sys.exit(1)
    print("PASS(a): rediscovered planted overflow ->", found)
    # (b) replay determinism
    sol2 = os.path.join(d, "sol2")
    found2, out2 = run([BOOT, "--fuzz", "--mem", "96", "--initramfs", INITRAMFS,
                        "--solutions", sol2, "--replay", found, KERNEL], sol2, 30)
    if not found2:
        print(out2); print("FAIL: replayed crash input did not reproduce", file=sys.stderr); sys.exit(1)
    print("PASS(b): replayed crash input reproduced ->", found2)
    print("PASS: M1 gate")

if __name__ == "__main__":
    main()
```

- [ ] **Step 2: Build, sign, run**

```bash
cargo build -p ignition-spike && scripts/sign.sh target/debug/boot
python3 scripts/fuzz_m1_test.py
```
Expected: `PASS(a)`, `PASS(b)`, `PASS: M1 gate`. If (a) does not converge in 90s blind, increase the budget or move the seed closer to the boundary (e.g. seed `len=16` with exactly the data the overflow needs); if it still won't converge, that confirms the coverage tension and is the signal to bring M2 forward — STOP and report rather than over-tuning.

- [ ] **Step 3: Commit**

```bash
git add scripts/fuzz_m1_test.py
git commit -m "fuzz(m1): gate test (rediscover planted overflow + replay determinism)"
```

---

## Self-Review

**Spec coverage (M1 milestone + spec §5/§8 ASan/death-callback, §7 determinism):**
- Real target with a planted, realistic bug (length-field heap overflow): Task 2. ✓
- Sanitizer detection in-guest (catches non-segfaulting corruption): Tasks 1, 3 (ASan) or guard-page fallback. ✓
- ASan death callback → CRASH doorbell (spec §8): Task 3. ✓
- Rediscover from a seed corpus, blind (spec M1, coverage deferred): Task 5(a). ✓
- Deterministic reproduction from the saved input (spec §7): Tasks 4, 5(b). ✓

**Deferred to M2 (correct per the scope decision):** SanCov coverage window, libAFL feedback/corpus, libpng target, dirty-page reset. Not in this plan.

**Type/name consistency:** `target_parse(const uint8_t*, unsigned long)` (prototype in harness.c matches target.c). `asan_on_death`, `__asan_default_options`, `__asan_set_death_callback`, `CRASH_CODE_ASAN`. `FuzzController::new` gains a `replay: Option<Vec<u8>>` param — every call site (M0 boot + any test) updated together. `replay_into` helper used by both the test and `prepare_next_input`. Boundary seed bytes identical in the harness expectation (`"FUZ"`, v1, `'C'`, len=16) and the gate.

**Known soft spots (flagged, not placeholders):**
1. Task 1 is genuinely exploratory — the linkage outcome (1/2/3) branches Tasks 2/3. The plan handles all three; the implementer reports which was chosen.
2. ASan + musl static is the highest-risk item; the guard-page fallback guarantees the milestone completes with the same detection guarantee even if ASan-in-guest is abandoned.
3. Blind rediscovery convergence (Task 5a) depends on the seed sitting close to the boundary. If it won't converge in budget, that is the empirical signal to pull M2 (coverage) forward — the plan says stop and report rather than over-tune the seed into triviality.
4. If the chosen linkage is dynamic (glibc), the initramfs grows (bundled libs). Confirm it still fits below the FDT at `--mem 96` (M0's initramfs placement check at `RAM_BASE + 0x0400_0000` errors if not — bump `--mem` if the libs push it over).
