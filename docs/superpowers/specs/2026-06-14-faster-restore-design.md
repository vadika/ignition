# Faster Restore/Resume — Design

**Status:** approved for planning (2026-06-14)

**Goal:** Make `boot --restore` resume faster. The host-side `Restore-time` is
~245 ms and flat across diff-chain depth, but we have no breakdown of where that
245 ms goes. Attribute it first, then attack the single dominant stage.

**Non-goal:** A forked pre-warm process pool. If the data shows HVF object
creation dwarfs everything reducible in-process, pooling is the real fix — it is
recorded as a follow-up here, not built.

---

## Background

`restore_snapshot` (`spike/src/bin/boot.rs:~905–1262`) runs serially:

1. `resolve_chain` + shape/`mem_size` validation, `base_len` check.
2. `read_snapshot(leaf_dir)` — leaf `vmstate.json` + `gic.bin`.
3. instance dir create + `clonefile_or_copy` of the **root** `memory.bin` (`:965`).
4. `mmap(MAP_SHARED)` the instance `memory.bin` (`:971`).
5. diff overlay loop — `read_diff_pages` + `apply_diff` per diff layer (`:991`).
6. `Vm::new` → `hv_vm_create` (`:1008`).
7. `HvfGicV3::new` → `hv_gic_create` (`:1013`).
8. `map_memory` → `hv_vm_map` (`:1019`).
9. `protect_memory` — write-protect all RAM, only with `--track-dirty` (`:1027`).
10. device wiring — `setup_devices` + a second `clonefile_or_copy` for the
    **disk** (`:1059`).
11. console / handler setup, then `Restore-time` is logged (`:1262`).

The `Restore-time` clock brackets steps 1–11. The vCPU run loop (where the guest
actually resumes and lazily faults pages) is **after** the clock; `restore wall`
(~257 ms) shows the guest reaches first console output ~12 ms after handoff, so
lazy paging is already cheap. The target is the 245 ms of host setup.

### Measured constraints (already confirmed in code)

- Root `memory.bin` is **not** read eagerly — pure clonefile + `mmap`, faulted
  lazily. The only eager memory read is per-diff `read_diff_pages` + `apply_diff`
  memcpy, which is flat across chain depth.
- `hv_vm_create` is a **per-process singleton** (`crates/hvf/src/lib.rs:293`) —
  one VM per process. "Fan-out" therefore means N independent processes, each
  paying the fixed host setup. Base RAM pages are shared via the unified buffer
  cache (warm faults for clone 2+), but HVF object setup is not shareable. This
  is why an in-process pool is impossible and pooling means multi-process.

### No stage is pre-dismissed

`clonefile` is O(1) in **file size**, not free: it allocates an inode and copies
the extent map (constant, metadata-bound work), and the path makes **two** such
calls (root RAM at `:965`, disk at `:1059`). `hv_vm_map` registers a 512 MiB
region and may walk structures. Every stage in steps 1–10 is a genuine candidate
until measured. The point of this work is to stop guessing.

---

## Phase 1 — Instrumentation

Add cumulative-elapsed snapshots after each stage of `restore_snapshot`, then
emit one machine-parseable line just before the existing `Restore-time` log.

- Reuse the existing `restore_start: Instant` (`:905`). After each stage capture
  `restore_start.elapsed()` into a named local (`t_chain`, `t_read`, `t_clone`,
  `t_mmap`, `t_diff`, `t_vm`, `t_gic`, `t_map`, `t_protect`, `t_dev`). Stages
  skipped at runtime (diff overlay when chain len == 1; protect when not
  `--track-dirty`) record the same cumulative value as the prior stage, so their
  delta is 0.
- Compute per-stage deltas and log microseconds (so sub-ms stages do not round
  to 0):

```
Restore-breakdown = chain:Nus read:Nus clone:Nus mmap:Nus diff:Nus vm:Nus gic:Nus map:Nus protect:Nus dev:Nus total:Nus
```

  `total` is `restore_start.elapsed()` at log time and must equal the sum of the
  deltas to within rounding (cross-check against `Restore-time`).
