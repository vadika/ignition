# MCP server for agent sandboxes — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship `ignition-mcp`, a stdio MCP server that gives any MCP-capable agent a persistent sandboxed microVM session (open → run → write_file → reset → close), each session a clone of a warm Alpine+Python "tools base" snapshot.

**Architecture:** A new Rust workspace crate using the `rmcp` SDK serves five tools over stdio. A `SessionManager` spawns one `boot --restore` child per session, reachable over a per-session vsock UDS; commands run in the guest via a Python `ign-exec` agent behind `socat VSOCK-LISTEN:7000`. Sessions persist VM+filesystem state across calls; `reset` cold-relaunches a fresh clone.

**Tech Stack:** Rust (`rmcp`, `tokio`, `serde`), Python (guest `ign-exec`), Alpine rootfs build (overlay-root), virtio-vsock E2 host→guest control protocol.

**Spec:** `docs/superpowers/specs/2026-06-17-mcp-server-design.md`

**Cross-cutting reminders:**
- **No apostrophes** inside `sh -euxc '...'` blocks in the rootfs build scripts (breaks the command string).
- **Re-sign** `target/debug/boot` with `./scripts/sign.sh target/debug/boot` after any `cargo build` before a live run.
- Plain commit messages — no co-author/generated trailer.
- Docker is unavailable locally; rootfs builds run on the **artemis2** build host (`ssh artemis2`, Linux/arm64 docker). Rust builds run locally.
- vsock exec uses port **7000** (vmid uses 9000 — no collision).

---

## File Structure

- `kimage/build/ign-exec.py` — guest exec agent (framed JSON over stdin/stdout).
- `kimage/build/build-rootfs-tools.sh` — tools base rootfs (alpine + python + git + gcc, overlay-root, installs `ign-exec` + socat service).
- `scripts/make-tools-base.sh` — cold-boot the tools rootfs, snapshot as `tools-base`.
- `crates/mcp/Cargo.toml` — new workspace member `ignition-mcp`.
- `crates/mcp/src/main.rs` — rmcp stdio server bootstrap + tokio runtime.
- `crates/mcp/src/vsock_client.rs` — UDS connect + CONNECT/OK handshake + framed JSON exec.
- `crates/mcp/src/session.rs` — `SessionManager`, `Session`, `Spawner` trait, cap + idle-GC logic.
- `crates/mcp/src/tools.rs` — the five MCP tools wired to `SessionManager`.
- `scripts/mcp_live_test.py` — live HVF integration test.
- `Cargo.toml` (root) — add the member.
- `docs/src/features/mcp-server.md`, `docs/src/SUMMARY.md`, `ROADMAP.md` — docs.

---

### Task 1: Guest exec agent `ign-exec.py`

**Files:**
- Create: `kimage/build/ign-exec.py`
- Test: `kimage/build/test_ign_exec.py`

- [ ] **Step 1: Write the failing test**

Create `kimage/build/test_ign_exec.py`:

```python
import json, struct, subprocess, sys, os
HERE = os.path.dirname(os.path.abspath(__file__))
AGENT = os.path.join(HERE, "ign-exec.py")

def roundtrip(req: dict) -> dict:
    body = json.dumps(req).encode()
    frame = struct.pack("<I", len(body)) + body
    p = subprocess.run([sys.executable, AGENT], input=frame, capture_output=True)
    out = p.stdout
    n = struct.unpack("<I", out[:4])[0]
    return json.loads(out[4:4 + n])

def test_echo():
    r = roundtrip({"cmd": "echo hello"})
    assert r["exit"] == 0
    assert r["stdout"].strip() == "hello"
    assert r["timed_out"] is False

def test_exit_code_and_stderr():
    r = roundtrip({"cmd": "echo oops >&2; exit 3"})
    assert r["exit"] == 3
    assert r["stderr"].strip() == "oops"

def test_stdin_and_cwd():
    r = roundtrip({"cmd": "cat; pwd", "stdin": "piped\n", "cwd": "/tmp"})
    assert "piped" in r["stdout"]
    assert "/tmp" in r["stdout"]

def test_timeout():
    r = roundtrip({"cmd": "sleep 5", "timeout": 0.3})
    assert r["timed_out"] is True
    assert r["exit"] == 124

def test_bad_request():
    body = b"not json"
    frame = struct.pack("<I", len(body)) + body
    p = subprocess.run([sys.executable, AGENT], input=frame, capture_output=True)
    n = struct.unpack("<I", p.stdout[:4])[0]
    r = json.loads(p.stdout[4:4 + n])
    assert r["exit"] != 0
    assert "bad request" in r["stderr"]
```

- [ ] **Step 2: Run it, verify it fails**

Run: `python3 -m pytest kimage/build/test_ign_exec.py -q`
Expected: FAIL (agent file does not exist / import error).

- [ ] **Step 3: Implement `kimage/build/ign-exec.py`**

```python
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
    data = b""
    while len(data) < n:
        chunk = f.read(n - len(data))
        if not chunk:
            break
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
        p = subprocess.run(
            ["/bin/sh", "-c", cmd],
            input=(stdin.encode() if stdin else None),
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
```

- [ ] **Step 4: Run the tests, verify they pass**

Run: `python3 -m pytest kimage/build/test_ign_exec.py -q`
Expected: 5 passed.

- [ ] **Step 5: Commit**

```bash
git add kimage/build/ign-exec.py kimage/build/test_ign_exec.py
git commit -m "mcp: guest exec agent (framed JSON over vsock) + unit tests"
```

---

### Task 2: Tools base rootfs + warm-base maker

**Files:**
- Create: `kimage/build/build-rootfs-tools.sh`
- Create: `scripts/make-tools-base.sh`

- [ ] **Step 1: Write `kimage/build/build-rootfs-tools.sh`**

Modeled on `build-rootfs.sh` (plain) plus the overlay-init + ready-marker from
`build-rootfs-browser.sh`. NO apostrophes inside the `sh -euxc '...'` block.

