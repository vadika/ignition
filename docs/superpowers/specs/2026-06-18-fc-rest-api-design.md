# Firecracker REST control API — design

_Design spec. Status: approved in brainstorming, awaiting implementation plan._

## Goal

Expose ignition's VM lifecycle through a Firecracker-compatible REST API so that
unmodified Firecracker orchestration — `firecracker-go-sdk`, flintlock — can boot,
snapshot, and restore guests on macOS / Apple Silicon. Adoption cost for the client
is ~zero: it speaks the Firecracker HTTP API it already speaks, pointed at our
`--api-sock`.

This is adoption-track seam #2 (MCP was #1). The visible win over a plain
Firecracker port is the fast clone-from-warm primitive: snapshot/load go through
ignition's `clonefile` + `MAP_PRIVATE`-over-base restore.

## Scope (settled in brainstorming)

**v1 = Launch + snapshot.** The route subset that `firecracker-go-sdk`
`Machine.Start` / `CreateSnapshot` / `LoadSnapshot` / `PauseVM` / `ResumeVM` and
flintlock-create actually call. NOT in v1: MMDS, balloon, rate-limiters, CPU
templates, metrics, hot PATCH of drives/net (those are unbuilt FC-parity infra,
separate specs).

**Approach: translate-and-spawn.** The API server holds accumulated config, and on
`InstanceStart` maps it to `boot` CLI flags and spawns the `boot` child — the same
pattern MCP and disposable-browser use. The VMM is NOT embedded in the HTTP crate;
`boot` stays the one place a VM is assembled and run.

**Control plane: a proper internal RPC, not escape bytes.** Rather than feed
`Ctrl-A s` keystrokes to the child's serial stdin, `boot` gains a control socket
whose commands call the same `VcpuManager.request_*()` methods the serial Ctrl-A FSM
already calls. The serial FSM stays for interactive use; both feed one internal
entry point.

## Architecture

```
firecracker-go-sdk / flintlock  (unmodified Go net/http client)
        │  HTTP/1.1 over unix socket  (--api-sock)
        ▼
ignition-fc-api  (new crate, hyper server)
   • VmConfig accumulator   (machine-config, boot-source, drives, net ifaces)
   • lifecycle state machine (NotStarted -> Running <-> Paused -> Stopped)
   • InstanceStart: map VmConfig -> boot CLI flags, spawn child
   • control client: connect boot's --control-sock, send action, read reply
        │  spawn + control UDS
        ▼
boot  (existing binary, two additions)
   • --control-sock <path>: listener thread calling manager.request_*()
   • VcpuManager pause/resume op (rendezvous that parks-and-holds)
```

Three units, clean boundaries. No FC types leak into `boot`; no HVF/VMM types leak
into the HTTP crate.

## Component 1: boot control channel

New flag `--control-sock <path>`. On startup (after the `VcpuManager` is built),
`boot` binds a `UnixListener` at `path` and spawns one listener thread holding an
`Arc<VcpuManager>` clone (the serial FSM thread already holds one and calls
`request_*()` — same pattern, same thread-safety story).

Wire protocol: line-framed JSON, one request line -> one response line (mirrors the
existing vsock/ign-exec framing convention in the repo, but newline-delimited
because these are tiny control messages):

```
{"action":"snapshot","name":"snap-7"}   -> {"ok":true}
{"action":"checkpoint"}                  -> {"ok":true}
{"action":"reset"}                        -> {"ok":true}
{"action":"pause"}                        -> {"ok":true}
{"action":"resume"}                       -> {"ok":true}
```

On error the reply is `{"ok":false,"error":"<message>"}`. Dispatch calls the
existing `manager.request_snapshot(..)` / `request_checkpoint()` / `request_reset()`
and the new `request_pause()` / `request_resume()`. No escape bytes are ever
synthesized.

### Per-request snapshot name (the refactor)

Today the snapshot name is fixed at launch: `write_name = name.unwrap_or(generate)`
is captured into the leader snapshot-handler closure, and `request_snapshot()` takes
no arguments (`spike/src/bin/boot.rs` ~line 2081; `crates/vmm/src/vstate/vcpu_manager.rs:233`).
FC clients choose the snapshot path per `CreateSnapshot` call, so the control path
must carry a name.

