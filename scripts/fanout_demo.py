#!/usr/bin/env python3
"""Fan-out demo driver: fork N clones from one base snapshot in parallel, run an
identical workload in each over vsock ign-exec, and show CRNG divergence,
copy-on-write filesystem isolation, and fork speed. Stdlib only."""
import argparse
import json
import os
import socket
import struct
import subprocess
import sys
import threading
import time

EXEC_PORT = 7000
MAX_FRAME = 64 * 1024 * 1024


def vsock_connect(uds_path, deadline):
    """Connect to the guest ign-exec agent and complete the CONNECT/OK handshake.

    Retries the FULL connect + handshake until `deadline` seconds elapse, and
    returns a live socket. Two things race: the control UDS appears only once
    boot wires up vsock, and the guest's port-7000 listener comes up later still
    (on a cold boot, ~hundreds of ms after the UDS, via openrc). boot answers a
    CONNECT to a not-yet-listening port by closing the connection, so the
    handshake itself must be retried, not just connect(). On a hot restore the
    listener is already up, so the first attempt succeeds. This is the spawn ->
    guest-ready latency, i.e. the real fork/boot cost. Raises on timeout."""
    end = time.monotonic() + deadline
    while True:
        s = None
        try:
            s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            s.settimeout(max(0.5, end - time.monotonic()))
            s.connect(uds_path)
            s.sendall(f"CONNECT {EXEC_PORT}\n".encode())
            line = b""
            while not line.endswith(b"\n"):
                b = s.recv(1)
                if not b:
                    raise IOError("vsock: connection closed before OK")
                line += b
                if len(line) > 128:
                    raise IOError("vsock: oversized ack line")
            if not line.startswith(b"OK "):
                raise IOError(f"vsock: expected OK, got {line!r}")
            return s
        except (OSError, IOError) as e:  # OSError covers connect + IOError handshake failures
            if s:
                s.close()
            if time.monotonic() >= end:
                raise TimeoutError(f"vsock: {uds_path} not ready before deadline (last: {e})")
            time.sleep(0.05)


def vsock_run(sock, cmd, timeout):
    """Frame one exec request on an already-handshaken socket, read the framed
    response, and close the socket. Returns the parsed response dict."""
    sock.settimeout(timeout)
    try:
        req = json.dumps({"cmd": cmd, "timeout": timeout}).encode()
        sock.sendall(struct.pack("<I", len(req)) + req)
        hdr = _recvn(sock, 4)
        n = struct.unpack("<I", hdr)[0]
        if n > MAX_FRAME:
            raise IOError("vsock: response frame too large")
        return json.loads(_recvn(sock, n))
    finally:
        sock.close()


def vsock_exec(uds_path, cmd, timeout, deadline):
    """Connect + handshake + one framed exec request against the guest ign-exec
    agent. Thin wrapper over vsock_connect + vsock_run."""
    sock = vsock_connect(uds_path, deadline)
    return vsock_run(sock, cmd, timeout)


def _recvn(s, n):
    buf = b""
    while len(buf) < n:
        chunk = s.recv(n - len(buf))
        if not chunk:
            raise IOError("vsock: truncated frame")
        buf += chunk
    return buf


WORKLOAD = (
    "r=$(head -c8 /dev/urandom | od -An -tx1 | tr -d ' \\n'); "
    "printf 'BOOTID=%s\\n' \"$(cat /proc/sys/kernel/random/boot_id)\"; "
    "printf 'RAND=%s\\n' \"$r\"; "
    "m=/tmp/fork-marker; printf '%s' \"$r\" > \"$m\"; "
    "printf 'FILE=%s:%s\\n' \"$m\" \"$(cat \"$m\")\""
)


def parse_workload(stdout):
    """Parse the BOOTID/RAND/FILE lines the workload prints. Returns a dict;
    raises ValueError if a field is missing."""
    fields = {}
    for ln in stdout.splitlines():
        if ln.startswith("BOOTID="):
            fields["bootid"] = ln[len("BOOTID="):].strip()
        elif ln.startswith("RAND="):
            fields["rand"] = ln[len("RAND="):].strip()
        elif ln.startswith("FILE="):
            path, _, val = ln[len("FILE="):].partition(":")
            fields["file_path"] = path.strip()
            fields["file_readback"] = val.strip()
    missing = {"bootid", "rand", "file_path", "file_readback"} - fields.keys()
    if missing:
        raise ValueError(f"workload output missing {missing}: {stdout!r}")
    return fields


def verdict(forks):
    """Compute the pass/fail verdict over collected per-fork records.

    Pass gate: all forks present and exit 0, AND randoms_distinct (every `rand`
    unique), AND cow_isolated (every fork's file_readback == its own rand).
    identities_distinct (all bootids unique) is informational only: post-restore
    each clone lazily derives boot_id from its vmid-reseeded CRNG, so distinct
    bootids are bonus evidence of identity divergence, not a lineage marker."""
    good = [f for f in forks if f and not f.get("error") and f.get("exit") == 0]
    all_ok = len(good) == len(forks) and len(forks) > 0
    bootids = [f["bootid"] for f in good]
    rands = [f["rand"] for f in good]
    distinct = all_ok and len(set(rands)) == len(rands)
    cow = all_ok and all(f["rand"] == f["file_readback"] for f in good)
    identities = all_ok and len(set(bootids)) == len(bootids)
    return {
        "randoms_distinct": distinct,
        "cow_isolated": cow,
        "identities_distinct": identities,
        "ok": bool(all_ok and distinct and cow),
    }


