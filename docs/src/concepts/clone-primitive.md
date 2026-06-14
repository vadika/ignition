# The clone primitive

The reason ignition exists is the fast snapshot and clone-from-warm-base
primitive on bare HVF: an immutable base, lazy copy-on-write clones that idle near
0% CPU and touch only their own dirtied pages, and a microsecond-budget in-loop
reset. This chapter walks the primitive from the bottom up, in the order the
pieces were built.

## 1. Snapshot and restore

A running guest can be snapshotted and later restored into a fresh guest that
resumes from the saved PC, keeps time, accepts console input, and idles at roughly
0% CPU at its WFI. Restore loads RAM, creates the GIC and vCPUs, restores the GIC
state, applies the saved register, timer, and device state, and resumes. There is
no kernel reload and no FDT regeneration.

The on-disk format is self-describing (v2, magic `ignition-snapshot-v2`): a list
of `DeviceRecord` entries rather than a hand-listed set of device fields, guarded
by a version check that rejects older snapshots. With more than one vCPU, snapshot
is a stop-the-world rendezvous: every online core saves itself and, on restore,
resumes at its own PC.

## 2. Fast restore

Restore does not copy RAM. It uses `clonefile` to make a copy-on-write clone of the
base `memory.bin`, then maps it with `mmap(MAP_SHARED)`. Pages fault in lazily as
the guest touches them, and the immutable base is never mutated. macOS has no
`userfaultfd`, so this is the macOS analogue of Firecracker's `MAP_PRIVATE`/UFFD
restore: `clonefile` plus `MAP_SHARED` already demand-pages host-side.

## 3. Snapshot store

The store lays clones out so the base stays immutable and every instance is
isolated:

```text
snapshots/<name>/        immutable bases (memory.bin, gic.bin, vmstate.json, disk.img)
instances/<name>-<pid>/  per-instance CoW clones of the base
manifest.json            named lineage and metadata
```

A snapshot writes a base under `snapshots/<name>/`; each restore clones it into its
own `instances/<name>-<pid>/` directory. Two restores of the same base yield two
fully independent guests.

## 4. Dirty tracking on HVF

HVF has no `KVM_GET_DIRTY_LOG` and no exposed hardware stage-2 dirty bit, so dirty
tracking is the genuinely novel platform bit. ignition arms it with
`hv_vm_protect`: it drops `HV_MEMORY_WRITE` on the guest RAM pages, so the first
write to each clean page traps. The trap arrives as a Data Abort (EC `0x24`) whose
faulting IPA is exactly the dirtied page; ignition marks the page dirty, re-grants
write permission, and resumes **without** advancing the PC so the store
re-executes.

Two hardware facts shaped this. The protect granule is 16 KiB (the Apple Silicon
host page); a 4 KiB sub-range is rejected with `HV_BAD_ARGUMENT`, so the dirty
bitmap is one bit per 16 KiB page. And HVF reports these as translation faults
(DFSC `0x07`/`0x0f`), not permission faults, so the dirty path keys off "write data
abort whose faulting address lands inside the RAM region" rather than a specific
DFSC sub-code. Measured cost is roughly 4.9 µs per first-write fault, one vmexit
per first write to each page per interval.

## 5. Diff / incremental snapshots

With dirty tracking armed, a restored guest can write a Diff layer that contains
only the pages it changed, with its `parent` set to the leaf it restored from. The
result is an immutable delta chain rather than a base file that mutates in place.
Restore reassembles the guest transparently by layering the root base plus each
diff in order.

## 6. In-loop `reset()`

The fuzzer needs to roll a live guest back to a known state on every iteration,
inside the running VMM, with a microsecond budget. The in-loop `reset()` does this
entirely in memory: it copies back only the dirtied pages and restores the vCPU
registers, with no disk, no format, and no versioning. It reuses the dirty-tracking
substrate, so the work per reset is proportional to the dirty set, not to total
RAM. Measured reset p50 is about 36 µs (page-copy roughly 35 µs plus register
restore roughly 1 µs).

## See also

- [Snapshot & restore](../features/snapshot-restore.md)
- [Diff / incremental snapshots](../features/diff-snapshots.md)
- [How snapshot fuzzing works](../fuzzing/overview.md)
