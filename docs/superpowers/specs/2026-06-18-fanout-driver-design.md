# Fan-out driver design

## Goal

One command that demonstrates the fork-a-warm-VM story end to end: take an
existing base snapshot, fork N clones in parallel, run an identical workload in
each, and show that the clones share a common lineage yet diverge (distinct
CRNG, isolated copy-on-write filesystem). Output a human-readable table plus an
optional machine-readable `--json` dump with per-fork and aggregate timings.

This adds **no VMM capability**. Every primitive already ships:
`boot --restore` (fast clonefile+mmap restore), vmid per-clone CRNG reseed,
per-restore distinct MAC, and the in-guest `ign-exec` agent on vsock port 7000.
The driver is the missing orchestration + presentation layer.

## Non-goals

- No web UI (terminal table is the deliverable; `--json` covers scripting/CI).
- No new guest tooling — reuse the MCP `tools-base` rootfs and `ign-exec`.
- No session persistence / GC — forks are spawned, probed once, torn down. The
  MCP server already owns long-lived sessions; this is a one-shot demo driver.
- No base-snapshot creation — `scripts/make-tools-base.sh` already does that.
  The driver requires the snapshot to exist and prints a hint if it is missing.

## Architecture

A single standalone Python script, `scripts/fanout_demo.py`, with no
third-party dependencies (stdlib only — `subprocess`, `socket`, `struct`,
`json`, `threading`, `argparse`, `time`). Python over Rust because: it is a demo
driver, the existing live-proof scripts (`vmid_live_proof.py`,
`mcp_live_test.py`) are Python, and the host-side vsock client is ~40 lines.

### Fork path (direct `boot --restore`)

The driver spawns N `boot --restore <base>` child processes directly, each in
its own thread, so the restores run concurrently and the wall-clock reflects
true fan-out throughput rather than a serial sum.

Per fork `i` (0-based):

```
target/debug/boot --restore <base> --store <store> \
  --mem <mem> --vsock-uds /tmp/fanout-<run>-<i>.sock \
  <kernel> <rootfs>
```

