# Fast Restore via clonefile + mmap — Design

Date: 2026-06-13
Status: approved (brainstorming) → ready for plan

## Goal

Cut microVM **resume latency** by loading guest memory lazily instead of eagerly
reading the whole RAM dump on restore, while making the snapshot a **first-class
immutable base**: a restore must never mutate the snapshot directory. As a
byproduct, parametrize guest RAM size (today a hardcoded compile-time constant) so
restore reconstructs the exact size the snapshot was taken with.

This work also introduces a **light snapshot-placement convention** (a "VM store"
directory with named bases + ephemeral instances + a per-base manifest), aligning
ignition with Firecracker's model where the VMM provides immutable-base primitives
and a thin layer above organizes them. Diff/incremental snapshots for *size*
reduction (dirty-page tracking) and full snapshot *management* (parent/child diff
chains, GC, listing) are explicitly **out of scope** here — this work targets restore
*latency*, immutability, and a minimal placement convention. The write side still
dumps full memory.

## Firecracker reference

Firecracker's snapshot is two caller-named files — a **state file** (vCPU/device
state) and a flat **memory file**. Restore `mmap`s the memory file `MAP_PRIVATE`
(file backend) so guest writes are copy-on-write and the base file is never mutated —
one base restores into many microVMs. (Its UFFD backend serves pages from a separate
process for demand paging; macOS has no `userfaultfd`, so it is not an option here.)
Firecracker does **not** snapshot the disk and imposes **no** directory layout —
placement/naming/lifecycle live in the orchestrator above it. ignition mirrors the
immutability guarantee with `clonefile` + `MAP_SHARED` (APFS gives CoW for free,
sidestepping the unverified MAP_PRIVATE-as-HVF-guest-RAM question), bundles the disk
for self-containment, and bakes a *light* placement convention into the VMM rather
than leaving it wholly to a caller.

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

## Snapshot placement convention (the "VM store")

A `--store <dir>` root (default `./vmstore`) organizes named bases and their
ephemeral instances:

```
<store>/
  snapshots/<name>/             immutable base (restore opens read-only)
    memory.bin  gic.bin  disk.img  vmstate.json  manifest.json
  instances/<name>-<pid>/        ephemeral CoW clone (best-effort removed on clean exit)
    memory.bin  <- clonefile(base/memory.bin)   APFS metadata CoW, O(1)
    disk.img    <- clonefile(base/disk.img)      APFS metadata CoW, O(1)
```

- `--name <name>` (default `default`) selects which base to write/read.
- **Boot + `Ctrl-A s`** writes the base to `<store>/snapshots/<name>/`, including a
  `manifest.json`: `{ name, created, mem_size, vcpu_count }` (`created` = seconds
  since the Unix epoch). The manifest is human/management metadata distinct from the
  machine state in `vmstate.json`, and the seed for future listing/GC.
- **`--restore <name>`** reads `<store>/snapshots/<name>/` and materializes the
  instance under `<store>/instances/<name>-<pid>/`.
- This replaces the old `--snap-dir <dir>` / `--restore <dir>` direct-path flags.
- No diff chains, no GC, no `list` command yet — those belong to the deferred
  snapshot-management spec.

## Approach: instance = copy-on-write clone of an immutable base

A base snapshot directory is treated as **read-only**. Every `--restore` materializes
an ephemeral **instance directory** (under `<store>/instances/`) whose memory and disk
are APFS `clonefile(2)` copies of the base; the instance is what the running guest
mutates. The base is never written.

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
`<store>/instances/<name>-<pid>` directory with independent `clonefile` clones, so
clone independence is structural — no shared mutable state, no explicit copy loop.

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

## Snapshot naming (auto-generated fancy names)

When `--name` is omitted, a memorable name is generated instead of forcing the user
to pick one: `crates/vmm/src/names.rs` holds ~40 adjectives × ~40 scientist surnames
(~1600 combos) and `generate() -> String` returns `"adjective-surname"` (e.g.
`brave-hopper`). The seed mixes `SystemTime` nanoseconds with the pid through a small
splitmix64 step — no `rand` dependency. On collision (target `snapshots/<name>/`
already exists) it regenerates a few times, then falls back to suffixing `-2`, `-3`,
… The resolved name is always printed at snapshot time:
`snapshot 'brave-hopper' written to <store>/snapshots/brave-hopper/`, so the operator
knows what to pass to `--restore`.

Auto-naming is what makes re-snapshot immutability-safe with zero ceremony: an
omitted name yields a fresh unique base every time, so a restored guest's snapshot
can never collide with (and thus never overwrite) the base it was restored from.

## Re-snapshot (snapshotting a restored guest)

Today the restore path installs no snapshot handler, so `Ctrl-A s` in a restored
guest is a no-op. This work **wires the same snapshot handler onto the restore path**,
so a restored guest can be snapshotted into a new base. The only immutability guard
needed: if the user passes an explicit `--name` equal to the name the guest was
restored from, refuse the write unless `--force` is also passed (prevents clobbering
the base you are currently running on). An omitted (generated) name never triggers
this guard.

## Snapshot write side

- Memory: unchanged — `write_snapshot` still writes the full `memory.bin`.
- Disk: the `fs::copy(disk_src, p.disk)` in `write_snapshot` switches to the same
  `clonefile`-with-fallback helper, so taking a snapshot is instant on the disk side.
