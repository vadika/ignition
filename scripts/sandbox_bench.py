#!/usr/bin/env python3
"""Sandbox throughput/latency benchmark: run an identical command in N microVM
sandboxes, comparing COLD (fresh boot each) vs HOT (restore from a warm snapshot
each). Reuses the fan-out driver's vsock ign-exec client. Stdlib only.

Default workload: `import numpy; numpy.zeros(5).tolist()` in the guest. The
tools-base rootfs ships py3-numpy, so it runs offline.

A concurrency cap bounds how many sandboxes are live at once (100 x 1 GiB will
not fit in RAM); per-sandbox latency percentiles are cap-independent and are the
real cold-vs-hot comparison, while wall-clock is cap-dependent and reported
separately.
"""
import argparse
import json
import os
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor

import fanout_demo as fd  # vsock_connect / vsock_run, same scripts/ dir

# Default guest command. Single line; its stdout must start with the zeros list.
WORKLOAD = "python3 -c 'import numpy; print(numpy.zeros(5).tolist())'"
EXPECT_PREFIX = "[0.0, 0.0, 0.0, 0.0, 0.0]"
# Kernel cmdline for a COLD boot of the overlay-root tools rootfs (mirrors
# scripts/make-tools-base.sh). HOT restore carries this in the snapshot.
COLD_APPEND = "ro init=/sbin/overlay-init"


def _pct(xs, q):
    if not xs:
        return None
    s = sorted(xs)
    return s[min(len(s) - 1, int(q * len(s)))]