- `<run>` is a per-invocation token (the driver's own PID) so concurrent runs
  and leftover sockets never collide.
- Each fork gets a unique `--vsock-uds`, which is how the driver reaches that
  fork's `ign-exec` agent.
- `--net` is intentionally omitted: the demo needs no guest networking, and
  omitting it keeps the driver sudo-free and the divergence proof focused on
  CRNG + filesystem. (Distinct per-fork MAC/IP is already covered by the
  networking + vmid features and their own proofs.)

### Workload channel (vsock `ign-exec`, port 7000)

The workload is driven over the same vsock E2 control handshake the MCP server
uses, not the serial console. Serial is fragile under parallelism (16-byte RX
FIFO, byte-pacing, interleaved stdout across N children). vsock gives each fork
a clean, framed, independent request/response.

Host-side client (ported from `crates/mcp/src/vsock_client.rs`):

1. Connect to the fork's control UDS (the `--vsock-uds` base path).
2. Send `CONNECT 7000\n`; read the `OK <host_port>\n` ack one byte at a time.
3. Send a 4-byte little-endian length prefix + JSON request:
   `{"cmd": <str>, "timeout": <number>}`.
4. Read a 4-byte LE length prefix + JSON response:
   `{"exit": int, "stdout": str, "stderr": str, "timed_out": bool}`.

The `ign-exec` listener (`socat VSOCK-LISTEN:7000,fork EXEC:/usr/bin/ign-exec`)
must already be up in the base snapshot. The MCP `tools-base` provides it; that
is the expected `--base`.

### Workload (identity + file write)

One `sh -c` snippet, identical in every fork, emitting a parseable block:

```sh
printf 'BOOTID=%s\n' "$(cat /proc/sys/kernel/random/boot_id)"
printf 'RAND=%s\n'   "$(head -c8 /dev/urandom | od -An -tx1 | tr -d ' \n')"
m=/tmp/fork-marker
head -c8 /dev/urandom | od -An -tx1 | tr -d ' \n' > "$m"
printf 'FILE=%s:%s\n' "$m" "$(cat "$m")"
```

- **BOOTID** — `/proc/sys/kernel/random/boot_id` is captured in the snapshot, so
  it is **identical** across all forks. This is the "shared lineage" column: it
  proves every fork came from the one base.
- **RAND** — `/dev/urandom`, **distinct** per fork (vmid reseed). The headline
  divergence column.
- **FILE** — every fork writes the *same* path `/tmp/fork-marker` with its own
  random value and reads it back. The readback equals that fork's own write and
  differs across forks, proving each fork has an **isolated copy-on-write**
  filesystem (no write bleeds between clones).

### Presentation

Default: a terminal table, one row per fork:

```
fork  restore_ms  exec_ms  bootid(shared)  rand(diverged)  file_readback
0     128         14       3f9a…           a17c4e…         a17c4e…  ok
1     131         13       3f9a…           90b2f1…         90b2f1…  ok
...
aggregate: N forks, wall-clock <T> ms, restore p50/p95 <..>/<..> ms
verdict: lineage shared=<bool>  randoms distinct=<bool>  cow isolated=<bool>
```

`--json` emits `{"forks": [{"i","restore_ms","exec_ms","bootid","rand",
"file_path","file_readback","exit"}...], "wall_clock_ms", "verdict": {...}}`
to stdout (and suppresses the table) for scripting/CI.

The driver's exit code is 0 only when the verdict holds:
`bootid` identical across all forks (shared lineage), `rand` distinct across all
forks (CRNG diverged), and each fork's `file_readback == rand-it-wrote`
(CoW isolated). Otherwise non-zero.

## Error handling

- **Missing inputs** (`boot`, kernel, rootfs) or **missing snapshot** in the
  store: print a one-line hint (`run scripts/make-tools-base.sh`) and exit 2
  before spawning anything.
- **A fork that fails to restore, never answers the handshake, or times out** on
  exec: that fork's row is marked `ERR` with the reason; it does not abort the
  other forks. The run's verdict fails if any fork errored.
- **vsock connect race**: the control UDS appears only once boot has set up the
  vsock device. The client retries `connect()` with a short backoff up to a
  per-fork deadline (default 15 s) before declaring the fork dead.
- **Teardown is unconditional** (`finally`): every child is killed and every
  `/tmp/fanout-<run>-*.sock` removed, even on exception or partial failure, so
  no orphan VMs or scratch sockets leak.

## Testing

The host-side framing/handshake logic is the only non-trivial code and is tested
without a VM, against an in-process fake guest over a Unix socket (the pattern
`vsock_client.rs` already uses):

- `connect → CONNECT 7000 → OK → framed request → framed response` round-trips,
  asserting the request JSON and the parsed response fields.
- A fake guest that closes before `OK` surfaces as a fork error, not a hang.
- The verdict logic is a pure function of the collected per-fork records and is
  unit-tested directly: identical bootids + distinct rands + matching readbacks
  ⇒ pass; any violation ⇒ fail.

Live HVF verification (run by hand on M-series, like the other proofs): build +
sign `boot`, `make-tools-base.sh`, then `fanout_demo.py --base tools-base -n 8`
and confirm the table shows one shared bootid, eight distinct randoms, eight
matching readbacks, and a sub-second aggregate fan-out.

## Files

- Create: `scripts/fanout_demo.py` — the driver (CLI, fork spawn, vsock client,
  workload, table/JSON, verdict, teardown).
- Create: `scripts/test_fanout_demo.py` — stdlib `unittest` for the framing
  round-trip and the verdict function (no VM).
- Create: `docs/src/features/fanout-demo.md` — user-facing doc (what it shows,
  how to run, sample output) + a `SUMMARY.md` TOC entry.
- Modify: `docs/src/SUMMARY.md` — add the new page under Features.
