# MCP server for agent sandboxes — design

_Design spec. Status: approved, awaiting implementation plan._

## Goal

Expose ignition's clone-from-warm primitive to any MCP-capable agent (Claude Code,
Codex, Gemini CLI) as a sandboxed code-execution tool. An agent opens a session —
a microVM cloned from a warm snapshot — runs commands in it across a conversation,
and closes it. Adoption cost for the agent is ~zero: it speaks standard MCP, no
ignition-specific code.

This is the adoption-track lead item. The clone primitive (fast restore, immutable
base, per-clone CRNG reseed via vmid) is the visible win over cold-boot sandbox
competitors.

## Decisions (settled in brainstorming)

- **Session model: persistent.** A session is a live microVM; its filesystem and
  the VM stay alive across calls. The agent writes a file in one `run` and uses it
  in the next.
- **Implementation: Rust, in-repo.** A new workspace crate using the official Rust
  MCP SDK (`rmcp`), shipped as one cargo-built binary. Shells out to `boot
  --restore` to drive clones.
- **Transport: stdio.** The MCP client spawns the server binary and speaks
  JSON-RPC over stdio (the default for local Claude Code / Codex / Gemini servers).
- **Tools base: Alpine + Python3 + basics**, overlay-root immutable.
- **Networking: off by default** (sudo-free, safest); `--net` is an opt-in server
  flag that requires the vmnet entitlement.

## What "persistent" means (and does not)

Persistence is **VM + filesystem level**: the same `boot` process and its guest
filesystem (tmpfs overlay) survive between `run` calls, so files and installed
state persist. Each `run` is an **independent `sh -c`**, so shell-process state
(cwd, environment, shell variables) does **not** carry across runs — pass `cwd` and
any env inline per call. A live REPL / long-lived interpreter is out of scope (a
future `interactive` tool could add it). `reset` rolls a session back to the warm
base by cold-relaunching a fresh clone.

## Architecture

```
MCP client ──stdio JSON-RPC──> ignition-mcp (rmcp server)
                                 SessionManager
                                   sid -> Session { boot_pid, vsock_uds, inst_dir, last_used }
                                 tools: open_session, run, write_file, reset, close

open_session(): spawn `boot --restore tools-base --store <store>
                  --vsock-uds /tmp/ign-mcp-<sid>.sock <kernel> <rootfs>`
                wait until the guest exec agent answers; return sid
run(sid,cmd):   connect /tmp/ign-mcp-<sid>.sock, CONNECT 7000, send request frame,
                guest ign-exec runs `sh -c cmd`, returns response frame
reset(sid):     kill boot child, re-spawn `boot --restore` (fresh clone), same sid
close(sid):     kill boot child, remove instance dir, drop sid
```

## Components

### a. `ignition-mcp` crate (new workspace member)

Builds the stdio server binary `ignition-mcp`. Responsibilities:

- rmcp server setup; register the five tools (below) with JSON schemas.
- `SessionManager`: owns the session table, allocates session ids, spawns/kills
  `boot` children, enforces the cap, runs the idle reaper.
- A vsock client helper: connect to a session's control UDS, perform the
  `CONNECT 7000` → `OK` handshake (the existing E2 host→guest protocol), send a
  request frame, read the response frame.

Config (env vars / CLI flags, with defaults):
- `IGN_MCP_KERNEL` / `--kernel` (default `kimage/out/Image`)
- `IGN_MCP_ROOTFS` / `--rootfs` (default `kimage/out/rootfs-tools.ext4`)
- `IGN_MCP_STORE` / `--store` (default `./mcp-store`)
- `IGN_MCP_BASE` / `--base` (snapshot name, default `tools-base`)
- `IGN_MCP_MAX_SESSIONS` (default 8)
- `IGN_MCP_IDLE_SECS` (default 600)
- `IGN_MCP_NET` / `--net` (default off)
- `IGN_MCP_BOOT` (default `target/debug/boot`)

### b. Guest exec agent `ign-exec` (Python, in the tools rootfs)

A ~30-line Python script at `/usr/bin/ign-exec`, launched per connection by
`socat VSOCK-LISTEN:7000,fork EXEC:/usr/bin/ign-exec` (started from
`/etc/local.d/ign-exec.start` at boot, so it is listening at snapshot time).