```bash
#!/usr/bin/env bash
# Build the MCP "tools base" rootfs: alpine + python3 + git + a C toolchain, with
# the ign-exec agent behind a socat vsock listener, on an overlay root (immutable
# ext4 lower + tmpfs upper via /sbin/overlay-init). Output: ~/kbuild/out/rootfs-tools.ext4
set -euo pipefail

OUT="$HOME/kbuild/out"
STAGE="$HOME/kbuild"
mkdir -p "$OUT"
TAR="$STAGE/rootfs-tools.tar"

docker rm -f fcroot_tools_build >/dev/null 2>&1 || true
docker run --platform linux/arm64 --name fcroot_tools_build \
  -v "$(cd "$(dirname "$0")" && pwd)/devmem.c:/devmem.c:ro" \
  -v "$(cd "$(dirname "$0")" && pwd)/vmid-reseed.c:/vmid-reseed.c:ro" \
  -v "$(cd "$(dirname "$0")" && pwd)/ign-exec.py:/ign-exec.py:ro" \
  alpine:3.19 sh -euxc '
  apk add --no-cache openrc util-linux ifupdown-ng socat python3 py3-pip git gcc musl-dev coreutils

  # static helpers (devmem for the boot timer, vmid-reseed for the CRNG reseed)
  apk add --no-cache --virtual .build gcc musl-dev linux-headers
  gcc -O2 -static /devmem.c -o /usr/bin/devmem
  gcc -O2 -static /vmid-reseed.c -o /usr/bin/vmid-reseed
  apk del .build

  install -m 0755 /ign-exec.py /usr/bin/ign-exec

  ln -sf agetty /etc/init.d/agetty.ttyS0
  echo ttyS0 > /etc/securetty
  rc-update add agetty.ttyS0 default
  rc-update add devfs boot
  rc-update add procfs boot
  rc-update add sysfs boot
  passwd -d root || true

  grep -q "ln -sf /dev/ttyS0 /dev/tty" /etc/inittab ||
    printf "::sysinit:/bin/ln -sf /dev/ttyS0 /dev/tty\n" >> /etc/inittab

  mkdir -p /etc/network /etc/local.d
  printf "auto lo\niface lo inet loopback\n\nauto eth0\niface eth0 inet dhcp\n" > /etc/network/interfaces
  printf "#!/bin/sh\nifup -a\n" > /etc/local.d/network.start
  chmod +x /etc/local.d/network.start
  printf "#!/bin/sh\ndevmem 0x091FF000 8 123\n" > /etc/local.d/boottime.start
  chmod +x /etc/local.d/boottime.start

  # vmid CRNG reseed listener (host pushes a fresh seed on restore)
  printf "#!/bin/sh\nsocat VSOCK-LISTEN:9000,fork EXEC:/usr/bin/vmid-reseed &\n" > /etc/local.d/vmid.start
  chmod +x /etc/local.d/vmid.start

  # MCP exec listener: socat accepts a host connection on AF_VSOCK port 7000 and
  # pipes the framed request to ign-exec, which runs it and frames the response.
  printf "#!/bin/sh\nsocat VSOCK-LISTEN:7000,fork EXEC:/usr/bin/ign-exec &\n" > /etc/local.d/ign-exec.start
  chmod +x /etc/local.d/ign-exec.start

  # Readiness sentinel for make-tools-base.sh: print a marker once the exec
  # listener is up so the host can snapshot. socat binds quickly, so a short
  # settle is enough.
  cat > /etc/local.d/tools-ready.start <<'"'"'RDYEOF'"'"'
#!/bin/sh
( i=0
  while [ "$i" -lt 60 ]; do
    if pgrep -f "VSOCK-LISTEN:7000" >/dev/null 2>&1; then
      sleep 1
      echo TOOLS_READY > /dev/ttyS0
      exit 0
    fi
    sleep 1
    i=$((i + 1))
  done
  echo TOOLS_TIMEOUT > /dev/ttyS0 ) &
RDYEOF
  chmod +x /etc/local.d/tools-ready.start

  # overlay-root: tmpfs upper over the RO ext4 lower, then switch_root. Every guest
  # write lands in RAM so the disk never diverges.
  cat > /sbin/overlay-init <<'"'"'OVLEOF'"'"'
#!/bin/sh
mount -t proc proc /proc 2>/dev/null
mount -t sysfs sys /sys 2>/dev/null
mount -t tmpfs tmpfs /mnt
mkdir -p /mnt/up /mnt/work /mnt/root /mnt/lower
mount --bind / /mnt/lower
mount -o remount,ro /mnt/lower 2>/dev/null || true
mount -t overlay overlay -o lowerdir=/mnt/lower,upperdir=/mnt/up,workdir=/mnt/work /mnt/root
exec switch_root /mnt/root /sbin/init
OVLEOF
  chmod +x /sbin/overlay-init

  rc-update add local boot
'

docker export fcroot_tools_build -o "$TAR"
docker rm fcroot_tools_build >/dev/null

docker run --rm -v "$STAGE:/work" ubuntu:22.04 bash -euxc '
  export DEBIAN_FRONTEND=noninteractive
  apt-get update -qq
  apt-get install -y -qq --no-install-recommends e2fsprogs >/dev/null
  rm -rf /tmp/rootfs && mkdir -p /tmp/rootfs
  tar xf /work/rootfs-tools.tar -C /tmp/rootfs
  rm -f /tmp/rootfs/.dockerenv
  for d in dev proc run sys tmp mnt; do mkdir -p /tmp/rootfs/$d; done
  rm -f /work/out/rootfs-tools.ext4
  mke2fs -q -t ext4 -d /tmp/rootfs -L rootfs-tools /work/out/rootfs-tools.ext4 512M
  ls -la /work/out/rootfs-tools.ext4
'

rm -f "$TAR"
```

- [ ] **Step 2: Build it on artemis2 and verify contents**

