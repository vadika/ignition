# vmid — per-clone CRNG reseed on restore

_Design spec. Status: approved, awaiting implementation plan._

## Problem

`boot --restore` brings up a guest from a snapshot taken after the guest fully booted.
The kernel CRNG state lives in `memory.bin` and is restored byte-for-byte, so every clone
fanned out from a single base resumes with **identical CRNG state**. Until the guest kernel
performs its next scheduled reseed, sibling clones can emit identical "random" output —
UUIDs, TLS session keys, nonces, `getrandom()` results.

What partially masks this today:

- `virtio-rng` is stateless (`crates/devices/src/virtio/rng.rs:14` calls `getentropy()` per
  RX notify), so each clone pulls fresh host entropy *whenever the guest touches the device*.
- Each clone gets a fresh MAC (`crates/vmnet/src/vmnet_shim.c:38`).

Neither closes the window: the kernel does not know it was forked, so it trusts its existing
CRNG pool and will not reseed *immediately* on restore. The exposure is the interval between
restore and the next kernel reseed. Low risk for the disposable browser (Firefox gathers its
own entropy, clones reset constantly). Real risk for the adoption-track **MCP agent-sandbox**
direction (fork-per-conversation with shared crypto state is a correctness hole).

On x86 / ACPI systems the standard mechanism is the `vmgenid` device: the VMM exposes a
128-bit generation ID, bumps it on restore, and the mainline Linux `vmgenid` driver force-
reseeds the CRNG (`add_vmfork_randomness()` → `crng_reseed()`). This codebase emits **FDT,
not ACPI**, and the upstream `vmgenid` driver binds only via ACPI, so the standard driver
will not load here. vmid achieves the same effect over infrastructure this project already
has.

## Approach

The host already orchestrates every restore and already runs a vsock control plane. On every
`--restore`, the host pushes a fresh per-instance seed to the guest over the existing vsock
control channel; a small guest userspace daemon force-reseeds the kernel CRNG. No kernel
module, no ACPI, no new transport.

Pure userspace on both sides:

- **Host side** — generate 32 random bytes, push them over the control UDS after the vCPUs
  go live.
- **Guest side** — a tiny static C daemon (`vmidd`) listening on a fixed `AF_VSOCK` port,
  reseeding via `RNDADDENTROPY` + `RNDRESEEDCRNG` ioctls. Already running at snapshot time
  (OpenRC autostart), so it resumes listening on every restore.

### Scope

In scope: the per-clone CRNG reseed described above. The reseed window after restore is the
only thing this closes.

Out of scope (related, separate work — note, do not build here):

- The hardcoded FDT `RNG_SEED` constant (`crates/arch/src/aarch64/fdt.rs:146`) is identical
  across *cold boots*. That is a cold-boot seeding concern, not the clone concern (the
  snapshot is taken post-boot, after that seed is already consumed). A follow-up could make
  the host write per-boot random bytes into the FDT seed; it is independent of vmid.

## Components

### a. Guest agent `vmidd` (C, static)

A small C binary, modeled on the existing `devmem.c` precedent installed at `/usr/bin/devmem`.

- Open `socket(AF_VSOCK, SOCK_STREAM, 0)`, bind `sockaddr_vm { svm_cid = VMADDR_CID_ANY,
  svm_port = VMID_PORT }`, `listen`, loop on `accept`.
