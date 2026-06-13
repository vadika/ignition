# Fast Restore via clonefile + mmap — Design

Date: 2026-06-13
Status: approved (brainstorming) → ready for plan

## Goal

Cut microVM **resume latency** by loading guest memory lazily instead of eagerly
reading the whole RAM dump on restore, while making the snapshot a **first-class
immutable base**: a restore must never mutate the snapshot directory. As a
byproduct, parametrize guest RAM size (today a hardcoded compile-time constant) so
restore reconstructs the exact size the snapshot was taken with.

Diff/incremental snapshots for *size* reduction (dirty-page tracking on the write
side) are explicitly **out of scope** here — this work targets restore *latency*
and immutability. The write side still dumps full memory.

## Background — current behavior

- **Snapshot** (`crates/vmm/src/snapshot.rs::write_snapshot`): writes a full RAM dump
  (`memory.bin`), `gic.bin`, a full `fs::copy` of the rootfs (`disk.img`), and
  `vmstate.json`, into a temp dir that is atomically renamed into place.
- **Restore** (`spike/src/bin/boot.rs::run_restore`): `mmap`s a fresh anonymous
  region, then `fs::read`s the *entire* `memory.bin` and `copy_from_slice`s it into
  that region; copies `disk.img` to a private per-instance file under
  `std::env::temp_dir()`. Guest RAM size is the compile-time `RAM_SIZE` constant
  (`0x2000_0000`, 512 MiB); restore implicitly assumes the snapshot used the same
  size.

So base immutability today holds only because restore makes full eager copies, and
every snapshot/restore pays for the entire 512 MiB regardless of working set.

## Approach: instance = copy-on-write clone of an immutable base

A snapshot directory is treated as **read-only**. Every `--restore` materializes an
ephemeral **instance directory** whose memory and disk are APFS `clonefile(2)` copies
of the base; the instance is what the running guest mutates. The base is never
written.

```
<base snap dir>/              (immutable; restore opens read-only)
  memory.bin   gic.bin   disk.img   vmstate.json

$TMPDIR/ignition-inst-<pid>/  (ephemeral; best-effort removed on clean exit)
  memory.bin   <- clonefile(base/memory.bin)   APFS metadata CoW, O(1)
  disk.img     <- clonefile(base/disk.img)      APFS metadata CoW, O(1)
```

`clonefile(2)` (`<sys/clonefile.h>`) creates a copy that shares the source's data
blocks via APFS copy-on-write: it returns in O(1) regardless of file size, and the
two files diverge block-by-block only as one is written. The destination must not
already exist.

### Restore memory path (the latency win)

Replaces the anonymous-mmap + full `fs::read` with:

1. `clonefile(base/memory.bin -> inst/memory.bin)` — instant, no bytes copied.
2. `open(inst/memory.bin, O_RDWR)` then
   `mmap(NULL, mem_size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0)`.
   That host virtual address is the guest RAM slice.
3. `hv_vm_map` the VA (unchanged from today).
4. Pages fault in lazily from the clone on first guest access; guest writes land in
   the clone (APFS copy-on-writes the block off the base on first write). The base
   `memory.bin` is never touched.

Resume no longer reads 512 MiB up front; it touches only the pages the guest
actually uses, so resume latency drops toward GIC + vCPU restore cost alone.

### Restore disk path

