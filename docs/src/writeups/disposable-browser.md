# A browser you can throw away in 130 milliseconds

I built a disposable browser on Apple Silicon. Every session is a fresh Firefox
running in its own microVM, cloned from a warm snapshot, networked, and discarded
when you close the window. One keystroke resets it to a clean homepage.

What makes it fast is the snapshot. Cold-booting the VM and waiting for Firefox to
paint its homepage takes about 7.8 seconds. Restoring an already-running,
already-loaded Firefox from a snapshot takes 130 milliseconds. That is roughly 60x.
The mechanism is an APFS clonefile plus a copy-on-write memory map, so restore does
almost no work up front and pages fault in lazily as you browse.

The measurements, on an M-series Mac, 2 GiB guest, two cores:

- Cold boot to a painted homepage: 7774 ms (range 7618 to 8084 over three runs)
- Kernel plus early init alone: 599 ms (the rest of that 7.8 s is Firefox starting)
- Restore from snapshot to a running guest: 130 ms (range 127 to 131)

Two things stand out. The restore is flat: 127 to 131 ms regardless of what the
browser was doing when the snapshot was taken, because nothing large is read up
front. And almost all of the 130 ms is one specific cost. Breaking down a 132 ms
restore: 93 ms is rebuilding the virtio device set (gpu, net, block, input), 39 ms is
reattaching stdin, and everything else is under a millisecond. So if I ever want it
faster, I know exactly where to look.

The disk never changes. The root filesystem is read-only, with a tmpfs overlay
holding everything writable: profile, cache, downloads. All session state lives in
RAM. Reset throws the RAM away and you are back to a clean machine. Nothing to scrub.

Getting a window on screen at all was its own piece of work. The microVM started life
headless, just a serial console. Putting a real browser in front of someone meant the
guest needed a display and a way to take input. That was two evenings.

The first evening was the display: a virtio-gpu device, 2D only, no Metal and no GL.
The guest's framebuffer renders into a host buffer, and the host blits that into a
plain macOS window. Two commands carry the whole path, one to copy guest pixels out,
one to present a frame. The second evening was input: two virtio-input devices, a
keyboard and an absolute pointer, translating macOS key and mouse events into Linux
evdev events and injecting them into the guest. After that the window could be typed
into and clicked, the cursor tracked one to one, and cage could fullscreen Firefox
inside it. Modest scope on purpose, and that is exactly why it landed in two sittings.

None of it is fast in the GPU sense. It is a software framebuffer. But for a browser
you reset constantly, predictable beats clever, and a CPU blit you fully understand is
easier to reason about than a render path you don't.

Built on Hypervisor.framework, in Rust, sharing zero lines with Firecracker.

## Where the pieces live

- [Disposable browser](../features/disposable-browser.md) — the overlay-root model,
  building `rootfs-browser.ext4`, the warm-base snapshot, fan-out, and the cold-reset
  (relaunch) hotkey.
- [GUI display](../features/gui-display.md) — the virtio-gpu (2D) + virtio-input stack
  and the cage compositor behind the window.
- [Snapshot & restore](../features/snapshot-restore.md) — the clonefile + `MAP_SHARED`
  restore path these numbers measure.
- [Boot & restore latency](../benchmarks/boot-and-restore.md) — the full benchmark
  table the figures above come from.
