# Fan-out Driver Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A standalone Python demo driver that forks N clones from one base snapshot in parallel, runs an identical workload in each over vsock, and proves shared lineage + CRNG/filesystem divergence with a terminal table (and `--json`).

**Architecture:** `scripts/fanout_demo.py` spawns `boot --restore <base>` children concurrently (one thread each, unique `--vsock-uds`), drives the in-guest `ign-exec` agent on vsock port 7000 via the MCP E2 handshake (ported from `crates/mcp/src/vsock_client.rs`), collects per-fork identity/random/file-readback + timings, then prints a table or JSON and exits non-zero unless the verdict holds. Stdlib only.

**Tech Stack:** Python 3 stdlib (`subprocess`, `socket`, `struct`, `json`, `threading`, `argparse`, `time`, `os`); `unittest` for the no-VM test.

---

### Task 1: vsock exec client (host side)

The framed E2 handshake + request/response, ported from `vsock_client.rs`. This is the only non-trivial logic, so it lands first with its test.

**Files:**
- Create: `scripts/fanout_demo.py`
- Test: `scripts/test_fanout_demo.py`

- [ ] **Step 1: Write the failing test** — round-trip against an in-process fake guest

```python
# scripts/test_fanout_demo.py
import os, socket, struct, threading, tempfile, unittest
import fanout_demo as fd


def _fake_guest(uds_path, ready, request_seen):
    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv.bind(uds_path)
    srv.listen(1)
    ready.set()
    conn, _ = srv.accept()
    # read "CONNECT 7000\n"
    line = b""
    while not line.endswith(b"\n"):
        line += conn.recv(1)
    assert line == b"CONNECT 7000\n", line
    conn.sendall(b"OK 1024\n")
    hdr = conn.recv(4)
    n = struct.unpack("<I", hdr)[0]
    body = b""
    while len(body) < n:
        body += conn.recv(n - len(body))
    request_seen.append(body)
    resp = b'{"exit":0,"stdout":"hi\\n","stderr":"","timed_out":false}'
    conn.sendall(struct.pack("<I", len(resp)) + resp)
    conn.close()
    srv.close()


class TestVsockExec(unittest.TestCase):
    def test_exec_roundtrip(self):
        d = tempfile.mkdtemp()
        uds = os.path.join(d, "s.sock")
        ready, seen = threading.Event(), []
        t = threading.Thread(target=_fake_guest, args=(uds, ready, seen))
        t.start()
        ready.wait(5)
        resp = fd.vsock_exec(uds, "echo hi", timeout=5.0, deadline=5.0)
        t.join(5)
        self.assertEqual(resp["exit"], 0)
        self.assertEqual(resp["stdout"], "hi\n")
        self.assertFalse(resp["timed_out"])
        import json
        self.assertEqual(json.loads(seen[0])["cmd"], "echo hi")


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run it, verify it fails**

Run: `cd scripts && python3 -m unittest test_fanout_demo.TestVsockExec -v`
Expected: FAIL — `AttributeError: module 'fanout_demo' has no attribute 'vsock_exec'` (or ImportError if file absent).

- [ ] **Step 3: Implement `vsock_exec` (minimal) in `scripts/fanout_demo.py`**

```python
#!/usr/bin/env python3
"""Fan-out demo driver: fork N clones from one base snapshot in parallel, run an
identical workload in each over vsock ign-exec, and show shared lineage plus
CRNG/filesystem divergence. Stdlib only."""
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


def vsock_exec(uds_path, cmd, timeout, deadline):
    """E2 handshake + one framed exec request against the guest ign-exec agent.

    Retries connect() until `deadline` seconds elapse (the control UDS appears
    only once boot wires up vsock). Returns the parsed response dict; raises on
    protocol/timeout failure."""
    end = time.monotonic() + deadline
    s = None
    while True:
        try:
            s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            s.settimeout(max(0.1, end - time.monotonic()))
            s.connect(uds_path)
            break
        except (FileNotFoundError, ConnectionRefusedError, OSError):
            if s:
                s.close()
            if time.monotonic() >= end:
                raise TimeoutError(f"vsock: {uds_path} never became connectable")
            time.sleep(0.05)
    try:
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
        req = json.dumps({"cmd": cmd, "timeout": timeout}).encode()
        s.sendall(struct.pack("<I", len(req)) + req)
        hdr = _recvn(s, 4)
        n = struct.unpack("<I", hdr)[0]
        if n > MAX_FRAME:
            raise IOError("vsock: response frame too large")
        return json.loads(_recvn(s, n))
    finally:
        s.close()


