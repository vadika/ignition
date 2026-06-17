# Per-clone RNG reseed (vmid)

Fan out N microVMs from one warm snapshot and every clone resumes with the same
kernel CRNG state — it was captured in the snapshot's RAM. Until the guest kernel
reseeds, sibling clones can hand out the same "random" bytes: the same UUIDs, TLS
session keys, nonces. vmid closes that window by giving each restored clone a fresh
seed.

## The mechanism

On every `boot --restore`, once the restored vCPUs are live, the host draws 32
fresh bytes from `getentropy` and pushes them to the guest over the
[vsock](devices.md) control channel it already has. A small guest program mixes
them into the kernel entropy pool and forces an immediate reseed. No ACPI, no
kernel module.

The host side connects to the control socket, sends `CONNECT 9000`, waits for the
`OK` ack, and writes a 37-byte frame:

```
"VMID" (4 bytes) | version 0x01 (1 byte) | seed (32 bytes)
```

It is best-effort. It retries for about three seconds while the guest scheduler
brings the listener back after resume, then logs a warning and continues. Restore
never blocks on the push, and the run loop is unaffected if it fails — the guest
still gathers entropy the usual way, just later.

The guest side is `socat VSOCK-LISTEN:9000,fork EXEC:/usr/bin/vmid-reseed`, started
from `/etc/local.d` at boot so it is already listening when the snapshot is taken
and resumes listening on every restore. `vmid-reseed` reads the frame, checks the
magic and version, then does `ioctl(RNDADDENTROPY)` (mix the seed in, credit 256
bits) followed by `ioctl(RNDRESEEDCRNG)` (force the reseed now). A short or
malformed frame is rejected before any ioctl, so a bad message can never reseed the
pool with garbage.

x86 systems solve this with the ACPI vmgenid device and the in-kernel driver that
reseeds when a generation ID changes. This VMM emits a device tree, not ACPI, and
the upstream vmgenid driver binds only via ACPI — so vmid carries the seed over
infrastructure the project already has rather than adding an ACPI layer.

## Engaging it

vmid only runs when the guest actually has a vsock device, which means passing
`--vsock-uds <path>` to both the cold boot that builds the base and to the restore.
`scripts/disposable-browser.sh` gives each clone its own socket, so the
[disposable browser](disposable-browser.md) gets reseeding for free.
`scripts/fanout-gui.sh` does not pass one, so those demo clones are not reseeded
(noted in the script). Pass `--no-reseed` to skip the push — useful for debugging
or for measuring the unseeded window.

## What the live test showed

`scripts/vmid_live_proof.py` runs the whole path on HVF (plain rootfs, vsock only,
no networking): cold-boot and snapshot a base, then restore clones. The mechanism
works — restore takes about a millisecond, the host logs the seed push, and two
reseeded clones produce different `/dev/urandom` output.

The more interesting result is the negative one. The shared-CRNG bug does **not**
reproduce observably on this platform, even with virtio-rng disabled. The guest CPU
exposes no arch RNG (no `RNDR`), and `random: crng init done` fires at boot from the
fixed device-tree seed, so the CRNG state genuinely is identical across clones at
the instant of resume. But the kernel mixes interrupt-timing entropy and reseeds
within the first scheduling quantum after resume, before any userspace program can
read `/dev/urandom`, so even two un-reseeded siblings diverge. The window vmid
closes is real but sub-millisecond here.

So vmid is correct, cheap insurance. It matters most for a guest that generates
randomness in early userspace before interrupts start flowing, for
deterministic-replay scenarios, and on platforms or configs without continuous
interrupt-entropy mixing. On bare HVF aarch64 with this kernel, its practical
necessity is, by measurement, low — but the cost of carrying it is a 37-byte write
on restore, so it stays on by default.