Change: on `VcpuManager`, pair the snapshot-request flag with a name slot
`snapshot_name: Mutex<Option<String>>`. Signature becomes
`request_snapshot(&self, name: Option<&str>)`:

- Serial Ctrl-A s path passes `None` -> leader uses the launch-time `write_name`
  (behavior unchanged).
- Control-socket path passes `Some(name)` -> leader uses that name, overriding
  `write_name` for this snapshot only.

In the leader handler the only change is, at the top:
`let write_name_snap = req_name.take().unwrap_or_else(|| write_name_snap.clone());`
where `req_name` reads-and-clears the new name slot. All existing guards
(restored-from guard, same-name-as-parent guard, diff-vs-full selection) operate on
the resolved name unchanged. `request_checkpoint` / `request_reset` are untouched
(no name).

### Pause / resume (new VcpuManager op)

Add `request_pause(&self)` / `request_resume(&self)` and a gate
`paused: (Mutex<bool>, Condvar)`. Modeled on the existing snapshot rendezvous
(`begin_rendezvous` + per-rendezvous `Barrier`):

- `request_pause`: arms a `pause_req` flag and publishes the barrier, exactly like
  `request_snapshot`. Each vCPU reaches the rendezvous barrier, then — instead of
  resuming — blocks on the condvar while `*paused == true`. Parked at ~0% CPU, no
  WFI/timer spin.
- `request_resume`: sets `*paused = false` and `notify_all()`; vCPUs fall through and
  resume their run loops at their current PC.

Not SIGSTOP: a stopped process cannot service the control socket, so it could never
be resumed over the same channel; signal-pause and socket-control would also be an
inconsistent surface.

`ResetMode::Full` and the existing reset/checkpoint paths are unchanged. Pause/resume
is additive and orthogonal.

## Component 2: ignition-fc-api crate

One microVM per API socket (Firecracker's model: one process = one VM = one
`--api-sock`). The server holds a single `Mutex<VmState>`; no VM table.

### Routes honored

| Method + path | Maps to |
|---|---|
| `GET /` | InstanceInfo `{id, state, vmm_version, app_name}` |
| `PUT /machine-config` | `vcpu_count`->`--smp`, `mem_size_mib`->`--mem`, `track_dirty_pages`->`--track-dirty` |
| `GET /machine-config` | echo accumulated config |
| `PUT /boot-source` | `kernel_image_path`->positional kernel, `boot_args`->`--append` |
| `PUT /drives/{id}` | root drive (`is_root_device:true`) `path_on_host`->positional rootfs |
| `PUT /network-interfaces/{id}` | presence -> `--net` (socket_vmnet backend; `host_dev_name` accepted+ignored; MAC VMM-generated) |
| `PUT /actions` `{action_type:"InstanceStart"}` | validate config, map -> flags, spawn `boot --control-sock …` |
| `PATCH /vm` `{state:"Paused"\|"Resumed"}` | control `pause` / `resume` |
| `PUT /snapshot/create` `{snapshot_path, mem_file_path, snapshot_type?}` | control `snapshot <name>`; record `snapshot_path -> name` |
| `PUT /snapshot/load` `{snapshot_path, mem_file_path, resume_vm?, enable_diff_snapshots?}` | (pre-start) spawn `boot --restore <name>`; `resume_vm:false` -> start paused |

Non-root drives in `PUT /drives/{id}` are accepted and recorded but only the root
drive maps to a boot arg in v1 (ignition takes a single rootfs positional). A second
data drive is a future PATCH-drives item; v1 returns `400` if a client marks two root
devices.

### Lifecycle state machine (vm.rs)

```
NotStarted ──InstanceStart──▶ Running ⇄ Paused (PATCH /vm)
NotStarted ──snapshot/load──▶ Running | Paused        (resume_vm decides)
Running|Paused ──child exits / server killed──▶ Stopped
```

- Config PUTs (`machine-config`, `boot-source`, `drives`, `network-interfaces`) are
  valid only in `NotStarted`; after boot they return `400`.
- `snapshot/create` requires `Paused` (Firecracker's documented precondition; go-sdk
  pauses first). Returns `400` otherwise.
