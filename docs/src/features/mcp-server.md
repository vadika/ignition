# MCP server for agents (ignition-mcp)

`ignition-mcp` is a stdio MCP server that gives any MCP-capable agent — Claude Code,
Codex, Gemini CLI, or any other tool that speaks JSON-RPC over stdio — a sandboxed
microVM per session. The agent calls standard MCP tools; no ignition-specific code
required on the client side.

Each session is a microVM cloned from a warm snapshot (the [clone primitive](snapshot-restore.md)).
Clone startup is fast because the base image is already running in RAM; `boot --restore`
picks up where the snapshot left off. Each clone gets a fresh CRNG seed via
[vmid](vmid.md).

## The five tools

```
open_session()
  -> { session_id: string }

run(session_id, command, timeout_s?=30, cwd?, stdin?)
  -> { stdout, stderr, exit_code, timed_out }

write_file(session_id, path, content_base64)
  -> "ok"

reset(session_id)
  -> "ok"

close(session_id)
  -> "ok"
```

`open_session` clones the warm base, boots it, waits until the guest exec agent
answers a probe, and returns a session id. `run` executes `sh -c <command>` inside
the session's VM and returns stdout, stderr, the exit code, and a boolean for
timeout. `write_file` drops a binary-safe file at `path` (base64-decoded); for
reading, `run("cat path")` is sufficient. `reset` cold-relaunches a fresh clone
under the same id, discarding all session state. `close` kills the boot child and
drops the id.

Any call to `run`, `write_file`, `reset`, or `close` with an unknown or dead
`session_id` returns an MCP error naming the id.

## Persistent-session semantics

Persistence is at the VM and filesystem level. The same `boot` child and its guest
filesystem (a tmpfs overlay over a read-only ext4 root) survive across calls:
files written in one `run` are visible in the next.

Each `run` is an independent `sh -c`, so shell-process state — working directory,
environment variables, shell variables — does **not** carry across calls. Pass `cwd`
and any inputs you need per call. A long-lived REPL is out of scope.

`reset` discards everything: it kills the running VM and boots a fresh clone of the
warm base. The written files are gone, the overlay is cleared.

The idle reaper closes sessions unused for `IGN_MCP_IDLE_SECS` (default 600 s).
`run`, `write_file`, and `reset` refresh the idle timer.

## Architecture

One `boot --restore` child per session, each with its own per-session vsock UDS.
The server connects to that UDS, performs the `CONNECT 7000` handshake, and sends
a framed JSON request. The guest runs `socat VSOCK-LISTEN:7000,fork
EXEC:/usr/bin/ign-exec`, started at boot from `/etc/local.d` so it is listening
at snapshot time and resumes on restore. `ign-exec` is a small Python script (~30
lines) that reads the request, runs `subprocess.run(["/bin/sh","-c",cmd], ...)`,
and writes back a framed JSON response. On timeout it kills the command's process
group and sets `timed_out: true`; the VM stays alive.

Each session auto-engages vmid: `boot --restore` pushes a fresh 32-byte entropy
seed over the vsock control channel before the guest exec agent is probed.

The `ignition-mcp` binary is a Rust workspace crate built on `rmcp` 0.9 (the
official Rust MCP SDK). The `SessionManager` owns the session table, spawns and
kills `boot` children, enforces the session cap, and runs the idle reaper.

## Running it

Point an MCP client at the `ignition-mcp` binary over stdio. For Claude Code, add
it to your MCP server config:

```json
{
  "mcpServers": {
    "ignition": {
      "command": "/path/to/target/debug/ignition-mcp",
      "env": {
        "IGN_MCP_KERNEL": "/path/to/kimage/out/Image",
        "IGN_MCP_ROOTFS": "/path/to/kimage/out/rootfs-tools.ext4",
        "IGN_MCP_STORE":  "/path/to/mcp-store"
      }
    }
  }
}
```

The warm base must be built before starting the server:

```sh
scripts/make-tools-base.sh
```

This cold-boots `kimage/build/build-rootfs-tools.sh` (Alpine + Python 3 + git +
gcc + socat), waits for a serial ready marker, snapshots as `tools-base`, and
quits. The pattern mirrors `make-browser-base.sh`.

### Environment variables

| Variable | Default | Notes |
|---|---|---|
| `IGN_MCP_KERNEL` | `kimage/out/Image` | Guest kernel image |
| `IGN_MCP_ROOTFS` | `kimage/out/rootfs-tools.ext4` | Tools base rootfs |
| `IGN_MCP_STORE` | `./mcp-store` | Snapshot store directory |
| `IGN_MCP_BASE` | `tools-base` | Warm base snapshot name |
| `IGN_MCP_MAX_SESSIONS` | `8` | Session cap; `open_session` errors past it |
| `IGN_MCP_IDLE_SECS` | `600` | Idle timeout before a session is reaped |
| `IGN_MCP_NET` | (unset) | Set to any value to enable `--net` (needs the vmnet entitlement) |

## Networking and security

No network by default. Agent code cannot reach the host network or the internet
unless `IGN_MCP_NET` is set, which requires the vmnet entitlement (same requirement
as `--net` elsewhere). Running without networking is sudo-free.

The VMM self-sandboxes via [Seatbelt v1](sandbox.md): no IP egress, no exec/fork,
writes confined to VM-state directories, host secrets denied. The guest is
hardware-isolated by HVF. The overlay-root filesystem means a session's writes
live only in RAM; `reset` or `close` discards them with no disk scrub needed.

Honest framing: this is "your agent's code on your machine." Multi-tenant or
untrusted-code positioning waits on Seatbelt v2 (deny-default read and mach
confinement plus uid drop), tracked in the roadmap.

## Where the pieces live

- Clone primitive and snapshot store: [snapshot-restore.md](snapshot-restore.md)
- Per-clone CRNG reseed: [vmid.md](vmid.md)
- VMM self-sandboxing (Seatbelt): [sandbox.md](sandbox.md)
- Guest exec agent source: `kimage/build/ign-exec.py`
- Tools rootfs builder: `kimage/build/build-rootfs-tools.sh`
- Warm base maker: `scripts/make-tools-base.sh`
- MCP server crate: `crates/mcp/`