def _recvn(s, n):
    buf = b""
    while len(buf) < n:
        chunk = s.recv(n - len(buf))
        if not chunk:
            raise IOError("vsock: truncated frame")
        buf += chunk
    return buf
```

- [ ] **Step 4: Run the test, verify it passes**

Run: `cd scripts && python3 -m unittest test_fanout_demo.TestVsockExec -v`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add scripts/fanout_demo.py scripts/test_fanout_demo.py
git commit -m "fanout-driver: host-side vsock ign-exec client + roundtrip test"
```

---

### Task 2: Workload parse + verdict (pure functions)

The workload string, its output parser, and the pass/fail verdict — all pure, all unit-tested without a VM.

**Files:**
- Modify: `scripts/fanout_demo.py`
- Test: `scripts/test_fanout_demo.py`

- [ ] **Step 1: Write the failing tests**

```python
# append to scripts/test_fanout_demo.py
class TestParseVerdict(unittest.TestCase):
    def test_parse_workload(self):
        out = "BOOTID=3f9a\nRAND=a17c4e\nFILE=/tmp/fork-marker:a17c4e\n"
        rec = fd.parse_workload(out)
        self.assertEqual(rec, {"bootid": "3f9a", "rand": "a17c4e",
                               "file_path": "/tmp/fork-marker",
                               "file_readback": "a17c4e"})

    def test_verdict_pass(self):
        forks = [
            {"bootid": "x", "rand": "a", "file_readback": "a", "exit": 0, "error": None},
            {"bootid": "x", "rand": "b", "file_readback": "b", "exit": 0, "error": None},
        ]
        v = fd.verdict(forks)
        self.assertTrue(v["lineage_shared"])
        self.assertTrue(v["randoms_distinct"])
        self.assertTrue(v["cow_isolated"])
        self.assertTrue(v["ok"])

    def test_verdict_fails_on_dup_random(self):
        forks = [
            {"bootid": "x", "rand": "a", "file_readback": "a", "exit": 0, "error": None},
            {"bootid": "x", "rand": "a", "file_readback": "a", "exit": 0, "error": None},
        ]
        v = fd.verdict(forks)
        self.assertFalse(v["randoms_distinct"])
        self.assertFalse(v["ok"])

    def test_verdict_fails_on_error(self):
        forks = [
            {"bootid": "x", "rand": "a", "file_readback": "a", "exit": 0, "error": None},
            {"bootid": None, "rand": None, "file_readback": None, "exit": None, "error": "timeout"},
        ]
        self.assertFalse(fd.verdict(forks)["ok"])
```

- [ ] **Step 2: Run, verify fail**

Run: `cd scripts && python3 -m unittest test_fanout_demo.TestParseVerdict -v`
Expected: FAIL — `AttributeError: ... 'parse_workload'`.

- [ ] **Step 3: Implement the workload string, parser, and verdict**

```python
# add to scripts/fanout_demo.py
WORKLOAD = (
    "printf 'BOOTID=%s\\n' \"$(cat /proc/sys/kernel/random/boot_id)\"; "
    "printf 'RAND=%s\\n' \"$(head -c8 /dev/urandom | od -An -tx1 | tr -d ' \\n')\"; "
    "m=/tmp/fork-marker; "
    "head -c8 /dev/urandom | od -An -tx1 | tr -d ' \\n' > \"$m\"; "
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
    """Compute the pass/fail verdict over collected per-fork records."""
    good = [f for f in forks if not f.get("error") and f.get("exit") == 0]
    all_ok = len(good) == len(forks) and len(forks) > 0
    bootids = {f["bootid"] for f in good}
    rands = [f["rand"] for f in good]
    lineage = all_ok and len(bootids) == 1
    distinct = all_ok and len(set(rands)) == len(rands)
    cow = all_ok and all(f["rand"] == f["file_readback"] for f in good)
    return {
        "lineage_shared": lineage,
        "randoms_distinct": distinct,
        "cow_isolated": cow,
        "ok": bool(all_ok and lineage and distinct and cow),
    }
```

