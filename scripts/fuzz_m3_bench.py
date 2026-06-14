#!/usr/bin/env python3
"""M3 benchmark: run the snapshot fuzzer against real libpng-current and capture
the benchmark metrics, plus the deterministic time-to-rediscover number on the
synthetic ASan target.

Runs (all single-core, fixed wall-clock, SIGINT to stop):
  1. libpng / dirty reset  -> execs/sec, reset-latency p50/p99 (page-copy vs
     register split), dirty-set-size distribution, coverage curve.
  2. libpng / full reset   -> execs/sec, for the dirty-vs-full speedup.
  3. synthetic / dirty     -> time-to-rediscover the planted CVE (correctness).

Parses the controller's metrics file (written on clean shutdown via --metrics).

Gate (asserts the machinery produced usable numbers; it does NOT assert specific
latencies, which are host-dependent):
  - libpng dirty run: execs_per_sec > 0, coverage_final > 0, dirty_pages_p50
    present, reset_us_p50 present.
  - dirty execs/sec > full execs/sec (the snapshot speedup holds on a real target).
  - synthetic run: time_to_crash_s is a number (planted CVE rediscovered).
"""
import os, re, signal, subprocess, sys, tempfile, time

BOOT = os.environ.get("BOOT_BIN", "target/debug/boot")
KERNEL = os.environ.get("FUZZ_KERNEL", "kimage/out/Image")
PNG_INITRAMFS = os.environ.get("FUZZ_INITRAMFS_LIBPNG", "kimage/out/fuzz-initramfs-libpng.cpio")
SYN_INITRAMFS = os.environ.get("FUZZ_INITRAMFS", "kimage/out/fuzz-initramfs.cpio")
DURATION = float(os.environ.get("M3_DURATION", "60"))
MEM = os.environ.get("M3_MEM", "128")

# Canonical valid 1x1 RGBA PNG (8-bit), generated via struct/zlib/crc32.
PNG_SEED = bytes.fromhex("89504e470d0a1a0a0000000d49484452000000010000000108060000001f15c4890000000d49444154789c63f8cfc0f01f00050001ff89993d1d0000000049454e44ae426082")

# Synthetic seed: near-boundary 'C' chunk (len == 16); a byte bump overflows.
SYN_SEED = bytes([ord('F'), ord('U'), ord('Z'), 1, ord('C'), 16, 0] + list(range(1, 21)))

METRIC = re.compile(r"^metric (.+)$", re.M)

def parse_metrics(path):
    d = {}
    if not os.path.exists(path):
        return d
    with open(path) as f:
        text = f.read()
    for line in METRIC.findall(text):
        for tok in line.split():
            if "=" in tok:
                k, v = tok.split("=", 1)
                d[k] = v
    return d

def run(initramfs, reset, seed_bytes, duration, metrics_path, sols):
    seed = sols + ".seed"
    with open(seed, "wb") as f:
        f.write(seed_bytes)
    logf = open(sols + ".log", "w+b")
    cmd = [BOOT, "--fuzz", "--mem", MEM, "--initramfs", initramfs,
           "--solutions", sols, "--reset", reset, "--seed", seed,
           "--metrics", metrics_path, KERNEL]
    p = subprocess.Popen(cmd, stdout=logf, stderr=subprocess.STDOUT)
    deadline = time.time() + duration
    while time.time() < deadline and p.poll() is None:
        time.sleep(0.5)
    try:
        p.send_signal(signal.SIGINT); p.wait(timeout=10)
    except Exception:
        p.kill()
    logf.close()
    return parse_metrics(metrics_path)

def main():
    for x in (BOOT, KERNEL, PNG_INITRAMFS, SYN_INITRAMFS):
        if not os.path.exists(x):
            print(f"missing artifact: {x}", file=sys.stderr); sys.exit(2)
    d = tempfile.mkdtemp(prefix="fuzz-m3-")

    print(f"[1/3] libpng / dirty reset ({DURATION:.0f}s) ...")
    md = run(PNG_INITRAMFS, "dirty", PNG_SEED, DURATION,
             os.path.join(d, "png_dirty.txt"), os.path.join(d, "png_dirty"))
    print(f"[2/3] libpng / full reset ({DURATION:.0f}s) ...")
    mf = run(PNG_INITRAMFS, "full", PNG_SEED, DURATION,
             os.path.join(d, "png_full.txt"), os.path.join(d, "png_full"))
    print(f"[3/3] synthetic / dirty reset (time-to-rediscover, {DURATION:.0f}s) ...")
    ms = run(SYN_INITRAMFS, "dirty", SYN_SEED, DURATION,
             os.path.join(d, "syn_dirty.txt"), os.path.join(d, "syn_dirty"))

    def num(m, k, default=0.0):
        try:
            return float(m.get(k, default))
        except ValueError:
            return default

    eps_dirty = num(md, "execs_per_sec")
    eps_full = num(mf, "execs_per_sec")
    cov = num(md, "coverage_final")
    rp50, rp99 = num(md, "reset_us_p50"), num(md, "reset_us_p99")
    sp50 = num(md, "restore_us_p50"); gp50 = num(md, "regs_us_p50")
    dp50, dp99, dmax = num(md, "dirty_pages_p50"), num(md, "dirty_pages_p99"), num(md, "dirty_pages_max")
    ttc = ms.get("time_to_crash_s", "none")

    print("\n=== M3 benchmark ===")
    print(f"libpng dirty: {eps_dirty:.0f} execs/sec | coverage={cov:.0f} edges")
    print(f"libpng full : {eps_full:.0f} execs/sec")
    print(f"reset latency (dirty): p50={rp50:.0f}us p99={rp99:.0f}us  (page-copy p50={sp50:.0f}us, regs p50={gp50:.0f}us)")
    print(f"dirty-set size: p50={dp50:.0f} p99={dp99:.0f} max={dmax:.0f} pages (16 KiB each)")
    print(f"time-to-rediscover planted CVE (synthetic): {ttc} s")

    fail = []
    if not (eps_dirty > 0): fail.append(f"libpng dirty execs/sec not positive ({eps_dirty})")
    if not (cov > 0): fail.append(f"libpng coverage did not register ({cov})")
    if "reset_us_p50" not in md: fail.append("reset latency p50 missing")
    if "dirty_pages_p50" not in md: fail.append("dirty-set distribution missing")
    if not (eps_dirty > eps_full > 0): fail.append(f"dirty not faster than full ({eps_dirty:.0f} vs {eps_full:.0f})")
    if ttc == "none": fail.append("synthetic run did not rediscover the planted CVE")
    if fail:
        for f in fail: print("FAIL:", f, file=sys.stderr)
        sys.exit(1)
    print("PASS: M3 benchmark gate")

if __name__ == "__main__":
    main()