Run:
```bash
rsync -az kimage/build/ artemis2:~/firecracker-mac/kimage/build/
ssh artemis2 'cd ~/firecracker-mac && bash kimage/build/build-rootfs-tools.sh' 2>&1 | tail -5
ssh artemis2 'docker run --rm -v "$HOME/kbuild/out:/o" ubuntu:22.04 bash -c \
  "apt-get update -qq>/dev/null 2>&1; apt-get install -y -qq e2fsprogs>/dev/null 2>&1; \
   debugfs -R \"stat /usr/bin/ign-exec\" /o/rootfs-tools.ext4 2>/dev/null | head -1; \
   debugfs -R \"cat /etc/local.d/ign-exec.start\" /o/rootfs-tools.ext4 2>/dev/null; \
   debugfs -R \"stat /usr/bin/python3\" /o/rootfs-tools.ext4 2>/dev/null | head -1"'
```
Expected: build ends with the `rootfs-tools.ext4` `ls -la` line; `ign-exec` and `python3` inodes present; `ign-exec.start` body contains `socat VSOCK-LISTEN:7000`.

- [ ] **Step 3: Write `scripts/make-tools-base.sh`**

Modeled on `make-browser-base.sh`, but no `--gui`/`--net`, with `--vsock-uds` so the
guest has a vsock device, overlay-init, and watching for `TOOLS_READY`.

```bash
#!/usr/bin/env bash
# Create the warm-base snapshot for the MCP server: cold-boot the tools rootfs
# (overlay root), wait for TOOLS_READY on the serial console, snapshot as
# "tools-base" via Ctrl-A s, then quit with Ctrl-A x. One-time step.
# usage: make-tools-base.sh [snapshot-name] [kernel] [rootfs] [store]
set -euo pipefail

NAME="${1:-tools-base}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
KERNEL="${2:-$ROOT/kimage/out/Image}"
ROOTFS="${3:-$ROOT/kimage/out/rootfs-tools.ext4}"
STORE="${4:-$ROOT/mcp-store}"
BOOT="$ROOT/target/debug/boot"

[ -x "$BOOT" ] || { echo "boot not built/signed: $BOOT" >&2; exit 1; }
[ -f "$KERNEL" ] || { echo "kernel not found: $KERNEL" >&2; exit 1; }
[ -f "$ROOTFS" ] || { echo "rootfs not found: $ROOTFS" >&2; exit 1; }

fifo="$(mktemp -u)"; mkfifo "$fifo"
cleanup() { rm -f "$fifo"; [ -n "${boot_pid:-}" ] && kill "$boot_pid" 2>/dev/null || true; }
trap cleanup EXIT INT TERM
exec 3<>"$fifo"

echo "cold-booting tools rootfs to create snapshot '$NAME' ..."
"$BOOT" --mem 1024 --vsock-uds /tmp/ign-toolsbase.sock --store "$STORE" \
  --name "$NAME" --force --append "ro init=/sbin/overlay-init" \
  "$KERNEL" "$ROOTFS" <"$fifo" 2>&1 | (
    while IFS= read -r line; do
      echo "$line"
      case "$line" in
        *TOOLS_TIMEOUT*)
          echo ">> guest never reported ready; aborting" >&2
          printf '\001x' >&3; exit 1 ;;
        *TOOLS_READY*)
          echo ">> guest ready; snapshotting as '$NAME'"
          printf '\001s' >&3 ;;
        *"[snapshot]"*written*)
          sleep 1; printf '\001x' >&3 ;;
      esac
    done
  ) &
boot_pid=$!
wait "$boot_pid"
echo "done. snapshot '$NAME' written to $STORE."
```

- [ ] **Step 4: Verify the base builds (live, on the Mac)**

Requires `target/debug/boot` built + signed and `kimage/out/rootfs-tools.ext4` present
(scp from artemis2: `scp artemis2:~/kbuild/out/rootfs-tools.ext4 kimage/out/`).
Run: `bash scripts/make-tools-base.sh`
Expected: prints `TOOLS_READY`, then `[snapshot] full 'tools-base' written ...`, exits cleanly; `mcp-store/snapshots/tools-base/` exists.

(If running headless/CI without HVF, this step is deferred to the live integration in Task 7; mark DONE_WITH_CONCERNS noting it.)

- [ ] **Step 5: Commit**

```bash
git add kimage/build/build-rootfs-tools.sh scripts/make-tools-base.sh
git commit -m "mcp: tools-base rootfs (alpine+python, overlay-root, ign-exec) + base maker"
```

---

### Task 3: `ignition-mcp` crate scaffold — compiling rmcp stdio server

This de-risks the SDK first: get an `rmcp` stdio server building and serving one
trivial tool before adding session logic.

**Files:**
- Create: `crates/mcp/Cargo.toml`, `crates/mcp/src/main.rs`
- Modify: `Cargo.toml` (root)

- [ ] **Step 1: Add the workspace member**

In root `Cargo.toml`, add `"crates/mcp",` to `members`.

- [ ] **Step 2: Create `crates/mcp/Cargo.toml`**

```toml
[package]
name = "ignition-mcp"
version = "0.0.0"
edition = "2024"
description = "MCP server exposing ignition microVM sandboxes to agents"
license = "Apache-2.0"

[[bin]]
name = "ignition-mcp"
path = "src/main.rs"

[dependencies]
rmcp = { version = "0.9", features = ["server", "transport-io", "macros"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread", "process", "time", "io-std", "sync"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
schemars = "1"
anyhow = "1"
log = "0.4"
env_logger = "0.11"

[dev-dependencies]
tempfile = "3"
```

