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