- `VMID_PORT = 9000` (dedicated; avoids the E2 test port 6000).
- Per connection: read the framed message (see Wire format), validate magic + version +
  length. On valid:

  ```c
  struct { int entropy_count; int buf_size; unsigned char buf[32]; } pool;
  pool.entropy_count = 256;  /* bits credited */
  pool.buf_size = 32;
  memcpy(pool.buf, seed, 32);
  int fd = open("/dev/random", O_RDWR);
  ioctl(fd, RNDADDENTROPY, &pool);   /* mix new bytes + credit entropy */
  ioctl(fd, RNDRESEEDCRNG);          /* force immediate CRNG reseed */
  close(fd);
  ```

  Runs as root (Alpine init), so `CAP_SYS_ADMIN` for both ioctls is satisfied. `RNDRESEEDCRNG`
  requires the guest kernel ≥ 5.10 (the project's guest kernel is 6.x — satisfied).
- Malformed / short message → ignore, keep listening. ioctl failure → log to console,
  continue. The daemon never exits on a bad message.

Installed at `/usr/bin/vmidd`.

### b. OpenRC service

A `/etc/local.d/vmidd.start` script that launches `vmidd` **in the background** at guest
boot, mirroring the existing boot-time service pattern (`netwatch.start`, `boottime.start`).
`vmidd` is a long-running daemon, so the start script backgrounds it (`vmidd &`) rather than
blocking boot. Because it starts at boot, the daemon is **already listening when the snapshot
is taken**, and resumes listening on every restore.

### c. Host push `reseed_guest()`

A new function in `spike/src/bin/boot.rs`, called from `run_restore` **after
`manager.run_restored(...)` returns** (around `boot.rs:2256`, the point where the vCPUs are
live and the vsock reactor — spawned earlier at ~`boot.rs:1960` — is bound):

1. `getentropy()` 32 fresh bytes on the macOS host.
2. Open a `UnixStream` to the control UDS, send `CONNECT 9000\n`, await `OK 9000\n`
   (the existing hybrid control protocol; `crates/devices/src/virtio/vsock/muxer.rs:108`).
3. Write the framed message (below) as the connection payload.
4. **Retry** the CONNECT with bounded backoff (30 attempts × 100 ms ≈ 3 s; the restored
   socat listener needs the guest scheduler to reschedule it after vCPU resume) until the guest daemon
   accepts — covers the few ms before the guest reschedules `vmidd` after vCPU resume.
5. On total failure: **WARN and continue, non-fatal.** The vCPUs are already running; the
   guest still receives `virtio-rng` entropy over time. This mirrors the loud-but-continue
   philosophy of `--no-sandbox`.

### Flag

`--no-reseed` — visible escape hatch to skip the push (debugging / measuring the unseeded
window), in the spirit of `--no-sandbox`. Default: push on every `--restore`. Cold boot
(`boot` without `--restore`) does not push.

## Wire format

A minimal framed message on the vsock data channel, for forward compatibility:

```
offset  size  field
0       4     magic   = "VMID"  (0x56 0x4D 0x49 0x44)
4       1     version = 0x01
5       32    seed    (32 random bytes)
              total   = 37 bytes
```

The guest validates `magic` and `version` and that it read exactly 37 bytes before
reseeding. Anything else is ignored (connection dropped, daemon keeps listening).

## Data flow

```
boot --restore <name>
  ... mmap instance RAM, apply diffs, restore devices ...
  spawn_vsock_reactor (control UDS bound)        # boot.rs ~1960
  manager.run_restored(...)  -> vCPUs live       # boot.rs ~2256
  reseed_guest():                                # NEW, immediately after
    seed = getentropy(32)
    retry up to 30x/100ms:
      connect control UDS; send "CONNECT 9000\n"; await "OK 9000\n"
      write  "VMID" | 0x01 | seed                # 37 bytes
    on success -> done; on exhaustion -> WARN, continue

guest (already booted in snapshot):
  vmidd  accept() on AF_VSOCK port 9000
         read 37 bytes; validate "VMID"/0x01
         ioctl(RNDADDENTROPY, seed, 256 bits)
         ioctl(RNDRESEEDCRNG)
  -> CRNG diverges from siblings before first post-restore getrandom()
```

## Build integration

Compile `vmidd.c`, install the binary, and install the OpenRC service in **all three** rootfs
variants so plain / GUI / browser snapshots all carry the daemon:

- `kimage/build/build-rootfs.sh`
- `kimage/build/build-rootfs-gui.sh`
- `kimage/build/build-rootfs-browser.sh`

Build mechanism follows the existing `devmem.c` path (compile in the Docker builder, install
into the staging tree, embedded via `mke2fs -d`).

**Constraint:** rootfs build scripts run shell inside `sh -euxc '...'`. Do **not** use
apostrophes in comments or strings inside those blocks — an apostrophe terminates the single-
quoted command string and breaks the build. (Use the established `'"'"'` quoting workaround
only where genuinely needed.)

## Error handling

| Failure | Behavior |
|---|---|
| Host: CONNECT never accepted (10 retries exhausted) | WARN, continue. vCPUs already live; virtio-rng still feeds entropy. Non-fatal. |
| Host: `getentropy` fails | WARN, skip push, continue. |
| Guest: short / malformed message | Ignore, drop connection, keep listening. |
| Guest: ioctl fails | Log to console, keep listening. |

vmid never blocks restore completion and never aborts the VMM.

## Testing

- **Host unit** — `reseed_guest` seed generation yields distinct 32-byte values across calls;
  the framed message has correct magic / version / length bytes.
- **Guest helper unit** — the message parse/validate path accepts a well-formed 37-byte
  `VMID|0x01|seed` and rejects wrong magic, wrong version, and short reads.
- **Integration (automated)** — fan out 2 clones from one base; each reads
  `od -An -tx1 -N16 /dev/urandom` back to the host over vsock; assert the two reads differ.
- **Manual (immediate-window proof)** — with `virtio-rng` temporarily disabled, restore two
  clones with and without `--no-reseed`; assert the random reads are *identical* without vmid
  and *divergent* with it. This isolates the mechanism from virtio-rng masking. Human-run,
  like prior live-HVF checks.

## Sandbox / safety

No new `SandboxPaths`. The host uses the already-permitted control UDS (the vsock reactor
already binds and accepts on it) and `getentropy` (allowed). No change to `build_profile`.

## Live verification (2026-06-17)

Run end-to-end on M-series HVF (plain rootfs, vsock only, no `--net`) via
`scripts/vmid_live_proof.py`: cold-boot + snapshot a base, then restore clones.

**Verified working:** each reseeded clone restores in ~1 ms, the host prints
`vmid: pushed fresh CRNG seed to guest (vsock port 9000)`, and two reseeded clones
produce different `/dev/urandom` output. The full path — host `getentropy` →
vsock control `CONNECT 9000`/`OK` → 37-byte frame → guest socat → `vmid-reseed`
`RNDADDENTROPY`+`RNDRESEEDCRNG` — works on real hardware.

**Finding — the shared-CRNG bug does not reproduce observably on this platform,
even with virtio-rng disabled (`IGN_NO_RNG=1`).** The guest CPU exposes no arch RNG
(no `rng` in `/proc/cpuinfo` Features, so no `RNDR`), and `random: crng init done`
fires at t=0 from the fixed FDT `rng-seed` — so the CRNG state is genuinely
identical across clones at the instant of resume. But the kernel mixes
interrupt-timing entropy (`add_interrupt_randomness`) and reseeds within the first
scheduling quantum after resume, before any serial-shell read can run, so two
`--no-reseed` siblings still diverge. The window vmid closes is real but
sub-millisecond here. vmid remains correct, cheap insurance — and matters more for
guests that draw randomness in early userspace before interrupts flow, for
deterministic-replay scenarios, and on platforms/configs without continuous
interrupt-entropy mixing. Its practical necessity on bare HVF aarch64 with this
kernel is, by this measurement, low.

## Files

- Create: `kimage/build/vmidd.c` (guest daemon).
- Create: guest OpenRC service file (installed via the rootfs build).
- Modify: `kimage/build/build-rootfs.sh`, `build-rootfs-gui.sh`, `build-rootfs-browser.sh`
  (compile + install `vmidd` and its service).
- Modify: `spike/src/bin/boot.rs` (`reseed_guest()`, call site after `run_restored`,
  `--no-reseed` flag, `VMID_PORT` const).
