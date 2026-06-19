#!/usr/bin/env python3
"""TPM2 benchmark: dirty-vs-full execs/sec, reset latency, dirty-set size, and
coverage on the ms-tpm-20-ref command processor. Mirrors fuzz_m3_bench.py. Uses
the clean GetCapability seed so the run measures throughput (no early crash-stop).

Gate (asserts the machinery produced usable numbers; not specific latencies):
  - dirty run: execs_per_sec > 0, coverage_final > 0, reset_us_p50 present,
    dirty_pages_p50 present.
  - dirty execs/sec > full execs/sec (the snapshot speedup holds on the TPM).
"""
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
    if not os.path.exists(path):
        return d
    for line in METRIC.findall(open(path).read()):
        for tok in line.split():
            if "=" in tok:
                k, v = tok.split("=", 1); d[k] = v
    return d


def run(reset, metrics_path, sols):
    cmd = [BOOT, "--fuzz", "--mem", MEM, "--initramfs", INITRAMFS, "--solutions", sols,
           "--reset", reset, "--seed", SEED, "--metrics", metrics_path, KERNEL]
    with open(sols + ".log", "w+b") as logf:
        p = subprocess.Popen(cmd, stdout=logf, stderr=subprocess.STDOUT)
        deadline = time.time() + DURATION
        while time.time() < deadline and p.poll() is None:
            time.sleep(0.5)
        try:
            p.send_signal(signal.SIGINT); p.wait(timeout=10)
        except Exception:
            p.kill(); p.wait()
    return parse_metrics(metrics_path)


def num(m, k, d=0.0):
    try:
        return float(m.get(k, d))
    except ValueError:
        return d


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
        for f in fail:
            print("FAIL:", f, file=sys.stderr)
        sys.exit(1)
    print("PASS: TPM2 benchmark gate")


if __name__ == "__main__":
    main()
