# Seatbelt sandbox for the VMM process (v1) — Design

Date: 2026-06-15. Status: approved design, ready for an implementation plan.

## Context

The HVF *hardware* boundary (the guest can't touch host memory outside its mapped
RAM) is real and strong. The VMM *process* is not yet jailed: a hypothetical
guest-escape-into-the-VMM, or a bug in the VMM itself, currently runs with the full
authority of the invoking user — it can open network connections, exec helpers, and
read or write anywhere the user can. This **gates any "untrusted / multi-tenant"
positioning** in the adoption track (MCP server, CI executor): until the VMM process
is confined, the honest framing is "your own code, your own machine."

This spec adds a **Seatbelt (macOS `sandbox_init`) profile** that the VMM applies to
itself at startup, confining the high-value escape surfaces while leaving the
hypervisor and vmnet networking fully functional.

### Decisions locked in brainstorming

- **Seatbelt profile only.** The separate-uid privilege drop named in the roadmap is
  **deferred** (needs a provisioned account + root, conflicts with the no-sudo HVF
  story). v1 is a self-applied profile, no root required.
- **In-process, self-applied.** `sandbox_init(profile, 0, &err)` via FFI against
  libSystem, with a literal SBPL profile string (flags = 0). The jail ships in the
  binary and is always on — not an external `sandbox-exec` wrapper a caller might
  forget.
- **Applied late.** After args are parsed, kernel/rootfs are open, HVF is up, vmnet is
  started, the vsock `{uds}` listener is bound, and any restore reassembly is done —
  immediately before entering the vCPU run loop.
- **Targeted-deny v1, staged.** `(allow default)` then deny the high-value surfaces
  (network egress, exec/fork, writes outside the VM-state dirs, secret reads). Robust
  across macOS versions, ships now, networking intact. Structured so a v2 can flip to
  full `(deny default)` + allow-list. This is a documented, honest partial gate.

### Key finding that shapes the profile

vmnet (`--net`) moves L2 frames through **vmnet.framework's XPC/dispatch path** to a
privileged system daemon — it does **not** open a BSD `AF_INET` socket in the VMM
process (verified: `crates/vmnet` calls `ig_vmnet_start` in a C shim;
libxpc/libdispatch ride in libSystem). Therefore the sandbox can deny the VMM's *own*
outbound IP sockets (the exfiltration threat) and vmnet still works, provided the
profile leaves `mach-lookup`/XPC untouched. HVF likewise uses mach traps that
`(allow default)` leaves alone, so targeted-deny needs **zero** mach-service-name
enumeration.

## Goal

The VMM, by default, runs under a Seatbelt profile that denies: outbound/inbound IP
networking by the VMM process, `process-exec*` and `process-fork`, filesystem writes
outside the declared VM-state directories, and reads of common host-secret locations.
HVF, vmnet `--net`, vsock (AF_UNIX), snapshot-on-demand writes, and stdio all keep
working. The sandbox fails closed (apply error → exit non-zero) and has one visible
opt-out (`--no-sandbox`, logged loudly).

Non-goals (v1): confining arbitrary host *reads* or the full mach surface (that is the
v2 deny-default tightening); the separate-uid privilege drop; sandboxing on non-macOS
(the binary is macOS/aarch64-only).

## Architecture — new crate `crates/sandbox`

One responsibility: build and apply the SBPL profile. Public surface:

```rust
/// Host paths the sandboxed VMM must keep accessing at runtime.
pub struct SandboxPaths {
    /// Dirs/files the VMM reads (kernel, rootfs, initramfs, restore base).
    pub readable: Vec<PathBuf>,
    /// Dirs the VMM writes (snapshot store, temp_dir clones, solutions,
    /// metrics parent, vsock uds parent).
    pub writable: Vec<PathBuf>,
}

#[derive(Debug)]
pub enum SandboxError {
    /// sandbox_init returned non-zero; carries the freed errbuf text.
    Apply(String),
    /// a writable dir could not be created/canonicalized.
    Path(std::io::Error),
}

/// Render the profile for `paths` and apply it to the current process.
/// Process-wide: affects every thread. Idempotent is NOT required (called once).
pub fn apply(paths: &SandboxPaths) -> Result<(), SandboxError>;

/// Render the SBPL profile string (no syscall) — unit-testable.
pub fn build_profile(paths: &SandboxPaths) -> String;
```

