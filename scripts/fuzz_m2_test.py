#!/usr/bin/env python3
"""M2 gate: coverage feedback + dirty-page reset.

(a) Coverage grows: the periodic "cov=" stat increases above its first reading
    and the corpus expands past the single seed.
(b) The planted overflow is still rediscovered (via the dirty reset) and the
    saved input replays deterministically.
(c) Throughput: dirty-reset execs/sec > full-copy execs/sec on equal wall-clock.

Parses the controller's periodic stderr line:
    fuzz: iters=.. execs/sec=.. cov=.. corpus=.. dirty_pages=.. reset=..
"""
import glob, os, re, signal, subprocess, sys, tempfile, time

BOOT = os.environ.get("BOOT_BIN", "target/debug/boot")
KERNEL = os.environ.get("FUZZ_KERNEL", "kimage/out/Image")
INITRAMFS = os.environ.get("FUZZ_INITRAMFS", "kimage/out/fuzz-initramfs.cpio")
SEED = bytes([ord('F'), ord('U'), ord('Z'), 1, ord('C'), 16, 0] + list(range(1, 21)))
STAT = re.compile(r"fuzz: iters=(\d+) execs/sec=([\d.]+) cov=(\d+) corpus=(\d+)")

def run(extra, sol, timeout, stop_on_crash):
    cmd = [BOOT, "--fuzz", "--mem", "96", "--initramfs", INITRAMFS,
           "--solutions", sol] + extra + [KERNEL]
    # Redirect child output to a file rather than an in-process PIPE. The planted
    # bug fires within ~1s and ASan dumps a multi-KB report on every crash (~1000
    # crashes/sec); an unread PIPE's 64KB buffer fills almost immediately, the
    # child blocks on write(), and fuzzing stalls at iteration 1 -- so the only
    # stat line ever emitted would be the initial cov= reading and coverage would
    # appear flat. A file sink never blocks, so the loop keeps running.
    logf = open(os.path.join(os.path.dirname(sol) or ".",
                             os.path.basename(sol) + ".log"), "w+b")
    p = subprocess.Popen(cmd, stdout=logf, stderr=subprocess.STDOUT)
    deadline = time.time() + timeout
    found = None
    # The planted bug is found within ~1s, often before the second periodic
    # stat checkpoint. If we stopped the instant the first crash file appears
    # we would only ever observe the iteration-1 stat line and could not see
    # coverage grow. So hold off on the crash-stop until at least one full
    # stat interval (2000 iters, emitted every ~2s) has elapsed; that lets the
    # coverage curve be observed while still stopping promptly on the crash.
    crash_floor = time.time() + 4.0
    while time.time() < deadline:
        if stop_on_crash and time.time() >= crash_floor and glob.glob(os.path.join(sol, "crash-*.bin")):
            found = sorted(glob.glob(os.path.join(sol, "crash-*.bin")))[0]
            break
        if p.poll() is not None:
            break
        time.sleep(0.5)
    if found is None and glob.glob(os.path.join(sol, "crash-*.bin")):
        found = sorted(glob.glob(os.path.join(sol, "crash-*.bin")))[0]
    try:
        p.send_signal(signal.SIGINT); p.wait(timeout=5)
    except Exception:
        p.kill()
    logf.flush(); logf.seek(0)
    out = logf.read().decode(errors="replace")
    logf.close()
    stats = STAT.findall(out)
    return found, out, stats

def eps_of(stats):
    return float(stats[-1][1]) if stats else 0.0

def main():
    for x in (BOOT, KERNEL, INITRAMFS):
        if not os.path.exists(x):
            print(f"missing artifact: {x}", file=sys.stderr); sys.exit(2)
    d = tempfile.mkdtemp(prefix="fuzz-m2-")
    seed = os.path.join(d, "seed.bin"); open(seed, "wb").write(SEED)

    # (a)+(b1): coverage growth + crash rediscovery on the dirty reset.
    sol_d = os.path.join(d, "dirty")
    found, out, stats = run(["--reset", "dirty", "--seed", seed], sol_d, 90, True)
    if not stats:
        print(out); print("FAIL: no fuzz stats line parsed", file=sys.stderr); sys.exit(1)
    cov_first = int(stats[0][2]); cov_last = int(stats[-1][2])
    corpus_last = int(stats[-1][3])
    if cov_last <= cov_first:
        print(out); print(f"FAIL: coverage did not grow ({cov_first}->{cov_last})", file=sys.stderr); sys.exit(1)
    if corpus_last <= 1:
        print(out); print(f"FAIL: corpus did not grow past seed ({corpus_last})", file=sys.stderr); sys.exit(1)
    print(f"PASS(a): coverage grew {cov_first}->{cov_last}, corpus={corpus_last}")
    if not found:
        print(out); print("FAIL: planted overflow not rediscovered (dirty reset)", file=sys.stderr); sys.exit(1)
    print("PASS(b1): rediscovered planted overflow ->", found)

    # (b2): replay determinism (dirty reset).
    sol_r = os.path.join(d, "replay")
    found2, out2, _ = run(["--reset", "dirty", "--replay", found], sol_r, 30, True)
    if not found2:
        print(out2); print("FAIL: replayed crash did not reproduce", file=sys.stderr); sys.exit(1)
    print("PASS(b2): replayed crash reproduced ->", found2)

    # (c): throughput jump. Run each mode for a fixed wall-clock (no crash stop)
    # and compare steady-state execs/sec.
    sol_f = os.path.join(d, "full")
    _, outf, sf = run(["--reset", "full", "--seed", seed], sol_f, 25, False)
    sol_d2 = os.path.join(d, "dirty2")
    _, outd, sd = run(["--reset", "dirty", "--seed", seed], sol_d2, 25, False)
    eps_full, eps_dirty = eps_of(sf), eps_of(sd)
    print(f"throughput: full={eps_full:.0f} execs/sec, dirty={eps_dirty:.0f} execs/sec")
    if not (eps_dirty > eps_full > 0):
        print(outf); print(outd)
        print(f"FAIL: dirty reset not faster ({eps_dirty:.0f} <= {eps_full:.0f})", file=sys.stderr); sys.exit(1)
    print(f"PASS(c): dirty reset faster ({eps_dirty:.0f} > {eps_full:.0f} execs/sec)")
    print("PASS: M2 gate")

if __name__ == "__main__":
    main()