Protocol on the connection (stdin/stdout of the EXEC'd script):
- Request: 4-byte little-endian length prefix + JSON
  `{"cmd": str, "stdin": str|null, "cwd": str|null, "timeout": number|null}`.
- Response: 4-byte little-endian length prefix + JSON
  `{"exit": int, "stdout": str, "stderr": str, "timed_out": bool}`.
  stdout/stderr are UTF-8 with `errors="replace"`.

`ign-exec` runs `subprocess.run(["/bin/sh","-c",cmd], input=stdin, cwd=cwd,
capture_output=True, timeout=timeout)`. On timeout it kills the process group and
returns `timed_out=true` with whatever was captured. A malformed/short request is
answered with a nonzero `exit` and an error in `stderr`; it never crashes the
listener (socat fork isolates each connection anyway).

### c. Tools base rootfs `build-rootfs-tools.sh`

New builder in the `kimage/build/build-rootfs*.sh` family. Alpine + `python3`,
`py3-pip`, `git`, `gcc`, `musl-dev`, `coreutils`, plus `socat` (already used).
Overlay-root immutable, reusing `/sbin/overlay-init` (as the browser rootfs does):
the ext4 disk is read-only, a tmpfs holds the writable upper, so a session's writes
live only in RAM and a `reset`/`close` discards them with no disk scrub. Installs
`/usr/bin/ign-exec` and `/etc/local.d/ign-exec.start`. Inherits the vmid reseed
service so each clone reseeds.

`make-tools-base.sh`: cold-boot the tools rootfs with `--vsock-uds`, wait for a
serial ready marker, snapshot as `tools-base`, quit. Modeled on
`make-browser-base.sh`.

## MCP tool surface (MVP)

- `open_session() -> { session_id: string }` — clone the base, boot, wait until
  `ign-exec` answers the probe (a `{"cmd":":"}` no-op), return the id. Errors if the session cap is reached
  or boot fails.
- `run(session_id, command, timeout_s?=30, cwd?, stdin?) ->
  { stdout, stderr, exit_code, timed_out }` — run `sh -c command` in the session.
- `write_file(session_id, path, content_base64) -> { bytes_written }` — binary-safe
  file drop (implemented as a `run` that base64-decodes to `path`). Text reads use
  `run("cat path")`; a dedicated `read_file` is deferred (YAGNI).
- `reset(session_id) -> {}` — cold-relaunch a fresh clone under the same id; all
  session state is discarded.
- `close(session_id) -> {}` — kill the boot child, remove the instance dir, drop
  the id.

`run`/`write_file`/`reset`/`close` on an unknown or dead `session_id` return an MCP
error naming the id.

## Data flow

```
open_session
  sid = next id; uds = /tmp/ign-mcp-<sid>.sock
  spawn boot --restore <base> --store <store> --vsock-uds <uds> [--net] <kernel> <rootfs>
  poll up to ~10s: connect uds, CONNECT 7000, send a probe request frame
    {"cmd": ":", "timeout": 5} (sh no-op); success = a well-formed response frame
    with exit==0 (confirms the guest exec agent is up and answering)
  on success: record Session, return { session_id: sid }
  on cap exceeded / spawn fail / probe timeout: kill child, return MCP error

run(sid, cmd, ...)
  look up Session (error if absent); update last_used
  connect uds, CONNECT 7000 -> expect "OK", send request frame {cmd,stdin,cwd,timeout}
  read response frame -> return { stdout, stderr, exit_code, timed_out }

reset(sid)
  kill boot child; rm instance dir; re-spawn boot --restore (fresh clone) on the
  same uds/sid; re-probe; update Session

close(sid)
  kill boot child; rm instance dir; drop sid from the table
```

## Lifecycle, limits, error handling

- **Session cap** (`IGN_MCP_MAX_SESSIONS`, default 8): `open_session` past the cap
  returns an error rather than evicting a live session.
- **Idle GC**: a background reaper closes any session unused for
  `IGN_MCP_IDLE_SECS` (default 600s). `run`/`write_file`/`reset` refresh `last_used`.
- **Server shutdown** (stdio EOF / SIGTERM): kill every boot child, remove all
  instance dirs, unlink the UDS files.
- **Per-run timeout**: `run` passes `timeout_s` to `ign-exec`; on timeout the guest
  kills the command's process group, the VM stays alive, and the result has
  `timed_out=true`.
- **Boot child dies unexpectedly**: detected on the next `run` (vsock connect
  fails); the session is marked dead and the call returns an error telling the agent
  to `reset` or `open_session`.

## Security / threat model

- The VMM self-sandboxes (Seatbelt v1: no IP egress, no exec/fork, writes confined,
  host secrets denied). The guest is HVF hardware-isolated. Arbitrary command
  execution is confined to the guest — that is the product.
- No network by default: agent code cannot reach the host network or the internet.
- The control UDS is local AF_UNIX; `ign-mcp` is the only client. Each session has
  its own UDS under the temp dir (already in the sandbox writable set).
- Honest framing: "your agent's code on your machine." Multi-tenant/untrusted
  positioning still waits on the sandbox v2 work (deny-default read + mach
  confinement + uid drop), tracked separately.

## Testing

- **Rust unit (`ignition-mcp`)**: `SessionManager` with a faked boot-spawner —
  allocation, cap enforcement, idle GC closes the right sessions, shutdown tears
  everything down, dead-child detection. The vsock request/response framing
  (length prefix + JSON round-trip).
- **`ign-exec` unit** (host python): feed a framed JSON request on stdin, assert the
  framed JSON response — exit code, stdout/stderr capture, timeout path, malformed
  request handling.
- **Integration (live HVF, `scripts/mcp_live_test.py`)**: open_session → `run`
  `python3 -c 'print(2+2)'` (assert stdout `4`) → `write_file` a script + `run` it
  → assert filesystem state persists across two runs → `reset` → assert the written
  file is gone → `close`. No `--net` needed.

## Files

- Create: `crates/mcp/` (new workspace member `ignition-mcp`: `Cargo.toml`,
  `src/main.rs`, `src/session.rs`, `src/vsock_client.rs`, `src/tools.rs`).
- Modify: root `Cargo.toml` (add the member).
- Create: `kimage/build/ign-exec.py` (guest exec agent).
- Create: `kimage/build/build-rootfs-tools.sh` (tools base rootfs).
- Create: `scripts/make-tools-base.sh` (warm-base maker).
- Create: `scripts/mcp_live_test.py` (integration test).
- Modify: `ROADMAP.md` (mark the MCP item in progress), docs page under `docs/src/`.

## Out of scope (noted, not built)

- `read_file` tool (use `run("cat …")`); a live interactive REPL tool; multi-language
  base (Node, etc.); remote/HTTP transport; per-session resource (CPU/mem) quotas
  beyond the VM defaults; `pip install` from the internet (needs `--net`).
- Firecracker REST API and the OCI shim are separate adoption-track items.