FFI (declared locally; `sandbox_init` is deprecated and has no SDK header, but the
symbol is live in libSystem — no extra link needed):

```rust
unsafe extern "C" {
    fn sandbox_init(profile: *const c_char, flags: u64, errorbuf: *mut *mut c_char) -> c_int;
    fn sandbox_free_error(errorbuf: *mut c_char);
}
```

`apply`:
1. For each `writable` dir: create it if absent, then canonicalize (mapping IO errors
   to `SandboxError::Path`). For each `readable`: canonicalize (already validated as
   existing by the boot path before `apply` is called).
2. `let profile = build_profile(paths);` (CString).
3. `sandbox_init(profile, 0, &mut err)`; on non-zero, read `err` to a `String`,
   `sandbox_free_error(err)`, return `SandboxError::Apply(text)`.

`build_profile` interpolates each canonicalized path as a properly-escaped SBPL string
literal (escape `"` and `\`), so a path with spaces/quotes cannot inject profile
syntax.

## The SBPL profile (targeted-deny v1)

```scheme
(version 1)
(allow default)                          ; targeted-deny: permissive base, carve the risks

;; --- network egress: the VMM opens no IP sockets itself ---
;; (vsock is AF_UNIX-local; vmnet rides vmnet.framework's XPC path, not a socket here)
(deny network-outbound (remote ip))
(deny network-inbound  (remote ip))

;; --- no spawning shells/helpers out of a compromised VMM ---
(deny process-exec*)
(deny process-fork)

;; --- writes only to the declared VM-state dirs ---
(deny file-write*)
(allow file-write*
  (subpath "/private/var/folders")       ; temp_dir() CoW-clone root on macOS
  (subpath "{store}")                     ; each canonicalized SandboxPaths.writable
  (subpath "{writable_i}") ...)

;; --- host secrets: ALWAYS the last block, so these denies win over any
;;     user-supplied --store/writable path that overlaps a secret dir
;;     (SBPL is last-match-wins). Denies both read and write. ---
(deny file-read* file-write*
  (subpath "{home}/.ssh")
  (subpath "{home}/Library/Keychains")
  (subpath "/Library/Keychains")
  (subpath "{home}/.aws")
  (subpath "{home}/.gnupg"))
```

Rationale:

- **`(allow default)` first.** `mach-lookup`, `mach*`, `iokit*`, `sysctl*`, dyld,
  libdispatch/XPC stay allowed → HVF and vmnet work with no service enumeration. This
  is the deliberate v1 tradeoff (robustness over completeness).
- **`network-outbound (remote ip)`** blocks AF_INET/AF_INET6 egress while leaving
  `(local unix-socket)` alone → vsock UDS unaffected; vmnet unaffected (XPC).
- **`file-write*` deny-then-allow** is the one inverted island: writes denied
  everywhere except `/private/var/folders` (temp clones) and each canonicalized
  writable dir (store, solutions, metrics parent, uds parent).
- **secret denies** are belt-and-suspenders against a guest-escape reading *or
  writing* host creds. Reads are otherwise allowed (kernel/rootfs/dyld need broad read;
  enumerating every legit read is the brittle path v1 avoids). This block is emitted
  **last** and denies both `file-read*` and `file-write*`, so under SBPL last-match-wins
  it overrides the `file-write*` allow above — a user who points `--store` (or any
  writable path) inside a secret dir is refused, not granted. `build_profile` must
  always render this block after the writable-allow block; a unit test asserts the
  ordering.
- **Staged seam:** v2 flips the first line to `(deny default)` and grows an explicit
  allow-list (including the HVF + vmnet mach services). `build_profile` is the single
  place that changes.

## Integration in `spike/src/bin/boot.rs`

- A `--no-sandbox` flag (default false) parsed alongside the existing flags.
- A helper assembles `SandboxPaths` from the already-parsed config: `readable` =
  kernel + rootfs + initramfs (whichever are set) + the restore base dir; `writable` =
  `store`, `std::env::temp_dir()`, `solutions` (fuzz), `metrics_path` parent,
  `vsock_uds` parent (whichever are set).
- On each run path (boot, restore, fuzz), immediately before the vCPU run loop:
  - if `--no-sandbox`: `eprintln!("WARN: sandbox disabled (--no-sandbox)")` and skip.
  - else: `sandbox::apply(&paths)`; on `Err`, log it and `process::exit(1)`
    (fail-closed).
- Threads already spawned (vCPU, vmnet RX feeder, vsock reactor) keep running under
  the now-applied process-wide profile; no per-thread handling needed.

## Error handling

- **Fail closed**: apply error → log `errbuf` → exit non-zero. Never continue
  unsandboxed except via the explicit, logged `--no-sandbox`.
- **Missing writable dir**: created before canonicalize (the VMM owns these).
- **Missing readable path**: already a hard error earlier on the boot path; `apply` is
  not reached.
- **Path with odd characters**: escaped into the SBPL literal; no injection.

## Testing

Unit (`crates/sandbox`, no entitlement, pure string — run in CI):
1. **Profile renders** — `build_profile` output contains each canonicalized writable
   subpath, the `network-outbound`/`network-inbound` denies, `process-exec*`,
   `process-fork`, and the secret-read denies.
2. **SBPL escaping** — a writable path containing a space and a `"` is emitted as a
   valid, escaped SBPL string literal (assert no raw unescaped quote, no syntax break).
3. **Secret-deny is last** — the `(deny file-read* file-write* ... secret subpaths)`
   block appears at a byte offset *after* the `(allow file-write* ...)` block, even when
   a writable path is passed that overlaps a secret dir. Guards the last-match-wins
   guarantee.

Integration (macOS, actually calls `sandbox_init`; `#[cfg(target_os = "macos")]`):
4. **Apply succeeds** — `apply(&minimal_paths)` returns `Ok` on the test host.
5. **Egress denied** — after `apply`, `TcpStream::connect` to a routable host:port
   fails with `PermissionDenied`; an `AF_UNIX` connect under an allowed dir succeeds.
6. **Write-jail** — after `apply`, writing `temp_dir()/ign-sbtest` succeeds; writing
   `$HOME/ign-sbtest` fails with `PermissionDenied`.

   (Tests 4–6 each fork a child process that applies the sandbox and asserts, because
   `sandbox_init` is irreversible and process-wide — a sandboxed test process would
   poison sibling tests. The child reports via exit code.)

Live (manual, documented in the spec/docs, needs the entitlement + a kernel/rootfs):
- `boot --net ...` → guest still gets DHCP + connectivity under the sandbox (proves
  vmnet survives the egress deny).
- `Ctrl-A s` snapshot still writes the store under the sandbox.
- `boot --no-sandbox ...` prints the WARN line.

## File structure

- Create `crates/sandbox/` — `Cargo.toml`, `src/lib.rs` (`SandboxPaths`,
  `SandboxError`, `build_profile`, `apply`, FFI, tests).
- Add `crates/sandbox` to the workspace members.
- Modify `spike/Cargo.toml` — depend on `ignition-sandbox`.
- Modify `spike/src/bin/boot.rs` — `--no-sandbox` flag, `SandboxPaths` assembly, the
  late `apply` call on each run path.
- Modify `docs/src/internals/design-decisions.md` (or a new
  `docs/src/internals/sandbox.md` linked from SUMMARY) — document the v1 profile, the
  honest gap (reads + mach not yet confined), and the v2 deny-default plan.
- Modify `ROADMAP.md` — mark the Seatbelt item `[~]`/`[x]` v1, note the deferred
  uid-drop and v2 deny-default.

## End state

`boot` runs self-sandboxed by default: no IP egress, no exec/fork, no writes outside
VM-state dirs, no host-secret reads — while HVF, vmnet `--net`, vsock, snapshots, and
stdio keep working. A documented `--no-sandbox` opt-out exists for debugging. The honest
threat-model line moves from "no process jail at all" to "egress/exec/write/secret-read
confined; full read + mach confinement (deny-default) and uid drop are tracked v2
follow-ups." This is the first concrete step toward the untrusted-tenant positioning
the adoption track needs.
