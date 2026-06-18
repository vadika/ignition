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