- Manifest: `write_snapshot` also writes `manifest.json`
  (`{ name, created, mem_size, vcpu_count }`, `created` = Unix-epoch seconds) into the
  base dir. A `SnapshotManifest` struct lives in `crates/vmm/src/snapshot.rs`.

## Components / files

- `crates/vmm/src/names.rs` (new) — `generate() -> String` fancy-name generator
  (adjective-surname word lists + splitmix64 over time/pid). Exported from the crate.
- `crates/vmm/src/snapshot.rs`
  - `clonefile_or_copy(src, dst) -> io::Result<()>` helper (done in Task 1): calls
    `clonefile(2)` with `fs::copy` fallback on `ENOTSUP`/`EXDEV`/`ENOSYS`. Used by
    `write_snapshot` (disk) and the restore path (memory + disk).
  - `SnapshotManifest { name, created, mem_size, vcpu_count }` struct + serde; helpers
    to write/read `manifest.json`.
  - Store-path resolution helpers: `base_dir(store, name)` → `<store>/snapshots/<name>`,
    `instance_dir(store, name, pid)` → `<store>/instances/<name>-<pid>`.
  - `write_snapshot` uses the helper for disk and writes the manifest.
- `spike/src/bin/boot.rs`
  - Flags: `--store <dir>` (default `./vmstore`), `--name <name>` (optional),
    `--mem <MiB>` (default 512), `--force`. Remove `--snap-dir`; change `--restore` to
    take a `<name>`.
  - Boot path: thread the runtime RAM size through; resolve the base dir from
    store+name (generating a name when omitted); install the snapshot handler that
    writes to the resolved base and prints the name.
  - `run_restore(store, name, force, ...)`: read base; build the instance dir under
    `<store>/instances/<name>-<pid>`; `clonefile_or_copy` memory + disk into it;
    `mmap(MAP_SHARED)` the instance `memory.bin` as guest RAM (no anon-mmap + `fs::read`);
    map exactly `snap.config.mem_size`; size-mismatch error; install the snapshot
    handler too (re-snapshot), with the same-name-as-source guard (`--force` to
    override); best-effort cleanup of the instance dir on clean exit.

## Error handling

- `clonefile` `ENOTSUP`/`EXDEV` → `fs::copy` fallback (warn once). Keeps non-APFS and
  cross-filesystem setups working without caller changes.
- `clonefile` destination-exists → treat as a stale instance; remove and retry once,
  else error.
- `mmap` failure (MAP_FAILED) → error out of restore.
- Clone size != `snap.config.mem_size` → error before mapping.
- Instance dir cleanup is best-effort (`let _ = remove_dir_all(...)`); a leaked
  instance dir under `<store>/instances/` is harmless because the base is never mutated.
- Name collision on snapshot write: regenerate a generated name a few times, then
  suffix `-2`/`-3`/…; an explicit `--name` that already exists is overwritten as today
  (the operator named it), EXCEPT the re-snapshot same-name-as-source case, which is
  refused unless `--force`.

## Feasibility gate (validate before building the rest)

The one load-bearing unknown is whether a **`MAP_SHARED` file-backed region works as
HVF guest RAM with lazy fault-in**. `MAP_SHARED` writable file mappings are ordinary,
and HVF maps host virtual-address ranges (faults resolve through the host MMU), so
this is expected to work — but it must be confirmed first. It is validated at the
earliest point a file-backed mapping exists: the restore-memory task wires
`clonefile` + `mmap(MAP_SHARED)` and is immediately live-tested (restore must resume
to a responsive prompt) before any later task builds on it. If it fails, stop and
reassess (macOS has no `userfaultfd`, so the fallback space is narrow and worth a
separate discussion).

## Testing

### Unit (`cargo test`, no entitlement needed)

- `clonefile_or_copy` CoW isolation (done in Task 1).
- `VmConfig`/`SnapshotManifest` serde round-trip with a non-default `mem_size`.
- `names::generate()` returns a non-empty `adjective-surname` string; two successive
  calls within the same process differ (the seed advances) — at minimum the function
  is deterministic-given-seed and well-formed.
- Store-path helpers: `base_dir`/`instance_dir` produce the documented paths.

### Live drivers (need entitlement + real kernel/rootfs)

- `scripts/restore_test.py` (updated to the `--store`/`--name` CLI):
  - **Latency benchmark (headline):** measure resume latency = process start →
    responsive login prompt; print it (compare against a pre-change run to quantify).
  - **Immutability:** checksum `snapshots/<name>/memory.bin` + `disk.img` before and
    after a restore; assert unchanged.
  - **Re-snapshot:** from a restored guest, `Ctrl-A s` writes a NEW base (generated
    name) and the source base is unchanged.

## Out of scope (possible future specs)

- Dirty-page tracking / diff snapshots for *size* reduction (`hv_vm_protect`
  write-protect + fault-driven dirty bitmap on the write side).
- Incremental snapshot chains (base + deltas).
- `MAP_PRIVATE` zero-extra-file variant (avoids the per-instance clone entirely) —
  deferred because it depends on unverified HVF copy-on-write-on-guest-write behavior;
  the `clonefile` + `MAP_SHARED` path is correct by construction.