Replaces today's `fs::copy` private instance with `clonefile(base/disk.img ->
inst/disk.img)` — same CoW semantics, instant. The guest opens `inst/disk.img`
read-write as it does today.

### Concurrency / clone

N concurrent `--restore` invocations of the same base each create their own
`ignition-inst-<pid>` directory with independent `clonefile` clones, so clone
independence is structural — no shared mutable state, no explicit copy loop.

## RAM size parametrization

- Add a `--mem <MiB>` flag to `boot` (default 512, i.e. the current `0x2000_0000`).
- On the **boot path**, the parsed size replaces every read of the `RAM_SIZE`
  constant: the guest-RAM `mmap` length, `layout::fdt_addr(size)`,
  `vm.map_memory(host, RAM_BASE, size)`, and the `VmConfig { mem_size, .. }` written
  into the snapshot.
- On the **restore path**, the size comes from `snap.config.mem_size` (the field
  already exists in `VmConfig`); the compiled-in constant is not consulted. Restore
  **errors** if the clone's byte length does not equal `mem_size` (guards against a
  truncated or mismatched `memory.bin`).
- `RAM_SIZE` may remain only as the literal default value for the flag.

## Snapshot write side

- Memory: unchanged — `write_snapshot` still writes the full `memory.bin`.
- Disk: the `fs::copy(disk_src, p.disk)` in `write_snapshot` switches to the same
  `clonefile`-with-fallback helper, so taking a snapshot is instant on the disk side.

## Components / files

- `crates/vmm/src/snapshot.rs`
  - New `clonefile_or_copy(src: &Path, dst: &Path) -> io::Result<()>` helper: calls
    `clonefile(2)` via an `extern "C"` declaration (`flags = 0`); on `ENOTSUP` or
    `EXDEV` (non-APFS / cross-filesystem) falls back to `fs::copy` and logs a
    warning. Used by both `write_snapshot` (disk) and the restore path (memory + disk).
  - `write_snapshot` uses the helper for the disk artifact.
- `spike/src/bin/boot.rs`
  - `--mem` flag parsing; thread the runtime size through the boot path.
  - `run_restore`: build the instance dir; `clonefile_or_copy` memory + disk into it;
    `mmap(MAP_SHARED)` the instance `memory.bin` as guest RAM instead of anon-mmap +
    `fs::read`; map exactly `snap.config.mem_size`; size-mismatch error; best-effort
    cleanup of the instance dir on exit.

## Error handling

- `clonefile` `ENOTSUP`/`EXDEV` → `fs::copy` fallback (warn once). Keeps non-APFS and
  cross-filesystem setups working without caller changes.
- `clonefile` destination-exists → treat as a stale instance; remove and retry once,
  else error.
- `mmap` failure (MAP_FAILED) → error out of restore.
- Clone size != `snap.config.mem_size` → error before mapping.
- Instance dir cleanup is best-effort (`let _ = remove_dir_all(...)`), matching the
  existing temp-disk lifetime model; a leaked instance dir is a tempdir artifact, not
  a correctness bug, because the base is never mutated.

## Feasibility gate (validate before building the rest)

The one load-bearing unknown is whether a **`MAP_SHARED` file-backed region works as
HVF guest RAM with lazy fault-in**. `MAP_SHARED` writable file mappings are ordinary,
and HVF maps host virtual-address ranges (faults resolve through the host MMU), so
this is expected to work — but it must be confirmed first. The plan's first task is a
minimal smoke check: back guest RAM with a `MAP_SHARED` file mapping on a normal
boot (no snapshot involved) and confirm the guest boots to a shell. Only after that
passes does the rest of the restore rework proceed. If it fails, stop and reassess
(macOS has no `userfaultfd`, so the fallback space is narrow and worth a separate
discussion).

## Testing

### Unit (`cargo test`, no entitlement needed)

- `--mem` flag parses to the right byte count; default is 512 MiB.
- `VmConfig` serde round-trip with a non-default `mem_size`.
- `clonefile_or_copy` on a temp file: destination content equals source; after
  editing the destination, the source is byte-identical (CoW isolation). On a
  filesystem where `clonefile` is unsupported, the fallback still produces an equal
  copy.

### Live drivers (need entitlement + real kernel/rootfs)

- `scripts/restore_test.py`:
  - **Latency benchmark (headline):** measure resume latency = process start →
    responsive login prompt, and report the new `mmap` path against the old
    eager-read path side by side.
  - **Immutability:** checksum `base/memory.bin` before and after a restore; assert
    unchanged. Same for `base/disk.img`.
  - **Clone isolation:** a write inside one restored guest does not change the base
    artifacts (re-checksum).

## Out of scope (possible future specs)

- Dirty-page tracking / diff snapshots for *size* reduction (`hv_vm_protect`
  write-protect + fault-driven dirty bitmap on the write side).
- Incremental snapshot chains (base + deltas).
- `MAP_PRIVATE` zero-extra-file variant (avoids the per-instance clone entirely) —
  deferred because it depends on unverified HVF copy-on-write-on-guest-write behavior;
  the `clonefile` + `MAP_SHARED` path is correct by construction.
