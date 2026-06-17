#!/usr/bin/env python3
"""Guest exec agent for the ignition MCP server.

Launched per connection by `socat VSOCK-LISTEN:7000,fork EXEC:/usr/bin/ign-exec`.
Reads one length-prefixed JSON request from stdin, runs it under `sh -c`, writes
one length-prefixed JSON response to stdout.

Request:  4-byte LE length + {"cmd": str, "stdin": str|null, "cwd": str|null,
          "timeout": number|null}
Response: 4-byte LE length + {"exit": int, "stdout": str, "stderr": str,
          "timed_out": bool}
"""
import json
import struct
import subprocess
import sys


def read_frame(f):
    hdr = f.read(4)
    if len(hdr) < 4:
        return None
    n = struct.unpack("<I", hdr)[0]
    if n > 64 * 1024 * 1024:
        return None  # implausibly large frame; reject
    data = b""
    while len(data) < n:
        chunk = f.read(n - len(data))
        if not chunk:
            return None  # truncated: not a complete frame
        data += chunk
    return data


def write_frame(f, obj):
    body = json.dumps(obj).encode()
    f.write(struct.pack("<I", len(body)))
    f.write(body)
    f.flush()


def main():
    fin, fout = sys.stdin.buffer, sys.stdout.buffer
    raw = read_frame(fin)
    if raw is None:
        return
    try:
        req = json.loads(raw)
        cmd = req["cmd"]
        stdin = req.get("stdin")
        cwd = req.get("cwd")
        timeout = req.get("timeout")
    except Exception as e:
        write_frame(fout, {"exit": 1, "stdout": "",
                           "stderr": f"ign-exec: bad request: {e}", "timed_out": False})
        return
    try:
        # start_new_session isolates the command in its own process group. Note:
        # subprocess.run only kills the direct child on timeout, not the whole
        # group, so backgrounded grandchildren may outlive a timeout. Acceptable
        # for the MVP (trusted local peer; the VM is discarded on close/reset).
        p = subprocess.run(
            ["/bin/sh", "-c", cmd],
            input=(stdin.encode() if stdin is not None else None),
            cwd=cwd, capture_output=True, timeout=timeout, start_new_session=True)
        write_frame(fout, {"exit": p.returncode,
                           "stdout": p.stdout.decode("utf-8", "replace"),
                           "stderr": p.stderr.decode("utf-8", "replace"),
                           "timed_out": False})
    except subprocess.TimeoutExpired as e:
        out = (e.stdout or b"").decode("utf-8", "replace")
        err = (e.stderr or b"").decode("utf-8", "replace")
        write_frame(fout, {"exit": 124, "stdout": out, "stderr": err, "timed_out": True})


if __name__ == "__main__":
    main()