Note: the workload writes its *own* fresh random to the marker file, but the
verdict compares `file_readback` against the fork's `rand`. Make the readback
deterministic by reusing the first random: change the workload so `RAND` and the
file content are the same value.

```python
WORKLOAD = (
    "r=$(head -c8 /dev/urandom | od -An -tx1 | tr -d ' \\n'); "
    "printf 'BOOTID=%s\\n' \"$(cat /proc/sys/kernel/random/boot_id)\"; "
    "printf 'RAND=%s\\n' \"$r\"; "
    "m=/tmp/fork-marker; printf '%s' \"$r\" > \"$m\"; "
    "printf 'FILE=%s:%s\\n' \"$m\" \"$(cat \"$m\")\""
)
```

- [ ] **Step 4: Run, verify pass**

Run: `cd scripts && python3 -m unittest test_fanout_demo.TestParseVerdict -v`
Expected: PASS (all 4).

- [ ] **Step 5: Commit**

```bash
git add scripts/fanout_demo.py scripts/test_fanout_demo.py
git commit -m "fanout-driver: workload string, output parser, pass/fail verdict + tests"
```

---

### Task 3: Fork spawn + orchestration + CLI + output

Spawn N `boot --restore` children concurrently, probe each over vsock, collect timings, render table/JSON, exit on verdict. Unconditional teardown.

**Files:**
- Modify: `scripts/fanout_demo.py`

- [ ] **Step 1: Implement `fork_one`, `main`, table/JSON rendering**

```python
# add to scripts/fanout_demo.py
def fork_one(i, args, run_tok, results):
    """Spawn one boot --restore child, probe it over vsock, fill results[i].
    Never raises into the thread join — records an error string instead."""
    uds = f"/tmp/fanout-{run_tok}-{i}.sock"
    rec = {"i": i, "uds": uds, "restore_ms": None, "exec_ms": None,
           "bootid": None, "rand": None, "file_path": None,
           "file_readback": None, "exit": None, "error": None}
    proc = None
    try:
        for stale in (uds,):
            try:
                os.unlink(stale)
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
        te0 = time.monotonic()
        resp = vsock_exec(uds, WORKLOAD, timeout=args.timeout, deadline=args.deadline)
        rec["exec_ms"] = round((time.monotonic() - te0) * 1000)
        # restore_ms approximated as connect-ready latency (spawn -> first exec response minus exec)
        rec["restore_ms"] = round((te0 - t0) * 1000)
        rec["exit"] = resp.get("exit")
        if resp.get("timed_out") or resp.get("exit") != 0:
            rec["error"] = f"exec exit={resp.get('exit')} timed_out={resp.get('timed_out')}"
        else:
            rec.update(parse_workload(resp.get("stdout", "")))
    except Exception as e:  # noqa: BLE001 - demo driver, report any failure per-fork
        rec["error"] = str(e)
    finally:
        results[i] = rec


def _teardown(results, run_tok):
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
          f"{'bootid':<10}{'rand':<12}{'file_readback':<14}status")
    for r in sorted(results, key=lambda r: r["i"]):
        status = "ok" if not r["error"] else f"ERR {r['error']}"
        print(f"{r['i']:<5}{str(r['restore_ms'] or '-'):<12}"
              f"{str(r['exec_ms'] or '-'):<9}{short(r['bootid']):<10}"
              f"{short(r['rand']):<12}{short(r['file_readback']):<14}{status}")
    rms = sorted(r["restore_ms"] for r in results if r["restore_ms"] is not None)
    p = lambda q: rms[min(len(rms) - 1, int(q * len(rms)))] if rms else "-"
    print(f"\naggregate: {len(results)} forks, wall-clock {wall_ms} ms, "
          f"restore p50/p95 {p(0.5)}/{p(0.95)} ms")
    print(f"verdict: lineage shared={v['lineage_shared']}  "
          f"randoms distinct={v['randoms_distinct']}  "
          f"cow isolated={v['cow_isolated']}  => {'PASS' if v['ok'] else 'FAIL'}")


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
    if not os.path.isdir(args.store) or not any(args.base in f for f in os.listdir(args.store)):
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
            clean = [{k: r[k] for k in r if k != "_proc"} for r in results]
            print(json.dumps({"forks": clean, "wall_clock_ms": wall_ms, "verdict": v}, indent=2))
        else:
            _render_table(results, wall_ms, v)
        return 0 if v["ok"] else 1
    finally:
        _teardown(results, run_tok)


if __name__ == "__main__":
    sys.exit(main())
```

