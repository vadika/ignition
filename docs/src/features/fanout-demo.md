# Fan-out demo

`scripts/fanout_demo.py` forks N clones from one warm base snapshot in parallel,
runs an identical workload in each, and shows they share a common lineage yet
diverge. It adds no new VMM capability: it orchestrates `boot --restore` (the
[clone primitive](snapshot-restore.md)) plus the in-guest `ign-exec` agent on
vsock port 7000 that the [MCP server](mcp-server.md) already ships.

## What it proves

Each fork runs the same shell workload: read `boot_id`, draw 8 bytes from
`/dev/urandom`, write those bytes to `/tmp/fork-marker`, then read the file back.
Three columns of the output map to three properties:

1. **Shared lineage** — `bootid` from `/proc/sys/kernel/random/boot_id` was
   captured in the snapshot's RAM, so it is **identical** across every fork. All
   clones descend from the same warm base.
2. **CRNG divergence** — `rand` from `/dev/urandom` is **distinct** per fork. Each
   restored clone gets a fresh entropy seed via [vmid](vmid.md), so siblings do not
   hand out the same "random" bytes.
3. **Copy-on-write isolation** — every fork writes the same path
   `/tmp/fork-marker` with its own value and reads back its own value, never
   another fork's. The `file_readback` column matches that fork's `rand` and no
   other.

## Running it

Build the `tools-base` snapshot once (cold-boots the tools rootfs, snapshots, and
quits):

```sh
scripts/make-tools-base.sh
```

Then fan out:

```sh
python3 scripts/fanout_demo.py --base tools-base -n 8
```

### Flags

| Flag | Default | Notes |
|---|---|---|
| `-n`, `--count` | `8` | Number of clones to fork |
| `--base` | `tools-base` | Warm base snapshot name |
| `--store` | `<repo>/mcp-store` | Snapshot store directory |
| `--boot` | `<repo>/target/debug/boot` | `boot` binary |
| `--kernel` | `<repo>/kimage/out/Image` | Guest kernel image |
| `--rootfs` | `<repo>/kimage/out/rootfs-tools.ext4` | Tools base rootfs |
| `--mem` | `1024` | Per-clone guest memory (MiB) |
| `--timeout` | `20.0` | Guest exec timeout (s) |
| `--deadline` | `20.0` | Per-fork connect deadline (s) |
| `--json` | (off) | Emit JSON instead of the table |

## Sample output

```console
$ python3 scripts/fanout_demo.py --base tools-base -n 8
fork restore_ms  exec_ms  bootid    rand        file_readback status
0    142         38       3f9a1c…   8b2e4f7a…   8b2e4f7a…     ok
1    151         41       3f9a1c…   c0d11e93…   c0d11e93…     ok
2    138         37       3f9a1c…   5a7740bb…   5a7740bb…     ok
3    160         44       3f9a1c…   91ff02ce…   91ff02ce…     ok
4    147         39       3f9a1c…   2d6c8a14…   2d6c8a14…     ok
5    155         42       3f9a1c…   e4b3097f…   e4b3097f…     ok
6    144         38       3f9a1c…   77a1d650…   77a1d650…     ok
7    149         40       3f9a1c…   0fbe23d8…   0fbe23d8…     ok

aggregate: 8 forks, wall-clock 318 ms, restore p50/p95 149/160 ms
verdict: lineage shared=True  randoms distinct=True  cow isolated=True  => PASS
```

The single shared `bootid` confirms common lineage, the eight distinct `rand`
values confirm per-clone reseed, and each `file_readback` matching its own `rand`
confirms filesystem isolation.

`restore_ms` is the spawn-to-handshake-ready latency (process launch until the
first exec response, minus the exec itself), not an internal VMM timer.

## JSON mode

`--json` suppresses the table and emits a single object:

```json
{
  "forks": [ { "i": 0, "bootid": "...", "rand": "...", "file_readback": "...", "exit": 0, "restore_ms": 142, "exec_ms": 38, "error": null }, ... ],
  "wall_clock_ms": 318,
  "verdict": { "lineage_shared": true, "randoms_distinct": true, "cow_isolated": true, "ok": true }
}
```

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Verdict ok: lineage shared **and** randoms distinct **and** cow isolated **and** all forks exited 0 |
| `1` | Verdict failed (any property above is false) |
| `2` | Missing input (`boot`/kernel/rootfs) or the base snapshot not found in the store |

## Related

- [Per-clone RNG reseed (vmid)](vmid.md) — the reseed that makes the randoms diverge.
- [MCP server for agents](mcp-server.md) — the `ign-exec` agent and the `tools-base` snapshot.
- [Snapshot & restore](snapshot-restore.md) — the `boot --restore` primitive this drives.