- `snapshot/load` is valid only in `NotStarted`.

### snapshot_path <-> name mapping

FC clients treat snapshot paths as opaque handles passed back to `load`. The server
keeps a `path -> store-name` map:

- `create`: derive a sanitized store name from the `snapshot_path` basename (strip
  directory + extension, replace non-`[A-Za-z0-9_-]` with `_`); record
  `snapshot_path -> name`; trigger control `snapshot <name>`.
- `load`: look the `snapshot_path` up; if absent (snapshot created out-of-band), fall
  back to the sanitized basename as the store name directly.
- `mem_file_path` is accepted and ignored — ignition stores RAM + device/vCPU state
  together under the store dir, not as two files.

Honest limitation (documented in the feature page): the literal files named by
`snapshot_path` / `mem_file_path` do not exist at those host paths. This is invisible
to go-sdk / flintlock, which never stat them — they only round-trip the handles. A
client that inspects the host filesystem at those paths would be surprised.

### Wire faithfulness

Real Go `net/http` clients use keep-alive, Content-Length, possibly
`Expect: 100-continue`. Use `hyper` over a tokio `UnixListener` (Firecracker itself
uses hyper) rather than hand-rolling HTTP/1.1. Response contract matches Firecracker:

- Success: `204 No Content` for PUT/PATCH, `200 OK` + JSON for GET.
- Error: `400 Bad Request` + `{"fault_message":"<message>"}` (the shape go-sdk
  decodes), `404`/`405` for unknown route/method, `400 "failed to parse body"` for
  malformed JSON.

### Crate layout

```
crates/fc-api/
  Cargo.toml     hyper, tokio, serde, serde_json  (all already in the workspace except hyper)
  src/main.rs    arg parse (--api-sock, --store, --boot, kernel/rootfs defaults), bind UDS, serve
  src/api.rs     hyper router: (method, path) -> handler; status + JSON encoding
  src/model.rs   serde request/response types matching FC field names
  src/config.rs  VmConfig accumulator + to_boot_flags() -> Result<Vec<String>, Fault>
  src/vm.rs      lifecycle state machine, spawn boot child + reaper, control-socket client, path<->name map
```

Binary name: `ignition-fc-api`. CLI / env config (defaults mirror MCP):
`--api-sock` (required), `--store` (default `./fc-store`), `--boot` (default
`target/debug/boot`), `--kernel` (default `kimage/out/Image`), `--rootfs` default is
overridden by the root drive's `path_on_host`.

## Data flow

```
PUT /machine-config, /boot-source, /drives, /network-interfaces
   -> accumulate into VmState.config            (NotStarted only)

PUT /actions {InstanceStart}
   -> config.to_boot_flags()? -> spawn `boot --smp N --mem M --append "…"
        [--net] [--track-dirty] --control-sock <ctl> --store <store>
        <kernel> <rootfs>`
   -> poll-connect <ctl> until ready (deadline)  -> state = Running, 204

PATCH /vm {Paused}    -> ctl {"action":"pause"}  -> state = Paused
PATCH /vm {Resumed}   -> ctl {"action":"resume"} -> state = Running

PUT /snapshot/create  (requires Paused)
   -> name = sanitize(snapshot_path); record path->name
   -> ctl {"action":"snapshot","name":name}      -> 204

PUT /snapshot/load    (NotStarted only)
   -> name = lookup(snapshot_path) or sanitize(basename)
   -> spawn `boot --restore name --control-sock <ctl> --store <store> <kernel> <rootfs>`
   -> if resume_vm == false: ctl {"action":"pause"} right after ready
   -> state = Running | Paused, 204
```

## Error handling

FC error contract (every failure -> `400` + `{"fault_message":…}`):

- Config PUT after start -> `"operation not allowed post-boot"`.
- `InstanceStart` with no boot-source or no root drive -> `"no kernel/root drive configured"`.
- `snapshot/create` when not Paused -> `"vm must be paused before snapshotting"`.
- `snapshot/load` after start -> `"cannot load after boot"`.
- `PATCH /vm Resumed` when already running / `Paused` when already paused -> `400`
  (Firecracker is strict; mirror it).
- Unknown route/method -> `404` / `405`; malformed JSON -> `400 "failed to parse body"`.

