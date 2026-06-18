# MAP_PRIVATE-over-base restore design

## Goal

Make restore map guest RAM as `MAP_PRIVATE` over the shared, immutable base
`memory.bin` instead of `clonefile`-ing a per-instance copy and mapping it
`MAP_SHARED`. This is the only restore path (no flag). It cuts restore start
latency ~4x and the first-workload page-in tax ~2x by letting every restore
share one cache-warm vnode, with the host page cache amortized across launches.

Measured on the prototype (n=20, concurrency 1, numpy workload):

| restore mapping | start (ready) p50 | first exec p50 | warm exec p50 | wall (20) |
|---|---|---|---|---|
| clonefile + MAP_SHARED (old) | 245 ms | 741 ms | 120 ms | 23.0 s |
| MAP_PRIVATE over base (new) | 62 ms | 333 ms | 113 ms | 10.5 s |

## Why MAP_PRIVATE wins

Each `clonefile` produces a distinct APFS vnode. The unified buffer cache is
keyed per-vnode, so a per-instance clone's pages are cold even when the base is
fully cached (measured: reading a fresh clone of a cached base still takes
0.47 s/GB). Under `MAP_PRIVATE` over the base, all restores map the same base
vnode; its pages stay warm across launches and guest writes copy-on-write to
private anonymous pages, so the base is never modified and concurrent restores
do not interfere.

## Non-goals

- No `MADV_DONTNEED`-based rollback. Reset stays byte-copy from a pristine
  slice (portable, proven), which works regardless of how live RAM is mapped.
  This deliberately avoids Darwin's underspecified `MADV_DONTNEED` semantics.
- No change to the fresh-boot path (`MAP_ANON`; checkpoint already uses
  `from_copy`).
- No change to the disk per-instance clone (virtio-blk needs a real backing
  file; it is not mapped into guest RAM).
- No change to the stop-the-world vCPU rendezvous.
- The `--restore-private` prototype flag is removed (behavior is now default).

## Architecture

### Guest RAM mapping (`run_restore`)

Replace the per-instance memory `clonefile` + `MAP_SHARED` open with a direct
`MAP_PRIVATE` mmap of the root base `memory.bin`:

```rust
// Map the shared, immutable base memory.bin MAP_PRIVATE: guest writes copy-on-
// write to anonymous pages (base never modified), and every restore shares the
// same cache-warm vnode. No per-instance memory clone.
let basef = fs::File::open(&root_paths.memory)?;
let host = unsafe {
    libc::mmap(std::ptr::null_mut(), mem_size as usize,
               libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE,
               basef.as_raw_fd(), 0)
};
if host == libc::MAP_FAILED {
    return Err(io::Error::other("mmap of base memory.bin failed"));
}
drop(basef); // the mapping keeps the file alive after the fd closes
```

The diff-overlay step (chain > 1) writes into this mapping exactly as today;
those writes become private anonymous pages. The `inst_mem` path
(`clonefile_or_copy(root memory.bin -> instance memory.bin)`) is deleted. The
instance directory is still created for the disk clone.

### Reset-point pristine seeding (`run_restore`)

The seeded `ResetPoint.pristine` (what `Ctrl-A r` rolls back to) is built from
the post-restore image:

- **chain.len() == 1 (full snapshot):** `PristineRam::map_file_ro(&root_paths.memory, mem_size)`
  — a read-only `MAP_PRIVATE` mmap of the base file. Zero-copy, shares the warm
  vnode. Rollback copies base pages back over dirtied live pages.
- **chain.len() > 1 (diff chain):** `PristineRam::from_copy(live)` taken after
  the diff overlay — a heap copy of the reassembled RAM. Only diff chains pay
  this one-time copy; full snapshots stay zero-copy.

### `PristineRam` (crates/vmm/src/reset.rs)

Add one constructor that maps an existing file read-only without cloning:

```rust
/// Map an existing file read-only (no clone). Used to point the reset pristine
/// at the immutable base memory.bin directly.
pub fn map_file_ro(path: &Path, len: usize) -> io::Result<PristineRam> {
    let f = std::fs::OpenOptions::new().read(true).open(path)?;
    let ptr = unsafe {
        libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ,
                   libc::MAP_PRIVATE, f.as_raw_fd(), 0)
    };
    if ptr == libc::MAP_FAILED {
        return Err(io::Error::other("mmap of pristine file failed"));
    }
    Ok(PristineRam::Mapped { ptr, len })
}
```