NOTE on `rmcp`: pin to the latest published `0.x`. The macro/transport names below
follow the SDK's stdio-server example
(<https://github.com/modelcontextprotocol/rust-sdk/tree/main/examples/servers>).
If the pinned version's API differs, match THAT version's example for the
`#[tool]`/`#[tool_router]`/`#[tool_handler]` macros and the stdio transport
constructor — adapt the wiring, keep the tool method bodies as written here.

- [ ] **Step 3: Create `crates/mcp/src/main.rs` with one `ping` tool**

```rust
//! ignition-mcp: stdio MCP server exposing ignition microVM sandboxes.

use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::router::tool::ToolRouter,
    handler::server::wrapper::Parameters,
    model::{ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
    transport::stdio,
};

#[derive(Clone)]
struct Mcp {
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct PingRequest {
    #[schemars(description = "text to echo back")]
    text: String,
}

#[tool_router]
impl Mcp {
    fn new() -> Self {
        Self { tool_router: Self::tool_router() }
    }

    #[tool(description = "Health check; echoes the supplied text")]
    async fn ping(&self, Parameters(PingRequest { text }): Parameters<PingRequest>) -> String {
        format!("pong: {text}")
    }
}

#[tool_handler]
impl ServerHandler for Mcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            instructions: Some(
                "ignition microVM sandboxes: open_session, run, write_file, reset, close."
                    .into(),
            ),
            ..Default::default()
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();
    let service = Mcp::new().serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
```

- [ ] **Step 4: Build and smoke-test the stdio handshake**

Run: `cargo build -p ignition-mcp`
Expected: builds clean. (If rmcp macro names differ for the pinned version, fix the
wiring per its example until it builds.)

Smoke test the stdio protocol with an `initialize` + `tools/list` exchange:
```bash
printf '%s\n%s\n' \
 '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}' \
 '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
 | ./target/debug/ignition-mcp 2>/dev/null
```
Expected: two JSON-RPC responses; the second lists a `ping` tool.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml crates/mcp/Cargo.toml crates/mcp/src/main.rs
git commit -m "mcp: ignition-mcp crate scaffold — rmcp stdio server with ping tool"
```

---

### Task 4: vsock client (`vsock_client.rs`)

**Files:**
- Create: `crates/mcp/src/vsock_client.rs`
- Modify: `crates/mcp/src/main.rs` (add `mod vsock_client;`)

- [ ] **Step 1: Write the failing test**

Add to the bottom of `crates/mcp/src/vsock_client.rs` (write the test first, then the
impl above it):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::thread;

    // A fake guest: accept the UDS connection, read the "CONNECT 7000\n" line,
    // reply "OK 1024\n", read the framed request, reply a framed response.
    #[test]
    fn exec_roundtrip_against_fake_guest() {
        let dir = tempfile::tempdir().unwrap();
        let uds = dir.path().join("s.sock");
        let listener = UnixListener::bind(&uds).unwrap();
        let h = thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut line = Vec::new();
            let mut b = [0u8; 1];
            while s.read(&mut b).unwrap() == 1 {
                line.push(b[0]);
                if b[0] == b'\n' { break; }
            }
            assert_eq!(line, b"CONNECT 7000\n");
            s.write_all(b"OK 1024\n").unwrap();
            let mut lenb = [0u8; 4];
            s.read_exact(&mut lenb).unwrap();
            let n = u32::from_le_bytes(lenb) as usize;
            let mut buf = vec![0u8; n];
            s.read_exact(&mut buf).unwrap();
            let req: serde_json::Value = serde_json::from_slice(&buf).unwrap();
            assert_eq!(req["cmd"], "echo hi");
            let resp = br#"{"exit":0,"stdout":"hi\n","stderr":"","timed_out":false}"#;
            s.write_all(&(resp.len() as u32).to_le_bytes()).unwrap();
            s.write_all(resp).unwrap();
        });
        let req = ExecRequest { cmd: "echo hi".into(), stdin: None, cwd: None, timeout: Some(5.0) };
        let resp = exec(&uds, &req, std::time::Duration::from_secs(5)).unwrap();
        assert_eq!(resp.exit, 0);
        assert_eq!(resp.stdout, "hi\n");
        assert!(!resp.timed_out);
        h.join().unwrap();
    }
}
```

- [ ] **Step 2: Run it, verify it fails**

Run: `cargo test -p ignition-mcp vsock`
Expected: FAIL to compile (`exec`, `ExecRequest`, `ExecResponse` undefined).

- [ ] **Step 3: Implement the module (above the test)**

```rust
//! Host-side vsock client: drive the E2 host->guest control handshake and run a
//! framed exec request against the in-guest ign-exec agent.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The guest vsock port the ign-exec socat listener binds.
pub const EXEC_PORT: u32 = 7000;

#[derive(Debug, Serialize)]
pub struct ExecRequest {
    pub cmd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<f64>,
}

#[derive(Debug, Deserialize, PartialEq)]
pub struct ExecResponse {
    pub exit: i32,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

fn io_err(msg: impl Into<String>) -> std::io::Error {
    std::io::Error::other(msg.into())
}

/// Connect to a session control UDS, perform `CONNECT <EXEC_PORT>` / `OK`, send a
/// length-prefixed JSON request, and read the length-prefixed JSON response.
pub fn exec(uds: &Path, req: &ExecRequest, op_timeout: Duration) -> std::io::Result<ExecResponse> {
    let mut s = UnixStream::connect(uds)?;
    s.set_read_timeout(Some(op_timeout))?;
    s.set_write_timeout(Some(op_timeout))?;

    s.write_all(format!("CONNECT {EXEC_PORT}\n").as_bytes())?;

    // Read the "OK <host_port>\n" ack one byte at a time (it precedes the binary frame).
    let mut line = Vec::new();
    let mut b = [0u8; 1];
    loop {
        let n = s.read(&mut b)?;
        if n == 0 {
            return Err(io_err("vsock: connection closed before OK"));
        }
        line.push(b[0]);
        if b[0] == b'\n' {
            break;
        }
        if line.len() > 128 {
            return Err(io_err("vsock: oversized ack line"));
        }
    }
    if !line.starts_with(b"OK ") {
        return Err(io_err(format!("vsock: expected OK, got {:?}", String::from_utf8_lossy(&line))));
    }

    let body = serde_json::to_vec(req)?;
    s.write_all(&(body.len() as u32).to_le_bytes())?;
    s.write_all(&body)?;

    let mut lenb = [0u8; 4];
    s.read_exact(&mut lenb)?;
    let n = u32::from_le_bytes(lenb) as usize;
    if n > 64 * 1024 * 1024 {
        return Err(io_err("vsock: response frame too large"));
    }
    let mut buf = vec![0u8; n];
    s.read_exact(&mut buf)?;
    let resp: ExecResponse = serde_json::from_slice(&buf)?;
    Ok(resp)
}
```

Add `mod vsock_client;` near the top of `crates/mcp/src/main.rs`.

- [ ] **Step 4: Run the test, verify it passes**

Run: `cargo test -p ignition-mcp vsock`
Expected: 1 passed.

- [ ] **Step 5: Commit**

```bash
git add crates/mcp/src/vsock_client.rs crates/mcp/src/main.rs
git commit -m "mcp: vsock client — CONNECT/OK handshake + framed JSON exec"
```

---

### Task 5: SessionManager (`session.rs`)

**Files:**
- Create: `crates/mcp/src/session.rs`
- Modify: `crates/mcp/src/main.rs` (add `mod session;`)

- [ ] **Step 1: Write the failing tests**

Put these tests at the bottom of `crates/mcp/src/session.rs`; implement above them.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn cap_blocks_new_sessions() {
        let mut mgr = SessionManager::new(SessionConfig { max_sessions: 2, ..test_cfg() });
        assert!(mgr.open(&FakeSpawner).is_ok());
        assert!(mgr.open(&FakeSpawner).is_ok());
        assert!(matches!(mgr.open(&FakeSpawner), Err(SessionError::CapReached(2))));
    }

    #[test]
    fn elapsed_idle_predicate() {
        let t0 = std::time::Instant::now();
        let idle = Duration::from_secs(600);
        assert!(elapsed_idle(t0 + Duration::from_secs(700), t0, idle));   // 700s > 600s
        assert!(!elapsed_idle(t0 + Duration::from_secs(50), t0, idle));   // 50s <= 600s
    }

    #[test]
    fn close_removes_and_frees_slot() {
        let mut mgr = SessionManager::new(SessionConfig { max_sessions: 1, ..test_cfg() });
        let sid = mgr.open(&FakeSpawner).unwrap();
        assert!(matches!(mgr.open(&FakeSpawner), Err(SessionError::CapReached(1))));
        mgr.close(&sid).unwrap();
        assert!(mgr.open(&FakeSpawner).is_ok());
    }
}
```

(`test_cfg`, `FakeSpawner`, `elapsed_idle` are defined in the implementation below so
the tests compile.)

- [ ] **Step 2: Run, verify fail**

Run: `cargo test -p ignition-mcp session`
Expected: FAIL to compile (types undefined).

- [ ] **Step 3: Implement `crates/mcp/src/session.rs`**

```rust
//! Session table: one live `boot --restore` child per session, keyed by id.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Child;
use std::time::{Duration, Instant};

