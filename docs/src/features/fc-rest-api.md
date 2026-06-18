# Firecracker REST API (ignition-fc-api)

`ignition-fc-api` is a Firecracker-compatible REST server over a unix socket. It
lets unmodified Firecracker orchestration — `firecracker-go-sdk`, flintlock, and
the tools built on them — drive an ignition microVM on macOS without an
ignition-specific client. The field names on every route match Firecracker's
wire format, so an off-the-shelf SDK serializes straight to them.

The visible win is fast clone-from-warm. A `PUT /snapshot/create` followed by a
`PUT /snapshot/load` goes through ignition's [clone primitive](snapshot-restore.md):
`clonefile` + `MAP_PRIVATE`-over-base restore. An orchestrator that already
snapshots-and-restores for fleet scaling gets that path for free, faster than a
cold boot.

## The route table (v1 subset)

```
GET  /                          -> { id, state, vmm_version, app_name }
PUT  /machine-config            { vcpu_count, mem_size_mib, track_dirty_pages }
GET  /machine-config            -> the three fields above
PUT  /boot-source               { kernel_image_path, boot_args }
PUT  /drives/{id}               { drive_id, path_on_host, is_root_device }
PUT  /network-interfaces/{id}   { iface_id, host_dev_name, guest_mac }
PUT  /actions                   { action_type: "InstanceStart" }
PATCH /vm                       { state: "Paused" | "Resumed" }
PUT  /snapshot/create           { snapshot_path }
PUT  /snapshot/load             { snapshot_path, resume_vm }
```

Config PUTs accumulate; they are only accepted before boot (a PUT after
`InstanceStart` returns a 400 fault). A successful mutation returns `204 No
Content`. `GET /` reports state as `Not started`, `Running`, or `Paused`.

Each config field maps to a `boot` flag or positional:

| REST field | `boot` argument |
|---|---|
| `vcpu_count` | `--smp N` |
| `mem_size_mib` | `--mem N` |
| `track_dirty_pages` | `--track-dirty` |
| `boot_args` | `--append <args>` |
| `network-interfaces` (any) | `--net` (socket_vmnet) |
| `kernel_image_path` | kernel positional |
| root drive `path_on_host` | rootfs positional |

## Translate and spawn

The server holds the accumulated config in memory. There is no VM until
`PUT /actions {InstanceStart}`: that maps the config to a `boot` argv
(`config.to_boot_flags`) and spawns a headless `boot` child, adding
`--control-sock <store>/control.sock` and `--store <store>`. The server then
poll-connects the control socket until `boot` answers, and marks the VM
`Running`.

The control socket is line-JSON: one `{"action":"...","name":"..."}` request per
line, one `{"ok":true}` reply per line. The snapshot routes drive the VM by writing
those lines, which call `boot`'s `VcpuManager.request_snapshot` — the same method
the interactive serial `Ctrl-A` FSM uses. One transport (REST), one driver
underneath.

## Snapshot while paused

Pause is **advisory** — REST state only. `PATCH /vm {Paused}` / `{Resumed}` just
records the requested state; the guest keeps running and no control command is
sent. There is no holding rendezvous to freeze the vCPUs.

A real freeze is unnecessary because `PUT /snapshot/create` issues one atomic
stop-the-world snapshot rendezvous on its own: every vCPU exits to the snapshot
barrier, saves its registers, and the leader writes the snapshot before any vCPU
resumes. `boot`'s snapshot control command is synchronous (`boot` writes the reply
line only once the snapshot is on disk), so the 204 means the snapshot is written.
The VM stays in the `Paused` REST state, exactly as the client left it. The
snapshot's name in the ignition store is the sanitized basename of the client's
`snapshot_path` (see below).

## Honest limitations

This is one faithful seam, not a full Firecracker reimplementation. The v1 gaps
are deliberate and bounded:

- **One microVM per API socket.** This matches Firecracker: one process is one VM.
  Run multiple servers on multiple sockets for multiple VMs.
- **Pause is advisory** — a busy guest is not frozen. `PATCH /vm {Paused}` only
  records REST state; snapshot consistency comes from the atomic snapshot
  rendezvous, not from pause (fine for a sandbox idle between calls).