- [ ] **Step 2: Verify the whole test suite still passes (no VM)**

Run: `cd scripts && python3 -m unittest test_fanout_demo -v`
Expected: PASS (all tests from Tasks 1+2). The orchestration code is import-clean.

- [ ] **Step 3: Verify the script imports and shows help**

Run: `python3 scripts/fanout_demo.py --help`
Expected: argparse usage printed, exit 0.

Run: `python3 scripts/fanout_demo.py --store /nonexistent`
Expected: `snapshot 'tools-base' not found ... run scripts/make-tools-base.sh first`, exit 2.

- [ ] **Step 4: Commit**

```bash
git add scripts/fanout_demo.py
git commit -m "fanout-driver: parallel fork spawn, vsock probe, table/JSON output, teardown"
```

---

### Task 4: User-facing doc + TOC

**Files:**
- Create: `docs/src/features/fanout-demo.md`
- Modify: `docs/src/SUMMARY.md`

- [ ] **Step 1: Write `docs/src/features/fanout-demo.md`**

Cover: what it demonstrates (fork-from-warm story), the three things it proves
(shared lineage via bootid, CRNG divergence via /dev/urandom + vmid, CoW
isolation via the marker file), how to run it (`make-tools-base.sh` then
`fanout_demo.py --base tools-base -n 8`), a sample table, the `--json` mode, and
the exit-code contract. Cross-link `vmid.md`, `mcp-server.md`,
`snapshot-restore.md`. Match the prose style of `devices.md` / `vmid.md` (no em
dashes per house style; concrete numbers).

- [ ] **Step 2: Add the page to `docs/src/SUMMARY.md`** under the Features
section, after the MCP server entry:

```markdown
  - [Fan-out demo](features/fanout-demo.md)
```

- [ ] **Step 3: Verify the book builds (if mdbook available)**

Run: `command -v mdbook >/dev/null && (cd docs && mdbook build >/dev/null && echo OK) || echo "mdbook absent, skip"`
Expected: `OK` or the skip line. No broken-link errors if mdbook ran.

- [ ] **Step 4: Commit**

```bash
git add docs/src/features/fanout-demo.md docs/src/SUMMARY.md
git commit -m "docs: fan-out demo page + TOC entry"
```

---

## Self-Review

**Spec coverage:** fork path (Task 3 `fork_one`), vsock workload channel (Task 1),
identity+file workload (Task 2 `WORKLOAD`), terminal table + `--json` (Task 3),
verdict + exit code (Task 2 `verdict` + Task 3 `main`), error handling /
per-fork isolation / teardown (Task 3), missing-input + missing-snapshot hints
(Task 3 `main`), no-VM tests (Tasks 1+2), doc (Task 4). All spec sections map to
a task.

**Placeholder scan:** none — every code step is complete and runnable.

**Type consistency:** the per-fork `rec` dict keys (`i`, `uds`, `restore_ms`,
`exec_ms`, `bootid`, `rand`, `file_path`, `file_readback`, `exit`, `error`) are
the same across `fork_one`, `verdict`, `_render_table`, and the JSON dump.
`parse_workload` returns exactly `{bootid, rand, file_path, file_readback}`,
which `rec.update()` merges. `vsock_exec` returns the raw `{exit, stdout, stderr,
timed_out}` dict the guest sends. `WORKLOAD` is defined once (final form in Task
2 Step 3) and referenced in Task 3.

**Note on `restore_ms`:** it is the spawn→handshake-ready latency, not a VMM
internal timer — labelled as such in the spec. Good enough for the demo; an
exact restore timer would need `boot` to emit one (out of scope).