#[derive(Clone)]
pub struct SessionConfig {
    pub boot_bin: PathBuf,
    pub kernel: PathBuf,
    pub rootfs: PathBuf,
    pub store: PathBuf,
    pub base: String,
    pub uds_dir: PathBuf,
    pub max_sessions: usize,
    pub idle: Duration,
    pub net: bool,
}

#[derive(Debug)]
pub enum SessionError {
    CapReached(usize),
    Unknown(String),
    Spawn(String),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::CapReached(n) => write!(f, "session cap reached ({n})"),
            SessionError::Unknown(id) => write!(f, "unknown session: {id}"),
            SessionError::Spawn(e) => write!(f, "failed to start sandbox: {e}"),
        }
    }
}

/// Spawns a `boot --restore` child for a session. Abstracted so tests can fake it.
pub trait Spawner {
    fn spawn(&self, cfg: &SessionConfig, uds: &PathBuf) -> Result<Child, String>;
}

/// Real spawner: `boot --restore <base> --store <store> --vsock-uds <uds> [--net] <kernel> <rootfs>`.
pub struct BootSpawner;

impl Spawner for BootSpawner {
    fn spawn(&self, cfg: &SessionConfig, uds: &PathBuf) -> Result<Child, String> {
        let mut c = std::process::Command::new(&cfg.boot_bin);
        c.arg("--mem").arg("1024")
            .arg("--restore").arg(&cfg.base)
            .arg("--store").arg(&cfg.store)
            .arg("--vsock-uds").arg(uds);
        if cfg.net {
            c.arg("--net");
        }
        c.arg(&cfg.kernel).arg(&cfg.rootfs);
        c.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        c.spawn().map_err(|e| e.to_string())
    }
}

pub struct Session {
    pub uds: PathBuf,
    pub child: Child,
    pub last_used: Instant,
}

pub struct SessionManager {
    cfg: SessionConfig,
    sessions: HashMap<String, Session>,
    next: u64,
}

/// Pure predicate used by the reaper (and unit-tested): has `last` aged past `idle`
/// relative to `now`?
pub fn elapsed_idle(now: Instant, last: Instant, idle: Duration) -> bool {
    now.duration_since(last) > idle
}

impl SessionManager {
    pub fn new(cfg: SessionConfig) -> Self {
        Self { cfg, sessions: HashMap::new(), next: 0 }
    }

    pub fn config(&self) -> &SessionConfig {
        &self.cfg
    }

    pub fn get_uds(&mut self, id: &str) -> Result<PathBuf, SessionError> {
        let s = self.sessions.get_mut(id).ok_or_else(|| SessionError::Unknown(id.to_string()))?;
        s.last_used = Instant::now();
        Ok(s.uds.clone())
    }

    pub fn open(&mut self, spawner: &dyn Spawner) -> Result<String, SessionError> {
        if self.sessions.len() >= self.cfg.max_sessions {
            return Err(SessionError::CapReached(self.cfg.max_sessions));
        }
        let id = format!("s{}", self.next);
        self.next += 1;
        let uds = self.cfg.uds_dir.join(format!("ign-mcp-{id}.sock"));
        let child = spawner.spawn(&self.cfg, &uds).map_err(SessionError::Spawn)?;
        self.sessions.insert(id.clone(), Session { uds, child, last_used: Instant::now() });
        Ok(id)
    }