- Always-on diagnostic. Cost: ~10 `Instant::now()` calls + one formatted log
  line. Negligible against a 245 ms budget.
- Emit via the same channel as `Restore-time` (`log::info!`).

No behavior change beyond the added log line.

## Phase 2 — Measure

- Extend `scripts/diff_snapshot_bench.py`: add a `Restore-breakdown` regex,
  parse each stage to an int (µs), aggregate per-stage **median** across the
  existing restore runs (Full-only and golden+N diff, n=5).
- Record a stage-attribution table in `docs/diff-snapshot-benchmarks.md` §3
  (Restore latency) showing where the 245 ms goes, for both the Full-only and a
  deep-chain restore.
- This phase produces the data that selects the Phase 3 lever. Do not pick the
  lever before this table exists.

## Phase 3 — One lever (data-gated)

Pick exactly one, by the measured dominant stage.

### Default lever: concurrent restructure (if `vm`+`gic` and/or `clone` dominate)

The serial chain has two independent tracks until `map_memory` (which needs both
the VM handle and the mapped host address):

- **Track A** (side thread): `Vm::new` → `HvfGicV3::new`. `HvfGicV3::new` needs
  `vcpu_count`; read the root manifest (cheap) first to get it.
- **Track B** (main thread): clonefile root `memory.bin` → `mmap` →
  `read_snapshot(leaf)` → diff overlay.
- **Track C** (optional, if disk clonefile is non-trivial): clonefile the leaf
  `disk.img` concurrently — it is independent until device wiring.

Join all tracks, then resume the serial tail: `map_memory` → `protect_memory` →
device wiring → console/handler. Hides `min(HVF-create, I/O)` from the critical
path.

**Gating check before committing this lever** (do this in the implementation, do
not assume): confirm `Vm` and `HvfGicV3` are `Send` so they cross the
thread-join boundary, and that `hv_vm_create` / `hv_gic_create` behave when
called from a spawned thread (the VM handle / GIC must remain usable from the
main thread and the vCPU threads, as they already are). If either fails, fall
back to the matching narrower lever below or stop and report.

### Fallback levers (if the dominant stage is elsewhere)

- `protect` dominates (`--track-dirty` runs only) → defer/lazy write-protect, or
  batch the protect call; the next interval can start clean without
  protect-on-the-critical-path.
- device wiring dominates → parallelize the disk clonefile with HVF create
  (Track C above) and/or lazily initialize devices.
- `map` (`hv_vm_map`) dominates → investigate; likely irreducible HVF cost,
  document and stop.

Whichever lever lands: re-run Phase 2 and update the §3 table + the doc's
summary with the new `Restore-time`.

---

## Testing

- **Instrumentation present:** `scripts/restore_test.py` asserts a
  `Restore-breakdown` line is emitted and that the stage deltas sum to `total`
  within tolerance (a few hundred µs for rounding).
- **Lever correctness gate:** `scripts/restore_test.py` and
  `scripts/restore_clone_test.py` still pass — the restored guest resumes from
  the saved PC, idles at ~0% CPU, and is responsive; clones stay independent and
  the base stays byte-immutable. `cargo test` and `cargo clippy` clean.
- **Win gate:** `Restore-time` median drops measurably vs the recorded 245 ms
  baseline. If the chosen lever does not move the median, revert it and record
  why (the measurement still has value).

## Follow-ups (out of scope)

- Forked pre-warm pool: a set of processes that have already run `hv_vm_create` +
  `hv_gic_create` and wait to be handed a snapshot. The only way to hide HVF
  object creation entirely, given the per-process-singleton constraint. Revisit
  if Phase 2 shows HVF create dominates and the in-process lever's gain is small.
- Working-set prefault on resume (`madvise(WILLNEED)`): only worth it if
  `restore wall − Restore-time` grows; currently ~12 ms, not worth it.
