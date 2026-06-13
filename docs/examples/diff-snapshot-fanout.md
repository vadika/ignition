# Use case: warm-base fan-out with diff snapshots

**Scenario.** You have a microVM that takes real time to boot and warm up (kernel,
init, services, a loaded dataset). You want many *divergent* copies of that warm
state — one per test shard, per experiment, per tenant — without paying a full RAM
dump (512 MiB+) for each. Diff snapshots give you one immutable **golden base** plus
a small **delta per fork** (only the 16 KiB pages each fork dirtied).

```
           snapshots/golden/      (Full, immutable, ~512 MiB)
                  │
      ┌───────────┼───────────┐
   exp-a        exp-b        exp-c      (Diff layers, parent=golden, ~MBs each)
```

Each fork restores instantly (`clonefile` the golden RAM + overlay its own diff into
a private CoW clone), idles at ~0% CPU, and **never mutates the golden base**.

## Walkthrough

Console keys: `Ctrl-A s` = snapshot, `Ctrl-A x` = quit.

### 1. Boot the base once and capture the golden Full snapshot

`--track-dirty` arms write-protect dirty tracking so later snapshots can diff.

```sh
cargo build -p ignition-spike --bin boot && scripts/sign.sh target/debug/boot

target/debug/boot --store vmstore --name golden --track-dirty --mem 512 \
    kimage/out/Image kimage/out/rootfs.ext4
# ... boots to a shell, warm up whatever you need ...
# press Ctrl-A s   -> writes the immutable Full base 'golden'
# press Ctrl-A x   -> quit
```

The first `Ctrl-A s` on a fresh boot is always **Full** (nothing to diff against):

```sh
ls -lh vmstore/snapshots/golden/      # memory.bin ~512M, gic.bin, disk.img, vmstate.json, manifest.json
cat   vmstore/snapshots/golden/manifest.json   # "snapshot_type":"Full","parent":null
```

### 2. Fork experiment A — restore the base, diverge, snapshot a Diff

Restore reads the RAM size from the snapshot, so no `--mem` on the restore side.
`--track-dirty` re-arms so this fork's `Ctrl-A s` writes a Diff with `parent=golden`.

```sh
target/debug/boot --store vmstore --restore golden --track-dirty --name exp-a
# resumes from the saved PC (no re-boot); press Enter for a prompt
# ... run experiment A: write files, mutate memory ...
# press Ctrl-A s   -> writes Diff layer 'exp-a' (parent=golden, only dirtied pages)
# press Ctrl-A x
```

### 3. Fork experiments B and C the same way

```sh
target/debug/boot --store vmstore --restore golden --track-dirty --name exp-b   # ... Ctrl-A s, Ctrl-A x
target/debug/boot --store vmstore --restore golden --track-dirty --name exp-c   # ... Ctrl-A s, Ctrl-A x
```

Each fork starts from the *same* warm `golden` RAM but records only its own changes:

```sh
du -h vmstore/snapshots/*/memory.bin
# 512M  vmstore/snapshots/golden/memory.bin     <- the one full copy
#  16M  vmstore/snapshots/exp-a/memory.bin       <- only exp-a's dirtied 16K pages
#  12M  vmstore/snapshots/exp-b/memory.bin
#  20M  vmstore/snapshots/exp-c/memory.bin
cat vmstore/snapshots/exp-a/manifest.json        # "snapshot_type":"Diff","parent":"golden"
```

Three warm forks for ~512 MiB + a few tens of MiB, instead of ~2 GiB of full dumps.

### 4. Bring any fork back up

Restore resolves the chain (`golden` → `exp-c`), clonefiles the golden RAM, overlays
`exp-c`'s diff into a private clone, and resumes. The golden base stays byte-immutable.

```sh
target/debug/boot --store vmstore --restore exp-c    # exactly the state at exp-c's snapshot
```

Run several at once in separate terminals — each gets its own ephemeral CoW clone
under `vmstore/instances/<name>-<pid>/`, removed on clean exit.

## What's going on underneath

- **Tracking:** with `--track-dirty`, all guest RAM is write-protected; the first
  write to each 16 KiB page traps (HVF Data Abort), gets recorded + re-granted, and
  re-executes. Cost ≈ 5 µs per first-write-per-page per interval.
- **Diff layer:** `Ctrl-A s` drains the dirty set and writes only those pages
  (`memory.bin` packed + `dirty.idx`), plus *full* vCPU/GIC/device state every layer.
- **Chain & immutability:** each layer points at its `parent`; restore reassembles
  root + diffs into a private `clonefile`+`MAP_SHARED` clone — stored layers are never
  mutated. Reusing the parent/restored-from name is refused without `--force`.

## Guard rails

- Diff requires `--track-dirty`; without it, `Ctrl-A s` writes a Full.
- `--force` is needed to overwrite a layer named like its parent or its restored-from
  source (protects the base you depend on).
- Deep chains apply more layers at restore; a flatten/compaction step is a planned
  follow-up (see `ROADMAP.md`).

End-to-end this flow is exercised headlessly by `scripts/diff_snapshot_test.py`.
