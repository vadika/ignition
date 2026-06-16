# Disposable browser

ignition can run a throwaway Firefox-ESR kiosk in a microVM where every write
lands in guest RAM, never touches the disk, and a single hotkey resets the session
back to a warm homepage — without reloading the kernel or replaying the overlay
boot path.

## What it is

Each browser session is an independent clone of a pre-warmed snapshot. The guest
boots once (the "cold boot"), Firefox opens on the homepage, and that moment is
frozen as the `browser-base` snapshot. From then on every session is a
sub-second restore: the kernel and overlay setup are already baked in. Closing
the window tears the clone down. The base snapshot is never mutated.

Fan-out is first-class: `disposable-browser.sh -n N` starts N independent
clones in parallel, each with its own macOS window, its own copy-on-write
instance directory, and (under `--net`) its own MAC address and DHCP lease.

## The overlay-root model

The browser rootfs is designed to keep the backing ext4 image read-only
throughout the life of every session. On the cold boot, `init=/sbin/overlay-init`
runs before the normal init: it mounts the ext4 device read-only as the lower
layer of an overlay filesystem, places a tmpfs as the upper layer, and
`switch_root`s into the merged view. The design intent is that `overlay-init`
uses a `pivot_root`-style sequence to make the merged overlay the new root, with
`/tmp`, the browser profile directory, and any download paths living in the tmpfs
upper layer.

The consequence is that **every write the guest makes — browser cache, cookies,
history, tab state — lives in guest RAM and only in guest RAM**. The ext4 image
is never written.