    pub fn close(&mut self, id: &str) -> Result<(), SessionError> {
        let mut s = self.sessions.remove(id).ok_or_else(|| SessionError::Unknown(id.to_string()))?;
        let _ = s.child.kill();
        let _ = s.child.wait();
        let _ = std::fs::remove_file(&s.uds);
        Ok(())
    }

    /// Kill the current child and spawn a fresh clone under the same id (cold reset).
    pub fn reset(&mut self, id: &str, spawner: &dyn Spawner) -> Result<(), SessionError> {
        let uds = {
            let s = self.sessions.get_mut(id).ok_or_else(|| SessionError::Unknown(id.to_string()))?;
            let _ = s.child.kill();
            let _ = s.child.wait();
            s.uds.clone()
        };
        let child = spawner.spawn(&self.cfg, &uds).map_err(SessionError::Spawn)?;
        let s = self.sessions.get_mut(id).unwrap();
        s.child = child;
        s.last_used = Instant::now();
        Ok(())
    }

    /// Close every session whose idle time exceeds the configured idle window.
    pub fn reap_idle(&mut self) {
        let now = Instant::now();
        let stale: Vec<String> = self.sessions.iter()
            .filter(|(_, s)| elapsed_idle(now, s.last_used, self.cfg.idle))
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            let _ = self.close(&id);
        }
    }

    pub fn shutdown(&mut self) {
        let ids: Vec<String> = self.sessions.keys().cloned().collect();
        for id in ids {
            let _ = self.close(&id);
        }
    }
}

#[cfg(test)]
fn test_cfg() -> SessionConfig {
    SessionConfig {
        boot_bin: "/bin/false".into(),
        kernel: "/x/Image".into(),
        rootfs: "/x/rootfs.ext4".into(),
        store: "/x/store".into(),
        base: "tools-base".into(),
        uds_dir: std::env::temp_dir(),
        max_sessions: 8,
        idle: Duration::from_secs(600),
        net: false,
    }
}

#[cfg(test)]
struct FakeSpawner;

#[cfg(test)]
impl Spawner for FakeSpawner {
    fn spawn(&self, _cfg: &SessionConfig, _uds: &PathBuf) -> Result<Child, String> {
        // A trivially-spawnable, immediately-exiting child stands in for boot.
        std::process::Command::new("/bin/sh").arg("-c").arg("exit 0")
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
            .spawn().map_err(|e| e.to_string())
    }
}
```

Add `mod session;` near the top of `crates/mcp/src/main.rs`.

- [ ] **Step 4: Run the tests, verify they pass**

Run: `cargo test -p ignition-mcp session`
Expected: 3 passed.

- [ ] **Step 5: Commit**

```bash
git add crates/mcp/src/session.rs crates/mcp/src/main.rs
git commit -m "mcp: SessionManager — spawn/cap/reset/idle-GC over a Spawner trait"
```

---

### Task 6: Wire the five tools (`tools.rs`)

**Files:**
- Create: `crates/mcp/src/tools.rs`
- Modify: `crates/mcp/src/main.rs` (use the real tools instead of `ping`; shared state)

- [ ] **Step 1: Implement `crates/mcp/src/tools.rs`**

The `Mcp` struct holds the `SessionManager` behind a `tokio::sync::Mutex` (tools are
async; the blocking vsock/spawn calls run on `spawn_blocking`). The boot probe after
open/reset polls `vsock_client::exec` with a `{"cmd":":"}` no-op until it answers.

```rust
//! The five MCP tools, wired to the SessionManager and the vsock exec client.

use std::sync::Arc;
use std::time::{Duration, Instant};

use rmcp::{
    ErrorData, handler::server::router::tool::ToolRouter,
    handler::server::wrapper::Parameters, model::*, tool, tool_handler, tool_router,
};
use tokio::sync::Mutex;

use crate::session::{BootSpawner, SessionConfig, SessionError, SessionManager};
use crate::vsock_client::{self, ExecRequest};

