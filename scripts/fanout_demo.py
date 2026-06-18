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
