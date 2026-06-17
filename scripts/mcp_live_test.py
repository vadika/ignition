#!/usr/bin/env python3
"""Live MCP-server integration test (needs HVF + tools-base snapshot + signed boot).

Speaks JSON-RPC line protocol to the ignition-mcp stdio server, exercising the full
session lifecycle: open -> run -> filesystem persistence across runs -> write_file ->
reset clears state -> close. Exit 0 on success.
"""
import base64
import json
import os
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SERVER = os.path.join(ROOT, "target/debug/ignition-mcp")


class Client:
    def __init__(self, proc):
        self.proc = proc
        self.id = 0

    def call(self, method, params=None):
        self.id += 1
        msg = {"jsonrpc": "2.0", "id": self.id, "method": method, "params": params or {}}
        self.proc.stdin.write((json.dumps(msg) + "\n").encode())
        self.proc.stdin.flush()
        while True:
            line = self.proc.stdout.readline()
            if not line:
                raise RuntimeError("server closed")
            resp = json.loads(line)
            if resp.get("id") == self.id:
                if "error" in resp:
                    raise RuntimeError(resp["error"])
                return resp["result"]

    def notify(self, method, params=None):
        msg = {"jsonrpc": "2.0", "method": method, "params": params or {}}
        self.proc.stdin.write((json.dumps(msg) + "\n").encode())
        self.proc.stdin.flush()

    def tool(self, name, args=None):
        res = self.call("tools/call", {"name": name, "arguments": args or {}})
        # rmcp returns content blocks; pull the first text block.
        return res["content"][0]["text"]


def main():
    if not os.path.exists(SERVER):
        print(f"missing server binary: {SERVER}", file=sys.stderr)
        return 2
    proc = subprocess.Popen([SERVER], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                            stderr=subprocess.DEVNULL, env=dict(os.environ))
    try:
        c = Client(proc)
        c.call("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                              "clientInfo": {"name": "live", "version": "0"}})
        c.notify("notifications/initialized")

        sid = c.tool("open_session")  # returns the bare session-id string
        print("session:", sid)

        out = json.loads(c.tool("run", {"session_id": sid, "command": "python3 -c 'print(2+2)'"}))
        assert out["exit_code"] == 0 and out["stdout"].strip() == "4", out
        print("run python3 -> 4: ok")

        # Persistence: write a file in one run, read it in the next.
        c.tool("run", {"session_id": sid, "command": "echo persisted > /root/marker"})
        out = json.loads(c.tool("run", {"session_id": sid, "command": "cat /root/marker"}))
        assert out["stdout"].strip() == "persisted", out
        print("filesystem persists across runs: ok")

        # write_file tool (base64) then run it.
        script = base64.b64encode(b"print('from-file')\n").decode()
        c.tool("write_file", {"session_id": sid, "path": "/root/s.py", "content_base64": script})
        out = json.loads(c.tool("run", {"session_id": sid, "command": "python3 /root/s.py"}))
        assert out["stdout"].strip() == "from-file", out
        print("write_file + run: ok")

        # reset wipes state.
        c.tool("reset", {"session_id": sid})
        out = json.loads(c.tool("run", {"session_id": sid, "command": "cat /root/marker 2>&1 || true"}))
        assert "persisted" not in out["stdout"], out
        print("reset clears state: ok")

        c.tool("close", {"session_id": sid})
        print("PASS")
        return 0
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()


if __name__ == "__main__":
    sys.exit(main())
