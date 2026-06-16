# Seatbelt sandbox

`boot` confines itself with a macOS Seatbelt profile applied to the process at startup
— self-sandboxing, no root required. The profile is embedded in the binary and active
by default on every run path (boot, restore, fuzz).

## On by default; failure is fatal

The sandbox applies late in startup: after arguments are parsed, the kernel and rootfs
are open, Hypervisor.framework is up, vmnet is started, and the vsock control socket is
bound — immediately before the vCPU run loop begins. Threads already spawned at that
point (vCPU, vmnet RX feeder, vsock reactor) come under the profile immediately; it is
process-wide and irreversible.

Pass `--no-sandbox` to skip the apply. The flag is intentionally visible — the process
prints a loud warning and continues unconfined:

```
WARN: sandbox disabled (--no-sandbox) — VMM runs unconfined
```

If the profile fails to apply (the `sandbox_init` call returns non-zero), the process
prints the error and exits immediately:

```
FATAL: failed to apply sandbox: <errbuf text>
```

Fail-closed: the VMM never continues unsandboxed unless `--no-sandbox` is explicit.

## The allowlist model — `SandboxPaths`

The sandbox crate assembles `SandboxPaths` from the already-parsed config before
calling `apply`. Two sets of paths are declared:

**`readable`** — host files the VMM legitimately reads at runtime:

- The kernel `Image`
- The rootfs image
- The initramfs (when present)
- The restore base directory (when restoring from a snapshot chain)

These are emitted as explicit `(allow file-read* (subpath ...))` rules. They are
redundant under the current `(allow default)` base, but are already in place so a
future v2 deny-default flip requires no per-path changes.

**`writable`** — directories the VMM writes into at runtime:

- The snapshot store (`--store`)
- `/private/var/folders` (the system `temp_dir()` root used for CoW-clone staging)
- The vsock UDS parent directory (when `--vsock-uds` is set)
- Solutions directory (fuzz mode)

Writable paths are canonicalized and created if absent before the profile string is
rendered; a canonicalization failure is a fatal error.

One subtlety on fresh boot: the rootfs is opened read+write by the virtio-blk driver
*before* the sandbox applies. Seatbelt checks `file-write*` at `open()` time, not on
writes through an already-open fd, so guest disk writes keep working even though the
rootfs path is not in the writable set. Restore writes a copy-on-write instance under
the store, which is covered directly.

## What targeted-deny v1 confines

The profile is `(allow default)` with targeted denials carved out for the high-value
escape surfaces:

- **Network egress and ingress** — `(deny network-outbound (remote ip))` and
  `(deny network-inbound (remote ip))` block the VMM from opening IP sockets. vsock
  is AF_UNIX-local and is unaffected. vmnet moves L2 frames through vmnet.framework's
  XPC/dispatch path (not a BSD socket in the VMM process), so guest networking is
  unaffected.

- **Process execution and fork** — `(deny process-exec*)` and `(deny process-fork)`
  prevent a compromised VMM from spawning shells or helpers.

- **Filesystem writes** — `(deny file-write*)` blocks all writes, then re-allows only
  `/private/var/folders` and each canonicalized `writable` path. Everything else on the
  host filesystem is write-denied.

- **Host secrets** — `~/.ssh`, `~/.aws`, `~/.gnupg`, `~/Library/Keychains`, and
  `/Library/Keychains` are denied for both read *and* write. This block is always
  emitted last in the profile. SBPL is last-match-wins, so the secret deny overrides any
  user-supplied `--store` path that happens to overlap a secret directory.

## What v1 does not yet confine

v1 leaves arbitrary host **reads** allowed (other than the secret directories listed
above). A compromised VMM could still read most of the host filesystem. The full
**mach** surface is also left open — that is what keeps HVF and vmnet.framework working
without enumerating undocumented service names.

Closing that gap is the **v2** plan: flip the base to `(deny default)` and grow an
explicit allow-list that covers the HVF and vmnet mach services. The `readable` paths
are already declared and emitted as explicit read-allows so that flip is a one-liner in
`build_profile`. A separate-uid privilege drop (needs a provisioned account and root) is
a further deferred follow-up.

## Threat model

With v1: egress, exec, arbitrary writes, and host-secret reads are confined. "Your own
code, your own machine" with a real process jail around the VMM. Multi-tenant or
untrusted-workload positioning still waits on v2 (full read + mach confinement) and the
uid drop.

## Related

- [Sandbox internals](../internals/sandbox.md) — implementation notes and v2 design.
- [Snapshot & restore](snapshot-restore.md) — snapshot store paths that the sandbox
  keeps writable.
