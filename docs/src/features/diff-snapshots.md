# Diff / incremental snapshots

A diff snapshot writes only the guest RAM pages that changed since the base, instead
of dumping all of RAM every time.

## Arming dirty tracking with `--track-dirty`

`--track-dirty` arms write-protect dirty tracking. Guest RAM is mapped read-only and
the first write to each 16 KiB page traps as a data abort, faults the page back to
writable through `hv_vm_protect`, and marks it dirty. The faulting guest IPA is exactly
the page address the tracker needs, so the store re-executes after the page is granted
write access (the PC does not advance). 16 KiB is the tracking granule because it
matches the Apple Silicon host page; `hv_vm_protect` rejects sub-page ranges. HVF has
no native dirty-bitmap API, so write-protect plus data-abort interception is the only
precise dirty mechanism on the platform.

## The delta-chain model

A restored guest armed this way writes a Diff layer on `Ctrl-A s`. The layer records:

- `parent` = the leaf it was restored from.
- Only the changed RAM pages (RAM is the only deltified state).
- vmstate, the GIC blob, and device records, always written full per layer.

Layers form an immutable delta chain rooted at a full base. The runtime cost is one
vmexit per first write to each clean page (about 5 microseconds per fault, measured),
amortized because each page faults at most once per interval. Snapshotting under the
same name as the parent, or the base it was restored from, is refused without
`--force`.

## Restore reassembly

Restore reassembles the chain transparently: `clonefile` the root base, then overlay
each diff's pages in order. Because the base is cloned with copy-on-write and the
deltas are layered at restore time, the chain stays immutable at rest.

## Example

```console
# boot armed for diff tracking, snapshot a root, then restore + diff-snapshot
target/debug/boot --store vmstore --name base --track-dirty kimage/out/Image kimage/out/rootfs.ext4
target/debug/boot --store vmstore --restore base --track-dirty --name base-diff

# full cycle: diff ~3% of RAM, mutation survives, bases immutable
python3 scripts/diff_snapshot_test.py
```

Worked example of one warm golden base fanning out into many cheap divergent forks:
[diff-snapshot-fanout.md](https://github.com/vadika/ignition/blob/main/examples/diff-snapshot-fanout.md).

## Related

- [The clone primitive](../concepts/clone-primitive.md) — dirty tracking and the delta chain.
- [Snapshot & restore](snapshot-restore.md) — the full snapshot this builds on.
- [Diff-snapshot benchmarks](../benchmarks/diff-snapshots.md) — tracking overhead and sizes.