boot child failures:

- Spawn fails (binary missing, bad flags) -> `400` on the `InstanceStart` response,
  state stays `NotStarted`.
- Child dies after start -> a tokio `child.wait()` reaper flips state to `Stopped`;
  subsequent control actions -> `400 "vm not running"`.
- Control-socket connect/IO error or a non-`{ok:true}` reply -> `400` echoing the
  boot-side message.

Readiness: `InstanceStart` / `snapshot/load` do not return `204` until the child's
control socket is connectable (poll-connect with a deadline, the retry-until-deadline
pattern from `scripts/fanout_demo.py:vsock_connect`), so an immediate
`snapshot/create` cannot race a half-up VM.

Concurrency: a single `tokio::Mutex<VmState>` serializes all mutating routes. FC
clients drive one VM sequentially, so this is not a throughput concern.
`ponytail:` comment marks the global lock with the "per-VM locks if the server ever
hosts >1 VM" upgrade path.

Cleanup: on SIGTERM/SIGINT the server kills the boot child and unlinks the api-sock +
control-sock. go-sdk's `StopVMM` sends SIGTERM to our server, so the child dies with
it.

## Testing

**Unit — crates/fc-api:**
- `config.rs`: `to_boot_flags()` maps a fully-populated `VmConfig` to the exact
  expected arg vector (table test); missing kernel or root drive -> `Err(Fault)`.
- `vm.rs`: state-machine transitions — each illegal transition
  (config-after-boot, snapshot-while-running, double-pause, load-after-boot) returns
  the right fault. Pure logic, no real boot child.
- `model.rs`: round-trip a captured real `firecracker-go-sdk` request body
  (machine-config, drives, snapshot/create) through serde — proves we accept its
  exact field names.

**Unit — crates/vmm:** `request_pause` then `request_resume` round-trips; a paused
manager parks and a resume releases it (existing rendezvous tests in
`vcpu_manager.rs` as the template; no HVF needed).

**Integration — no HVF, mock boot:** a stub `boot` script that binds the control
socket and records the actions it receives. Drive the full FC sequence
(machine-config -> boot-source -> drives -> InstanceStart -> pause ->
snapshot/create -> resume) over the api-sock with a plain HTTP client; assert the
status codes and that the stub received `snapshot <name>` / `pause` / `resume`.

**Live — M-series HVF, by hand (the real proof):**
1. Run `firecracker-go-sdk`'s own example unmodified against our api-sock:
   `Machine.Start()` boots a guest; `PauseVM` -> `CreateSnapshot` -> `ResumeVM`; then
   a fresh `Machine` `LoadSnapshot` resumes it. Harness in
   `scripts/fc_api_live_test.py` (plain HTTP over the UDS) or a tiny Go program.
2. Cross-check: the snapshot created via the API restores with plain
   `boot --restore <name>` too, proving the store artifact is a normal ignition
   snapshot, not an API-only object.

## Files

- New crate `crates/fc-api/` (main.rs, api.rs, model.rs, config.rs, vm.rs, Cargo.toml).
- `Cargo.toml` (workspace): add `crates/fc-api` member; add `hyper` to the workspace
  dependency set.
- Modify `crates/vmm/src/vstate/vcpu_manager.rs`: `request_snapshot` gains an
  `Option<&str>` name; new `snapshot_name` slot; new `request_pause` / `request_resume`
  + `paused` gate; vCPU run loop honors the pause gate at the rendezvous.
- Modify `spike/src/bin/boot.rs`: `--control-sock` flag; control-listener thread;
  pass the per-request name through the snapshot handler.
- Docs: new `docs/src/features/fc-rest-api.md`; add it under the MCP/adoption section
  of `docs/src/SUMMARY.md`; mark the ROADMAP item shipped when done.
- `scripts/fc_api_live_test.py`: the live FC-sequence harness.

## Non-goals (v1)

- MMDS, balloon over REST, rate-limiters, CPU templates, metrics endpoints.
- Hot PATCH of drives / network interfaces post-boot.
- A second data drive (single rootfs positional only).
- Multi-VM per API socket (one VM per socket, matching Firecracker).
- Faithful two-file (state + mem) snapshot artifacts on disk at client-named paths.