def fork_one(i, args, run_tok, results):
    """Spawn one boot --restore child, probe it over vsock, fill results[i].
    Never raises into the thread join — records an error string instead."""
    uds = f"/tmp/fanout-{run_tok}-{i}.sock"
    rec = {"i": i, "uds": uds, "restore_ms": None, "exec_ms": None,
           "bootid": None, "rand": None, "file_path": None,
           "file_readback": None, "exit": None, "error": None}
    proc = None
    try:
        try:
            os.unlink(uds)
        except FileNotFoundError:
            pass
        t0 = time.monotonic()
        cmd = [args.boot, "--restore", args.base, "--store", args.store,
               "--mem", str(args.mem), "--vsock-uds", uds,
               args.kernel, args.rootfs]
        proc = subprocess.Popen(cmd, stdin=subprocess.DEVNULL,
                                stdout=subprocess.DEVNULL,
                                stderr=subprocess.DEVNULL)
        rec["_proc"] = proc
        sock = vsock_connect(uds, deadline=args.deadline)
        # restore_ms: spawn -> guest vsock-ready latency (the fork cost)
        rec["restore_ms"] = round((time.monotonic() - t0) * 1000)
        te0 = time.monotonic()
        resp = vsock_run(sock, WORKLOAD, timeout=args.timeout)
        rec["exec_ms"] = round((time.monotonic() - te0) * 1000)
        rec["exit"] = resp.get("exit")
        if resp.get("timed_out") or resp.get("exit") != 0:
            rec["error"] = f"exec exit={resp.get('exit')} timed_out={resp.get('timed_out')}"
        else:
            rec.update(parse_workload(resp.get("stdout", "")))
    except Exception as e:  # noqa: BLE001 - demo driver, report any failure per-fork
        rec["error"] = str(e)
    finally:
        results[i] = rec


def _teardown(results):
    for rec in results:
        if not rec:
            continue
        proc = rec.pop("_proc", None)
        if proc and proc.poll() is None:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
        try:
            os.unlink(rec["uds"])
        except (FileNotFoundError, KeyError, TypeError):
            pass


def _render_table(results, wall_ms, v):
    def short(x):
        return (x[:6] + "…") if x and len(x) > 7 else (x or "-")
    print(f"{'fork':<5}{'restore_ms':<12}{'exec_ms':<9}"
          f"{'bootid(distinct)':<18}{'rand':<12}{'file_readback':<14}status")
    for r in sorted([r for r in results if r], key=lambda r: r["i"]):
        status = "ok" if not r["error"] else f"ERR {r['error']}"
        print(f"{r['i']:<5}{str(r['restore_ms'] or '-'):<12}"
              f"{str(r['exec_ms'] or '-'):<9}{short(r['bootid']):<18}"
              f"{short(r['rand']):<12}{short(r['file_readback']):<14}{status}")
    rms = sorted(r["restore_ms"] for r in results if r and r["restore_ms"] is not None)
    p = lambda q: rms[min(len(rms) - 1, int(q * len(rms)))] if rms else "-"
    print(f"\naggregate: {len(results)} forks, wall-clock {wall_ms} ms, "
          f"restore p50/p95 {p(0.5)}/{p(0.95)} ms")
    print(f"verdict: identities distinct={v['identities_distinct']}  "
          f"randoms distinct={v['randoms_distinct']}  "
          f"cow isolated={v['cow_isolated']}  => {'PASS' if v['ok'] else 'FAIL'}")
    print(f"fork cost: {p(0.5)} ms/clone restore, {wall_ms} ms wall-clock "
          f"for {len(results)} clones (cold boot is ~645 ms)")


def main():
    ap = argparse.ArgumentParser(description="Fan-out demo: fork N clones from a base snapshot.")
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    ap.add_argument("-n", "--count", type=int, default=8)
    ap.add_argument("--base", default="tools-base")
    ap.add_argument("--store", default=os.path.join(root, "mcp-store"))
    ap.add_argument("--boot", default=os.path.join(root, "target/debug/boot"))
    ap.add_argument("--kernel", default=os.path.join(root, "kimage/out/Image"))
    ap.add_argument("--rootfs", default=os.path.join(root, "kimage/out/rootfs-tools.ext4"))
    ap.add_argument("--mem", type=int, default=1024)
    ap.add_argument("--timeout", type=float, default=20.0, help="guest exec timeout (s)")
    ap.add_argument("--deadline", type=float, default=20.0, help="per-fork connect deadline (s)")
    ap.add_argument("--json", action="store_true")
    args = ap.parse_args()

    for label, path in (("boot", args.boot), ("kernel", args.kernel), ("rootfs", args.rootfs)):
        if not os.path.exists(path):
            print(f"missing {label}: {path}", file=sys.stderr)
            return 2
    if not os.path.exists(os.path.join(args.store, "snapshots", args.base)):
        print(f"snapshot '{args.base}' not found in {args.store}; "
              f"run scripts/make-tools-base.sh first", file=sys.stderr)
        return 2

    run_tok = str(os.getpid())
    results = [None] * args.count
    wall0 = time.monotonic()
    threads = [threading.Thread(target=fork_one, args=(i, args, run_tok, results))
               for i in range(args.count)]
    try:
        for t in threads:
            t.start()
        for t in threads:
            t.join()
        wall_ms = round((time.monotonic() - wall0) * 1000)
        v = verdict(results)
        if args.json:
            clean = [{k: r[k] for k in r if k not in ("_proc", "uds")} for r in results if r]
            print(json.dumps({"forks": clean, "wall_clock_ms": wall_ms, "verdict": v}, indent=2))
        else:
            _render_table(results, wall_ms, v)
        return 0 if v["ok"] else 1
    finally:
        _teardown(results)


if __name__ == "__main__":
    sys.exit(main())
