#!/usr/bin/env python3
"""TPM2 correctness gate: rediscover the planted NV_Write length-field OOB and
replay it deterministically. Mirrors fuzz_m1_test.py for the TPM target.

(a) Boot the fuzzer with the near-boundary NV_Write seed; mutation bumps the size
    field past 32 -> ASan stack overflow -> CRASH doorbell -> solution file.
(b) Replay the saved crash input verbatim and confirm it re-crashes.
"""
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
