# Seatbelt sandbox

`boot` confines itself with a macOS Seatbelt profile (`sandbox_init`) applied late in
startup — after Hypervisor.framework is up, vmnet is started, and the vsock control
socket is bound, but before any guest code runs. The profile ships in the binary and
is on by default; `--no-sandbox` disables it (logged loudly), and an apply failure is
fatal (fail-closed).

## What v1 denies (targeted-deny)

The profile is `(allow default)` then carves out the high-value escape surfaces:

- **Network egress/ingress** — the VMM opens no IP sockets itself. vsock is
  AF_UNIX-local and vmnet rides vmnet.framework's XPC path, so neither is affected.
- **`process-exec*` / `process-fork`** — no spawning shells or helpers.
- **Filesystem writes** — denied everywhere except `/private/var/folders` (the
  `temp_dir()` CoW-clone root) and the declared VM-state dirs (the `--store`, plus the
  solutions/metrics/vsock paths in scope for the run mode). On a fresh boot the rootfs
  is *not* in the writable set, yet guest disk writes still work: virtio-blk opens the
  rootfs read+write before the profile is applied, and Seatbelt checks `file-write*` at
  `open()` time, not on writes through an already-open fd. (Restore writes a CoW
  instance copy under the store, so it is covered directly.)
- **Host secrets** — `~/.ssh`, `~/.aws`, `~/.gnupg`, and the Keychains are denied for
  both read and write. This block is emitted **last**, so it overrides a `--store`
  that a user points inside a secret dir (SBPL is last-match-wins).

## What v1 does NOT yet confine (honest gap)

v1 leaves arbitrary host **reads** and the full **mach** surface allowed (that is what
keeps HVF and vmnet working without enumerating undocumented service names). A
compromised VMM could still read most of the filesystem. Closing that is **v2**: flip
the base to `(deny default)` and grow an explicit allow-list (including the HVF and
vmnet mach services). The declared `readable` paths are already emitted as explicit
read-allows so that flip is a one-liner. The separate-uid privilege drop is also a
deferred follow-up.

## Threat-model line

With v1: egress, exec, arbitrary writes, and secret access are confined — "your own
code, your own machine" with a real process jail around the VMM. Multi-tenant /
untrusted positioning still waits on v2 (full read + mach confinement) and the uid
drop.