This is what makes `Ctrl-A r` safe. The [interactive reset-to-checkpoint](snapshot-restore.md#interactive-reset-to-checkpoint)
mechanism rolls back guest RAM, vCPU registers, GIC state, and virtio-device
state to a saved point. For that rollback to be correct, the disk must not have
diverged between the checkpoint and the reset. The overlay root guarantees this
invariant: there is nothing to diverge. As the snapshot-restore page puts it, the
intended usage "mounts the rootfs read-only and places all writable state on a
tmpfs overlay that lives in guest RAM" — that is exactly the arrangement
`overlay-init` establishes.

When `boot --restore <dir>` starts, the restored snapshot is automatically
installed as the initial reset point, so `Ctrl-A r` works from the first
keystroke without needing a prior `Ctrl-A c`.

## Build rootfs-browser.ext4

The browser rootfs is built by `kimage/build/build-rootfs-browser.sh`. See
[Building guest assets](../getting-started/guest-assets.md#rebuild-the-browser-rootfs)
for the full scp/ssh/scp workflow. The short version:

```bash
cd kimage
scp build/build-rootfs-browser.sh build/devmem.c artemis2:~/kbuild/
ssh artemis2 'cd ~/kbuild && chmod +x build-rootfs-browser.sh && HOMEPAGE=https://duckduckgo.com ./build-rootfs-browser.sh'
scp artemis2:'~/kbuild/out/rootfs-browser.ext4' out/rootfs-browser.ext4
```

The `HOMEPAGE` build argument sets the URL Firefox opens on first paint. The
rootfs ships `overlay-init` at `/sbin/overlay-init`, which the cold boot
activates via `--append "init=/sbin/overlay-init"`.

## Create the warm-base snapshot

This is a one-time step. After the warm base exists, sessions restore from it
instead of cold-booting.

### Helper script (recommended)

```console
sudo scripts/make-browser-base.sh
```

The script cold-boots the browser rootfs with `--gui --net --track-dirty --mem
1024` and `init=/sbin/overlay-init`, watches the serial console for the
`BROWSER_READY` signal that the guest emits when Firefox has painted the
homepage, sends `Ctrl-A s` to snapshot the live guest as `browser-base`, waits
for the snapshot write to complete, then exits. No manual timing is required.

An optional snapshot name can be passed as the first argument:

```console
sudo scripts/make-browser-base.sh my-base
```

### Manual flow

If you prefer to watch the boot yourself and choose when to snapshot:

```console
sudo target/debug/boot --gui --net --track-dirty --mem 1024 --name browser-base \
     --append "init=/sbin/overlay-init" kimage/out/Image kimage/out/rootfs-browser.ext4
```

Pass `--name browser-base` so the snapshot you take is written under that name
(the name `disposable-browser.sh` restores by default). Wait for the Firefox
window to paint the homepage (the design intent is that the guest prints
`BROWSER_READY` on the serial console and the llvmpipe software renderer presents
the first frame in the macOS window). Once it looks right, press `Ctrl-A s` to
write the snapshot, then `Ctrl-A x` to quit. (`Ctrl-A s` writes immediately under
`--name`; there is no name prompt. Without `--name` the snapshot gets an
auto-generated name, which `disposable-browser.sh` will not find.)

The cold boot passes `--append "init=/sbin/overlay-init"` to hand control to
the overlay setup before normal init. `--track-dirty` arms write-protect dirty
tracking so the snapshot records only the pages that changed. Restore does not
reload the kernel or re-run the overlay pivot; it resumes from the frozen moment.

## Run a disposable session

```console
scripts/disposable-browser.sh
```

This restores one clone of `browser-base`: a GUI window opens with Firefox at
the homepage. Under the hood it runs:

```console
target/debug/boot --gui --net --mem 1024 --track-dirty --restore browser-base
```

`--net` is included by default; because vmnet shared mode requires elevated
privileges, run under sudo when you want networking:

```console
sudo scripts/disposable-browser.sh
```

### Fan-out: N independent sessions

```console
sudo scripts/disposable-browser.sh -n 3
```

This launches 3 clones in parallel, each with its own macOS window. Under
`--net` each clone gets a distinct MAC address and its own DHCP lease —
the browser rootfs carries the same `netwatch` carrier-poller as the GUI
rootfs, which rebinds virtio-net on restore and re-runs DHCP. Without
`--net` (no sudo) the clones are isolated but share the host network stack.

The base snapshot is never written; each clone's copy-on-write instance
directory is private and keyed by the clone's pid. Closing a clone's window
tears down only that guest. If the script is killed with `Ctrl-C` it cleans
up all child processes.

A non-default base name or additional `boot` flags can be passed after the
clone count:

```console
sudo scripts/disposable-browser.sh -n 2 my-base --store /data/vmstore
```

## Reset a session in place

Press **`Ctrl-A r`** inside any running clone to roll it back to the warm
homepage. Guest RAM, vCPU registers, GIC state, and virtio-device state are
all restored to the snapshot moment; the macOS window repaints the resumed
screen. Browser history, cookies, cache, open tabs, and any downloads evaporate
— they lived only in the tmpfs upper layer, which is part of the guest RAM that
just rolled back. The same microVM keeps running; there is no restart, no kernel
reload, and no new window.

Because the rootfs ext4 is read-only throughout, disk and RAM are always
consistent at the checkpoint, so the reset is always safe.

## Memory and resource footprint

`--mem 1024` (1 GiB) is the default for both `make-browser-base.sh` and
`disposable-browser.sh`. For N clones the RAM cost is approximately N GiB of
guest-visible address space, though Apple Silicon memory compression and the
CoW instance directories mean the actual resident footprint is lower in
practice. The `rootfs-browser.ext4` disk image is shared read-only across all
clones — only the per-clone tmpfs upper layer (in guest RAM) diverges.

## Related

- [Snapshot & restore](snapshot-restore.md) — the restore and fan-out mechanism,
  and the full `Ctrl-A c` / `Ctrl-A r` interactive checkpoint behaviour.
- [Devices, SMP & networking](devices.md) — `--gui`, `--net`, virtio-gpu, and
  the `netwatch` carrier-poller.
- [Building guest assets](../getting-started/guest-assets.md) — kernel config
  requirements and the artemis2 build workflow.