- **`snapshot_path` / `mem_file_path` are opaque handles, not real files.** The
  server maps a sanitized basename of `snapshot_path` to an ignition store name
  and records the mapping (`path -> store name`); the actual snapshot lives in the
  `--store` directory under that name. The literal files named in `snapshot_path` /
  `mem_file_path` do **not** exist at those host paths. This is invisible to
  `firecracker-go-sdk` / flintlock, which only round-trip the handles between
  create and load — but a client that `stat`s those paths will be surprised.
- **`snapshot_type` (create) and `enable_diff_snapshots` (load) are accepted but
  ignored.** `boot` decides Full vs Diff from `--track-dirty` plus the restored-from
  leaf (see [diff snapshots](diff-snapshots.md)), not from these fields.
- **Networking is socket_vmnet**, so `host_dev_name` is ignored and the guest MAC
  is VMM-generated. Any `network-interfaces` PUT enables `--net`; the iface fields
  are accepted for wire-compat and otherwise unused.
- **No background child reaper in v1.** If the `boot` child exits on its own, the
  server's reported state stays `Running` / `Paused` until the next control
  operation fails — that failure surfaces as a 400 fault. Recovering requires
  restarting the server. Acceptable v1 gap.
- **A dead boot during the readiness poll costs up to ~30 s.** `InstanceStart` and
  `snapshot/load` poll the control socket for readiness with a 30 s deadline; if
  `boot` dies before binding it, the call blocks for that long before returning an
  error.
- **A snapshot/load-only client must still PUT a boot-source and a root drive
  first.** `boot --restore` still needs the kernel and rootfs positionals.
  `firecracker-go-sdk` does exactly this in its snapshot-resume flow, so the
  requirement is invisible to it, but a hand-rolled load-only client must supply
  them.

## Using it from firecracker-go-sdk

The SDK talks to the unix socket; point it at the `--api-sock` path. A
start-pause-snapshot-resume cycle:

```go
cfg := firecracker.Config{
    SocketPath:      apiSock,
    KernelImagePath: kernel,                  // PUT /boot-source
    KernelArgs:      "ro init=/sbin/overlay-init",
    MachineCfg: models.MachineConfiguration{
        VcpuCount:       firecracker.Int64(1),
        MemSizeMib:      firecracker.Int64(512),
        TrackDirtyPages: firecracker.Bool(true),
    },
    Drives: []models.Drive{{
        DriveID:      firecracker.String("rootfs"),
        PathOnHost:   firecracker.String(rootfs),
        IsRootDevice: firecracker.Bool(true),
    }},
}
m, _ := firecracker.NewMachine(ctx, cfg)
m.Start(ctx)                                  // PUT /actions InstanceStart
m.PauseVM(ctx)                                // PATCH /vm {Paused}
m.CreateSnapshot(ctx, "/snap/mem", "/snap/state") // PUT /snapshot/create
m.ResumeVM(ctx)                               // PATCH /vm {Resumed}
```

To clone from the snapshot, start a **second** server on the same `--store`, PUT
the same boot-source + root drive, then `LoadSnapshot` (`PUT /snapshot/load` with
`resume_vm: true`) using the same `snapshot_path` handle.

### The live harness

`scripts/fc_api_live_test.py` runs that whole sequence against a real
`ignition-fc-api` + real `boot` + the tools-base assets. It starts a server,
PUTs the config, `InstanceStart`s, polls `GET /` until `Running`, pauses,
snapshots, resumes, then starts a second server on the same store and
snapshot-loads into a running clone. It needs an Apple-Silicon Mac with HVF and a
signed `target/debug/boot`, so it is a by-hand proof, not a CI test. The
HVF-free counterpart, `scripts/fc_api_mock_test.py`, drives the same routes
against a stub `boot` and runs anywhere.

## Where the pieces live

- Sibling adoption seam (MCP for agents): [MCP server](mcp-server.md)
- Clone primitive and snapshot store: [snapshot & restore](snapshot-restore.md)
- Full vs diff snapshot decision: [diff snapshots](diff-snapshots.md)
- socket_vmnet networking: [devices, SMP & networking](devices.md)
- Server crate: `crates/fc-api/`
- Mock test (CI-safe): `scripts/fc_api_mock_test.py`
- Live test (HVF): `scripts/fc_api_live_test.py`
