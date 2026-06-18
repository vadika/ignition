# Fan-out demo

`scripts/fanout_demo.py` forks N clones from one warm base snapshot in parallel,
runs an identical workload in each, and shows they diverge cleanly. It adds no
new VMM capability: it orchestrates `boot --restore` (the
[clone primitive](snapshot-restore.md)) plus the in-guest `ign-exec` agent on
vsock port 7000 that the [MCP server](mcp-server.md) already ships.

## What it proves

Each fork runs the same shell workload: read `boot_id`, draw 8 bytes from
`/dev/urandom`, write those bytes to `/tmp/fork-marker`, then read the file back.
The demo proves three things:

1. **CRNG divergence** — `rand` from `/dev/urandom` is **distinct** per clone.
   Each restored clone gets a fresh entropy seed via [vmid](vmid.md), so siblings
   never hand out the same "random" bytes. This is the pass/fail proof that the
   reseed works.
2. **Copy-on-write filesystem isolation** — every fork writes the same path
   `/tmp/fork-marker` with its own value and reads back its own value, never
   another fork's. The `file_readback` column matches that fork's `rand` and no
   other.
3. **Fork speed** — N clones restore in parallel. The output reports per-clone
   restore latency and wall-clock for the whole batch, against a ~645 ms cold
   boot reference.

### A note on `boot_id`

`boot_id` is **not** a shared-lineage marker here, contrary to what you might
expect. The kernel derives `/proc/sys/kernel/random/boot_id` lazily from the
CRNG on first read, and nothing reads it before the snapshot is taken. So after
restore, each clone materializes its own `boot_id` from its vmid-reseeded CRNG,
and the values come out **distinct** per clone.

That makes `boot_id` a bonus per-clone identity-divergence signal, reported as
`identities_distinct`, rather than a lineage marker. It is the same class of
insight as the per-clone reseed described in [vmid](vmid.md): the divergence is
real and load-bearing, just not where the naive reading puts it.
`identities_distinct` is informational; it does not gate the pass/fail verdict.

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
fork restore_ms  exec_ms  bootid(distinct)  rand        file_readback status
0    142         38       3f9a1c…           8b2e4f…     8b2e4f…       ok
1    151         41       c81d04…           c0d11e…     c0d11e…       ok
2    138         37       7e22b9…           5a7740…     5a7740…       ok
3    160         44       1aa6f3…           91ff02…     91ff02…       ok
4    147         39       9d4470…           2d6c8a…     2d6c8a…       ok
5    155         42       40ce8b…           e4b309…     e4b309…       ok
6    144         38       6b1f7a…           77a1d6…     77a1d6…       ok
7    149         40       d35e21…           0fbe23…     0fbe23…       ok

aggregate: 8 forks, wall-clock 318 ms, restore p50/p95 149/160 ms
verdict: identities distinct=True  randoms distinct=True  cow isolated=True  => PASS
fork cost: 149 ms/clone restore, 318 ms wall-clock for 8 clones (cold boot is ~645 ms)
```

The eight distinct `rand` values confirm the per-clone reseed, each
`file_readback` matching its own `rand` confirms filesystem isolation, and the
eight distinct `bootid` values are the bonus identity-divergence signal. The
fork-cost line puts the restore latency in context: each clone is ready in well
under a quarter of a cold boot, and the whole batch overlaps in parallel.

`restore_ms` is the spawn-to-guest-vsock-ready latency (process launch until the
vsock handshake completes), i.e. the real fork cost. `exec_ms` is the workload
roundtrip alone.

## JSON mode

`--json` suppresses the table and emits a single object:

```json
{
  "forks": [ { "i": 0, "restore_ms": 142, "exec_ms": 38, "bootid": "...", "rand": "...", "file_path": "/tmp/fork-marker", "file_readback": "...", "exit": 0, "error": null }, ... ],
  "wall_clock_ms": 318,
  "verdict": { "randoms_distinct": true, "cow_isolated": true, "identities_distinct": true, "ok": true }
}
```

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Verdict ok: randoms distinct **and** cow isolated **and** all forks exited 0 |
| `1` | Verdict failed (any property above is false) |
| `2` | Missing input (`boot`/kernel/rootfs) or the base snapshot not found in the store |

## Related

- [Per-clone RNG reseed (vmid)](vmid.md) — the reseed that makes the randoms (and `boot_id`) diverge.
- [MCP server for agents](mcp-server.md) — the `ign-exec` agent and the `tools-base` snapshot.
- [Snapshot & restore](snapshot-restore.md) — the `boot --restore` primitive this drives.