`from_clone` and its `pristine_mapped_round_trips_bytes` test become dead (its
only callers were the checkpoint clonefile branch and the reset-point seed, both
replaced here) — delete both. `rollback_full`/`rollback_pages`/`Mapped`/`Owned`
are unchanged.

Note: the restore-time reset-point seed currently at `boot.rs:~2275`
(`from_clone(&inst_mem, &pristine_dst, ...)`) is replaced by the chain-length
split above (`map_file_ro(base)` for full, `from_copy(live)` for diff), and its
`pristine_dst` file under the instance dir is no longer written.

### Checkpoint handler (`install_reset_handlers`)

Drop the `mem_file` clonefile branch; always take an owned copy of live RAM:

```rust
let pristine = ignition_vmm::reset::PristineRam::from_copy(live);
```

Delete the `mem_file` field from `ResetWiring`, its `msync` call, and the
clonefile fallback. This is the existing fresh-boot behavior, now universal.

### Reset handler, disk snapshot, fresh boot

Unchanged. The reset handler already rolls back by byte-copy from
`rp.pristine.as_slice()`. The disk-snapshot handler already reads the live host
slice via `from_raw_parts(host_usize, ...)`, so re-snapshotting a
`MAP_PRIVATE`-restored guest captures correct current RAM with no change.

## Data flow

```
restore:
  open(base memory.bin) -> mmap MAP_PRIVATE RW  ==> guest RAM (host)
  if chain>1: overlay diffs into host (anon CoW)
  seed ResetPoint.pristine:
    chain==1 -> map_file_ro(base)        (zero-copy RO mmap, shared vnode)
    chain>1  -> from_copy(live)          (heap copy of reassembled image)

Ctrl-A r (reset):
  rollback_pages/full(pristine.as_slice() -> live)   [unchanged byte copy]

Ctrl-A c (checkpoint):
  ResetPoint.pristine = from_copy(live)              [full live-RAM heap copy]

Ctrl-A s (disk snapshot):
  write_snapshot(live slice -> store/memory.bin)     [unchanged]
```

## Error handling

- `mmap(MAP_PRIVATE)` failure on the base → return an `io::Error` (restore
  aborts cleanly, same shape as the old mmap failure).
- `map_file_ro` failure → propagate; restore aborts before the guest runs.
- Base file integrity: `MAP_PRIVATE` guarantees no write-back, so concurrent
  restores cannot corrupt the base. No locking needed.

## Testing

**Unit (crates/vmm/src/reset.rs):**
- `map_file_ro` round-trips bytes: write a file, `map_file_ro`, assert
  `as_slice()` equals the written bytes (mirrors the existing
  `pristine_mapped_round_trips_bytes` test).
- Existing `rollback_full`/`rollback_pages` tests stay green (logic unchanged).

**Live (M-series HVF, by hand):**
1. **Launch perf / correctness:** `sandbox_bench.py -n 20 --mode hot` — confirm
   ready p50 ~60-70 ms, exec1 well below the old ~740 ms, 20/20 ok, numpy
   output correct.
2. **Fan-out:** `fanout_demo.py --base tools-base -n 8` still PASSes (distinct
   randoms, CoW isolated).
3. **Re-snapshot integrity:** restore tools-base, `Ctrl-A s` a new snapshot,
   restore that, run a workload — output correct (proves the live-slice disk
   snapshot path).
4. **In-place reset:** restore a guest with `--vsock-uds`, write a marker file
   in the guest, `Ctrl-A r`, confirm the guest rolled back (marker gone) and
   stays alive — exercises the new pristine seeding + unchanged rollback.
5. **Diff chain:** restore a diff-chain snapshot, run a workload, confirm
   correct reassembly (exercises the `from_copy` pristine path).

## Files

- Modify: `crates/vmm/src/reset.rs` — add `map_file_ro`; drop `from_clone` if
  unused.
- Modify: `spike/src/bin/boot.rs` — `run_restore` mmap + pristine seeding;
  `install_reset_handlers` checkpoint (drop `mem_file`); remove the
  `--restore-private` flag (decl, arg parse, call site, signature).
- Modify: `scripts/sandbox_bench.py` — drop the `--restore-private` flag
  (now default); keep `--prefetch` (still useful from a cold cache).
- Docs: update `docs/src/features/snapshot-restore.md` (and any clone-primitive
  reference) to describe the `MAP_PRIVATE`-over-base model.