#[derive(Clone)]
pub struct Mcp {
    mgr: Arc<Mutex<SessionManager>>,
    pub tool_router: ToolRouter<Self>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SessionId {
    #[schemars(description = "session id from open_session")]
    pub session_id: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RunArgs {
    pub session_id: String,
    #[schemars(description = "shell command run via sh -c in the sandbox")]
    pub command: String,
    #[schemars(description = "seconds before the command is killed (default 30)")]
    pub timeout_s: Option<f64>,
    pub cwd: Option<String>,
    pub stdin: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct WriteFileArgs {
    pub session_id: String,
    #[schemars(description = "absolute path inside the sandbox")]
    pub path: String,
    #[schemars(description = "file contents, base64-encoded")]
    pub content_base64: String,
}

fn mcp_err(e: impl std::fmt::Display) -> ErrorData {
    ErrorData::internal_error(e.to_string(), None)
}

impl Mcp {
    pub fn new(cfg: SessionConfig) -> Self {
        Self {
            mgr: Arc::new(Mutex::new(SessionManager::new(cfg))),
            tool_router: Self::tool_router(),
        }
    }

    pub fn manager(&self) -> Arc<Mutex<SessionManager>> {
        self.mgr.clone()
    }

    // Poll the guest exec agent until it answers a no-op, or time out.
    async fn wait_ready(&self, uds: std::path::PathBuf) -> Result<(), ErrorData> {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let u = uds.clone();
            let probe = tokio::task::spawn_blocking(move || {
                vsock_client::exec(
                    &u,
                    &ExecRequest { cmd: ":".into(), stdin: None, cwd: None, timeout: Some(5.0) },
                    Duration::from_millis(500),
                )
            })
            .await
            .map_err(mcp_err)?;
            if let Ok(r) = probe {
                if r.exit == 0 {
                    return Ok(());
                }
            }
            if Instant::now() > deadline {
                return Err(mcp_err("sandbox exec agent did not become ready"));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

#[tool_router]
impl Mcp {
    #[tool(description = "Open a sandbox session (a fresh microVM clone). Returns a session_id.")]
    async fn open_session(&self) -> Result<String, ErrorData> {
        let uds = {
            let mut mgr = self.mgr.lock().await;
            let id = mgr.open(&BootSpawner).map_err(mcp_err)?;
            let uds = mgr.get_uds(&id).map_err(mcp_err)?;
            (id, uds)
        };
        let (id, uds) = uds;
        if let Err(e) = self.wait_ready(uds).await {
            let _ = self.mgr.lock().await.close(&id);
            return Err(e);
        }
        Ok(id)
    }

    #[tool(description = "Run a shell command in a session. Returns stdout, stderr, exit_code, timed_out as JSON.")]
    async fn run(&self, Parameters(a): Parameters<RunArgs>) -> Result<String, ErrorData> {
        let uds = self.mgr.lock().await.get_uds(&a.session_id).map_err(mcp_err)?;
        let req = ExecRequest {
            cmd: a.command,
            stdin: a.stdin,
            cwd: a.cwd,
            timeout: Some(a.timeout_s.unwrap_or(30.0)),
        };
        let op = Duration::from_secs((a.timeout_s.unwrap_or(30.0) as u64) + 10);
        let resp = tokio::task::spawn_blocking(move || vsock_client::exec(&uds, &req, op))
            .await
            .map_err(mcp_err)?
            .map_err(mcp_err)?;
        Ok(serde_json::to_string(&serde_json::json!({
            "stdout": resp.stdout, "stderr": resp.stderr,
            "exit_code": resp.exit, "timed_out": resp.timed_out,
        })).unwrap())
    }

    #[tool(description = "Write a base64-encoded file into the session at an absolute path.")]
    async fn write_file(&self, Parameters(a): Parameters<WriteFileArgs>) -> Result<String, ErrorData> {
        let uds = self.mgr.lock().await.get_uds(&a.session_id).map_err(mcp_err)?;
        // Decode on the guest: pipe the base64 over stdin into `base64 -d > path`.
        let cmd = format!("base64 -d > {}", shell_quote(&a.path));
        let req = ExecRequest {
            cmd,
            stdin: Some(a.content_base64),
            cwd: None,
            timeout: Some(30.0),
        };
        let resp = tokio::task::spawn_blocking(move || {
            vsock_client::exec(&uds, &req, Duration::from_secs(40))
        })
        .await
        .map_err(mcp_err)?
        .map_err(mcp_err)?;
        if resp.exit != 0 {
            return Err(mcp_err(format!("write_file failed: {}", resp.stderr)));
        }
        Ok("ok".into())
    }

    #[tool(description = "Reset a session: discard its state and roll back to the warm base.")]
    async fn reset(&self, Parameters(a): Parameters<SessionId>) -> Result<String, ErrorData> {
        let uds = {
            let mut mgr = self.mgr.lock().await;
            mgr.reset(&a.session_id, &BootSpawner).map_err(mcp_err)?;
            mgr.get_uds(&a.session_id).map_err(mcp_err)?
        };
        self.wait_ready(uds).await?;
        Ok("ok".into())
    }

    #[tool(description = "Close a session and discard its microVM.")]
    async fn close(&self, Parameters(a): Parameters<SessionId>) -> Result<String, ErrorData> {
        self.mgr.lock().await.close(&a.session_id).map_err(mcp_err)?;
        Ok("ok".into())
    }
}

/// Minimal single-quote shell escaping for a path argument.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[tool_handler]
impl rmcp::ServerHandler for Mcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            instructions: Some(
                "ignition microVM sandboxes. open_session -> run/write_file -> reset/close.".into(),
            ),
            ..Default::default()
        }
    }
}
```

NOTE: `ErrorData::internal_error`, `ServerInfo`, and the macro names follow the
pinned `rmcp` example; adapt to the published version if they differ. Add the
`base64` crate only if you prefer host-side decode — this plan decodes in the guest
via `base64 -d`, which Alpine coreutils provides, so no new Rust dep.

- [ ] **Step 2: Rewrite `crates/mcp/src/main.rs` to mount the real server**

```rust
//! ignition-mcp: stdio MCP server exposing ignition microVM sandboxes.

mod session;
mod tools;
mod vsock_client;

use std::path::PathBuf;
use std::time::Duration;

use rmcp::{ServiceExt, transport::stdio};

use session::SessionConfig;
use tools::Mcp;

fn env_path(key: &str, default: &str) -> PathBuf {
    std::env::var(key).map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(default))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();
    let cfg = SessionConfig {
        boot_bin: env_path("IGN_MCP_BOOT", "target/debug/boot"),
        kernel: env_path("IGN_MCP_KERNEL", "kimage/out/Image"),
        rootfs: env_path("IGN_MCP_ROOTFS", "kimage/out/rootfs-tools.ext4"),
        store: env_path("IGN_MCP_STORE", "mcp-store"),
        base: std::env::var("IGN_MCP_BASE").unwrap_or_else(|_| "tools-base".into()),
        uds_dir: std::env::temp_dir(),
        max_sessions: std::env::var("IGN_MCP_MAX_SESSIONS").ok()
            .and_then(|v| v.parse().ok()).unwrap_or(8),
        idle: Duration::from_secs(std::env::var("IGN_MCP_IDLE_SECS").ok()
            .and_then(|v| v.parse().ok()).unwrap_or(600)),
        net: std::env::var("IGN_MCP_NET").is_ok(),
    };

    let server = Mcp::new(cfg);

    // Idle reaper.
    let mgr = server.manager();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            mgr.lock().await.reap_idle();
        }
    });

    let service = server.clone().serve(stdio()).await?;
    service.waiting().await?;
    server.manager().lock().await.shutdown();
    Ok(())
}
```

- [ ] **Step 3: Build + existing unit tests still pass**

Run: `cargo build -p ignition-mcp && cargo test -p ignition-mcp`
Expected: builds clean; `vsock` (1) + `session` (3) tests pass. Fix any rmcp wiring
mismatch against the pinned version until green.

- [ ] **Step 4: Stdio smoke test — tools are listed**

Run:
```bash
printf '%s\n%s\n' \
 '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}' \
 '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
 | ./target/debug/ignition-mcp 2>/dev/null
