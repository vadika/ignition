# Large-page-backed snapshot memory — investigation findings

_Investigation outcome. Status: **not viable on Apple Silicon** — no prototype. Date: 2026-06-18._

## Question

Does backing guest RAM with large pages (the forkd PR #230 "Back Mem Snapshot with
Hugepages" approach, 2 MiB hugetlb) reduce TLB pressure during clone fan-out and
the bulk memory-image copy on ignition (macOS / Apple Silicon / Hypervisor.framework)?

## Conclusion (do this first, per the task)

**Apple Silicon macOS exposes no explicit userspace large-page lever, so the forkd
approach does not port. No prototype is warranted.** The task therefore reduces to
documenting this and noting that base-page behavior is already what ships.

## Evidence (empirical, on this machine)

Base page size is **16 KiB**, not 4 KiB:

```
$ sysctl hw.pagesize  ->  hw.pagesize: 16384
```

The `VM_FLAGS_SUPERPAGE_*` macros exist in the SDK header
(`mach/vm_statistics.h`: `VM_FLAGS_SUPERPAGE_SIZE_2MB`, `..._SIZE_ANY`,
`VM_FLAGS_SUPERPAGE_MASK` is even listed in `VM_FLAGS_USER_ALLOCATE`), but that
header is shared across architectures. On arm64 the VM subsystem **rejects** the
flag. Direct probe (`mach_vm_allocate`, 2 MiB region, `VM_FLAGS_ANYWHERE | …`):

```
SUPERPAGE_SIZE_2MB: kr=4 ((os/kern) invalid argument)
SUPERPAGE_SIZE_ANY: kr=4 ((os/kern) invalid argument)
```

`KERN_INVALID_ARGUMENT` for both. Superpages were historically an x86-64-only XNU
feature; arm64 never implemented the userspace lever.

The Hypervisor.framework allocation API has no large-page option either —
`hv_vm_allocate(uvap, size, flags)` defines only `HV_ALLOCATE_DEFAULT = 0`
(`Hypervisor/hv_vm_allocate.h`). `hv_vm_map(uva, gpa, size, flags)` takes only
permission flags (`HV_MEMORY_{READ,WRITE,EXEC,…}`, `hv_types.h`); the host backing
and its page size are fixed by how the host UVA region was allocated, not by the
map call. There is no Darwin equivalent of `memfd_create` + `MFD_HUGETLB`,
`hugetlbfs`, or `madvise(MADV_HUGEPAGE)`.

What remains on arm64 is **transparent** behavior only: XNU may coalesce TLB
entries / use larger leaf mappings opportunistically (and HVF chooses the stage-2
granule for the guest IPA map), but none of this is userspace-controllable or
guaranteed, so there is nothing to opt into, measure as an A/B, or fall back from.

## Why the forkd mechanism does not transfer

forkd (Linux/KVM/Firecracker) backs its snapshot memory with a hugetlb memfd and
populates it with the mmap-dest + mmap-source + `memcpy` dance (because hugetlb
memory cannot be `write()`'d). The transferable *shape* (populate by mmap+memcpy,
round size up to page granularity with a zero-fill tail, degrade to base pages,
preflight check) all presupposes a hugetlb backing object to point at. Apple
Silicon has no such object and no flag to request one, so every transferable lesson
is moot here.

## Why the motivation is also weaker here than on Linux

Even setting aside the missing lever, the premise is softer on this platform:

1. **16 KiB base pages already give 4x the TLB reach** of Linux's 4 KiB base — the
   exact pressure hugepages relieve on Linux is already a quarter as severe before
   any large-page work.
2. **ignition's fast clone path does no bulk memcpy.** Restore maps the immutable
   base `memory.bin` `MAP_PRIVATE` and demand-pages copy-on-write
   (`spike/src/bin/boot.rs` `run_restore`, shipped 2026-06-18); there is no
   per-clone image copy to accelerate. Bulk copies survive only on the checkpoint
   path (owned heap copy) and diff-chain reassembly, not the common fan-out.
3. forkd shipped its hugepage harness but **published no results** — its TLB-pressure
   claim is unmeasured even on Linux, so there is no external number suggesting the
   win is large enough to chase through a missing-API workaround.

## Guest RAM backing today (for the record)

All 16 KiB base pages, `mmap`-backed, mapped via `hv_vm_map`:

- Fresh boot: `mmap(MAP_ANON | MAP_PRIVATE)` (`boot.rs` ~1030/1575).
- Restore: `mmap(base memory.bin, MAP_PRIVATE)` then CoW overlay of diffs
  (`boot.rs` `run_restore` ~1983).
- Reset pristine: `mmap(file, MAP_PRIVATE)` read-only, or an owned heap copy
  (`crates/vmm/src/reset.rs`).

None carry a page-size flag, and none can: arm64 rejects the only one that exists.

## Recommendation

Do not prototype a large-page backend. If clone/restore latency needs further work,
the productive levers on this platform are page-in / cache-warmth (already addressed
by the MAP_PRIVATE-over-base change) and stage-2 mapping behavior inside HVF, not
userspace large-page allocation. Revisit only if a future macOS exposes an arm64
superpage or hugetlb-equivalent API (`hv_vm_allocate` gaining a flag would be the
signal to watch).

## Reproduce

```
sysctl hw.pagesize
# probe: mach_vm_allocate(2 MiB, VM_FLAGS_ANYWHERE | VM_FLAGS_SUPERPAGE_SIZE_2MB)
#        -> KERN_INVALID_ARGUMENT on arm64 macOS
```