def run_one(mode, i, args, run_tok):
    """Spawn one sandbox in `mode` ('cold'|'hot'), run the workload over vsock,
    record timings + output, then kill the child. Never raises to the pool."""
    uds = f"/tmp/sbench-{run_tok}-{mode}-{i}.sock"
    rec = {"i": i, "mode": mode, "ready_ms": None, "exec_ms": None,
           "exit": None, "ok_output": False, "error": None}
    proc = None
    try:
        try:
            os.unlink(uds)
        except FileNotFoundError:
            pass
        if mode == "hot":
            cmd = [args.boot, "--restore", args.base, "--store", args.store,
                   "--mem", str(args.mem), "--vsock-uds", uds,
                   args.kernel, args.rootfs]
        else:  # cold: fresh boot, no snapshot
            cmd = [args.boot, "--mem", str(args.mem), "--vsock-uds", uds,
                   "--append", COLD_APPEND, args.kernel, args.rootfs]
        t0 = time.monotonic()
        proc = subprocess.Popen(cmd, stdin=subprocess.DEVNULL,
                                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        sock = fd.vsock_connect(uds, deadline=args.deadline)
        rec["ready_ms"] = round((time.monotonic() - t0) * 1000)
        te0 = time.monotonic()
        resp = fd.vsock_run(sock, WORKLOAD, timeout=args.timeout)
        rec["exec_ms"] = round((time.monotonic() - te0) * 1000)
        rec["exit"] = resp.get("exit")
        if resp.get("timed_out") or resp.get("exit") != 0:
            rec["error"] = f"exec exit={resp.get('exit')} timed_out={resp.get('timed_out')}"
        else:
            rec["ok_output"] = resp.get("stdout", "").strip().startswith(EXPECT_PREFIX)
            if not rec["ok_output"]:
                rec["error"] = f"unexpected output: {resp.get('stdout', '')!r}"
    except Exception as e:  # noqa: BLE001 - bench driver, report per-sandbox
        rec["error"] = str(e)
    finally:
        if proc and proc.poll() is None:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
        try:
            os.unlink(uds)
        except FileNotFoundError:
            pass
    return rec


def run_mode(mode, args, run_tok):
    """Run N sandboxes in `mode` with a bounded concurrency pool. Returns
    (records, wall_clock_ms)."""
    t0 = time.monotonic()
    with ThreadPoolExecutor(max_workers=args.concurrency) as ex:
        recs = list(ex.map(lambda i: run_one(mode, i, args, run_tok), range(args.count)))
    return recs, round((time.monotonic() - t0) * 1000)


def summarize(mode, recs, wall_ms):
    ok = [r for r in recs if not r["error"]]
    rdy = [r["ready_ms"] for r in ok if r["ready_ms"] is not None]
    ex = [r["exec_ms"] for r in ok if r["exec_ms"] is not None]
    mean = lambda xs: round(sum(xs) / len(xs)) if xs else None
    return {
        "mode": mode, "n": len(recs), "ok": len(ok), "failed": len(recs) - len(ok),
        "ready_ms": {"p50": _pct(rdy, 0.5), "p95": _pct(rdy, 0.95), "mean": mean(rdy)},
        "exec_ms": {"p50": _pct(ex, 0.5), "p95": _pct(ex, 0.95), "mean": mean(ex)},
        "wall_clock_ms": wall_ms,
    }


def _print_summary(s):
    r, e = s["ready_ms"], s["exec_ms"]
    print(f"  {s['mode']:<5} n={s['n']} ok={s['ok']} failed={s['failed']}  "
          f"ready p50/p95/mean {r['p50']}/{r['p95']}/{r['mean']} ms  "
          f"exec p50/p95/mean {e['p50']}/{e['p95']}/{e['mean']} ms  "
          f"wall {s['wall_clock_ms']} ms")


def main():
    ap = argparse.ArgumentParser(description="Cold vs hot sandbox benchmark.")
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    ap.add_argument("-n", "--count", type=int, default=100)
    ap.add_argument("-c", "--concurrency", type=int, default=8,
                    help="max sandboxes live at once (RAM-bound)")
    ap.add_argument("--mode", choices=["both", "cold", "hot"], default="both")
    ap.add_argument("--base", default="tools-base")
    ap.add_argument("--store", default=os.path.join(root, "mcp-store"))
    ap.add_argument("--boot", default=os.path.join(root, "target/debug/boot"))
    ap.add_argument("--kernel", default=os.path.join(root, "kimage/out/Image"))
    ap.add_argument("--rootfs", default=os.path.join(root, "kimage/out/rootfs-tools.ext4"))
    ap.add_argument("--mem", type=int, default=1024)
    ap.add_argument("--timeout", type=float, default=30.0)
    ap.add_argument("--deadline", type=float, default=30.0)
    ap.add_argument("--json", action="store_true")
    args = ap.parse_args()

    for label, path in (("boot", args.boot), ("kernel", args.kernel), ("rootfs", args.rootfs)):
        if not os.path.exists(path):
            print(f"missing {label}: {path}", file=sys.stderr)
            return 2
    need_hot = args.mode in ("both", "hot")
    if need_hot and not os.path.exists(os.path.join(args.store, "snapshots", args.base)):
        print(f"snapshot '{args.base}' not found in {args.store}; "
              f"run scripts/make-tools-base.sh first", file=sys.stderr)
        return 2

    run_tok = str(os.getpid())
    modes = ["cold", "hot"] if args.mode == "both" else [args.mode]
    summaries = []
    for m in modes:
        if not args.json:
            print(f"running {args.count} {m} sandboxes (concurrency {args.concurrency}) ...",
                  file=sys.stderr)
        recs, wall = run_mode(m, args, run_tok)
        summaries.append((summarize(m, recs, wall), recs))

    sm = {s["mode"]: s for s, _ in summaries}
    if args.json:
        out = {"summaries": [s for s, _ in summaries],
               "forks": [r for _, recs in summaries for r in recs]}
        if ("cold" in sm and "hot" in sm
                and sm["cold"]["ready_ms"]["p50"] and sm["hot"]["ready_ms"]["p50"]):
            out["speedup_ready_p50"] = round(
                sm["cold"]["ready_ms"]["p50"] / sm["hot"]["ready_ms"]["p50"], 1)
        print(json.dumps(out, indent=2))
    else:
        print(f"\nworkload: {WORKLOAD}")
        for s, _ in summaries:
            _print_summary(s)
        if ("cold" in sm and "hot" in sm
                and sm["cold"]["ready_ms"]["p50"] and sm["hot"]["ready_ms"]["p50"]):
            sp = sm["cold"]["ready_ms"]["p50"] / sm["hot"]["ready_ms"]["p50"]
            print(f"\nhot start is {sp:.1f}x faster than cold "
                  f"({sm['cold']['ready_ms']['p50']} -> {sm['hot']['ready_ms']['p50']} ms p50 ready)")

    failed = sum(s["failed"] for s, _ in summaries)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
