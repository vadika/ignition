#!/usr/bin/env python3
"""M0 fuzz gate: a planted-crash seed must be captured as a solution.

Boots `boot --fuzz` with a seed beginning 0xFF (the stub target overflows on
that), runs briefly, and asserts a crash-*.bin solution file is written. Proves
the full loop: inject -> guest parse -> crash -> doorbell -> capture -> reset.
"""
import glob, os, signal, subprocess, sys, tempfile, time

BOOT = os.environ.get("BOOT_BIN", "target/debug/boot")
KERNEL = os.environ.get("FUZZ_KERNEL", "kimage/out/Image")
INITRAMFS = os.environ.get("FUZZ_INITRAMFS", "kimage/out/fuzz-initramfs.cpio")

def main():
    for p in (BOOT, KERNEL, INITRAMFS):
        if not os.path.exists(p):
            print(f"missing required artifact: {p}", file=sys.stderr)
            sys.exit(2)
    d = tempfile.mkdtemp(prefix="fuzz-m0-")
    sol = os.path.join(d, "solutions")
    seed = os.path.join(d, "seed.bin")
    with open(seed, "wb") as f:
        f.write(b"\xff\x00\x00\x00")
    cmd = [BOOT, "--fuzz", "--mem", "96", "--window-mib", "2",
           "--initramfs", INITRAMFS, "--solutions", sol, "--seed", seed, KERNEL]
    print("run:", " ".join(cmd))
    p = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    deadline = time.time() + 40
    found = False
    while time.time() < deadline:
        if glob.glob(os.path.join(sol, "crash-*.bin")):
            found = True
            break
        if p.poll() is not None:
            break
        time.sleep(0.5)
    try:
        p.send_signal(signal.SIGINT)
        p.wait(timeout=5)
    except Exception:
        p.kill()
    out = p.stdout.read().decode(errors="replace") if p.stdout else ""
    if not found:
        print(out)
        print("FAIL: no crash solution captured", file=sys.stderr)
        sys.exit(1)
    print("PASS: crash solution captured in", sol)

if __name__ == "__main__":
    main()