```
Expected: the `tools/list` response lists `open_session`, `run`, `write_file`, `reset`, `close`.

- [ ] **Step 5: Commit**

```bash
git add crates/mcp/src/tools.rs crates/mcp/src/main.rs
git commit -m "mcp: wire the five session tools (open/run/write_file/reset/close)"
```

---

### Task 7: Live integration test (HUMAN / live HVF)

**Files:**
- Create: `scripts/mcp_live_test.py`

This needs HVF + the `tools-base` snapshot built (Task 2 step 4) and a signed `boot`.
Run by the human, like the prior live checks. Stop and hand back after Task 6.

- [ ] **Step 1: Write `scripts/mcp_live_test.py`**

Drives `ignition-mcp` over stdio with raw JSON-RPC: initialize → open_session →
run `python3 -c 'print(2+2)'` (assert `4`) → write_file a script + run it → assert a
file written in one run survives into the next run (persistence) → reset → assert the
file is gone → close.

```python
#!/usr/bin/env python3
"""Live MCP-server integration test (needs HVF + tools-base snapshot + signed boot).

Speaks JSON-RPC line protocol to the ignition-mcp stdio server, exercising the full
session lifecycle. Exit 0 on success.
"""
import base64, json, os, subprocess, sys

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

    def tool(self, name, args=None):
        res = self.call("tools/call", {"name": name, "arguments": args or {}})
        # rmcp returns content blocks; pull the first text block.
        return res["content"][0]["text"]


def main():
    env = dict(os.environ)
    proc = subprocess.Popen([SERVER], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                            stderr=subprocess.DEVNULL, env=env)
    try:
        c = Client(proc)
        c.call("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                              "clientInfo": {"name": "live", "version": "0"}})
        sid = c.tool("open_session")  # returns the bare session-id string
        print("session:", sid)

        out = json.loads(c.tool("run", {"session_id": sid, "command": "python3 -c 'print(2+2)'"}))
        assert out["exit_code"] == 0 and out["stdout"].strip() == "4", out

        # Persistence: write a file in one run, read it in the next.
        c.tool("run", {"session_id": sid, "command": "echo persisted > /root/marker"})
        out = json.loads(c.tool("run", {"session_id": sid, "command": "cat /root/marker"}))
        assert out["stdout"].strip() == "persisted", out

        # write_file tool (base64) then run it.
        script = base64.b64encode(b"print('from-file')\n").decode()
        c.tool("write_file", {"session_id": sid, "path": "/root/s.py", "content_base64": script})
        out = json.loads(c.tool("run", {"session_id": sid, "command": "python3 /root/s.py"}))
        assert out["stdout"].strip() == "from-file", out

        # reset wipes state.
        c.tool("reset", {"session_id": sid})
        out = json.loads(c.tool("run", {"session_id": sid, "command": "cat /root/marker 2>&1 || true"}))
        assert "persisted" not in out["stdout"], out

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
```

(Remove the `json.loads_or` placeholder line — it is a no-op `if False` guard kept
only so the reader sees `open_session` returns the bare id string; the implementer
should write `sid = c.tool("open_session")`.)

- [ ] **Step 2: Run it (human, live)**

Prereqs: `cargo build -p ignition-mcp` (server) + `cargo build -p ignition-spike --bin boot` then `./scripts/sign.sh target/debug/boot`; `kimage/out/rootfs-tools.ext4` present; `bash scripts/make-tools-base.sh` has created `mcp-store/snapshots/tools-base`.
Run: `python3 scripts/mcp_live_test.py`
Expected: prints a session id, then `PASS`.

- [ ] **Step 3: Commit**

```bash
git add scripts/mcp_live_test.py
git commit -m "mcp: live HVF integration test (session lifecycle + persistence + reset)"
```

---

### Task 8: Docs + ROADMAP

**Files:**
- Create: `docs/src/features/mcp-server.md`
- Modify: `docs/src/SUMMARY.md`, `ROADMAP.md`

- [ ] **Step 1: Write `docs/src/features/mcp-server.md`**

A feature page covering: what it is (MCP stdio server, sandbox-per-session),
the five tools, the persistent-session semantics (VM+filesystem persists; each run is
a fresh `sh -c`), how to point an MCP client at it (`command: ignition-mcp`, the env
config), the no-net default + `IGN_MCP_NET` opt-in, and that vmid reseeds each
session. Link to `snapshot-restore.md`, `vmid.md`, `sandbox.md`. Match the prose
style of `docs/src/features/vmid.md`.

- [ ] **Step 2: Add to `docs/src/SUMMARY.md`**

Under the adoption/features area, add: `- [MCP server for agents](features/mcp-server.md)`.

- [ ] **Step 3: Flip the ROADMAP item**

In `ROADMAP.md`, change the `- [ ] **MCP server for agent sandboxes**` line to `[x]`
(or `[~]` if the live test is still pending), append the verification status, and add
`docs/src/features/mcp-server.md` to its links.

- [ ] **Step 4: Verify links + commit**

If `mdbook` is available, `cd docs && mdbook build` (linkcheck must pass); otherwise
rely on CI. Commit:
```bash
git add docs/src/features/mcp-server.md docs/src/SUMMARY.md ROADMAP.md
git commit -m "docs: MCP server feature page + SUMMARY/ROADMAP"
```

---

## Notes for the executor

- **rmcp is the one external unknown.** Tasks 3 and 6 carry the SDK wiring; if the
  pinned version's macros/types differ from the snippets, adapt the wiring to that
  version's `examples/servers` and keep the tool method bodies. Get Task 3 compiling
  before proceeding — it is the de-risk gate.
- **Docker is remote** (artemis2); **HVF is local** (the Mac). Rust unit tests
  (Tasks 4–6) run locally with no VM. The rootfs build (Task 2) runs on artemis2; the
  base snapshot (Task 2 step 4) and live test (Task 7) need the Mac + a signed boot.
- **Re-sign boot** after any rebuild before the live steps.
